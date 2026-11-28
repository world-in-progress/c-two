package io.ctwo;

import io.ctwo.internal.NativeCalls;
import io.ctwo.internal.NativeHandle;
import java.time.Duration;
import java.util.function.LongFunction;

/** A topic receiver with independent delivery and subscription lifetimes. */
public final class Subscription<T> implements AutoCloseable {
    private final NativeHandle handle;
    private final String topicName;
    private final long startSeq;
    private final LongFunction<T> decode;
    private final LongFunction<Held<T>> decodeHeld;

    Subscription(long id, String topicName, LongFunction<T> decode, LongFunction<Held<T>> decodeHeld) {
        handle = new NativeHandle(id);
        this.topicName = topicName;
        this.decode = decode;
        this.decodeHeld = decodeHeld;
        try {
            startSeq = NativeCalls.subscriptionStartSeq(handle.id());
        } catch (RuntimeException | Error failure) {
            handle.close();
            throw failure;
        }
    }

    public String topicName() { return topicName; }
    public long startSeq() { return startSeq; }

    public Delivery<T> nextDelivery() {
        long[] item = NativeCalls.topicNext(handle.id(), false);
        return new Delivery<>(item[0], item[1], decode.apply(item[2]));
    }

    public Delivery<Held<T>> nextDeliveryHeld() {
        long[] item = NativeCalls.topicNext(handle.id(), true);
        return new Delivery<>(item[0], item[1], decodeHeld.apply(item[2]));
    }

    public void cancel() { NativeCalls.topicCancel(handle.id()); }

    public TopicCloseReceipt closeReceipt() {
        return NativeCalls.topicClose(handle.id(), -1, 0);
    }

    public TopicCloseReceipt closeReceipt(Duration timeout) {
        if (timeout.isNegative()) {
            throw new IllegalArgumentException("Negative close timeout");
        }
        return NativeCalls.topicClose(handle.id(), timeout.getSeconds(), timeout.getNano());
    }

    @Override
    public void close() {
        if (!handle.isClosed()) {
            try (handle) {
                closeReceipt();
            }
        }
    }
}
