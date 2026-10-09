use std::sync::Arc;
use std::time::Duration;

use c2_ipc::IpcClient;

use crate::relay::gossip::broadcast_route_withdraw;
use crate::relay::state::RelayState;
use crate::relay::types::{RouteEntry, UpstreamEndpointKey};

const CONTROL_RETRY_DELAY: Duration = Duration::from_secs(1);
const CONTROL_OBSERVE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) type UpstreamOwnerKey = UpstreamEndpointKey;

pub(crate) struct UpstreamControlTask {
    token: Arc<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl UpstreamControlTask {
    pub(crate) fn new(token: Arc<()>, handle: tokio::task::JoinHandle<()>) -> Self {
        Self { token, handle }
    }

    pub(crate) fn token_matches(&self, token: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.token, token)
    }

    pub(crate) async fn abort_and_wait(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    pub(crate) fn abort(self) {
        self.handle.abort();
    }
}

pub(crate) fn owner_key_for_route(entry: &RouteEntry) -> Option<UpstreamOwnerKey> {
    UpstreamEndpointKey::from_route(entry)
}

pub(crate) fn spawn(state: Arc<RelayState>, key: UpstreamOwnerKey) -> UpstreamControlTask {
    let task_key = key.clone();
    let token = Arc::new(());
    let task_token = Arc::clone(&token);
    let handle = tokio::spawn(async move {
        run_control_watch(state, task_key, task_token).await;
    });
    UpstreamControlTask::new(token, handle)
}

async fn run_control_watch(state: Arc<RelayState>, key: UpstreamOwnerKey, token: Arc<()>) {
    loop {
        if state.local_routes_for_owner(&key).is_empty() {
            state.clear_upstream_control_if_matches(&key, &token);
            return;
        }

        let endpoint = match state.endpoint_context().endpoint(key.address()) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                state.mark_upstream_control_watch_unavailable(
                    &key,
                    format!("control watch endpoint invalid: {error}"),
                );
                tokio::time::sleep(CONTROL_RETRY_DELAY).await;
                continue;
            }
        };
        let mut client = match state
            .clients
            .manage(|| IpcClient::with_endpoint(endpoint, c2_config::ClientIpcConfig::default()))
        {
            Ok(client) => client,
            Err(_) => {
                state.clear_upstream_control_if_matches(&key, &token);
                return;
            }
        };
        match client.connect().await {
            Ok(()) => {}
            Err(err) => {
                eprintln!(
                    "[relay] Upstream control watch connect failed: server_id={} server_instance_id={} address={} error={err}",
                    key.server_id(),
                    key.server_instance_id(),
                    key.address()
                );
                state.mark_upstream_control_watch_unavailable(
                    &key,
                    format!("control watch connect failed: {err}"),
                );
                tokio::time::sleep(CONTROL_RETRY_DELAY).await;
                continue;
            }
        }

        if client.server_id() != Some(key.server_id())
            || client.server_instance_id() != Some(key.server_instance_id())
        {
            eprintln!(
                "[relay] Upstream control watch identity mismatch: expected_server_id={} expected_server_instance_id={} address={} actual_server_id={} actual_server_instance_id={}",
                key.server_id(),
                key.server_instance_id(),
                key.address(),
                client.server_id().unwrap_or(""),
                client.server_instance_id().unwrap_or("")
            );
            remove_owner_routes(&state, &key, "identity_mismatch").await;
            drop(client);
            state.clear_upstream_control_if_matches(&key, &token);
            return;
        }
        state.clear_upstream_control_watch_unavailable(&key);

        loop {
            if state.local_routes_for_owner(&key).is_empty() {
                drop(client);
                state.clear_upstream_control_if_matches(&key, &token);
                return;
            }

            for route in state.local_routes_for_owner(&key) {
                if route_is_semantically_gone(&client, &route).await {
                    remove_route(&state, &route, "route_catalog_update").await;
                }
            }

            if !client.is_connected() {
                eprintln!(
                    "[relay] Upstream control watch disconnected: server_id={} server_instance_id={} address={}",
                    key.server_id(),
                    key.server_instance_id(),
                    key.address()
                );
                state.mark_upstream_control_watch_unavailable(&key, "control watch disconnected");
                break;
            }
            tokio::time::sleep(CONTROL_OBSERVE_INTERVAL).await;
        }

        drop(client);
        tokio::time::sleep(CONTROL_RETRY_DELAY).await;
    }
}

