package io.ctwo;

import java.time.Duration;
import java.util.Objects;

/** Immutable timeout and metadata settings for a client view. */
public final class CallOptions {
    private final int timeoutKind;
    private final Duration timeout;
    private final CallMetadata metadata;

    private CallOptions(int timeoutKind, Duration timeout, CallMetadata metadata) {
        this.timeoutKind = timeoutKind;
        this.timeout = timeout;
        this.metadata = Objects.requireNonNull(metadata);
    }

    public static CallOptions defaults() {
        return new CallOptions(0, null, CallMetadata.empty());
    }

    public CallOptions withTimeout(Duration duration) {
        if (duration.isNegative()) {
            throw new IllegalArgumentException("Negative call timeout");
        }
        return new CallOptions(2, duration, metadata);
    }

    public CallOptions withoutTimeout() { return new CallOptions(1, null, metadata); }
    public CallOptions withInheritedTimeout() { return new CallOptions(0, null, metadata); }
    public CallOptions withMetadata(CallMetadata value) { return new CallOptions(timeoutKind, timeout, value); }

    int timeoutKind() { return timeoutKind; }
    public Duration timeout() { return timeout; }
    public boolean hasUnlimitedTimeout() { return timeoutKind == 1; }
    public CallMetadata metadata() { return metadata; }
}
