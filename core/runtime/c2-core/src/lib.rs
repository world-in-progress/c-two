//! Language-neutral runtime and orchestration core for C-Two.
//!
//! `c2-core` owns process identity, route and relay lifecycle, transport error
//! normalization, and cross-language orchestration. It transports opaque bytes
//! and does not depend on payload-owner implementations such as FastDB.

mod client;
mod control;
pub mod error;
mod host;
mod identity;
mod lifetime;
pub mod memory;
mod owner_bound;
mod outcome;
mod session;

pub use client::{Client, Connect, EncodedClient, ObservedPath, ObservedRoute, PathCounters};
pub use control::{
    DirectIpcShutdownOutcome, DirectIpcShutdownRouteOutcome, direct_ipc_endpoint,
    direct_ipc_endpoint_with_protocol, ping_direct_ipc, ping_direct_ipc_with_protocol,
    shutdown_direct_ipc, shutdown_direct_ipc_with_protocol,
};
pub use error::{
    AdapterFailure, AdapterFailurePhase, Error, ExternalCause, LifecycleError, TransportError,
    TransportKind, TransportPhase, external_cause_details, normalize_adapter_failure,
    normalize_http_error, normalize_http_semantic_body, normalize_ipc_error,
    normalize_ipc_semantic_bytes, semantic_error_from_http_body, semantic_error_from_ipc_bytes,
};
pub use host::{
    EncodedService, Host, HostOptions, MethodDefinition, Registration, RouteConcurrency,
    RouteConcurrencyError, RouteConcurrencyGuard, RouteConcurrencySnapshot, ServiceConcurrencyMode,
    ServiceDefinition,
};
pub use identity::{
    auto_server_id, auto_server_instance_id, ipc_address_for_server_id, validate_server_id,
};
pub use lifetime::HeldResponse;
// The native local-endpoint lifecycle surface is owned by `c2-local`. Core
// re-exports it so SDKs and the CLI share one implementation instead of
// writing their own paths, parsers, or management loops.
pub use c2_local::{
    ENDPOINT_CREDENTIAL_MAX_BYTES, ENDPOINT_CREDENTIAL_SCHEMA_VERSION, EndpointCredential,
    EndpointCredentialError, EndpointCredentialErrorKind, EndpointInspection, EndpointIoError,
    EndpointReapResult, EndpointSweep, EndpointUnverifiedReason, LocalEndpoint, SweepBatch,
    SweepBudget, inspect_endpoint, reap_endpoint,
};
pub use memory::{
    MemoryCellStats, MemoryScopeStats, RetiredMemoryObservation, RetiredScopeReport,
    RetirementHandoff, RuntimeMemoryStats, scope,
};
pub use owner_bound::{HostClientHeldLeases, HostLifecyclePhase, HostLifecycleSnapshot};
pub use outcome::{
    RegisterFailureOutcome, RegisterOutcome, RelayCleanupError, RouteCloseOutcome, ShutdownOutcome,
    UnregisterOutcome,
};
pub use session::{Runtime, RuntimeIdentity, RuntimeOptions};
