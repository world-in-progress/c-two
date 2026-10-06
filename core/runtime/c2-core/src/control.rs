use c2_config::{
    ConfigResolver, ConfigSources, LocalEndpoint, LocalEndpointProtocol, RuntimeConfigOverrides,
};
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

/// Resolve the endpoint protocol a process-level admin probe must use.
///
/// The canonical client IPC resolution (explicit overrides, process
/// environment / `.env`, then defaults) is the only protocol source here:
/// admin probes never probe old and new endpoint derivations to find a live
/// server.
fn resolved_admin_endpoint_protocol() -> Result<LocalEndpointProtocol, LifecycleError> {
    let config = ConfigResolver::resolve_client_ipc(
        Default::default(),
        RuntimeConfigOverrides::default(),
        ConfigSources::from_process(),
    )
    .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
    Ok(config.base.endpoint_protocol)
}

/// Resolve a validated direct IPC address to its operating-system endpoint.
///
/// The protocol is the resolved process client IPC policy — the same source
/// the client pool reads — so endpoint diagnostics can never disagree with
/// the endpoint a probe actually reaches. It is never a probe across endpoint
/// namespaces.
pub fn direct_ipc_endpoint(address: &str) -> Result<LocalEndpoint, LifecycleError> {
    let protocol = resolved_admin_endpoint_protocol()?;
    direct_ipc_endpoint_with_protocol(address, protocol)
}

/// Resolve a direct IPC address with one strict, explicit endpoint protocol.
///
/// An empty `protocol` is rejected by [`LocalEndpointProtocol`]'s parser
/// before it reaches here; callers that mean "the process policy" must call
/// [`direct_ipc_endpoint`] instead.
pub fn direct_ipc_endpoint_with_protocol(
    address: &str,
    protocol: LocalEndpointProtocol,
) -> Result<LocalEndpoint, LifecycleError> {
    c2_ipc::local_endpoint_from_ipc_address_with_protocol(address, protocol)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Probe a direct IPC server without involving relay discovery.
///
/// The protocol is the resolved process client IPC policy, never a probe.
pub fn ping_direct_ipc(address: &str, timeout: Duration) -> Result<bool, LifecycleError> {
    let protocol = resolved_admin_endpoint_protocol()?;
    ping_direct_ipc_with_protocol(address, protocol, timeout)
}

/// Probe a direct IPC server with one explicit endpoint protocol.
pub fn ping_direct_ipc_with_protocol(
    address: &str,
    protocol: LocalEndpointProtocol,
    timeout: Duration,
) -> Result<bool, LifecycleError> {
    c2_ipc::ping_with_protocol(address, protocol, timeout)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))
}

/// Initiate direct IPC shutdown without waiting for the owner-side drain.
///
/// The protocol is the resolved process client IPC policy, never a probe.
pub fn shutdown_direct_ipc(
    address: &str,
    timeout: Duration,
) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
    let protocol = resolved_admin_endpoint_protocol()?;
    shutdown_direct_ipc_with_protocol(address, protocol, timeout)
}

/// Initiate direct IPC shutdown with one explicit endpoint protocol.
pub fn shutdown_direct_ipc_with_protocol(
    address: &str,
    protocol: LocalEndpointProtocol,
    timeout: Duration,
) -> Result<DirectIpcShutdownOutcome, LifecycleError> {
    c2_ipc::shutdown_with_protocol(address, protocol, timeout)
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
