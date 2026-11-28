import io.ctwo.CallOptions;
import io.ctwo.CallOutcomeStatus;
import io.ctwo.ConnectMode;
import io.ctwo.Host;
import io.ctwo.HostOptions;
import io.ctwo.Publisher;
import io.ctwo.Registration;
import io.ctwo.Runtime;
import io.ctwo.RuntimeOptions;
import io.ctwo.ServerStream;
import io.ctwo.SubscribeOptions;
import io.ctwo.TopicStart;
import io.ctwo.generated.test.java.AcceptanceContract;
import io.ctwo.internal.NativeCalls;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.BuildPolicy;
import io.fastdb.payload.Builder;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;

/** Consumer of c3's Java output for the SDK acceptance descriptor. */
public final class GeneratedJavaIpc {
    private static final String ROUTE = "generated-java";
    private static final Duration TIMEOUT = Duration.ofSeconds(10);

    private GeneratedJavaIpc() {}

    public static void main(String[] args) throws Exception {
        var release = AcceptanceContract.contractRelease();
        release.verifyReleaseReference(Files.readString(Path.of(args[0])));
        check(release.canonicalJson().equals(AcceptanceContract.CONTRACT_DESCRIPTOR_JSON), "descriptor bytes");
        check(release.descriptorSha256().equals(AcceptanceContract.DESCRIPTOR_SHA256), "descriptor hash");
        check(release.abiHash().equals(AcceptanceContract.ABI_HASH), "ABI hash");
        check(release.signatureHash().equals(AcceptanceContract.SIGNATURE_HASH), "signature hash");

        byte[] large = new byte[64 * 1024];
        for (int i = 0; i < large.length; i++) {
            large[i] = (byte) i;
        }
        byte[] initial = "initial".getBytes(StandardCharsets.UTF_8);
        byte[] keyed = "keyed".getBytes(StandardCharsets.UTF_8);
        var methods = AcceptanceContract.METHODS;
        check(methods.size() == 3 && AcceptanceContract.TOPICS.size() == 2, "binding tables");
        var pings = new AtomicInteger();
        var echoes = new AtomicInteger();
        try (CompiledSpec spec = CompiledSpec.compile("""
                {"schema":"fastdb.payload.v1","profile":"record.v1","components":[],
                 "entries":[{"id":"value","cardinality":"one","type":{"kind":"bytes"}}]}
                """);
                BuildPlan direct = plan(spec, large);
                BuildPlan initialPlan = plan(spec, initial);
                BuildPlan keyedPlan = plan(spec, keyed);
                Payload keyedInput = payload(keyedPlan);
                Runtime runtime = Runtime.create(RuntimeOptions.defaults().withoutRelay());
                Host host = runtime.startHost(HostOptions.defaults().withoutRelay()
                        .withServerId("c1java-" + ProcessHandle.current().pid()))) {
            var resource = new AcceptanceContract.Service() {
                @Override public void method_0_ping() {
                    pings.incrementAndGet();
                }

                @Override public Payload method_1_echo(Payload input) {
                    echoes.incrementAndGet();
                    return input;
                }

                @Override public BuildPlan method_1_echo_plan(Payload input) {
                    return bytes(input).length == large.length ? direct.retain() : null;
                }

                @Override public ServerStream<Payload> method_2_scan(Payload input) {
                    return new ServerStream<>() {
                        private int remaining = 2;
                        @Override public Payload next() {
                            return remaining-- > 0 ? input.retain() : null;
                        }
                        @Override public void close() {}
                    };
                }
            };
            var service = AcceptanceContract.topicService(ROUTE, resource)
                    .topic_0_state_capacity(2)
                    .initial_topic_0_state(initialPlan)
                    .initial_topic_1_ticks(keyedInput)
                    .build();
            var route = AcceptanceContract.expectedRoute(ROUTE);
            try (Registration registration = host.register(service);
                    Publisher<Payload> publisher = AcceptanceContract.registrationTopics(registration)
                            .topic_0_state();
                    var client = new AcceptanceContract.ContractClient(runtime.connect(
                            route, ConnectMode.directIpc(host.address()), TIMEOUT, CallOptions.defaults()));
                    var timed = client.withCallOptions(CallOptions.defaults().withTimeout(TIMEOUT))) {
                check(timed.expectedRoute().equals(route) && TIMEOUT.equals(timed.callOptions().timeout()),
                        "client route and call options");
                timed.method_0_ping();
                check(pings.get() == 1, "void unary");

                long[] before = NativeCalls.directSlabCounts();
                try (Payload result = timed.method_1_echo(direct)) {
                    checkBytes(result, large);
                }
                long[] after = NativeCalls.directSlabCounts();
                check(after[0] == before[0] + 1 && after[1] == before[1] + 1 && echoes.get() == 0,
                        "direct request and response slabs");

                try (var held = timed.hold_method_1_echo_with_call_id("once", keyedInput);
                        Payload replay = timed.method_1_echo_with_call_id("once", keyedPlan);
                        var recovered = timed.recover_hold_method_1_echo("once");
                        Payload recoveredOwned = timed.recover_method_1_echo("once")) {
                    checkBytes(held.value(), keyed);
                    checkBytes(replay, keyed);
                    checkBytes(recovered.value(), keyed);
                    checkBytes(recoveredOwned, keyed);
                }
                check(timed.queryCall("once").status() == CallOutcomeStatus.SUCCEEDED && echoes.get() == 1,
                        "keyed result applies once");

                try (var stream = timed.method_2_scan(keyedInput);
                        Payload first = stream.nextItem().orElseThrow();
                        var second = stream.nextItemHeld().orElseThrow()) {
                    checkBytes(first, keyed);
                    checkBytes(second.value(), keyed);
                    check(stream.nextItem().isEmpty(), "stream completion");
                }
                try (var state = timed.subscribe_topic_0_state(SubscribeOptions.defaults());
                        var ticks = timed.subscribe_topic_1_ticks(SubscribeOptions.defaults()
                                .withStart(TopicStart.LATEST));
                        Payload stateValue = state.nextDelivery().value();
                        Payload tickValue = ticks.nextDelivery().value()) {
                    checkBytes(stateValue, initial);
                    checkBytes(tickValue, keyed);
                    check(publisher.publish(direct) == 2, "publish plan");
                    try (var held = state.nextDeliveryHeld().value()) {
                        checkBytes(held.value(), large);
                    }
                    check(NativeCalls.directSlabCounts()[2] == after[2] + 1, "direct topic slab");
                }
                try (Payload decoded = AcceptanceContract.decode_method_1_echo_input(
                        AcceptanceContract.encode_method_1_echo_input(keyedInput))) {
                    checkBytes(decoded, keyed);
                }
            }
            check(host.shutdown(TIMEOUT).completed(), "host shutdown");
        }
        System.out.println("PASS generated Java IPC: unary, keyed recovery, stream, topics, direct slabs, release hashes");
    }

    private static BuildPlan plan(CompiledSpec spec, byte[] value) {
        try (Builder builder = Builder.create(spec)) {
            return builder.entryBegin(0, 1).valueBytes(value).freeze();
        }
    }

    private static Payload payload(BuildPlan plan) {
        try (var result = plan.execute(BuildPolicy.ALLOW_STAGING)) {
            return result.payload().retain();
        }
    }

    private static byte[] bytes(Payload payload) {
        try (var entry = payload.entryView(0); var value = entry.at(0)) {
            return value.getBytes();
        }
    }

    private static void checkBytes(Payload payload, byte[] expected) {
        check(Arrays.equals(bytes(payload), expected), "payload bytes");
    }

    private static void check(boolean condition, String operation) {
        if (!condition) {
            throw new AssertionError(operation);
        }
    }
}
