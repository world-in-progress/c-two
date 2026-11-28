package io.ctwo;

import io.ctwo.internal.NativeHandle;
import java.time.Duration;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;

/** A running Core Host that owns route registrations. */
public final class Host implements AutoCloseable {
    private final NativeHandle handle;
    private final AtomicBoolean closeRequested = new AtomicBoolean();

    Host(long id) {
        handle = new NativeHandle(id);
    }

    public String serverId() {
        return nativeIdentity(handle.id(), 0);
    }

    public String serverInstanceId() {
        return nativeIdentity(handle.id(), 1);
    }

    public String address() {
        return nativeIdentity(handle.id(), 2);
    }

    public Registration register(ServiceDefinition definition) {
        Objects.requireNonNull(definition);
        return new Registration(nativeRegister(handle.id(), definition.release().nativeId(),
                definition), definition.routeName());
    }

    /** Unregister this Host's route and report the native drain outcome. */
    public UnregisterOutcome unregister(String routeName) {
        return nativeUnregister(handle.id(), Objects.requireNonNull(routeName));
    }

    public boolean isRunning() {
        return nativeIsRunning(handle.id());
    }

    public HostLifecycleSnapshot lifecycleSnapshot() {
        return nativeLifecycleSnapshot(handle.id());
    }

    public MemoryStatsSnapshot memoryStats() {
        return nativeMemoryStats(handle.id());
    }

    public CallExecutionSnapshot callExecutionSnapshot() {
        return nativeCallExecutionSnapshot(handle.id());
    }

    /** Start or observe shutdown with the Host's configured wait. */
    public ShutdownOutcome shutdown() {
        return nativeShutdown(handle.id(), null);
    }

    /** Start or observe shutdown with a caller-specific wait. */
    public ShutdownOutcome shutdown(Duration timeout) {
        return nativeShutdown(handle.id(), Objects.requireNonNull(timeout));
    }

    @Override
    public void close() {
        if (closeRequested.compareAndSet(false, true)) {
            try {
                shutdown();
                handle.close();
            } catch (RuntimeException | Error failure) {
                closeRequested.set(false);
                throw failure;
            }
        }
    }

    private native String nativeIdentity(long id, int field);
    private native long nativeRegister(long id, long releaseId, ServiceDefinition definition);
    private native UnregisterOutcome nativeUnregister(long id, String routeName);
    private native boolean nativeIsRunning(long id);
    private native HostLifecycleSnapshot nativeLifecycleSnapshot(long id);
    private native MemoryStatsSnapshot nativeMemoryStats(long id);
    private native CallExecutionSnapshot nativeCallExecutionSnapshot(long id);
    private native ShutdownOutcome nativeShutdown(long id, Duration timeout);
}
