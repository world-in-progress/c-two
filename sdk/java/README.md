# C-Two Java SDK

The Java 17 SDK is `io.github.world-in-progress:c-two-sdk:0.13.0-rc.2`. It uses the FastDB Java 0.4.0 binding for portable payloads. The JNI library projects Core's runtime, transport, contracts, and lifecycle.

A Runtime creates a Host and route-bound Clients. The Host owns its registrations. Closing a Registration releases its handle. Use `Host.unregister(routeName)` to withdraw the route and observe its drain, or `Host.shutdown()` to shut down the Host. The `io.ctwo.generated` package provides bindings for generated contract clients and resource adapters. The [acceptance contract](src/test/java/io/ctwo/generated/AcceptanceContract.java) shows how these bindings fit together.

Close client payloads and holds when finished. A resource's unary input is borrowed until its callback returns. A producer can read its borrowed stream input through `ServerStream.close()`. A resource transfers ownership of each returned payload or unary build plan to the SDK and may return its original borrowed input. Close any FastDB Access pin before releasing the hold or returning from its callback. Hold release waits for remaining pins and invalidates checked views before returning the native backing.

## Building locally

Use Rust 1.95 or newer, Maven, and JDK 17. For offline Maven builds, preload FastDB Java 0.4.0, JUnit 5.10.2, and the plugins pinned in the POM. The SDK loads `c2_java` using `c2.java.library.path`, then `C2_JAVA_LIBRARY_PATH`, then the matching classpath resource, then the Java library search path. To bundle a native binary, place it under `META-INF/native/<os>-<arch>/` in the directory configured by `c2.java.nativeResourcesDir`. Its default is `target/native-resources` under this module.

Run these commands from the repository root on macOS.

```sh
export JAVA_HOME="$(/usr/libexec/java_home -v 17)"
cargo test --manifest-path sdk/java/native/Cargo.toml
cargo build --manifest-path sdk/java/native/Cargo.toml --release
cargo tree --manifest-path sdk/java/native/Cargo.toml -e normal
mvn -o -f sdk/java/pom.xml -B test \
  -Dc2.java.library.path="$PWD/sdk/java/native/target/release/libc2_java.dylib"
mvn -o -f sdk/java/pom.xml -B package source:jar-no-fork javadoc:jar -DskipTests
```

The default test JVM runs with `-Xcheck:jni -Djava.awt.headless=true`. On Linux use `libc2_java.so`, and on Windows use `c2_java.dll`. The supplied integration tests use direct IPC.

Core currently reads relay client proxy and TLS policy from `C2_RELAY_USE_PROXY`, `C2_RELAY_CA_CERT`, and `C2_RELAY_CERT_SHA256`. The Host advertises the IPC payload size configured through `HostOptions.maxPayloadSize`. Per-Runtime overrides for these policies require a Core configuration API.

## Local verification

Verified on macOS arm64 with Corretto 17.0.11, Maven 3.9.7, Rust 1.99.1, and the locally installed FastDB Java 0.4.0. The Core checkout was `cb73b80f3e48c697ba2cb7d3a4b062bd0109359b` with the SDK sources in this directory.

- `cargo test` reported `1 passed; 0 failed`. The release build exited successfully.
- The normal dependency tree contained zero entries for `fastdb`, `fastdb-sys`, `iroh`, `iroh-relay`, `noq`, or `quinn`.
- Offline Maven tests reported `Tests run: 11, Failures: 0, Errors: 0, Skipped: 0` under JNI checking. The tests cover Host drain, keyed calls, admission and call context, streams and topics, buffer lifetime, large payload integrity, direct slab handoff and fallback, and structured errors.
- Offline packaging built the main, sources, and Javadoc JARs. A staged macOS dylib appeared in the main JAR and was excluded from the sources JAR. The direct IPC lifecycle test also passed using the classpath resource loader.
