//! User-facing Rust SDK for the C-Two language-neutral runtime.
//!
//! Contract identity and runtime behavior are re-exported from their
//! authoritative Core crates. Portable payload values continue to use the
//! official [`fastdb`] crate directly.

mod error;
mod held;
mod payload;

pub use c2_contract::{
    ContractError, ContractLimits, ContractLimitsProfile, ContractRelease, ContractReleaseRef,
    ExpectedRouteContract,
};
pub use c2_core::{
    CallExecutionLimits, CallExecutionLimitsOverrides, CallExecutionObservation,
    CallExecutionObserver, CallExecutionSnapshot, CallOptions, CallTimeout, Client, ConfigSources,
    Connect, ConnectOptions, DirectIpcShutdownOutcome, DirectIpcShutdownRouteOutcome,
    ENDPOINT_CREDENTIAL_MAX_BYTES, ENDPOINT_CREDENTIAL_SCHEMA_VERSION, EndpointCredential,
    EndpointCredentialError, EndpointCredentialErrorKind, EndpointInspection, EndpointIoError,
    EndpointReapResult, EndpointSweep, EndpointSweepScope, EndpointUnverifiedReason, EnvFilePolicy,
    EnvMap, Host, HostOptions, LifecycleError, LocalEndpoint, LocalEndpointContext,
    LocalEndpointNamespace, LocalEndpointOptions, MemoryCellStats, MemoryScopeStats, ObservedPath,
    ObservedRoute, PathCounters, Registration, RouteConcurrency, RouteConcurrencyError,
    RouteConcurrencyGuard, RouteConcurrencySnapshot, Runtime, RuntimeIdentity, RuntimeMemoryStats,
    RuntimeOptions, ServiceConcurrencyMode, ServiceDefinition, SweepBatch, SweepBudget,
    direct_ipc_endpoint, direct_ipc_endpoint_with_context, inspect_endpoint, ping_direct_ipc,
    ping_direct_ipc_with_context, reap_endpoint, shutdown_direct_ipc,
    shutdown_direct_ipc_with_context,
};
pub use error::Error;
pub use held::Held;

/// Stable implementation seam consumed by C-Two generated Rust modules.
///
/// It is public because generated modules live in downstream crates, but it is
/// hidden from ordinary SDK documentation. In particular, [`EncodedClient`]
/// remains sealed by Core and cannot be implemented by downstream transports.
#[doc(hidden)]
pub mod generated {
    pub use c2_contract::MethodAccess;
    pub use c2_core::{
        AdapterFailure, AdapterFailurePhase, EncodedCall, EncodedClient, EncodedService,
        ExternalCause, HeldResponse, MethodDefinition, PreparedCall, ServiceDefinition,
        external_cause_details, normalize_adapter_failure,
    };
    pub use c2_error::{C2Error, ErrorCode};

    pub use crate::error::{adapter_error, fastdb_adapter_error, fastdb_cause_details};
    pub use crate::payload::{
        BorrowedPayload, charge_payload_input, open_borrowed, open_held, open_owned,
    };
}

/// Native lifecycle and private owner capability, projected from Core.
pub use c2_core::{
    HostClientHeldLeases, HostLifecyclePhase, HostLifecycleSnapshot, OwnerControlKeepalive,
    OwnerControlReceiver, ServerLifecyclePolicy, owner_control_pair,
};
