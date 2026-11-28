package io.ctwo.error;

import java.util.Map;

public final class CallDeadlineExceededException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public CallDeadlineExceededException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CALL_DEADLINE_EXCEEDED, message, Map.of(), null, null, null);
    }

    CallDeadlineExceededException(Facts facts) {
        super(facts);
    }
}
