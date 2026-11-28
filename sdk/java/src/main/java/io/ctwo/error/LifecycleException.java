package io.ctwo.error;

import java.util.Map;

public final class LifecycleException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public LifecycleException(String message) {
        super(ErrorCategory.LIFECYCLE, ErrorCode.UNKNOWN, message, Map.of(), null, null, null);
    }

    LifecycleException(Facts facts) {
        super(facts);
    }
}
