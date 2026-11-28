package io.ctwo;

/** Resource-side continuation. Returning null ends the stream. */
public interface ServerStream<T> extends AutoCloseable {
    T next();

    @Override
    void close();
}
