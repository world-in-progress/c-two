package io.ctwo;

import java.util.Objects;
import java.util.concurrent.atomic.AtomicReference;

/** A local response or topic item whose checked FastDB views expire on close. */
public final class Held<T> implements AutoCloseable {
    private record Owner<T>(T value, Runnable release) {}

    private final AtomicReference<Owner<T>> owner;

    public Held(T value, Runnable release) {
        owner = new AtomicReference<>(new Owner<>(value, Objects.requireNonNull(release)));
    }

    public T value() {
        Owner<T> current = owner.get();
        if (current == null) {
            throw new IllegalStateException("Held payload is closed");
        }
        return current.value();
    }

    public boolean isReleased() {
        return owner.get() == null;
    }

    /** Waits for FastDB access pins to drain. Close any access pins before releasing this hold. */
    public void release() {
        Owner<T> current = owner.getAndSet(null);
        if (current != null) {
            current.release().run();
        }
    }

    @Override
    public void close() {
        release();
    }
}