async fn route_is_semantically_gone(client: &IpcClient, route: &RouteEntry) -> bool {
    let expected = expected_contract_for_route(route);
    // Ordinary IPC connections do not subscribe to route updates, so cached
    // validation can retain a removed route indefinitely. Query the owner for
    // the complete contract on this existing observer cycle. Registration
    // publishes before opening business admission: Closed(RegisterCommitted)
    // still exists, so use the same control-plane attestation as registration.
    // Other closed states and contract errors remain semantic withdrawal;
    // transport errors still do not authorize withdrawal.
    matches!(
        client.attest_route_for_registration(&expected).await,
        Err(c2_ipc::IpcError::RouteNotFound(_))
            | Err(c2_ipc::IpcError::RouteRemoved { .. })
            | Err(c2_ipc::IpcError::RouteClosed { .. })
            | Err(c2_ipc::IpcError::ContractMismatch(_))
    )
}

async fn remove_owner_routes(state: &Arc<RelayState>, key: &UpstreamOwnerKey, reason: &str) {
    for route in state.local_routes_for_owner(key) {
        remove_route(state, &route, reason).await;
    }
}

async fn remove_route(state: &Arc<RelayState>, route: &RouteEntry, reason: &str) {
    let Some((entry, removed_at, removed_revision, client)) =
        state.remove_unreachable_local_upstream_if_matches(route)
    else {
        return;
    };
    if let Some(client) = client {
        state.clients.close(&client);
    }
    eprintln!(
        "[relay] Upstream control watch removed route: name={} server_id={} server_instance_id={} address={} removed_at={} removed_revision={} reason={reason}",
        entry.name,
        entry.server_id.as_deref().unwrap_or(""),
        entry.server_instance_id.as_deref().unwrap_or(""),
        entry.ipc_address.as_deref().unwrap_or(""),
        removed_at,
        removed_revision
    );
    broadcast_route_withdraw(state, &entry, removed_at, removed_revision);
}

