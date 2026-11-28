package io.ctwo;

import java.time.Duration;
import java.util.Objects;

/** Options for the server role of a Runtime. Omitted fields inherit Core configuration. */
public record HostOptions(
        String serverId, Duration startupTimeout, Duration shutdownTimeout,
        Long maxPayloadSize, Integer maxExecutionWorkers, Long outcomeMaxEntries,
        Long outcomeMaxRetainedBytes, Long memoryBudgetBytes, Long fileSpillBudgetBytes,
        Long maxOutstandingCalls, String relayAnchorAddress, boolean relayDisabled,
        Boolean relayUseProxy, AdmissionHook admissionHook, boolean clearAdmissionHook) {

    public HostOptions {
        if (relayDisabled && relayAnchorAddress != null) {
            throw new IllegalArgumentException("A disabled relay cannot specify an anchor");
        }
        if (clearAdmissionHook && admissionHook != null) {
            throw new IllegalArgumentException("Cannot set and clear an admission hook together");
        }
    }

    public static HostOptions defaults() {
        return builder().build();
    }

    public static Builder builder() {
        return new Builder();
    }

    public HostOptions withoutRelay() {
        return new HostOptions(serverId, startupTimeout, shutdownTimeout, maxPayloadSize,
                maxExecutionWorkers, outcomeMaxEntries, outcomeMaxRetainedBytes, memoryBudgetBytes,
                fileSpillBudgetBytes, maxOutstandingCalls, null, true, relayUseProxy,
                admissionHook, clearAdmissionHook);
    }

    public HostOptions withAdmissionHook(AdmissionHook hook) {
        return new HostOptions(serverId, startupTimeout, shutdownTimeout, maxPayloadSize,
                maxExecutionWorkers, outcomeMaxEntries, outcomeMaxRetainedBytes, memoryBudgetBytes,
                fileSpillBudgetBytes, maxOutstandingCalls, relayAnchorAddress, relayDisabled,
                relayUseProxy, Objects.requireNonNull(hook), false);
    }

    public HostOptions withServerId(String value) {
        return new HostOptions(Objects.requireNonNull(value), startupTimeout, shutdownTimeout,
                maxPayloadSize, maxExecutionWorkers, outcomeMaxEntries, outcomeMaxRetainedBytes,
                memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls, relayAnchorAddress,
                relayDisabled, relayUseProxy, admissionHook, clearAdmissionHook);
    }

    public static final class Builder {
        private String serverId;
        private Duration startupTimeout;
        private Duration shutdownTimeout;
        private Long maxPayloadSize;
        private Integer maxExecutionWorkers;
        private Long outcomeMaxEntries;
        private Long outcomeMaxRetainedBytes;
        private Long memoryBudgetBytes;
        private Long fileSpillBudgetBytes;
        private Long maxOutstandingCalls;
        private String relayAnchorAddress;
        private boolean relayDisabled;
        private Boolean relayUseProxy;
        private AdmissionHook admissionHook;
        private boolean clearAdmissionHook;

        private Builder() {}

        public Builder serverId(String value) {
            serverId = Objects.requireNonNull(value);
            return this;
        }

        public Builder startupTimeout(Duration value) {
            startupTimeout = Objects.requireNonNull(value);
            return this;
        }

        public Builder shutdownTimeout(Duration value) {
            shutdownTimeout = Objects.requireNonNull(value);
            return this;
        }

        public Builder maxPayloadSize(long value) {
            maxPayloadSize = value;
            return this;
        }

        public Builder maxExecutionWorkers(int value) {
            maxExecutionWorkers = value;
            return this;
        }

        public Builder outcomeMaxEntries(long value) {
            outcomeMaxEntries = value;
            return this;
        }

        public Builder outcomeMaxRetainedBytes(long value) {
            outcomeMaxRetainedBytes = value;
            return this;
        }

        public Builder memoryBudgetBytes(long value) {
            memoryBudgetBytes = value;
            return this;
        }

        public Builder fileSpillBudgetBytes(long value) {
            fileSpillBudgetBytes = value;
            return this;
        }

        public Builder maxOutstandingCalls(long value) {
            maxOutstandingCalls = value;
            return this;
        }

        public Builder relayAnchorAddress(String value) {
            relayAnchorAddress = Objects.requireNonNull(value);
            relayDisabled = false;
            return this;
        }

        public Builder withoutRelay() {
            relayAnchorAddress = null;
            relayDisabled = true;
            return this;
        }

        public Builder relayUseProxy(boolean value) {
            relayUseProxy = value;
            return this;
        }

        public Builder admissionHook(AdmissionHook hook) {
            admissionHook = Objects.requireNonNull(hook);
            clearAdmissionHook = false;
            return this;
        }

        public Builder withoutAdmissionHook() {
            admissionHook = null;
            clearAdmissionHook = true;
            return this;
        }

        public HostOptions build() {
            return new HostOptions(serverId, startupTimeout, shutdownTimeout, maxPayloadSize,
                    maxExecutionWorkers, outcomeMaxEntries, outcomeMaxRetainedBytes,
                    memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls,
                    relayAnchorAddress, relayDisabled, relayUseProxy, admissionHook, clearAdmissionHook);
        }
    }
}
