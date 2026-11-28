package io.ctwo;

import io.ctwo.internal.ServiceCallbacks;
import java.util.Optional;

/** Context bound for the duration of a resource method or a stream callback. */
public record CallContext(String routeName, String methodName, String callId, CallMetadata metadata,
        Transport transport) {
    public enum Transport { IPC, THREAD_LOCAL }

    public static Optional<CallContext> current() {
        return ServiceCallbacks.currentCallContext();
    }

    private static CallContext fromNative(String route, String method, String callId, String[] metadata) {
        return new CallContext(route, method, callId, CallMetadata.fromNative(metadata), Transport.IPC);
    }
}