fn expected_contract_for_route(route: &RouteEntry) -> c2_contract::ExpectedRouteContract {
    c2_contract::ExpectedRouteContract {
        route_name: route.name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH, reserve_echo_route};
    use crate::relay::types::Locality;
    use futures::FutureExt;

    const STEP: Duration = Duration::from_secs(2);
    const ROUTE_NAME: &str = "proxy/bypass/test";

    fn route_entry(
        server_id: &str,
        address: &str,
        route_uid: String,
        route_revision: u64,
    ) -> RouteEntry {
        RouteEntry {
            name: ROUTE_NAME.into(),
            relay_id: "test-relay".into(),
            relay_url: "http://127.0.0.1:1".into(),
            server_id: Some(server_id.into()),
            server_instance_id: Some(format!("{server_id}-instance")),
            ipc_address: Some(address.into()),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: c2_server::ServerIpcConfig::default().max_payload_size,
            route_uid,
            route_revision,
            locality: Locality::Local,
            registered_at: 0.0,
        }
    }

    /// Hold the real publish-before-open state, then observe later catalog
    /// transitions on the same connection without relying on watch timing.
    #[tokio::test]
    async fn watch_preserves_committed_registration_before_admission_opens() {
        let id = format!(
            "relay_watch_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let address = format!("ipc://{id}");
        let mut config = c2_server::ServerIpcConfig::default();
        config.base.pool_enabled = false;
        config.base.pool_prewarm_segments = 0;
        let server = Arc::new(
            c2_server::Server::new_with_identity(
                &address,
                config,
                c2_server::ServerIdentity {
                    server_id: id.clone(),
                    server_instance_id: format!("{id}-instance"),
                },
            )
            .unwrap(),
        );
        let reservation = reserve_echo_route(&server, ROUTE_NAME).await;
        let admission = server
            .commit_reserved_route_closed(reservation)
            .await
            .unwrap();
        let (route_uid, route_revision) = server.registered_route_identity(ROUTE_NAME).unwrap();
        let route = route_entry(&id, &address, route_uid, route_revision);
        let expected = expected_contract_for_route(&route);
        let run_server = server.clone();
        let mut server_task = tokio::spawn(async move { run_server.run().await });
        let mut ipc_config = c2_config::ClientIpcConfig::default();
        ipc_config.base.pool_enabled = false;
        ipc_config.base.pool_prewarm_segments = 0;
        let mut client = IpcClient::with_config(&address, ipc_config);

        // Cleanup runs after every assertion failure or bounded step timeout.
        let result = std::panic::AssertUnwindSafe(tokio::time::timeout(STEP, async {
            server.wait_until_ready(STEP).await.unwrap();
            client.connect().await.unwrap();
            assert_eq!(client.server_id(), Some(id.as_str()));
            assert_eq!(
                client.server_instance_id(),
                route.server_instance_id.as_deref()
            );
            assert!(
                matches!(
                    client.acquire_route(&expected).await,
                    Err(c2_ipc::IpcError::RouteClosed { .. })
                ),
                "business acquisition must reject the committed but unopened route"
            );
            assert!(
                client
                    .attest_route_for_registration(&expected)
                    .await
                    .is_ok()
            );
            assert!(
                !route_is_semantically_gone(&client, &route).await,
                "watch must preserve Closed(RegisterCommitted) until publication completes"
            );

            server.open_route_admission(admission).await.unwrap();
            client.acquire_route(&expected).await.unwrap();
            assert!(!route_is_semantically_gone(&client, &route).await);

            for field in 0..5 {
                let mut mismatched = route.clone();
                match field {
                    0 => mismatched.crm_ns = "test.other".into(),
                    1 => mismatched.crm_name = "Other".into(),
                    2 => mismatched.crm_ver = "0.2.0".into(),
                    3 => mismatched.abi_hash = TEST_SIGNATURE_HASH.into(),
                    _ => mismatched.signature_hash = TEST_ABI_HASH.into(),
                }
                assert!(
                    route_is_semantically_gone(&client, &mismatched).await,
                    "watch must reject mismatched contract field {field}"
                );
            }
            let mut missing = route.clone();
            missing.name = "missing/route".into();
            assert!(route_is_semantically_gone(&client, &missing).await);

            // The ordinary connection has a cached Ready contract. Closing
            // admission leaves the listener alive, giving a deterministic
            // terminal Closed(Shutdown) record for a fresh control query.
            assert_eq!(server.close_business_admission("shutdown").await, 1);
            assert!(
                client.validate_route_contract(&expected).is_ok(),
                "test must retain a stale ready contract in the ordinary client cache"
            );
            assert!(matches!(
                client.attest_route_for_registration(&expected).await,
                Err(c2_ipc::IpcError::RouteClosed { reason, .. }) if reason.contains("Shutdown")
            ));
            assert!(
                route_is_semantically_gone(&client, &route).await,
                "terminal closure must withdraw even with a cached ready contract"
            );

            assert!(server.unregister_route(ROUTE_NAME).await);
            assert!(
                client.validate_route_contract(&expected).is_ok(),
                "test must retain the cached contract after owner removal"
            );
            assert!(matches!(
                client.attest_route_for_registration(&expected).await,
                Err(c2_ipc::IpcError::RouteNotFound(_))
                    | Err(c2_ipc::IpcError::RouteRemoved { .. })
            ));
            assert!(
                route_is_semantically_gone(&client, &route).await,
                "removal must be read from the owner rather than the cached contract"
            );
        }))
        .catch_unwind()
        .await;

        let client_closed = client.close_shared_bounded(STEP).await;
        let server_stopped = server.shutdown_and_wait(STEP).await;
        let joined = tokio::time::timeout(STEP, &mut server_task).await;
        if joined.is_err() {
            server_task.abort();
            let _ = server_task.await;
        }
        assert!(client_closed, "watch test client did not close");
        assert!(joined.is_ok(), "watch test server task did not exit");
        match result {
            Ok(result) => result.expect("watch test exceeded its bounded observation window"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
        assert!(
            server_stopped.is_ok(),
            "watch test server did not stop: {server_stopped:?}"
        );
        assert!(
            matches!(joined, Ok(Ok(Ok(())))),
            "watch test server run failed: {joined:?}"
        );
    }

    #[tokio::test]
    async fn watch_transport_failure_does_not_withdraw_route() {
        let address = "ipc://relay_watch_unconnected";
        let client = IpcClient::new(address);
        let route = route_entry("unconnected", address, "unconnected-route".into(), 1);
        assert!(matches!(
            client
                .attest_route_for_registration(&expected_contract_for_route(&route))
                .await,
            Err(c2_ipc::IpcError::Closed)
        ));
        assert!(
            !route_is_semantically_gone(&client, &route).await,
            "a missing transport connection is not proof that the route disappeared"
        );
        assert!(client.close_shared_bounded(STEP).await);
    }
}
