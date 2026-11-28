package io.ctwo.error;

import java.util.Map;

public final class CallIdConflictException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public CallIdConflictException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.CALL_ID_CONFLICT, message, Map.of(), null, null, null);
    }

    CallIdConflictException(Facts facts) {
        super(facts);
    }
}
