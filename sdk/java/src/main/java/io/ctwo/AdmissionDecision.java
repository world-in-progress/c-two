package io.ctwo;

import java.util.Map;
import java.util.Objects;

/** A Host admission hook result. */
public final class AdmissionDecision {
    private final String reason;
    private final Map<String, String> details;

    private AdmissionDecision(String reason, Map<String, String> details) {
        this.reason = reason;
        this.details = Map.copyOf(details);
    }

    public static AdmissionDecision allow() { return new AdmissionDecision(null, Map.of()); }
    public static AdmissionDecision deny(String reason) { return deny(reason, Map.of()); }

    public static AdmissionDecision deny(String reason, Map<String, String> details) {
        return new AdmissionDecision(Objects.requireNonNull(reason), details);
    }

    public boolean allowed() { return reason == null; }
    public String reason() { return reason; }
    public Map<String, String> details() { return details; }

    String[] nativeEntries() { return CallMetadata.of(details).nativeEntries(); }
}
