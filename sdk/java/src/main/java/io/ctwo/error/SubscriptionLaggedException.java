package io.ctwo.error;

import java.util.Map;

public final class SubscriptionLaggedException extends C2Exception {
    private static final long serialVersionUID = 1L;

    public SubscriptionLaggedException(String message) {
        super(ErrorCategory.SEMANTIC, ErrorCode.SUBSCRIPTION_LAGGED, message, Map.of(), null, null, null);
    }

    SubscriptionLaggedException(Facts facts) {
        super(facts);
    }
}
