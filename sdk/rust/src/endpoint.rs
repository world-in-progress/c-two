//! Local endpoint administration for supervisors and maintenance tools.
//! Includes endpoint contexts and credentials, inspection, reaping and sweeps,
//! direct IPC probes, shutdown outcomes, and local I/O causes.
//! Shutdown distinguishes initiation, endpoint absence and an unconfirmed request.
//! Completion is observed through the original Host shutdown receipt.

pub use c2_core::{
    DirectIpcShutdownOutcome, ENDPOINT_CREDENTIAL_MAX_BYTES, ENDPOINT_CREDENTIAL_SCHEMA_VERSION,
    EndpointBindCause, EndpointCredential, EndpointCredentialError, EndpointCredentialErrorKind,
    EndpointInspection, EndpointIoError, EndpointReapResult, EndpointSweep, EndpointSweepScope,
    EndpointUnverifiedReason, LocalEndpoint, LocalEndpointContext, LocalEndpointNamespace,
    LocalEndpointOptions, LocalIoCause, RuntimeIdentity, SweepBatch, SweepBudget,
    direct_ipc_endpoint, inspect_endpoint, ping_direct_ipc, reap_endpoint, shutdown_direct_ipc,
};
