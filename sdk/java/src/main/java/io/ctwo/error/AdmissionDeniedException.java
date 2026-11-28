package io.ctwo.error;

import java.util.Map;

public final class AdmissionDeniedException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public AdmissionDeniedException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.ADMISSION_DENIED, message, Map.of(), null, null, null);
    }

    AdmissionDeniedException(Facts facts) {
        super(facts);
    }
}
