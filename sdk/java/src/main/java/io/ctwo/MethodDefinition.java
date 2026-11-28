package io.ctwo;

import java.util.Objects;

/** An ordered method entry of a validated contract release. */
public record MethodDefinition(int index, String name, MethodAccess access, MethodMode mode) {
    public MethodDefinition {
        if (index < 0 || index > 0xffff) {
            throw new IllegalArgumentException("Method index outside the unsigned 16-bit range");
        }
        Objects.requireNonNull(name);
        Objects.requireNonNull(access);
        Objects.requireNonNull(mode);
    }

    private MethodDefinition(int index, String name, boolean write, boolean serverStream) {
        this(index, name, write ? MethodAccess.WRITE : MethodAccess.READ,
                serverStream ? MethodMode.SERVER_STREAM : MethodMode.UNARY);
    }
}
