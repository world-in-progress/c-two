package io.ctwo.internal;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Locale;

/** Loads the JNI library once for this class loader. */
public final class NativeLibraryLoader {
    private NativeLibraryLoader() {}

    public static void load() {
        Loaded.ensureLoaded();
    }

    private static native void initialize(ClassLoader loader);

    private static final class Loaded {
        static {
            loadLibrary();
            initialize(NativeLibraryLoader.class.getClassLoader());
        }

        private static void ensureLoaded() {}
    }

    private static void loadLibrary() {
        String path = System.getProperty("c2.java.library.path");
        if (path == null || path.isBlank()) {
            path = System.getenv("C2_JAVA_LIBRARY_PATH");
        }
        if (path != null && !path.isBlank()) {
            System.load(Path.of(path).toAbsolutePath().toString());
            return;
        }
        String osName = System.getProperty("os.name").toLowerCase(Locale.ROOT);
        String os = osName.startsWith("windows") ? "windows"
                : osName.startsWith("mac") ? "macos" : osName.startsWith("linux") ? "linux" : "";
        String architecture = switch (System.getProperty("os.arch").toLowerCase(Locale.ROOT)) {
            case "amd64", "x86_64" -> "x86_64";
            case "aarch64", "arm64" -> "arm64";
            default -> System.getProperty("os.arch");
        };
        String library = System.mapLibraryName("c2_java");
        String resource = "/META-INF/native/" + os + "-" + architecture + "/" + library;
        try (InputStream input = NativeLibraryLoader.class.getResourceAsStream(resource)) {
            if (input != null) {
                Path directory = Files.createTempDirectory("c2-java-");
                directory.toFile().deleteOnExit();
                Path staging = directory.resolve(library + ".part");
                staging.toFile().deleteOnExit();
                Path nativeFile = directory.resolve(library);
                nativeFile.toFile().deleteOnExit();
                Files.copy(input, staging);
                Files.move(staging, nativeFile, StandardCopyOption.ATOMIC_MOVE);
                System.load(nativeFile.toAbsolutePath().toString());
                return;
            }
        } catch (IOException failure) {
            throw (UnsatisfiedLinkError) new UnsatisfiedLinkError("Cannot extract " + resource).initCause(failure);
        }
        System.loadLibrary("c2_java");
    }
}
