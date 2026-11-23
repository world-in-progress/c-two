pub use c2_ipc::pool::IpcClientCloseDetail;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterOutcome {
    pub route_name: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub ipc_address: String,
    pub relay_registered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterFailureOutcome {
    pub route_name: String,
    pub failure_source: String,
    pub error_message: String,
    pub status_code: Option<u16>,
    /// The relay's canonical refusal, alongside the original status and text.
    pub semantic_error: Option<c2_error::C2Error>,
    pub rollback: Option<RouteCloseOutcome>,
    pub relay_cleanup_error: Option<RelayCleanupError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCleanupError {
    pub route_name: String,
    pub status_code: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCloseOutcome {
    pub route_name: String,
    pub local_removed: bool,
    pub active_drained: bool,
    pub closed_reason: String,
    pub close_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnregisterOutcome {
    pub route_name: String,
    pub local_removed: bool,
    pub close: RouteCloseOutcome,
    pub relay_error: Option<RelayCleanupError>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownOutcome {
    pub removed_routes: Vec<String>,
    pub route_outcomes: Vec<RouteCloseOutcome>,
    pub relay_errors: Vec<RelayCleanupError>,
    pub server_was_started: bool,
    /// Explicit Runtime retirement drained its outgoing client scope. Ordinary
    /// Host shutdown does not select that scope and leaves this false.
    pub ipc_clients_drained: bool,
    pub route_close_error: Option<String>,
    pub runtime_barrier_error: Option<String>,
    /// Addresses in this Runtime's outgoing IPC client cache whose close
    /// barrier was unconfirmed within the shutdown deadline. `None` means
    /// every detached client confirmed its close.
    pub ipc_client_close_error: Option<String>,
    /// Locally closed outgoing connections without a normal disconnect ACK.
    /// These are terminal local closes, not pending client/server drain work.
    pub ipc_client_close_details: Vec<IpcClientCloseDetail>,
}

impl ShutdownOutcome {
    pub(crate) fn invalid_timeout(error: c2_config::TimeoutError) -> Self {
        Self {
            runtime_barrier_error: Some(format!("shutdown timeout configuration error: {error}")),
            ..Default::default()
        }
    }
}

/// Snapshot of one keyed unary call in the current Host registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    pub status: CallOutcomeStatus,
    pub method_name: Option<String>,
    pub result_available: bool,
    pub result_unavailable_reason: Option<ResultUnavailableReason>,
    pub error_code: Option<c2_error::ErrorCode>,
    pub error_details_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOutcomeStatus {
    /// Includes calls whose pending input can still be withdrawn.
    InFlight,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultUnavailableReason {
    NotRetained,
    Evicted,
}

impl From<c2_wire::rpc::WireCallOutcome> for CallOutcome {
    fn from(wire: c2_wire::rpc::WireCallOutcome) -> Self {
        use c2_wire::rpc::{WireCallOutcomeStatus, WireResultUnavailableReason};
        Self {
            status: match wire.status {
                WireCallOutcomeStatus::InFlight => CallOutcomeStatus::InFlight,
                WireCallOutcomeStatus::Succeeded => CallOutcomeStatus::Succeeded,
                WireCallOutcomeStatus::Failed => CallOutcomeStatus::Failed,
                WireCallOutcomeStatus::Unknown => CallOutcomeStatus::Unknown,
            },
            method_name: wire.method_name,
            result_available: wire.result_available,
            result_unavailable_reason: wire.result_unavailable_reason.map(|reason| match reason {
                WireResultUnavailableReason::NotRetained => ResultUnavailableReason::NotRetained,
                WireResultUnavailableReason::Evicted => ResultUnavailableReason::Evicted,
            }),
            error_code: wire.error_code,
            error_details_available: wire.error_details_available,
        }
    }
}
