package io.ctwo.generated;

import io.fastdb.payload.CompiledSpec;
import java.util.Objects;

/** FastDB input and output projections for one method in the generated release. */
public record GeneratedMethod(String name, CompiledSpec inputSpec, CompiledSpec outputSpec) {
    public GeneratedMethod {
        Objects.requireNonNull(name);
    }
}
