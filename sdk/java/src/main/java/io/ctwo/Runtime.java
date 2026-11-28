package io.ctwo;

import io.ctwo.internal.NativeHandle;
import io.ctwo.internal.NativeLibraryLoader;
import java.time.Duration;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;

/** Owns a Core runtime's client and server role domains. */
public final class Runtime implements AutoCloseable {
    private static final Duration DEFAULT_CLOSE_WAIT = Duration.ofSeconds(5);

    static {
        NativeLibraryLoader.load();
    }

    private final NativeHandle handle;
    private final AtomicBoolean closeRequested = new AtomicBoolean();

    private Runtime(long id) {
        handle = new NativeHandle(id);
    }

    public static Runtime create() {
        return create(RuntimeOptions.defaults());
    }

    public static Runtime create(RuntimeOptions options) {
        return new Runtime(nativeCreate(Objects.requireNonNull(options)));
    }

    public Host startHost() {
        return startHost(HostOptions.defaults());
    }

    public Host startHost(HostOptions options) {
        return new Host(nativeStartHost(handle.id(), Objects.requireNonNull(options)));
    }

    public Client connect(ExpectedRouteContract expected, ConnectMode mode) {
        return connect(expected, mode, null, CallOptions.defaults());
    }

    public Client connect(ExpectedRouteContract expected, ConnectMode mode, CallOptions callDefaults) {
        return connect(expected, mode, null, callDefaults);
    }

    /** The acquisition timeout is independent of the defaults for subsequent calls. */
    public Client connect(ExpectedRouteContract expected, ConnectMode mode, Duration connectTimeout,
                          CallOptions callDefaults) {
        Objects.requireNonNull(expected);
        Objects.requireNonNull(mode);
        Objects.requireNonNull(callDefaults);
        long id = nativeConnect(handle.id(), expected, mode.kind().ordinal(), mode.address(), connectTimeout, callDefaults);
        return new Client(id, expected, callDefaults);
    }

    public MemoryStatsSnapshot memoryStats() {
        return nativeMemoryStats(handle.id());
    }

    public CallExecutionSnapshot callExecutionSnapshot() {
        return nativeCallExecutionSnapshot(handle.id());
    }

    /** Permanently retire the runtime and observe its Core shutdown transaction for up to this duration. */
    public ShutdownOutcome shutdown(Duration timeout) {
        return nativeShutdown(handle.id(), Objects.requireNonNull(timeout));
    }

    @Override
    public void close() {
        if (closeRequested.compareAndSet(false, true)) {
            try {
                shutdown(DEFAULT_CLOSE_WAIT);
                handle.close();
            } catch (RuntimeException | Error failure) {
                closeRequested.set(false);
                throw failure;
            }
        }
    }

    private static native long nativeCreate(RuntimeOptions options);
    private native long nativeStartHost(long id, HostOptions options);
    private native long nativeConnect(long id, ExpectedRouteContract expected, int kind, String address,
                                      Duration timeout, CallOptions callDefaults);
    private native MemoryStatsSnapshot nativeMemoryStats(long id);
    private native CallExecutionSnapshot nativeCallExecutionSnapshot(long id);
    private native ShutdownOutcome nativeShutdown(long id, Duration timeout);
}
