package io.ctwo.error;

import java.util.Map;

public final class TransportException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public TransportException(String message) {
        super(ErrorCategory.TRANSPORT, ErrorCode.CLIENT_CALLING_RESOURCE, message, Map.of(), null, null, null);
    }

    TransportException(Facts facts) {
        super(facts);
    }
}
