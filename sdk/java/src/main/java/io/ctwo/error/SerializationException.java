package io.ctwo.error;

import java.util.Map;

public final class SerializationException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public SerializationException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CLIENT_INPUT_SERIALIZING, message, Map.of(), null, null, null);
    }

    SerializationException(Facts facts) {
        super(facts);
    }
}
