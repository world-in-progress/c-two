package io.ctwo;

import java.nio.file.Path;
import java.time.Duration;
import java.util.Objects;

/** Client-role configuration and local endpoint selection for one Runtime. */
public record RuntimeOptions(
        Long memoryBudgetBytes, Long fileSpillBudgetBytes, Long maxOutstandingCalls,
        Path unixEndpointRoot, String relayAnchorAddress, boolean useProcessRelayAnchor,
        Duration connectTimeout) {

    public static RuntimeOptions defaults() {
        return new RuntimeOptions(null, null, null, null, null, true, null);
    }

    public RuntimeOptions withMemoryBudgetBytes(long value) {
        return new RuntimeOptions(value, fileSpillBudgetBytes, maxOutstandingCalls,
                unixEndpointRoot, relayAnchorAddress, useProcessRelayAnchor, connectTimeout);
    }

    public RuntimeOptions withFileSpillBudgetBytes(long value) {
        return new RuntimeOptions(memoryBudgetBytes, value, maxOutstandingCalls,
                unixEndpointRoot, relayAnchorAddress, useProcessRelayAnchor, connectTimeout);
    }

    public RuntimeOptions withMaxOutstandingCalls(long value) {
        return new RuntimeOptions(memoryBudgetBytes, fileSpillBudgetBytes, value,
                unixEndpointRoot, relayAnchorAddress, useProcessRelayAnchor, connectTimeout);
    }

    public RuntimeOptions withUnixEndpointRoot(Path value) {
        return new RuntimeOptions(memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls,
                Objects.requireNonNull(value), relayAnchorAddress, useProcessRelayAnchor, connectTimeout);
    }

    public RuntimeOptions withRelayAnchorAddress(String address) {
        return new RuntimeOptions(memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls,
                unixEndpointRoot, Objects.requireNonNull(address), false, connectTimeout);
    }

    public RuntimeOptions withoutRelay() {
        return new RuntimeOptions(memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls,
                unixEndpointRoot, null, false, connectTimeout);
    }

    public RuntimeOptions withConnectTimeout(Duration value) {
        return new RuntimeOptions(memoryBudgetBytes, fileSpillBudgetBytes, maxOutstandingCalls,
                unixEndpointRoot, relayAnchorAddress, useProcessRelayAnchor, Objects.requireNonNull(value));
    }

    String nativeUnixEndpointRoot() {
        return unixEndpointRoot == null ? null : unixEndpointRoot.toString();
    }
}
