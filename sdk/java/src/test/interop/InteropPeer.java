import io.ctwo.AdmissionDecision;
import io.ctwo.CallMetadata;
import io.ctwo.CallOptions;
import io.ctwo.CallOutcomeStatus;
import io.ctwo.ConnectMode;
import io.ctwo.HostOptions;
import io.ctwo.Publisher;
import io.ctwo.Runtime;
import io.ctwo.RuntimeOptions;
import io.ctwo.ServerStream;
import io.ctwo.SubscribeOptions;
import io.ctwo.error.C2Exception;
import io.ctwo.error.TransportPhase;
import io.ctwo.generated.test.interop.InteropContract;
import io.ctwo.internal.NativeCalls;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.BuildPolicy;
import io.fastdb.payload.Builder;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import io.fastdb.payload.View;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Duration;
import java.util.Arrays;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Function;

public final class InteropPeer {
    private static final String ROUTE = "java-interop";
    private static final Duration TIMEOUT = Duration.ofSeconds(20);
    private static final String TEXT = "跨语言";
    private static final int BYTE = 231;
    private static final long COUNT = 4_000_000_007L;
    private static final double SCALE = -2.625;
    private static final byte[] DATA = new byte[1024 * 1024];

    static {
        for (int index = 0; index < DATA.length; index++) {
            DATA[index] = (byte) index;
        }
    }

