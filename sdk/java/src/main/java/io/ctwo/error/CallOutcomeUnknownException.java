package io.ctwo.error;

import java.util.Map;

public final class CallOutcomeUnknownException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public CallOutcomeUnknownException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CALL_OUTCOME_UNKNOWN, message, Map.of(), null, null, null);
    }

    CallOutcomeUnknownException(Facts facts) {
        super(facts);
    }
}
