import io.ctwo.CallOptions;
import io.ctwo.Client;
import io.ctwo.ConnectMode;
import io.ctwo.ContractRelease;
import io.ctwo.Held;
import io.ctwo.Host;
import io.ctwo.HostOptions;
import io.ctwo.Registration;
import io.ctwo.Runtime;
import io.ctwo.RuntimeOptions;
import io.ctwo.generated.GeneratedContractSupport;
import io.ctwo.generated.GeneratedMethod;
import io.ctwo.internal.NativeCalls;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.Builder;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import io.fastdb.payload.View;

import java.io.File;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Arrays;
import java.util.Map;
import java.util.Random;

public final class JavaPackageSmoke {
    public static void main(String[] args) throws Exception {
        require(System.getProperty("c2.java.library.path") == null, "Unexpected JNI path override");
        require(System.getenv("C2_JAVA_LIBRARY_PATH") == null, "Unexpected JNI path override");
        var options = RuntimeOptions.defaults().withoutRelay();
        if (File.separatorChar == '/') {
            options = options.withUnixEndpointRoot(Path.of("ipc").toAbsolutePath());
        }
        String route = "java-package-smoke";
        Duration wait = Duration.ofSeconds(10);
        byte[] expected = new byte[64 * 1024];
        new Random(713).nextBytes(expected);

        ContractRelease contract = ContractRelease.fromDescriptorJson(Files.readAllBytes(Path.of(args[0])));
        try (Runtime runtime = Runtime.create(options);
                Host host = runtime.startHost(HostOptions.defaults().withoutRelay()
                        .withServerId("java-smoke-" + ProcessHandle.current().pid()));
                CompiledSpec spec = CompiledSpec.compile(Files.readString(Path.of(args[1])));
                Builder builder = Builder.create(spec);
                BuildPlan plan = builder.entryBegin(0, 1).valueBytes(expected).freeze()) {
            var echo = new GeneratedMethod("echo", spec, spec);
            var ping = new GeneratedMethod("ping", null, null);
            var scan = new GeneratedMethod("scan", spec, spec);
            var service = GeneratedContractSupport.service(contract, route, Map.of(
                    ping, input -> null,
                    echo, input -> {
                        require(Arrays.equals(expected, bytes(input)), "IPC request differs");
                        return plan.retain();
                    }), Map.of(scan, input -> {
                        throw new AssertionError("Unexpected stream call");
                    }));
            try (Registration registration = host.register(service);
                    Client client = runtime.connect(contract.expectedRoute(route),
                            ConnectMode.directIpc(host.address()), wait, CallOptions.defaults().withTimeout(wait))) {
                require(route.equals(registration.name()), "Registration failed");
                long[] before = NativeCalls.directSlabCounts();
                Held<Payload> response = GeneratedContractSupport.hold(client, echo, plan);
                try (response) {
                    require(Arrays.equals(expected, bytes(response.value())), "Held IPC response differs");
                }
                require(response.isReleased(), "Held response was not released");
                long[] after = NativeCalls.directSlabCounts();
                require(after[0] == before[0] + 1 && after[1] == before[1] + 1 && after[2] == before[2],
                        "Request and response must each use one direct SHM slab");
            }
            require(host.shutdown().completed() && host.lifecycleSnapshot().finished(), "Host shutdown did not complete");
        }
        System.out.println("JAVA_PACKAGE_SMOKE_PASSED");
    }

    private static byte[] bytes(Payload payload) {
        try (View entry = payload.entryView(0); View value = entry.at(0)) {
            return value.getBytes();
        }
    }

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
