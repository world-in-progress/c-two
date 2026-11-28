package io.ctwo.generated;

import io.ctwo.AdmissionDecision;
import io.ctwo.CallContext;
import io.ctwo.CallMetadata;
import io.ctwo.CallOptions;
import io.ctwo.CallOutcomeStatus;
import io.ctwo.Client;
import io.ctwo.ConnectMode;
import io.ctwo.Delivery;
import io.ctwo.Held;
import io.ctwo.Host;
import io.ctwo.HostOptions;
import io.ctwo.LagPolicy;
import io.ctwo.Publisher;
import io.ctwo.Registration;
import io.ctwo.ResponseStream;
import io.ctwo.Runtime;
import io.ctwo.RuntimeOptions;
import io.ctwo.ServerStream;
import io.ctwo.ServiceDefinition;
import io.ctwo.SubscribeOptions;
import io.ctwo.Subscription;
import io.ctwo.TopicStart;
import io.ctwo.error.C2Exception;
import io.ctwo.error.ErrorCode;
import io.ctwo.error.TransportPhase;
import io.ctwo.internal.NativeCalls;
import io.fastdb.payload.Access;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.BuildPolicy;
import io.fastdb.payload.Builder;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.FastDbException;
import io.fastdb.payload.Payload;
import io.fastdb.payload.View;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;
import org.junit.jupiter.api.function.Executable;

import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.Map;
import java.util.Random;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.BooleanSupplier;

import static io.ctwo.generated.AcceptanceContract.RELEASE;
import static io.ctwo.generated.AcceptanceContract.bytes;
import static io.ctwo.generated.AcceptanceContract.payload;
import static io.ctwo.generated.AcceptanceContract.plan;
import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

@Timeout(35)
final class JavaSdkAcceptanceTest {
    private static final Duration WAIT = Duration.ofSeconds(10);
    private static final String ROUTE = "java-acceptance";
    private static final AtomicInteger HOST_IDS = new AtomicInteger();
    private static final HostOptions HOST_OPTIONS = HostOptions.defaults().withoutRelay();
    private static final CallOptions CALL_OPTIONS = CallOptions.defaults().withTimeout(WAIT);

    @TempDir
    Path ipcRoot;

