package io.ctwo;

import io.ctwo.internal.NativeCalls;
import io.ctwo.internal.NativeHandle;
import java.time.Duration;
import java.util.Optional;
import java.util.function.LongFunction;

/** A Core server-stream receiver. Each decoded item owns its local bytes. */
public final class ResponseStream<T> implements AutoCloseable {
    private final NativeHandle handle;
    private final LongFunction<T> decode;
    private final LongFunction<Held<T>> decodeHeld;

    ResponseStream(long id, LongFunction<T> decode, LongFunction<Held<T>> decodeHeld) {
        handle = new NativeHandle(id);
        this.decode = decode;
        this.decodeHeld = decodeHeld;
    }

    public Optional<T> nextItem() {
        long item = NativeCalls.streamNext(handle.id(), false);
        return item == 0 ? Optional.empty() : Optional.of(decode.apply(item));
    }

    public Optional<Held<T>> nextItemHeld() {
        long item = NativeCalls.streamNext(handle.id(), true);
        return item == 0 ? Optional.empty() : Optional.of(decodeHeld.apply(item));
    }

    public void cancel() { NativeCalls.streamCancel(handle.id()); }

    public StreamCloseReceipt closeReceipt() {
        return NativeCalls.streamClose(handle.id(), -1, 0);
    }

    public StreamCloseReceipt closeReceipt(Duration timeout) {
        if (timeout.isNegative()) {
            throw new IllegalArgumentException("Negative close timeout");
        }
        return NativeCalls.streamClose(handle.id(), timeout.getSeconds(), timeout.getNano());
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
