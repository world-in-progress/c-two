package io.ctwo.generated;

import io.fastdb.payload.CompiledSpec;
import java.util.Objects;

/** FastDB projection for a generated topic. */
public record GeneratedTopic(String name, CompiledSpec spec) {
    public GeneratedTopic {
        Objects.requireNonNull(name);
        Objects.requireNonNull(spec);
    }
}
