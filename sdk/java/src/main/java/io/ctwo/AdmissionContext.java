package io.ctwo;

/** Read-only facts for an incoming Host operation. A query has no target. */
public record AdmissionContext(String routeName, String crmNamespace, String crmName, String crmVersion,
        String target, MethodAccess access, Operation operation, CallContext.Transport transport,
        String callId, CallMetadata metadata) {
    public enum Operation { UNARY, SERVER_STREAM, SUBSCRIBE, RECOVER, QUERY_OUTCOME }

    private static AdmissionContext fromNative(String route, String namespace, String crm, String version,
            String target, String access, String operation, String transport, String callId, String[] metadata) {
        return new AdmissionContext(route, namespace, crm, version, target,
                access == null ? null : MethodAccess.valueOf(access), Operation.valueOf(operation),
                CallContext.Transport.valueOf(transport), callId, CallMetadata.fromNative(metadata));
    }
}