    @Test
    void unregisterAndShutdownDrainAnEnteredUnary() throws Exception {
        var entered = new CountDownLatch(1);
        var finish = new CountDownLatch(1);
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS)) {
            try (Registration registration = host.register(echoService(input -> input))) {
                assertEquals(ROUTE, registration.name());
                assertFalse(registration.isClosed());
                assertFalse(host.lifecycleSnapshot().listenerClosed());
            }
            try (Client client = connect(runtime, host);
                    Payload request = payload(utf8("before unregister"));
                    Payload result = new AcceptanceContract.Stub(client).echo(request)) {
                assertNull(new AcceptanceContract.Stub(client).ping());
                assertArrayEquals(utf8("before unregister"), bytes(result));
            }
            var removed = host.unregister(ROUTE);
            assertTrue(removed.localRemoved());
            assertTrue(removed.close().activeDrained());
            try (Registration registered = host.register(echoService(input -> {
                        entered.countDown();
                        await(finish);
                        return input;
                    }));
                    Client client = connect(runtime, host);
                    Payload request = payload(utf8("in flight"))) {
                assertFalse(registered.isClosed());
                var stub = new AcceptanceContract.Stub(client);
                var result = CompletableFuture.supplyAsync(() -> {
                    try (Payload response = stub.echo(request)) {
                        return bytes(response);
                    }
                });
                try {
                    await(entered);
                    assertFalse(host.shutdown(Duration.ofMillis(25)).completed());
                    assertFalse(host.lifecycleSnapshot().finished());
                    assertFalse(result.isDone());
                } finally {
                    finish.countDown();
                }
                assertArrayEquals(utf8("in flight"), result.get(WAIT.toSeconds(), TimeUnit.SECONDS));
                assertTrue(host.shutdown(WAIT).completed());
                var finished = host.lifecycleSnapshot();
                assertTrue(finished.finished());
                assertTrue(finished.listenerClosed());
                assertEquals(Boolean.TRUE, finished.workDrained());
            }
        }
    }

    @Test
    void buildPlanUsesCoreSlabsForLargeRequestAndResponseAndFallbackForSmall() {
        byte[] large = randomBytes(64 * 1024);
        byte[] small = utf8("inline");
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                BuildPlan largePlan = plan(large); BuildPlan smallPlan = plan(small);
                Registration registration = host.register(echoService(
                        input -> bytes(input).length == small.length ? smallPlan.retain() : largePlan.retain()));
                Client client = connect(runtime, host)) {
            assertFalse(registration.isClosed());
            var stub = new AcceptanceContract.Stub(client);
            long[] before = NativeCalls.directSlabCounts();
            try (Payload output = stub.echo(smallPlan)) {
                assertArrayEquals(small, bytes(output));
            }
            assertArrayEquals(before, NativeCalls.directSlabCounts());
            try (Payload output = stub.echo(largePlan)) {
                assertArrayEquals(large, bytes(output));
            }
            long[] after = NativeCalls.directSlabCounts();
            assertEquals(before[0] + 1, after[0], "large request committed into a Core slab");
            assertEquals(before[1] + 1, after[1], "large response committed into a Core slab");
            assertEquals(before[2], after[2]);
        }
    }

    @Test
    void fourMiBPayloadHasExactDigestAndHeldReleaseInvalidatesViews() throws Exception {
        byte[] content = randomBytes(4 * 1024 * 1024);
        byte[] digest = MessageDigest.getInstance("SHA-256").digest(content);
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(echoService(input -> input));
                Client client = connect(runtime, host); Payload input = payload(content)) {
            assertFalse(registration.isClosed());
            var stub = new AcceptanceContract.Stub(client);
            try (Payload owned = stub.echo(input)) {
                assertArrayEquals(digest, MessageDigest.getInstance("SHA-256").digest(bytes(owned)));
            }

            var held = stub.holdEcho(input);
            try (held; View entry = held.value().entryView(0); View value = entry.at(0)) {
                assertArrayEquals(digest, MessageDigest.getInstance("SHA-256").digest(value.getBytes()));
                assertEquals(1L, runtime.memoryStats().holds().activeHolds());
                assertTrue(runtime.memoryStats().holds().totalHeldBytes() >= content.length);
                held.release();
                assertInvalidated(value);
            }
            assertEquals(0L, runtime.memoryStats().holds().activeHolds());
            assertEquals(0L, runtime.memoryStats().holds().totalHeldBytes());
        }
    }

    @Test
    void heldReleaseWaitsForAnExistingAccessPin() throws Exception {
        byte[] content = utf8("pinned response");
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(echoService(input -> input));
                Client client = connect(runtime, host); Payload input = payload(content);
                Held<Payload> held = new AcceptanceContract.Stub(client).holdEcho(input);
                View entry = held.value().entryView(0); View value = entry.at(0)) {
            assertFalse(registration.isClosed());
            CompletableFuture<Void> released;
            try (Access pin = value.acquire()) {
                released = CompletableFuture.runAsync(held::release);
                await(held::isReleased);
                await(() -> rejectsNewAccess(value));
                assertFalse(released.isDone(), "release waits for the live FastDB access pin");
                assertEquals(1L, runtime.memoryStats().holds().activeHolds());
                assertArrayEquals(content, pin.toByteArray());
            }
            released.get(WAIT.toSeconds(), TimeUnit.SECONDS);
            assertInvalidated(value);
            assertEquals(0L, runtime.memoryStats().holds().activeHolds());
        }
    }

    @Test
    void keyedCallReplaysAndRecoversAcrossClientsAndRejectsConflict() {
        var executed = new AtomicInteger();
        try (Runtime runtime = runtime(); Runtime other = runtime();
                Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(echoService(input -> {
                    executed.incrementAndGet();
                    return input;
                }));
                Client first = connect(runtime, host); Client second = connect(other, host);
                Payload request = payload(utf8("once")); Payload changed = payload(utf8("different"))) {
            assertFalse(registration.isClosed());
            var firstStub = new AcceptanceContract.Stub(first);
            var secondStub = new AcceptanceContract.Stub(second);
            String callId = "operation-1";
            try (Held<Payload> original = firstStub.holdEchoKeyed(callId, request);
                    Payload replay = secondStub.echoKeyed(callId, request);
                    Payload recovered = secondStub.recoverEcho(callId);
                    Held<Payload> heldRecovery = secondStub.holdRecoverEcho(callId)) {
                assertArrayEquals(utf8("once"), bytes(original.value()));
                assertArrayEquals(bytes(original.value()), bytes(replay));
                assertArrayEquals(bytes(original.value()), bytes(recovered));
                assertArrayEquals(bytes(original.value()), bytes(heldRecovery.value()));
            }
            var outcome = second.queryCall(callId);
            assertEquals(CallOutcomeStatus.SUCCEEDED, outcome.status());
            assertTrue(outcome.resultAvailable());
            assertEquals("echo", outcome.methodName());
            assertNull(outcome.error());
            var conflict = fails(ErrorCode.CALL_ID_CONFLICT, () -> secondStub.echoKeyed(callId, changed));
            assertEquals(TransportPhase.PRE_DISPATCH, conflict.transportPhase());
            assertEquals(1, executed.get());
        }
    }

    @Test
    void admissionAndResourceCallContextPreserveMetadata() {
        var observed = new AtomicReference<CallContext>();
        var options = HOST_OPTIONS.withAdmissionHook(context -> {
            String token = context.metadata().entries().get("token");
            return "allowed".equals(token)
                    ? AdmissionDecision.allow()
                    : AdmissionDecision.deny("missing token", Map.of("required", "token"));
        });
        try (Runtime runtime = runtime(); Host host = host(runtime, options);
                Registration registration = host.register(echoService(input -> {
                    observed.set(CallContext.current().orElseThrow());
                    return input;
                }));
                Client denied = connect(runtime, host);
                Client allowed = connect(runtime, host, CALL_OPTIONS.withMetadata(CallMetadata.of(
                        Map.of("token", "allowed", "trace", "java-test"))));
                Payload input = payload(utf8("metadata"))) {
            assertFalse(registration.isClosed());
            var refusal = fails(ErrorCode.ADMISSION_DENIED, () -> new AcceptanceContract.Stub(denied).echo(input));
            assertEquals(TransportPhase.PRE_DISPATCH, refusal.transportPhase());
            assertEquals("token", refusal.details().get("required"));
            assertNull(observed.get());
            try (Payload result = new AcceptanceContract.Stub(allowed).echoKeyed("metadata-call", input)) {
                assertArrayEquals(utf8("metadata"), bytes(result));
            }
            var context = observed.get();
            assertNotNull(context);
            assertEquals(ROUTE, context.routeName());
            assertEquals("echo", context.methodName());
            assertEquals("metadata-call", context.callId());
            assertEquals(Map.of("token", "allowed", "trace", "java-test"), context.metadata().entries());
            assertEquals(CallContext.Transport.IPC, context.transport());
            assertTrue(CallContext.current().isEmpty());
        }
    }

    @Test
    void generatedAdapterAndResourceErrorsRemainStructured() {
        String otherSpec = """
                {"schema":"fastdb.payload.v1","profile":"record.v1",
                 "entries":[{"id":"other","cardinality":"one","type":{"kind":"bytes"}}],"components":[]}
                """;
        var executed = new AtomicInteger();
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                CompiledSpec mismatched = CompiledSpec.compile(otherSpec);
                Builder builder = Builder.create(mismatched);
                BuildPlan wrongPlan = builder.entryBegin(0, 1).valueBytes(utf8("wrong")).freeze();
                var wrong = wrongPlan.execute(BuildPolicy.ALLOW_STAGING);
                Registration registration = host.register(echoService(input -> {
                    return switch (executed.incrementAndGet()) {
                        case 1 -> throw new C2Exception(ErrorCode.ADMISSION_DENIED, "inner refusal",
                                Map.of("transport_phase", "pre_dispatch"));
                        case 2 -> throw new AssertionError("callback failed");
                        default -> wrongPlan.retain();
                    };
                }));
                Client client = connect(runtime, host); Payload good = payload(utf8("right"))) {
            assertFalse(registration.isClosed());
            var stub = new AcceptanceContract.Stub(client);
            var invalidPayload = fails(ErrorCode.CLIENT_INPUT_SERIALIZING, () -> stub.echo(wrong.payload()));
            assertEquals(TransportPhase.PRE_DISPATCH, invalidPayload.transportPhase());
            assertDigestMismatch(invalidPayload);
            var invalidPlan = fails(ErrorCode.CLIENT_INPUT_SERIALIZING, () -> stub.echo(wrongPlan));
            assertEquals(TransportPhase.PRE_DISPATCH, invalidPlan.transportPhase());
            assertDigestMismatch(invalidPlan);
            assertEquals(0, executed.get());

            var normalized = fails(ErrorCode.RESOURCE_FUNCTION_EXECUTING, () -> stub.echo(good));
            assertEquals("724", normalized.details().get("cause_code"));
            assertNull(normalized.transportPhase());
            var unexpected = fails(ErrorCode.RESOURCE_FUNCTION_EXECUTING, () -> stub.echo(good));
            assertNull(unexpected.transportPhase());
            var invalidResponse = fails(ErrorCode.RESOURCE_OUTPUT_SERIALIZING, () -> stub.echo(good));
            assertDigestMismatch(invalidResponse);
            assertNull(invalidResponse.transportPhase());
            assertEquals(3, executed.get());
        }
    }

    @Test
    void finiteAndEarlyClosedStreamsRunCleanupAndKeepDeliveredHolds() {
        var opens = new AtomicInteger();
        var finiteCleanup = new CountDownLatch(1);
        var earlyCleanup = new CountDownLatch(1);
        var service = AcceptanceContract.service(ROUTE, input -> input, input ->
                opens.getAndIncrement() == 0 ? sequence(input, 2, finiteCleanup)
                        : sequence(input, Integer.MAX_VALUE, earlyCleanup));
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(service); Client client = connect(runtime, host);
                Payload input = payload(utf8("stream item"))) {
            assertFalse(registration.isClosed());
            var stub = new AcceptanceContract.Stub(client);
            try (ResponseStream<Payload> stream = stub.scan(input);
                    Payload first = stream.nextItem().orElseThrow();
                    Held<Payload> second = stream.nextItemHeld().orElseThrow()) {
                assertArrayEquals(utf8("stream item"), bytes(first));
                assertTrue(stream.nextItem().isEmpty());
                var terminal = stream.closeReceipt(WAIT);
                assertTrue(terminal.completed());
                assertEquals(2L, terminal.completeItems());
                assertNull(terminal.error());
                await(finiteCleanup);
                assertArrayEquals(utf8("stream item"), bytes(second.value()));
            }
            try (ResponseStream<Payload> stream = stub.scan(input);
                    Held<Payload> item = stream.nextItemHeld().orElseThrow()) {
                var terminal = stream.closeReceipt(WAIT);
                assertTrue(terminal.completed());
                assertEquals(ErrorCode.CALL_CANCELLED, terminal.error().errorCode());
                assertTrue(terminal.error().getMessage().contains("consumer"));
                await(earlyCleanup);
                assertArrayEquals(utf8("stream item"), bytes(item.value()));
            }
            assertEquals(0L, runtime.memoryStats().holds().activeHolds());
        }
    }

    @Test
    void runtimeShutdownDeliversRunningStreamStepAndCancelsBetweenSteps() throws InterruptedException {
        var entered = new CountDownLatch(1);
        var finish = new CountDownLatch(1);
        var cleaned = new CountDownLatch(1);
        var steps = new AtomicInteger();
        var service = AcceptanceContract.service(ROUTE, input -> input, input -> new ServerStream<>() {
            @Override
            public Payload next() {
                steps.incrementAndGet();
                entered.countDown();
                await(finish);
                return input.retain();
            }

            @Override
            public void close() {
                cleaned.countDown();
            }
        });
        try (Runtime runtime = runtime(); Runtime consumer = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(service); Client client = connect(consumer, host);
                Payload input = payload(utf8("last item"));
                ResponseStream<Payload> stream = new AcceptanceContract.Stub(client).scan(input)) {
            try {
                await(entered);
                assertFalse(runtime.shutdown(Duration.ofMillis(25)).completed());
                assertTrue(registration.isClosed(), "retirement sealed the route");
                await(() -> shutdownClosedCatalog(consumer, host));
            } finally {
                finish.countDown();
            }
            try (Payload delivered = stream.nextItem().orElseThrow()) {
                assertArrayEquals(utf8("last item"), bytes(delivered));
            }
            var cancelled = fails(ErrorCode.CALL_CANCELLED, stream::nextItem);
            assertTrue(cancelled.getMessage().contains("shutdown"));
            var terminal = stream.closeReceipt(WAIT);
            assertTrue(terminal.completed());
            assertEquals(1L, terminal.completeItems());
            assertEquals(ErrorCode.CALL_CANCELLED, terminal.error().errorCode());
            await(cleaned);
            assertTrue(runtime.shutdown(WAIT).completed());
            assertTrue(host.lifecycleSnapshot().finished());
            assertTrue(registration.isClosed());
            assertEquals(1, steps.get());
        }
    }

    @Test
    void topicsPublishPayloadsAndPlansAndKeepHoldsAfterSubscriberClose() {
        byte[] initial = utf8("initial");
        byte[] second = randomBytes(64 * 1024);
        byte[] third = utf8("third");
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Payload initialPayload = payload(initial); BuildPlan secondPlan = plan(second);
                Payload thirdPayload = payload(third);
                Registration registration = host.register(echoService(input -> input)
                        .withTopicCapacity("state", 2)
                        .withInitialTopicBytes("state", initialPayload.binaryBytes()));
                Publisher<Payload> publisher = AcceptanceContract.state(registration);
                Client client = connect(runtime, host)) {
            var stub = new AcceptanceContract.Stub(client);
            assertEquals(1, publisher.headSeq(), "initial state exists before subscription");
            try (Subscription<Payload> latest = stub.state(SubscribeOptions.defaults().withStart(TopicStart.LATEST));
                    Subscription<Payload> next = stub.state(SubscribeOptions.defaults().withStart(TopicStart.NEXT))) {
                assertEquals(1, latest.startSeq());
                assertEquals(2, next.startSeq());
                assertDelivery(latest.nextDelivery(), 1, 0, initial);
                long[] before = NativeCalls.directSlabCounts();
                assertEquals(2, publisher.publish(secondPlan));
                assertEquals(before[2] + 1, NativeCalls.directSlabCounts()[2]);
                assertDelivery(next.nextDelivery(), 2, 0, second);

                assertHeldDeliveryAfterClose(latest, second);
                assertEquals(0, runtime.memoryStats().holds().activeHolds());
                assertEquals(3, publisher.publish(thirdPayload));
                assertDelivery(next.nextDelivery(), 3, 0, third);

                var removed = CompletableFuture.supplyAsync(() -> host.unregister(ROUTE));
                var cancelled = fails(ErrorCode.CALL_CANCELLED, next::nextDelivery);
                assertTrue(cancelled.getMessage().contains("route unregistered"));
                assertTrue(removed.orTimeout(WAIT.toSeconds(), TimeUnit.SECONDS).join().localRemoved());
                assertTrue(registration.isClosed());
                fails(ErrorCode.RESOURCE_CLOSED, () -> publisher.publish(thirdPayload));
            }
        }
    }

    @Test
    void boundedTopicReportsSkippedSequencesAndErrorPolicy() {
        try (Runtime runtime = runtime(); Host host = host(runtime, HOST_OPTIONS);
                Registration registration = host.register(echoService(input -> input).withTopicCapacity("ticks", 2));
                Publisher<Payload> publisher = AcceptanceContract.ticks(registration);
                Client client = connect(runtime, host)) {
            var stub = new AcceptanceContract.Stub(client);
            var next = SubscribeOptions.defaults().withStart(TopicStart.NEXT);
            try (Subscription<Payload> skip = stub.ticks(next.withLagPolicy(LagPolicy.SKIP));
                    Subscription<Payload> strict = stub.ticks(next.withLagPolicy(LagPolicy.ERROR))) {
                publishTick(publisher, 1);
                assertDelivery(skip.nextDelivery(), 1, 0, utf8("tick-1"));
                assertDelivery(strict.nextDelivery(), 1, 0, utf8("tick-1"));
                for (int sequence = 2; sequence <= 16; ++sequence) {
                    publishTick(publisher, sequence);
                }

                long previous = 1;
                boolean skipped = false;
                while (previous < 16) {
                    Delivery<Payload> delivery = skip.nextDelivery();
                    long sequence = delivery.seq();
                    assertTrue(sequence > previous);
                    long gap = sequence - previous - 1;
                    assertDelivery(delivery, sequence, gap, utf8("tick-" + sequence));
                    skipped |= gap > 0;
                    previous = sequence;
                }
                assertTrue(skipped, "the bounded ring dropped old sequences");

                C2Exception lag = null;
                for (int received = 1; received < 16; ++received) {
                    try {
                        var delivery = strict.nextDelivery();
                        assertDelivery(delivery, received + 1, 0, utf8("tick-" + (received + 1)));
                    } catch (C2Exception error) {
                        lag = error;
                        break;
                    }
                }
                assertNotNull(lag);
                assertEquals(ErrorCode.SUBSCRIPTION_LAGGED, lag.errorCode());
            }
        }
    }

    private static void assertHeldDeliveryAfterClose(Subscription<Payload> subscription, byte[] expected) {
        Delivery<Held<Payload>> delivered = subscription.nextDeliveryHeld();
        try (Held<Payload> held = delivered.value()) {
            assertEquals(2, delivered.seq());
            assertEquals(0, delivered.dropped());
            assertTrue(subscription.closeReceipt(WAIT).completed());
            subscription.close();
            assertArrayEquals(expected, bytes(held.value()));
        }
    }

    private static void assertDelivery(Delivery<Payload> delivery, long seq, long dropped, byte[] expected) {
        try (Payload value = delivery.value()) {
            assertEquals(seq, delivery.seq());
            assertEquals(dropped, delivery.dropped());
            assertArrayEquals(expected, bytes(value));
        }
    }

    private static void publishTick(Publisher<Payload> publisher, int sequence) {
        try (Payload tick = payload(utf8("tick-" + sequence))) {
            assertEquals(sequence, publisher.publish(tick));
        }
    }

    private Runtime runtime() {
        var options = RuntimeOptions.defaults().withoutRelay();
        if (java.io.File.separatorChar == '/') {
            options = options.withUnixEndpointRoot(ipcRoot);
        }
        return Runtime.create(options);
    }

    private static Host host(Runtime runtime, HostOptions options) {
        return runtime.startHost(options.withServerId(
                "java-" + ProcessHandle.current().pid() + "-" + HOST_IDS.incrementAndGet()));
    }

    private static Client connect(Runtime runtime, Host host) {
        return connect(runtime, host, CALL_OPTIONS);
    }

    private static Client connect(Runtime runtime, Host host, CallOptions options) {
        return runtime.connect(RELEASE.expectedRoute(ROUTE), ConnectMode.directIpc(host.address()), WAIT, options);
    }

    private static ServiceDefinition echoService(UnaryCodec echo) {
        return AcceptanceContract.service(ROUTE, echo, input -> sequence(input, 0, new CountDownLatch(0)));
    }

    private static ServerStream<Payload> sequence(Payload item, int count, CountDownLatch cleaned) {
        return new ServerStream<>() {
            private int remaining = count;

            @Override
            public Payload next() {
                return remaining-- > 0 ? item.retain() : null;
            }

            @Override
            public void close() {
                cleaned.countDown();
            }
        };
    }

    private static boolean shutdownClosedCatalog(Runtime runtime, Host host) {
        try {
            Client probe = connect(runtime, host);
            probe.close();
            return false;
        } catch (C2Exception error) {
            assertEquals(ErrorCode.RESOURCE_CLOSED, error.errorCode(), error::getMessage);
            return true;
        }
    }

    private static boolean rejectsNewAccess(View value) {
        try {
            value.getBytes();
            return false;
        } catch (FastDbException error) {
            assertEquals(FastDbException.E_VIEW_INVALIDATED, error.code());
            return true;
        }
    }

    private static void assertInvalidated(View value) {
        var invalidated = assertThrows(FastDbException.class, value::getBytes);
        assertEquals(FastDbException.E_VIEW_INVALIDATED, invalidated.code());
    }

    private static void assertDigestMismatch(C2Exception error) {
        assertEquals("fastdb", error.details().get("cause_owner"));
        assertEquals(Integer.toString(FastDbException.E_DIGEST_MISMATCH), error.details().get("fastdb_code"));
        assertEquals("DIGEST_MISMATCH", error.details().get("fastdb_symbol"));
        assertTrue(error.details().containsKey("fastdb_path"));
        assertTrue(error.details().containsKey("fastdb_details_json"));
    }

    private static C2Exception fails(ErrorCode code, Executable operation) {
        var error = assertThrows(C2Exception.class, operation);
        assertEquals(code, error.errorCode(), error::getMessage);
        return error;
    }

    private static void await(CountDownLatch latch) {
        try {
            assertTrue(latch.await(WAIT.toSeconds(), TimeUnit.SECONDS), "callback barrier timed out");
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            throw new AssertionError(error);
        }
    }

    private static void await(BooleanSupplier condition) throws InterruptedException {
        long start = System.nanoTime();
        while (!condition.getAsBoolean() && System.nanoTime() - start < WAIT.toNanos()) {
            Thread.sleep(1);
        }
        assertTrue(condition.getAsBoolean(), "condition timed out");
    }

    private static byte[] utf8(String value) {
        return value.getBytes(StandardCharsets.UTF_8);
    }

    private static byte[] randomBytes(int length) {
        byte[] bytes = new byte[length];
        new Random(1876).nextBytes(bytes);
        return bytes;
    }
}
