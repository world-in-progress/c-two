//! Language-neutral runtime and orchestration core for C-Two.
//!
//! `c2-core` owns process identity, route and relay lifecycle, transport error
//! normalization, and cross-language orchestration. It transports opaque bytes
//! and does not depend on payload-owner implementations such as FastDB.

mod call_execution;
mod call_scope;
mod client;
mod client_stream;
mod connect_deadline;
mod connection_observation;
mod control;
pub mod error;
mod host;
mod identity;
mod lifecycle;
mod lifetime;
mod local_stream;
pub mod memory;
mod outcome;
mod session;

pub use c2_config::{ConnectOptions, ConnectTimeoutError};
pub use c2_ipc::DirectIpcShutdownOutcome;
pub use c2_server::{ServerFailure, ServerStartupStage, ServerStartupTimeout};
#[doc(hidden)]
pub use c2_wire::handshake::ServerIdentity;
pub use call_scope::{CallScope, CallScopeError, CallState};
pub use client::{
    CallBudget, Client, Connect, EncodedCall, EncodedClient, HeldResponse, NativeEndpointContext,
    ObservedPath, PathCounters, PreparedCall,
};
#[doc(hidden)]
pub use client::{CallDispatchTarget, CallRecordOutcome, IpcCallTarget};
pub use client_stream::{CoreResponseStream, CoreStreamCloseReceipt};
pub use connect_deadline::{ConnectAttempt, connect_deadline_error};
pub use control::{direct_ipc_endpoint, ping_direct_ipc, shutdown_direct_ipc};
pub use error::{
    AdapterFailure, AdapterFailurePhase, Error, ExternalCause, LifecycleError, LocalIoCause,
    TransportError, TransportKind, TransportPhase, external_cause_details,
    normalize_adapter_failure, normalize_http_error, normalize_http_semantic_body,
    normalize_ipc_error, normalize_ipc_semantic_bytes, semantic_error_from_http_body,
    semantic_error_from_ipc_bytes,
};
pub use host::{
    EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, Host, HostOptions,
    MethodDefinition, Registration, RouteConcurrency, RouteConcurrencyError, RouteConcurrencyGuard,
    RouteConcurrencySnapshot, ServiceConcurrencyMode, ServiceDefinition,
};
pub use identity::{
    auto_server_id, auto_server_instance_id, ipc_address_for_server_id, validate_server_id,
};
pub use local_stream::{CoreLocalResponseStream, CoreLocalStreamCloseReceipt, LocalObjectStream};
// The native local-endpoint lifecycle surface is owned by `c2-local`. Core
// re-exports it so SDKs and the CLI share one implementation instead of
// writing their own paths, parsers, or management loops.
pub use c2_local::{
    ConnectionClosed, ConnectionEndReason, ConnectionEvent, ConnectionEvents,
    ConnectionEventsError, ConnectionObservation, ConnectionSide, ENDPOINT_CREDENTIAL_MAX_BYTES,
    ENDPOINT_CREDENTIAL_SCHEMA_VERSION, EndpointBindCause, EndpointCredential,
    EndpointCredentialError, EndpointCredentialErrorKind, EndpointInspection, EndpointIoError,
    EndpointReapResult, EndpointSweep, EndpointSweepScope, EndpointUnverifiedReason, LocalEndpoint,
    SweepBatch, SweepBudget, inspect_endpoint, reap_endpoint,
};
pub use call_execution::{CallExecutionObservation, CallExecutionObserver};
// Blocking waits for the connection-observation surface stay with Core's
// single runtime authority; SDKs project parameters and results only.
pub use connection_observation::{ConnectionEventWait, recv_connection_event};
pub use lifecycle::HostLifecycleSnapshot;
pub use memory::{
    MemoryCellStats, MemoryScopeStats, RetiredMemoryObservation, RetiredScopeReport,
    RetirementHandoff, RuntimeMemoryStats, scope,
};
pub use outcome::{
    IpcClientCloseDetail, RegisterFailureOutcome, RegisterOutcome, RelayCleanupError,
    RouteCloseOutcome, ShutdownOutcome, UnregisterOutcome,
};
pub use session::{
    FailedStartShutdownReceipt, Runtime, RuntimeIdentity, RuntimeOptions, RuntimeShutdownReceipt,
    RuntimeShutdownTarget,
};

pub use c2_config::{
    CallExecutionLimits, CallExecutionLimitsOverrides, CallOptions, CallTimeout, ConfigSources,
    EnvFilePolicy, EnvMap, LocalEndpointContext, LocalEndpointNamespace, LocalEndpointOptions,
};
/// Read-only operation-slot and payload-buffer counters for one Runtime.
pub use c2_mem::RetentionSnapshot as CallExecutionSnapshot;

/// Shared asynchronous executor for native client projections.
#[doc(hidden)]
pub fn native_client_executor() -> Result<tokio::runtime::Handle, Error> {
    call_execution::shared_executor()
}
