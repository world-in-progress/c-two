package io.ctwo.error;

/** Codes from the C-Two semantic error registry. */
public enum ErrorCode {
    UNKNOWN(0, null, null),
    RESOURCE_INPUT_DESERIALIZING(1, ErrorSide.RESOURCE, ErrorStage.DESERIALIZE),
    RESOURCE_OUTPUT_SERIALIZING(2, ErrorSide.RESOURCE, ErrorStage.SERIALIZE),
    RESOURCE_FUNCTION_EXECUTING(3, ErrorSide.RESOURCE, ErrorStage.EXECUTE),
    RESOURCE_INPUT_FROM_BUFFER(4, ErrorSide.RESOURCE, ErrorStage.FROM_BUFFER),
    CLIENT_INPUT_SERIALIZING(5, ErrorSide.CLIENT, ErrorStage.SERIALIZE),
    CLIENT_OUTPUT_DESERIALIZING(6, ErrorSide.CLIENT, ErrorStage.DESERIALIZE),
    CLIENT_CALLING_RESOURCE(7, ErrorSide.CLIENT, ErrorStage.CALL),
    CLIENT_OUTPUT_FROM_BUFFER(8, ErrorSide.CLIENT, ErrorStage.FROM_BUFFER),
    RESOURCE_NOT_FOUND(701, null, null),
    RESOURCE_UNAVAILABLE(702, null, null),
    RESOURCE_ALREADY_REGISTERED(703, null, null),
    REGISTRY_UNAVAILABLE(705, null, null),
    RESOURCE_CLOSED(707, null, null),
    RESOURCE_REMOVED(708, null, null),
    CONTRACT_MISMATCH(709, null, null),
    IDENTITY_MISMATCH(710, null, null),
    ROUTE_WATCH_UNAVAILABLE(712, null, null),
    PROTOCOL_VIOLATION(713, null, null),
    FALLBACK_DENIED(714, null, null),
    CALL_DEADLINE_EXCEEDED(715, null, null),
    UNSUPPORTED_CALL_MODE(716, null, null),
    CALL_CAPACITY_EXCEEDED(717, null, null),
    CALL_CANCELLED(718, null, null),
    ROUTE_CONFLICT(719, null, null),
    SUBSCRIPTION_LAGGED(720, null, null),
    CALL_ID_CONFLICT(721, null, null),
    CALL_RESULT_UNAVAILABLE(722, null, null),
    CALL_OUTCOME_UNKNOWN(723, null, null),
    ADMISSION_DENIED(724, null, null);

    private final int value;
    private final ErrorSide side;
    private final ErrorStage stage;

    ErrorCode(int value, ErrorSide side, ErrorStage stage) {
        this.value = value;
        this.side = side;
        this.stage = stage;
    }

    public int value() { return value; }
    public ErrorSide side() { return side; }
    public ErrorStage stage() { return stage; }

    public static ErrorCode fromValue(int value) {
        for (ErrorCode code : values()) {
            if (code.value == value) {
                return code;
            }
        }
        return UNKNOWN;
    }
}
