package io.ctwo.generated;

import io.ctwo.ServerStream;
import io.fastdb.payload.Payload;

/** Opens a stream with an input borrowed until the producer closes. */
@FunctionalInterface
public interface StreamServiceHandler {
    ServerStream<Payload> open(Payload input);
}
