package io.ctwo;

import java.util.Objects;

public record SubscribeOptions(TopicStart start, LagPolicy lagPolicy) {
    public SubscribeOptions {
        Objects.requireNonNull(start);
        Objects.requireNonNull(lagPolicy);
    }

    public static SubscribeOptions defaults() {
        return new SubscribeOptions(TopicStart.LATEST, LagPolicy.SKIP);
    }

    public SubscribeOptions withStart(TopicStart value) {
        return new SubscribeOptions(value, lagPolicy);
    }

    public SubscribeOptions withLagPolicy(LagPolicy value) {
        return new SubscribeOptions(start, value);
    }
}
