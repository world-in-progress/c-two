package io.ctwo.error;

import java.util.Map;

public final class ContractException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public ContractException(String message) {
        super(ErrorCategory.CONTRACT, ErrorCode.CONTRACT_MISMATCH, message, Map.of(), null, null, null);
    }

    ContractException(Facts facts) {
        super(facts);
    }
}
