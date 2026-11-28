package io.ctwo.error;

import io.fastdb.payload.FastDbException;
import java.util.LinkedHashMap;
import java.util.Map;

final class ErrorFactory {
    private ErrorFactory() {}

    static C2Exception create(String category, int value, String message, String[] entries,
            String phase, String callId, String lifecycleKind) {
        Map<String, String> details = new LinkedHashMap<>();
        for (int i = 0; i < entries.length; i += 2) {
            details.put(entries[i], entries[i + 1]);
        }
        if (!"DISPATCH_UNCERTAIN".equals(phase)) {
            details.remove("call_id");
        }
        ErrorCode code = ErrorCode.fromValue(value);
        var facts = new C2Exception.Facts(ErrorCategory.valueOf(category), code, message, details,
                phase == null ? null : TransportPhase.valueOf(phase), callId, lifecycleKind);
        return switch (code) {
            case RESOURCE_FUNCTION_EXECUTING -> new ResourceExecutionException(facts);
            case RESOURCE_INPUT_DESERIALIZING, RESOURCE_OUTPUT_SERIALIZING, RESOURCE_INPUT_FROM_BUFFER,
                    CLIENT_INPUT_SERIALIZING, CLIENT_OUTPUT_DESERIALIZING, CLIENT_OUTPUT_FROM_BUFFER ->
                    new SerializationException(facts);
            case CALL_DEADLINE_EXCEEDED -> new CallDeadlineExceededException(facts);
            case CALL_CANCELLED -> new CallCancelledException(facts);
            case SUBSCRIPTION_LAGGED -> new SubscriptionLaggedException(facts);
            case CALL_ID_CONFLICT -> new CallIdConflictException(facts);
            case CALL_RESULT_UNAVAILABLE -> new CallResultUnavailableException(facts);
            case CALL_OUTCOME_UNKNOWN -> new CallOutcomeUnknownException(facts);
            case ADMISSION_DENIED -> new AdmissionDeniedException(facts);
            case CONTRACT_MISMATCH, IDENTITY_MISMATCH -> new ContractException(facts);
            default -> switch (facts.category()) {
                case CONTRACT -> new ContractException(facts);
                case LIFECYCLE -> new LifecycleException(facts);
                case TRANSPORT -> new TransportException(facts);
                case SEMANTIC -> new C2Exception(facts);
            };
        };
    }

    static String[] describe(Throwable thrown) {
        if (thrown instanceof FastDbException fastDb) {
            return new String[] {"fastdb", Integer.toString(fastDb.code()), fastDb.symbol(), fastDb.path(),
                    fastDb.message(), fastDb.detailsJson()};
        }
        if (thrown instanceof C2Exception semantic) {
            String[] result = new String[3 + semantic.details().size() * 2];
            result[0] = "semantic";
            result[1] = Integer.toString(semantic.errorCode().value());
            result[2] = semantic.getMessage();
            int index = 3;
            for (var entry : semantic.details().entrySet()) {
                result[index++] = entry.getKey();
                result[index++] = entry.getValue();
            }
            return result;
        }
        return new String[] {"other", thrown.getClass().getName(), thrown.getMessage()};
    }
}
