use c2_config::{
    ConfigResolver, ConfigSources, LocalEndpoint, LocalEndpointContext, LocalEndpointOptions,
};
use std::time::Duration;

use crate::{LifecycleError, Runtime};

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

/// Capture process configuration once for an independent admin operation.
fn process_context() -> Result<LocalEndpointContext, LifecycleError> {
    ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions::default(),
        ConfigSources::from_process(),
    )
    .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Derive an endpoint from current process configuration without local I/O.
pub fn direct_ipc_endpoint(address: &str) -> Result<LocalEndpoint, LifecycleError> {
    direct_ipc_endpoint_with_context(address, &process_context()?)
}

pub fn direct_ipc_endpoint_with_context(
    address: &str,
    context: &LocalEndpointContext,
) -> Result<LocalEndpoint, LifecycleError> {
    context
        .endpoint(address)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Probe using one process configuration snapshot for all retries.
pub fn ping_direct_ipc(address: &str, timeout: Duration) -> Result<bool, LifecycleError> {
    ping_direct_ipc_with_context(address, &process_context()?, timeout)
}

pub fn ping_direct_ipc_with_context(
    address: &str,
    context: &LocalEndpointContext,
    timeout: Duration,
) -> Result<bool, LifecycleError> {
    c2_ipc::ping_with_context(address, context, timeout)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Initiate direct IPC shutdown without waiting for the owner-side drain.
/// All retries retain a single process configuration snapshot.
pub fn shutdown_direct_ipc(
    address: &str,
    timeout: Duration,
) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
    shutdown_direct_ipc_with_context(address, &process_context()?, timeout)
}

pub fn shutdown_direct_ipc_with_context(
    address: &str,
    context: &LocalEndpointContext,
    timeout: Duration,
) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
    c2_ipc::shutdown_with_context(address, context, timeout)
        .map(DirectIpcShutdownOutcome::from)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

impl Runtime {
    /// Probe inside this Runtime's frozen local domain. Does not freeze memory policy.
    pub fn ping_direct_ipc(
        &self,
        address: &str,
        timeout: Duration,
    ) -> Result<bool, LifecycleError> {
        ping_direct_ipc_with_context(address, &self.freeze_local_endpoint_context()?, timeout)
    }

    /// Initiate admin shutdown inside this Runtime's frozen local domain.
    /// The acknowledgement is initiation only; Host shutdown observes completion.
    pub fn shutdown_direct_ipc(
        &self,
        address: &str,
        timeout: Duration,
    ) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
        shutdown_direct_ipc_with_context(address, &self.freeze_local_endpoint_context()?, timeout)
    }
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
            direct_ipc_endpoint("tcp://not-ipc").expect_err("non-IPC address must be rejected");
        assert!(matches!(error, LifecycleError::Configuration(_)));
    }

    #[test]
    fn absent_server_probe_is_false() {
        let address = format!("ipc://core-control-absent-{}", std::process::id());
        direct_ipc_endpoint(&address).expect("valid address");

        assert!(
            !ping_direct_ipc(&address, Duration::from_millis(10))
                .expect("absent server is not an error")
        );
    }
}
