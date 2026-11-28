package io.ctwo.internal;

import java.lang.ref.Cleaner;
import java.util.concurrent.atomic.AtomicLong;

/** Owns one opaque JNI handle. Core operations take a strong reference before use. */
public final class NativeHandle implements AutoCloseable {
    private static final Cleaner CLEANER = Cleaner.create();

    static {
        NativeLibraryLoader.load();
    }

    private final Release release;
    private final Cleaner.Cleanable cleanable;

    public NativeHandle(long handle) {
        if (handle == 0) {
            throw new IllegalArgumentException("Null native handle");
        }
        release = new Release(handle);
        cleanable = CLEANER.register(this, release);
    }

    public long id() {
        long id = release.id.get();
        if (id == 0) {
            throw new IllegalStateException("Native handle is closed");
        }
        return id;
    }

    public boolean isClosed() {
        return release.id.get() == 0;
    }

    @Override
    public void close() {
        cleanable.clean();
    }

    private static native void drop(long id);

    private static final class Release implements Runnable {
        private final AtomicLong id;

        private Release(long id) {
            this.id = new AtomicLong(id);
        }

        @Override
        public void run() {
            long owned = id.getAndSet(0);
            if (owned != 0) {
                drop(owned);
            }
        }
    }
}
