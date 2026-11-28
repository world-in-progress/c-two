package io.ctwo;

import io.ctwo.error.ErrorCode;
import java.util.Locale;

/** What the current Host registration knows about a keyed call. */
public record CallOutcome(CallOutcomeStatus status, String methodName, boolean resultAvailable,
        ResultUnavailableReason resultUnavailableReason, ErrorCode error, boolean errorDetailsAvailable) {
    private static CallOutcome fromNative(int status, String methodName, boolean available,
            String reason, int error, boolean detailsAvailable) {
        return new CallOutcome(CallOutcomeStatus.values()[status], methodName, available,
                reason == null ? null : ResultUnavailableReason.valueOf(reason.toUpperCase(Locale.ROOT)),
                error < 0 ? null : ErrorCode.fromValue(error), detailsAvailable);
    }
}
