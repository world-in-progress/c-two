use std::path::PathBuf;
use std::time::Duration;

use crate::LifecycleError;

/// Per-route fact returned by a direct IPC shutdown acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectIpcShutdownRouteOutcome {
    pub route_name: String,
    pub active_drained: bool,
    pub closed_reason: String,
}

/// Language-neutral direct IPC shutdown acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectIpcShutdownOutcome {
    pub acknowledged: bool,
    pub shutdown_started: bool,
    pub server_stopped: bool,
    pub route_outcomes: Vec<DirectIpcShutdownRouteOutcome>,
}

/// Resolve a validated direct IPC address to its local socket path.
pub fn direct_ipc_socket_path(address: &str) -> Result<PathBuf, LifecycleError> {
    c2_ipc::socket_path_from_ipc_address(address)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Probe a direct IPC server without involving relay discovery.
pub fn ping_direct_ipc(address: &str, timeout: Duration) -> Result<bool, LifecycleError> {
    c2_ipc::ping(address, timeout).map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Initiate direct IPC shutdown without waiting for the owner-side drain.
pub fn shutdown_direct_ipc(
    address: &str,
    timeout: Duration,
) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
    c2_ipc::shutdown(address, timeout)
        .map(DirectIpcShutdownOutcome::from)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

impl From<c2_ipc::DirectShutdownAck> for DirectIpcShutdownOutcome {
    fn from(outcome: c2_ipc::DirectShutdownAck) -> Self {
        Self {
            acknowledged: outcome.acknowledged,
            shutdown_started: outcome.shutdown_started,
            server_stopped: outcome.server_stopped,
            route_outcomes: outcome
                .route_outcomes
                .into_iter()
                .map(|route| DirectIpcShutdownRouteOutcome {
                    route_name: route.route_name,
                    active_drained: route.active_drained,
                    closed_reason: route.closed_reason,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_address_is_a_core_configuration_error() {
        let error =
            direct_ipc_socket_path("tcp://not-ipc").expect_err("non-IPC address must be rejected");
        assert!(matches!(error, LifecycleError::Configuration(_)));
    }

    #[test]
    fn absent_server_probe_is_false() {
        let address = format!("ipc://core-control-absent-{}", std::process::id());
        let path = direct_ipc_socket_path(&address).expect("valid address");
        let _ = std::fs::remove_file(path);

        assert!(
            !ping_direct_ipc(&address, Duration::from_millis(10))
                .expect("absent server is not an error")
        );
    }
}
