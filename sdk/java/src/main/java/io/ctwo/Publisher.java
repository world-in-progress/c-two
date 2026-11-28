package io.ctwo;

import io.ctwo.internal.NativeCalls;
import io.ctwo.internal.NativeHandle;
import io.ctwo.internal.PayloadBridge;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import java.util.Objects;

/** Publishes frames to a topic on a live registration. */
public final class Publisher<T> implements AutoCloseable {
    private final NativeHandle handle;
    private final String topicName;
    private final CompiledSpec spec;

    Publisher(long id, String topicName, CompiledSpec spec) {
        handle = new NativeHandle(id);
        this.topicName = topicName;
        this.spec = spec;
    }

    public long publish(Payload payload) {
        return publishEncoded(payload);
    }

    public long publish(BuildPlan plan) {
        return publishEncoded(plan);
    }

    private long publishEncoded(Object input) {
        return NativeCalls.publish(handle.id(), PayloadBridge.encoding(spec, Objects.requireNonNull(input)));
    }

    public String topicName() { return topicName; }
    public boolean isClosed() { return handle.isClosed() || NativeCalls.publisherIsClosed(handle.id()); }
    public long headSeq() { return NativeCalls.publisherHeadSeq(handle.id()); }

    @Override
    public void close() {
        handle.close();
    }
}
