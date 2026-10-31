//! User-facing Rust SDK for the C-Two language-neutral runtime.
//!
//! Contract identity and runtime behavior are re-exported from their
//! authoritative Core crates. Portable payload values continue to use the
//! official [`fastdb`] crate directly. Endpoint administration, diagnostics
//! and contract tooling live in [`endpoint`], [`observe`]
//! and [`contract`].

mod error;
mod held;
mod payload;

pub mod contract;
pub mod endpoint;
pub mod observe;

pub use c2_contract::ContractRelease;
pub use c2_core::{
    CallExecutionLimits, CallExecutionLimitsOverrides, CallOptions, CallTimeout, Client,
    ConfigSources, Connect, ConnectOptions, EnvFilePolicy, EnvMap, Host, HostOptions,
    LifecycleError, Registration, RouteConcurrency, RouteConcurrencyError, RouteConcurrencyGuard,
    Runtime, RuntimeOptions, ServerFailure, ServerStartupStage, ServerStartupTimeout,
    ServiceConcurrencyMode, ServiceDefinition,
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
    pub use crate::held::release_held_after_error;
    pub use crate::payload::{
        BorrowedPayload, charge_payload_input, open_borrowed, open_held, open_owned,
    };
}
