# C-Two Java SDK

The Java 17 SDK uses the Maven coordinates `io.github.world-in-progress:c-two-sdk:0.13.0-rc.2` and depends on `io.github.world-in-progress:fastdb-java:0.4.0` for portable payloads. The JNI library projects Core's runtime, transport, contracts and lifecycle.

Generate a Java client and resource adapter from a portable contract descriptor using the c3 built from this checkout:

```sh
c3 contract codegen java contract.json --out-dir generated
```

Compile the sources under `generated/java/src/main/java` against the Java SDK and FastDB Java. A Runtime starts a Host and connects route-bound clients. The Host owns its registrations. Closing a Registration releases its handle. Use `Host.unregister(routeName)` to withdraw a route and observe its drain, or `Host.shutdown()` to drain and shut down the Host. The [generated Java example](../../cli/tests/fixtures/GeneratedJavaIpc.java) uses the [acceptance descriptor](src/test/resources/io/ctwo/generated/acceptance.contract.json).

Close client payloads and holds after use. A resource borrows a unary input until the callback returns, and a stream may borrow its input through `ServerStream.close()`. A resource transfers ownership of each returned payload or unary build plan to the SDK and may return its original borrowed input. Close FastDB Access pins before releasing a hold or returning from a callback. Hold release waits for remaining pins and invalidates checked views before returning the native backing.

## Build and verify

Build from the repository root with Rust 1.95 or newer, Maven and JDK 17. The native library is `libc2_java.dylib` on macOS, `libc2_java.so` on Linux and `c2_java.dll` on Windows. Provide its path through the `c2.java.library.path` Java property or the `C2_JAVA_LIBRARY_PATH` environment variable. The loader can also use a packaged classpath resource or the Java library search path.

Run the validation driver with the editable Python SDK, a prebuilt c3 and the official [FastDB 0.4.0 CoreSDK](https://github.com/world-in-progress/fastdb/releases/tag/v0.4.0). The FastDB source commit is `af2790068d87d34f19061cb02f4d1c0e7ba224d0` and the SHA-256 of its `release-manifest.json` is `3cd45cff112e545006293986d9d52b51df6587c6b7fcdd74de8778d9a6444dc2`. Set `FASTDB_PAYLOAD_LINK_MODE=system` and `FASTDB_PAYLOAD_SYSTEM_LIB_DIR` to the unpacked CoreSDK's `lib` directory for the Rust peer.

```sh
python tools/ci/test_java.py --python .venv/bin/python --c3 core/target/debug/c3 --output java-validation
```

On Windows, pass `.venv\Scripts\python.exe` and `core\target\debug\c3.exe`. With cached Maven dependencies and plugins, use `--maven-offline`. The driver builds and tests the JNI library, runs the JUnit suite with JNI checking, and exercises generated Java, Python and Rust bindings over direct IPC and relay HTTP. The output directory contains the logs and the validation receipt.

To bundle a JNI binary, stage it under `META-INF/native/<os>-<arch>/` in the directory configured by `c2.java.nativeResourcesDir`, which defaults to `target/native-resources` under this module. Run `mvn -f sdk/java/pom.xml -B -DskipTests package source:jar-no-fork`. Both JARs include `META-INF/LICENSE`. The sources JAR excludes native binaries.
