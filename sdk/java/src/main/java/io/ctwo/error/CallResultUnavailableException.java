package io.ctwo.error;

import java.util.Map;

public final class CallResultUnavailableException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public CallResultUnavailableException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CALL_RESULT_UNAVAILABLE, message, Map.of(), null, null, null);
    }

    CallResultUnavailableException(Facts facts) {
        super(facts);
    }
}
