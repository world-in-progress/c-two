package io.ctwo.generated;

import io.fastdb.payload.Payload;

/**
 * Runs a generated unary adapter with a callback-scoped borrowed input.
 * The result transfers a FastDB Payload or BuildPlan to the adapter.
 */
@FunctionalInterface
public interface UnaryCodec {
    Object invoke(Payload input);
}
