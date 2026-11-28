package io.ctwo;

import io.ctwo.internal.NativeHandle;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import java.util.Objects;

/** A handle for observing a Host registration and acquiring its topic publishers. */
public final class Registration implements AutoCloseable {
    private final NativeHandle handle;
    private final String routeName;

    Registration(long id, String routeName) {
        handle = new NativeHandle(id);
        this.routeName = routeName;
    }

    public String name() {
        return routeName;
    }

    public String routeName() {
        return routeName;
    }

    public boolean isClosed() {
        return handle.isClosed() || nativeIsClosed(handle.id());
    }

    public long activeSubscribers() {
        return handle.isClosed() ? 0 : nativeActiveSubscribers(handle.id());
    }

    public Publisher<Payload> publisher(String topicName) {
        return publisher(topicName, null);
    }

    public Publisher<Payload> publisher(String topicName, CompiledSpec spec) {
        return new Publisher<>(nativePublisher(handle.id(), Objects.requireNonNull(topicName)), topicName, spec);
    }

    /** Release this handle. The route remains live until Host.unregister or Host shutdown. */
    @Override
    public void close() {
        handle.close();
    }

    private native boolean nativeIsClosed(long id);
    private native long nativeActiveSubscribers(long id);
    private native long nativePublisher(long id, String name);
}