    private InteropPeer() {}

    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args[1]);
        boolean direct = args[2].equals("direct");
        try (var echoSpec = CompiledSpec.compile(Files.readString(fixtures.resolve("echo.json")));
                var numberSpec = CompiledSpec.compile(Files.readString(fixtures.resolve("number.json")))) {
            switch (args[0]) {
                case "host" -> host(echoSpec, numberSpec, direct, args[3], Path.of(args[4]));
                case "client" -> client(echoSpec, numberSpec, direct, args[3]);
                default -> throw new IllegalArgumentException("Unknown peer mode " + args[0]);
            }
        }
    }

    private static BuildPlan echoPlan(CompiledSpec spec, byte[] data) {
        try (var builder = Builder.create(spec)) {
            return builder.entryBegin(0, 1).valueStr(TEXT)
                    .entryBegin(1, 1).valueU8(BYTE)
                    .entryBegin(2, 1).valueU32(COUNT)
                    .entryBegin(3, 1).valueF64(SCALE)
                    .entryBegin(4, 1).valueBytes(data).freeze();
        }
    }

    private static Payload number(CompiledSpec spec, long value) {
        try (var builder = Builder.create(spec);
                var plan = builder.entryBegin(0, 1).valueU32(value).freeze();
                var result = plan.execute(BuildPolicy.ALLOW_STAGING)) {
            return result.payload().retain();
        }
    }

    private static <T> T read(Payload payload, int index, Function<View, T> getter) {
        try (var entry = payload.entryView(index); var view = entry.at(0)) {
            return getter.apply(view);
        }
    }

    private static byte[] digest(byte[] data) {
        try {
            return MessageDigest.getInstance("SHA-256").digest(data);
        } catch (NoSuchAlgorithmException error) {
            throw new AssertionError(error);
        }
    }

    private static byte[] verifyEcho(Payload payload) {
        check(TEXT.equals(read(payload, 0, View::getStr)), "echo str");
        check(read(payload, 1, View::getU8) == BYTE, "echo u8");
        check(read(payload, 2, View::getU32) == COUNT, "echo u32");
        check(read(payload, 3, View::getF64) == SCALE, "echo f64");
        byte[] data = read(payload, 4, View::getBytes);
        check(data.length == DATA.length && Arrays.equals(digest(data), digest(DATA)), "echo bytes SHA-256");
        return data;
    }

    private static final class Resource implements InteropContract.Service {
        private final CompiledSpec echoSpec;
        private final CompiledSpec numberSpec;
        private final AtomicInteger pings = new AtomicInteger();
        private final AtomicInteger applies = new AtomicInteger();
        private volatile Publisher<Payload> publisher;

        Resource(CompiledSpec echoSpec, CompiledSpec numberSpec) {
            this.echoSpec = echoSpec;
            this.numberSpec = numberSpec;
        }

        @Override
        public void method_0_ping() {
            if (pings.incrementAndGet() == 2) {
                try (var next = number(numberSpec, 20)) {
                    check(publisher.publish(next) == 2, "live publish sequence");
                }
            }
        }

        @Override
        public Payload method_1_echo(Payload input) {
            verifyEcho(input);
            return input;
        }

        @Override
        public BuildPlan method_1_echo_plan(Payload input) {
            return echoPlan(echoSpec, verifyEcho(input));
        }

        @Override
        public Payload method_2_apply(Payload input) {
            return number(numberSpec, read(input, 0, View::getU32) + applies.incrementAndGet());
        }

        @Override
        public ServerStream<Payload> method_3_scan() {
            return new ServerStream<>() {
                private int next = 31;

                @Override
                public Payload next() {
                    return next <= 33 ? number(numberSpec, next++) : null;
                }

                @Override
                public void close() {}
            };
        }
    }

    private static void host(CompiledSpec echoSpec, CompiledSpec numberSpec,
            boolean direct, String serverId, Path stop) throws Exception {
        var resource = new Resource(echoSpec, numberSpec);
        var options = HostOptions.defaults().withoutRelay().withServerId(serverId)
                .withAdmissionHook(context -> "deny".equals(context.metadata().get("interop"))
                        ? AdmissionDecision.deny("fixture denial", Map.of("reason", "interop"))
                        : AdmissionDecision.allow());
        try (var runtime = Runtime.create(RuntimeOptions.defaults().withoutRelay());
                var host = runtime.startHost(options);
                var initial = number(numberSpec, 10);
                var registration = host.register(InteropContract.topicService(ROUTE, resource)
                        .initial_topic_0_events(initial).build());
                var publisher = InteropContract.registrationTopics(registration).topic_0_events()) {
            resource.publisher = publisher;
            long before = NativeCalls.directSlabCounts()[1];
            System.out.println("READY " + host.address());
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(120);
            while (!Files.exists(stop)) {
                check(System.nanoTime() < deadline, "Host shutdown signal");
                Thread.sleep(25);
            }
            check(host.shutdown(TIMEOUT).completed(), "host shutdown completed");
            check(resource.pings.get() == 2 && resource.applies.get() == 1, "Host dispatch counts");
            if (direct) {
                check(NativeCalls.directSlabCounts()[1] == before + 1, "direct response slab");
                System.out.println("PASS Java direct response slab");
            }
        }
        System.out.println("PASS host shutdown");
    }

    private static void client(CompiledSpec echoSpec, CompiledSpec numberSpec,
            boolean direct, String address) {
        var mode = direct ? ConnectMode.directIpc(address) : ConnectMode.explicitRelay(address);
        var options = CallOptions.defaults().withTimeout(TIMEOUT);
        try (var runtime = Runtime.create(RuntimeOptions.defaults().withoutRelay());
                var client = new InteropContract.ContractClient(runtime.connect(
                        InteropContract.expectedRoute(ROUTE), mode, TIMEOUT, options))) {
            client.method_0_ping();
            long before = NativeCalls.directSlabCounts()[0];
            try (var plan = echoPlan(echoSpec, DATA); var held = client.hold_method_1_echo(plan)) {
                verifyEcho(held.value());
            }
            if (direct) {
                check(NativeCalls.directSlabCounts()[0] == before + 1, "direct request slab");
                System.out.println("PASS Java direct request slab and held response");
            }

            try (var request = number(numberSpec, 41);
                    var first = client.method_2_apply_with_call_id("apply-once", request);
                    var replay = client.method_2_apply_with_call_id("apply-once", request);
                    var recovered = client.recover_method_2_apply("apply-once")) {
                check(read(first, 0, View::getU32) == 42, "keyed result");
                check(read(replay, 0, View::getU32) == 42, "keyed replay");
                check(read(recovered, 0, View::getU32) == 42, "keyed recovery");
            }
            check(client.queryCall("apply-once").status() == CallOutcomeStatus.SUCCEEDED, "keyed query");

            try (var stream = client.method_3_scan()) {
                for (int expected = 31; expected <= 33; expected++) {
                    try (var item = stream.nextItem().orElseThrow()) {
                        check(read(item, 0, View::getU32) == expected, "scan item");
                    }
                }
                check(stream.nextItem().isEmpty(), "scan terminal");
            }

            try (var sub = client.subscribe_topic_0_events(SubscribeOptions.defaults())) {
                var initial = sub.nextDelivery();
                try (var item = initial.value()) {
                    check(initial.seq() == 1 && initial.dropped() == 0
                            && read(item, 0, View::getU32) == 10, "initial topic item");
                }
                client.method_0_ping();
                var next = sub.nextDelivery();
                try (var item = next.value()) {
                    check(next.seq() == 2 && next.dropped() == 0
                            && read(item, 0, View::getU32) == 20, "live topic item");
                }
            }

            try (var denied = client.withCallOptions(options.withMetadata(
                    CallMetadata.of(Map.of("interop", "deny"))))) {
                try {
                    denied.method_0_ping();
                    throw new AssertionError("metadata must deny the call before dispatch");
                } catch (C2Exception error) {
                    check(error.errorCode().value() == 724
                            && error.transportPhase() == TransportPhase.PRE_DISPATCH
                            && "interop".equals(error.details().get("reason")), "structured admission error");
                }
            }
        }
        System.out.println("PASS client operations");
    }

    private static void check(boolean condition, String operation) {
        if (!condition) {
            throw new AssertionError(operation);
        }
    }
}
