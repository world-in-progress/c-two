package io.ctwo.error;

import java.util.Map;
import java.util.Objects;

/** The structured error returned by a C-Two operation. */
public class C2Exception extends RuntimeException {
    private static final long serialVersionUID = 1L;
    private final ErrorCategory category;
    private final ErrorCode errorCode;
    private final Map<String, String> details;
    private final TransportPhase transportPhase;
    private final String callId;
    private final String lifecycleKind;

    public C2Exception(ErrorCode errorCode, String message, Map<String, String> details) {
        this(ErrorCategory.SEMANTIC, errorCode, message, details, null, null, null);
    }

    public C2Exception(ErrorCategory category, ErrorCode errorCode, String message,
            Map<String, String> details, TransportPhase transportPhase, String callId, String lifecycleKind) {
        super(message);
        this.category = Objects.requireNonNull(category);
        this.errorCode = Objects.requireNonNull(errorCode);
        this.details = Map.copyOf(details);
        this.transportPhase = transportPhase;
        this.callId = transportPhase == TransportPhase.DISPATCH_UNCERTAIN ? callId : null;
        this.lifecycleKind = lifecycleKind;
    }

    C2Exception(Facts facts) {
        this(facts.category(), facts.code(), facts.message(), facts.details(), facts.phase(),
                facts.callId(), facts.lifecycleKind());
    }

    record Facts(ErrorCategory category, ErrorCode code, String message, Map<String, String> details,
            TransportPhase phase, String callId, String lifecycleKind) {}

    public ErrorCategory category() { return category; }
    public ErrorCode errorCode() { return errorCode; }
    public Map<String, String> details() { return details; }
    public TransportPhase transportPhase() { return transportPhase; }
    public String callId() { return callId; }
    public String lifecycleKind() { return lifecycleKind; }
    public ErrorSide side() { return errorCode.side(); }
    public ErrorStage stage() { return errorCode.stage(); }
}
