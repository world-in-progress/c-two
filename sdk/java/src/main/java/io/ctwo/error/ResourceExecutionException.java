package io.ctwo.error;

import java.util.Map;

public final class ResourceExecutionException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public ResourceExecutionException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.RESOURCE_FUNCTION_EXECUTING, message, Map.of(), null, null, null);
    }

    ResourceExecutionException(Facts facts) {
        super(facts);
    }
}
