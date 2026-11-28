package io.ctwo.error;

import java.util.Map;

public final class CallCancelledException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public CallCancelledException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CALL_CANCELLED, message, Map.of(), null, null, null);
    }

    CallCancelledException(Facts facts) {
        super(facts);
    }
}
