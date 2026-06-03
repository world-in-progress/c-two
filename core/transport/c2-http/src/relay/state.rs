//! Central relay state — thread-safe wrapper around RouteTable + ConnectionPool.
//!
//! Lock ordering (must be followed everywhere):
//!   1. route_table (RwLock)
//!   2. conn_pool (internal slot mutexes)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c2_config::RelayConfig;
use c2_ipc::{ClientIpcConfig, IpcClient};
use parking_lot::RwLock;
use parking_lot::RwLockWriteGuard;

use crate::relay::authority::{
    ControlError, OwnerReplacement, RouteAuthority, RouteCommand, RouteCommandResult,
};
use crate::relay::conn_pool::{
    AcquireError as PoolAcquireError, CachedClient, ConnectionPool, OwnerReplaceError,
    OwnerReplacementEvidence, OwnerToken, UpstreamLease,
};
use crate::relay::route_table::{RouteTable, TombstoneGcEntry};
use crate::relay::types::*;

pub struct RelayState {
    route_table: RwLock<RouteTable>,
    conn_pool: ConnectionPool,
    config: Arc<RelayConfig>,
    disseminator: Arc<dyn crate::relay::disseminator::Disseminator>,
}

fn owner_lease_duration(config: &RelayConfig) -> Option<Duration> {
    match config.idle_timeout_secs {
        0 => None,
        seconds => Some(Duration::from_secs(seconds)),
    }
}

#[derive(Clone)]
pub enum RegisterCommitResult {
    Registered { entry: RouteEntry },
    SameOwner { entry: RouteEntry },
    Duplicate { existing_address: String },
    ConflictingOwner { existing_address: String },
    Invalid { reason: String },
}

pub enum UnregisterResult {
    Removed {
        entry: RouteEntry,
        removed_at: f64,
        client: Option<Arc<IpcClient>>,
    },
    AlreadyRemoved,
    NotFound,
    OwnerMismatch,
}

pub enum UpstreamAcquireError {
    NotFound,
    Unreachable {
        route: RouteEntry,
        address: String,
        error: c2_ipc::IpcError,
    },
}

impl RelayState {
    pub fn new(
        config: Arc<RelayConfig>,
        disseminator: Arc<dyn crate::relay::disseminator::Disseminator>,
    ) -> Self {
        let owner_lease_duration = owner_lease_duration(&config);
        Self {
            route_table: RwLock::new(RouteTable::new(config.relay_id.clone())),
            conn_pool: ConnectionPool::with_owner_lease_duration(owner_lease_duration),
            disseminator,
            config,
        }
    }

    pub fn disseminator(&self) -> &Arc<dyn crate::relay::disseminator::Disseminator> {
        &self.disseminator
    }

    pub fn config(&self) -> &RelayConfig {
        &self.config
    }
    pub fn relay_id(&self) -> &str {
        &self.config.relay_id
    }

    // -- Transactional: route + connection together --

    pub fn commit_register_upstream(
        &self,
        name: String,
        server_id: String,
        server_instance_id: String,
        address: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        max_payload_size: u64,
        replacement: Option<OwnerReplacement>,
    ) -> RegisterCommitResult {
        match RouteAuthority::new(self).execute(RouteCommand::RegisterLocal {
            name,
            server_id,
            server_instance_id,
            address,
            crm_ns,
            crm_name,
            crm_ver,
            abi_hash,
            signature_hash,
            max_payload_size,
            replacement,
        }) {
            Ok(RouteCommandResult::Registered { entry }) => {
                RegisterCommitResult::Registered { entry }
            }
            Ok(RouteCommandResult::SameOwner { entry }) => {
                RegisterCommitResult::SameOwner { entry }
            }
            Err(ControlError::AddressMismatch { existing_address }) => {
                RegisterCommitResult::ConflictingOwner { existing_address }
            }
            Err(ControlError::DuplicateRoute { existing_address }) => {
                RegisterCommitResult::Duplicate { existing_address }
            }
            Err(ControlError::InvalidName { reason })
            | Err(ControlError::InvalidServerId { reason })
            | Err(ControlError::InvalidServerInstanceId { reason })
            | Err(ControlError::InvalidAddress { reason })
            | Err(ControlError::ContractMismatch { reason }) => {
                RegisterCommitResult::Invalid { reason }
            }
            Ok(
                RouteCommandResult::Unregistered { .. }
                | RouteCommandResult::AlreadyUnregistered
                | RouteCommandResult::PeerRouteChanged
                | RouteCommandResult::PeerRoutesRemoved,
            )
            | Err(ControlError::OwnerMismatch)
            | Err(ControlError::NotFound) => RegisterCommitResult::Duplicate {
                existing_address: "<unknown>".to_string(),
            },
        }
    }

    /// Unregister a LOCAL upstream CRM.
    pub fn unregister_upstream(&self, name: &str, server_id: &str) -> UnregisterResult {
        match RouteAuthority::new(self).execute(RouteCommand::UnregisterLocal {
            name: name.to_string(),
            server_id: server_id.to_string(),
        }) {
            Ok(RouteCommandResult::Unregistered {
                entry,
                removed_at,
                client,
            }) => UnregisterResult::Removed {
                entry,
                removed_at,
                client,
            },
            Ok(
                RouteCommandResult::Registered { .. }
                | RouteCommandResult::SameOwner { .. }
                | RouteCommandResult::PeerRouteChanged
                | RouteCommandResult::PeerRoutesRemoved,
            ) => UnregisterResult::OwnerMismatch,
            Ok(RouteCommandResult::AlreadyUnregistered) => UnregisterResult::AlreadyRemoved,
            Err(ControlError::NotFound) => UnregisterResult::NotFound,
            Err(ControlError::OwnerMismatch)
            | Err(ControlError::AddressMismatch { .. })
            | Err(ControlError::InvalidName { .. })
            | Err(ControlError::InvalidServerId { .. })
            | Err(ControlError::InvalidServerInstanceId { .. })
            | Err(ControlError::InvalidAddress { .. })
            | Err(ControlError::ContractMismatch { .. }) => UnregisterResult::OwnerMismatch,
            Err(ControlError::DuplicateRoute { .. }) => UnregisterResult::OwnerMismatch,
        }
    }

    pub fn remove_unreachable_local_upstream_if_matches(
        &self,
        expected: &RouteEntry,
    ) -> Option<(RouteEntry, f64, Option<Arc<IpcClient>>)> {
        let (entry, removed_at, client) = {
            let mut route_table = self.route_table.write();
            let (entry, removed_at) = route_table.unregister_local_route_if_matches(expected);
            let client = if entry.is_some() {
                self.conn_pool.remove(&expected.name)
            } else {
                None
            };
            (entry, removed_at, client)
        };
        entry.map(|entry| (entry, removed_at, client))
    }

    // -- Route-only operations --

    #[cfg(test)]
    pub fn resolve(&self, name: &str) -> Vec<RouteInfo> {
        self.route_table.read().resolve(name)
    }

    pub fn resolve_matching(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Vec<RouteInfo> {
        self.route_table.read().resolve_matching(expected)
    }

    pub fn route_names(&self) -> Vec<String> {
        self.route_table.read().route_names()
    }

    pub fn list_routes(&self) -> Vec<RouteEntry> {
        self.route_table.read().list_routes()
    }

    pub(crate) fn local_route(&self, name: &str) -> Option<RouteEntry> {
        self.route_table.read().local_route(name)
    }

    // -- Connection-only operations --

    pub async fn acquire_upstream(
        &self,
        name: &str,
    ) -> Result<(UpstreamLease, RouteEntry), UpstreamAcquireError> {
        let expected = self
            .route_table
            .read()
            .local_route(name)
            .ok_or(UpstreamAcquireError::NotFound)?;
        let route_name = name.to_string();
        let expected_for_connect = expected.clone();

        let lease = match self
            .conn_pool
            .acquire_with(name, move |address| {
                let expected = expected_for_connect.clone();
                let route_name = route_name.clone();
                async move {
                    if expected.ipc_address.as_deref() != Some(address.as_str()) {
                        return Err(c2_ipc::IpcError::Handshake(format!(
                            "relay upstream address mismatch for route {route_name}: expected {:?}, got {address}",
                            expected.ipc_address
                        )));
                    }
                    let mut client =
                        IpcClient::with_config(&address, ClientIpcConfig::default());
                    client.connect().await?;
                    if client.server_id() != expected.server_id.as_deref()
                        || client.server_instance_id() != expected.server_instance_id.as_deref()
                    {
                        let got_server_id = client.server_id().unwrap_or("").to_string();
                        let got_server_instance_id =
                            client.server_instance_id().unwrap_or("").to_string();
                        client.close().await;
                        return Err(c2_ipc::IpcError::Handshake(format!(
                            "relay upstream identity mismatch for route {route_name}: expected {}/{}, got {got_server_id}/{got_server_instance_id}",
                            expected.server_id.as_deref().unwrap_or(""),
                            expected.server_instance_id.as_deref().unwrap_or(""),
                        )));
                    }
                    let expected_contract = c2_contract::ExpectedRouteContract {
                        route_name: route_name.clone(),
                        crm_ns: expected.crm_ns.clone(),
                        crm_name: expected.crm_name.clone(),
                        crm_ver: expected.crm_ver.clone(),
                        abi_hash: expected.abi_hash.clone(),
                        signature_hash: expected.signature_hash.clone(),
                    };
                    if let Err(err) = client.validate_route_contract(&expected_contract) {
                        client.close().await;
                        return Err(err);
                    }
                    if !client.has_route(&route_name) {
                        client.close().await;
                        return Err(c2_ipc::IpcError::RouteNotFound(route_name));
                    }
                    Ok(Arc::new(client))
                }
            })
            .await
        {
            Ok(lease) => lease,
            Err(PoolAcquireError::NotFound) => return Err(UpstreamAcquireError::NotFound),
            Err(PoolAcquireError::Unreachable { address, error }) => {
                return Err(UpstreamAcquireError::Unreachable {
                    route: expected,
                    address,
                    error,
                });
            }
        };

        let lease_address = lease.address();
        let route_matches_lease =
            self.renew_owner_lease_if_current_route(&expected, lease_address.as_str());

        if route_matches_lease {
            Ok((lease, expected))
        } else {
            let client = lease.client();
            drop(lease);
            client.close_shared().await;
            Err(UpstreamAcquireError::NotFound)
        }
    }

    fn renew_owner_lease_if_current_route(
        &self,
        expected: &RouteEntry,
        lease_address: &str,
    ) -> bool {
        let route_table = self.route_table.read();
        let Some(entry) = route_table.local_route(&expected.name) else {
            return false;
        };
        if !local_route_matches(&entry, expected)
            || entry.ipc_address.as_deref() != Some(lease_address)
        {
            return false;
        }
        self.conn_pool.renew_current_owner_lease(&expected.name)
    }

    #[cfg(test)]
    pub(crate) fn get_address(&self, name: &str) -> Option<String> {
        self.conn_pool.get_address(name)
    }

    pub(crate) fn owner_token(&self, name: &str) -> Option<OwnerToken> {
        self.conn_pool.owner_token(name)
    }

    pub(crate) fn matches_owner_token(&self, name: &str, token: &OwnerToken) -> bool {
        self.conn_pool.matches_owner_token(name, token)
    }

    pub(crate) fn renew_owner_lease(&self, name: &str, token: &OwnerToken) -> bool {
        self.conn_pool.renew_owner_lease(name, token)
    }

    pub(crate) fn replace_if_owner_token(
        &self,
        name: &str,
        token: &OwnerToken,
        new_address: String,
        evidence: OwnerReplacementEvidence,
    ) -> Result<Option<Arc<IpcClient>>, OwnerReplaceError> {
        self.conn_pool
            .replace_if_owner_token(name, token, new_address, evidence)
    }

    pub(crate) fn connection_lookup(&self, name: &str) -> CachedClient {
        self.conn_pool.lookup(name)
    }

    pub(crate) fn insert_owner_slot(&self, name: String, address: String) {
        self.conn_pool.insert_owner(name, address);
    }

    pub(crate) fn remove_connection(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.conn_pool.remove(name)
    }

    pub(crate) fn route_table_write(&self) -> RwLockWriteGuard<'_, RouteTable> {
        self.route_table.write()
    }

    pub(crate) fn evict_idle(&self, idle_timeout_ms: u64) -> Vec<(String, Option<Arc<IpcClient>>)> {
        self.conn_pool.evict_idle(idle_timeout_ms)
    }

    #[cfg(test)]
    pub(crate) fn evict_connection(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.conn_pool.evict(name)
    }

    #[cfg(test)]
    pub(crate) fn reconnect(&self, name: &str, client: Arc<IpcClient>) {
        self.conn_pool.reconnect(name, client);
    }

    // -- Peer management --

    #[cfg(test)]
    pub fn register_peer(&self, info: PeerInfo) {
        self.route_table.write().register_peer(info);
    }

    pub fn unregister_peer(&self, relay_id: &str) -> Option<PeerInfo> {
        self.route_table.write().unregister_peer(relay_id)
    }

    pub fn has_peer(&self, relay_id: &str) -> bool {
        self.route_table.read().has_peer(relay_id)
    }

    pub fn peer_is_alive(&self, relay_id: &str) -> bool {
        self.route_table.read().peer_is_alive(relay_id)
    }

    pub fn list_peers(&self) -> Vec<PeerSnapshot> {
        self.route_table
            .read()
            .list_peers()
            .into_iter()
            .map(|p| PeerSnapshot {
                relay_id: p.relay_id.clone(),
                url: p.url.clone(),
                route_count: p.route_count,
                status: p.status,
            })
            .collect()
    }

    pub fn local_route_count(&self) -> u32 {
        self.route_table.read().local_route_count()
    }

    // -- Snapshot operations --

    pub fn full_snapshot(&self) -> FullSync {
        self.route_table.read().full_snapshot()
    }

    pub fn merge_snapshot(&self, sync: ValidatedFullSync) {
        self.route_table.write().merge_validated_snapshot(sync);
    }

    pub fn route_digest(&self) -> HashMap<(String, String, bool), RouteDigestHash> {
        self.route_table.read().route_digest()
    }

    pub(crate) fn route_state_for_diff(
        &self,
        name: &str,
        relay_id: &str,
        deleted: bool,
    ) -> Option<crate::relay::peer::DigestDiffEntry> {
        self.route_table
            .read()
            .route_state_for_diff(name, relay_id, deleted)
    }

    pub(crate) fn authoritative_missing_tombstone(
        &self,
        name: &str,
        relay_id: &str,
    ) -> Option<RouteTombstone> {
        self.route_table
            .write()
            .authoritative_missing_tombstone(name, relay_id)
    }

    pub(crate) fn gc_tombstones(&self, retention: std::time::Duration) -> Vec<TombstoneGcEntry> {
        self.route_table.write().gc_tombstones(retention)
    }

    pub(crate) fn with_route_table<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&RouteTable) -> R,
    {
        f(&self.route_table.read())
    }

    pub(crate) fn with_route_table_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut RouteTable) -> R,
    {
        f(&mut self.route_table.write())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::authority::RegisterPreparation;

    const TEST_CRM_NS: &str = "test.relay";
    const TEST_CRM_NAME: &str = "RelayGrid";
    const TEST_CRM_VER: &str = "0.1.0";
    const TEST_ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_SIGNATURE_HASH: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    struct NullDisseminator;
    impl crate::relay::disseminator::Disseminator for NullDisseminator {
        fn broadcast(
            &self,
            _envelope: crate::relay::peer::PeerEnvelope,
            _peers: &[PeerSnapshot],
        ) -> Option<tokio::task::JoinHandle<()>> {
            None
        }
    }

    fn test_config() -> Arc<RelayConfig> {
        Arc::new(RelayConfig {
            relay_id: "test-relay".into(),
            advertise_url: "http://localhost:9999".into(),
            ..Default::default()
        })
    }

    fn null_disseminator() -> Arc<dyn crate::relay::disseminator::Disseminator> {
        Arc::new(NullDisseminator)
    }

    fn register_local(
        state: &RelayState,
        name: &str,
        server_id: &str,
        address: &str,
        client: Arc<IpcClient>,
    ) -> RouteEntry {
        register_local_with_instance(
            state,
            name,
            server_id,
            &format!("{server_id}-instance"),
            address,
            client,
        )
    }

    fn register_local_with_instance(
        state: &RelayState,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
        client: Arc<IpcClient>,
    ) -> RouteEntry {
        register_local_with_contract(
            state,
            name,
            server_id,
            server_instance_id,
            address,
            "test.echo",
            "Echo",
            "0.1.0",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            client,
        )
    }

    fn register_local_with_contract(
        state: &RelayState,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        _client: Arc<IpcClient>,
    ) -> RouteEntry {
        match state.commit_register_upstream(
            name.to_string(),
            server_id.to_string(),
            server_instance_id.to_string(),
            address.to_string(),
            crm_ns.to_string(),
            crm_name.to_string(),
            crm_ver.to_string(),
            abi_hash.to_string(),
            signature_hash.to_string(),
            1024,
            None,
        ) {
            RegisterCommitResult::Registered { entry }
            | RegisterCommitResult::SameOwner { entry } => entry,
            RegisterCommitResult::Duplicate { existing_address }
            | RegisterCommitResult::ConflictingOwner { existing_address } => {
                panic!("unexpected duplicate route at {existing_address}")
            }
            RegisterCommitResult::Invalid { reason } => {
                panic!("unexpected invalid route in test helper: {reason}")
            }
        }
    }

    async fn confirmed_dead_replacement(
        state: &RelayState,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
    ) -> OwnerReplacement {
        let preliminary = RouteAuthority::new(state)
            .prepare_register(name, server_id, server_instance_id, address)
            .await
            .expect("preliminary replacement should be available");
        let replacement = match preliminary {
            RegisterPreparation::Available {
                replacement: Some(replacement),
            } => replacement,
            _ => panic!("expected replacement candidate"),
        };
        RouteAuthority::new(state)
            .confirm_replacement_for_commit(Some(replacement))
            .await
            .expect("dead owner should confirm replacement")
            .expect("replacement proof should be present")
    }

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> Option<&'a str> {
        let start_idx = source.find(start)?;
        let after_start = &source[start_idx..];
        let end_idx = after_start.find(end)?;
        Some(&after_start[..end_idx])
    }

    #[test]
    fn state_layer_does_not_export_replacement_evidence_token() {
        let source = include_str!("state.rs");
        let token_type = concat!("Owner", "Replacement", "Token");
        let token_argument = concat!("replacement: Option<Owner", "Replacement", "Token>");
        assert!(
            !source.contains(token_type),
            "state.rs must not expose a replacement token that callers can fill with evidence"
        );
        assert!(
            !source.contains(token_argument),
            "commit_register_upstream must consume opaque OwnerReplacement directly"
        );
    }

    #[test]
    fn state_commit_does_not_reconstruct_replacement_proof() {
        let source = include_str!("state.rs");
        let body = source_between(
            source,
            "pub fn commit_register_upstream(",
            "pub fn unregister_upstream(",
        )
        .expect("commit_register_upstream body should be found");
        assert!(
            !body.contains("OwnerReplacement {"),
            "state commit must not reconstruct OwnerReplacement from caller-provided fields"
        );
        assert!(
            !body.contains("evidence:"),
            "state commit must not copy caller-provided evidence into replacement proof"
        );
    }

    fn announce_peer_route(state: &RelayState, entry: RouteEntry) {
        let sender_relay_id = entry.relay_id.clone();
        RouteAuthority::new(state)
            .execute(RouteCommand::AnnouncePeer {
                sender_relay_id,
                entry,
            })
            .unwrap();
    }

    fn withdraw_peer_route(state: &RelayState, name: &str, relay_id: &str) {
        RouteAuthority::new(state)
            .execute(RouteCommand::WithdrawPeer {
                sender_relay_id: relay_id.to_string(),
                name: name.to_string(),
                relay_id: relay_id.to_string(),
                removed_at: 1001.0,
            })
            .unwrap();
    }

    fn remove_peer_routes(state: &RelayState, relay_id: &str) {
        assert!(matches!(
            RouteAuthority::new(state)
                .execute(RouteCommand::RemovePeerRoutes {
                    relay_id: relay_id.to_string(),
                })
                .unwrap(),
            RouteCommandResult::PeerRoutesRemoved
        ));
    }

    #[test]
    fn local_commit_rejects_invalid_crm_tag_without_fake_duplicate() {
        let state = RelayState::new(test_config(), null_disseminator());

        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://grid".into(),
            "test.mesh".into(),
            "Grid\nInjected".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(
            matches!(result, RegisterCommitResult::Invalid { reason } if reason.contains("control characters"))
        );
        assert!(state.resolve("grid").is_empty());
    }

    #[test]
    fn local_commit_rejects_invalid_ipc_address_without_fake_duplicate() {
        let state = RelayState::new(test_config(), null_disseminator());

        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://../escape".into(),
            "test.mesh".into(),
            "Grid".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(
            matches!(result, RegisterCommitResult::Invalid { reason } if reason.contains("path separators"))
        );
        assert!(state.resolve("grid").is_empty());
    }

    #[test]
    fn register_and_resolve_upstream() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        register_local(&state, "grid", "server-grid", "ipc://grid", client);
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
        assert_eq!(
            state.list_routes()[0].server_id.as_deref(),
            Some("server-grid")
        );
    }

    #[test]
    fn local_registration_does_not_create_idle_data_plane_client() {
        let state = RelayState::new(test_config(), null_disseminator());

        match state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        ) {
            RegisterCommitResult::Registered { .. } => {}
            _ => panic!("unexpected registration result"),
        }

        assert!(
            state.evict_idle(0).is_empty(),
            "route registration must not attach a relay data-plane client"
        );
        assert_eq!(state.resolve("grid").len(), 1);
    }

    #[test]
    fn unregister_upstream() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        register_local(&state, "grid", "server-grid", "ipc://grid", client);
        assert!(matches!(
            state.unregister_upstream("grid", "server-grid"),
            UnregisterResult::Removed { .. }
        ));
        assert!(state.resolve("grid").is_empty());
    }

    #[test]
    fn unregister_upstream_rejects_wrong_server_id() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        client.force_connected(true);
        register_local(&state, "grid", "server-grid", "ipc://grid", client);

        assert!(matches!(
            state.unregister_upstream("grid", "server-other"),
            UnregisterResult::OwnerMismatch
        ));
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
    }

    #[test]
    fn peer_route_operations() {
        let state = RelayState::new(test_config(), null_disseminator());
        state.register_peer(PeerInfo {
            relay_id: "peer-1".into(),
            url: "http://peer-1:8080".into(),
            route_count: 0,
            last_heartbeat: std::time::Instant::now(),
            status: PeerStatus::Alive,
        });
        announce_peer_route(
            &state,
            RouteEntry {
                name: "remote".into(),
                relay_id: "peer-1".into(),
                relay_url: "http://peer-1:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        );
        assert_eq!(state.resolve("remote").len(), 1);
        withdraw_peer_route(&state, "remote", "peer-1");
        assert!(state.resolve("remote").is_empty());
    }

    #[test]
    fn register_peer_route_does_not_overwrite_local() {
        // Anti-entropy can echo our own routes back to us. The peer-route
        // entry path MUST refuse anything carrying our own relay_id, or it
        // would silently demote a Local route (with ipc_address) to a Peer
        // route (without ipc_address) and break local IPC dispatch.
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        register_local(&state, "grid", "server-grid", "ipc://grid", client);

        // Echo of our own route arriving via DigestDiff with our relay_id.
        let result = RouteAuthority::new(&state).execute(RouteCommand::AnnouncePeer {
            sender_relay_id: "test-relay".into(),
            entry: RouteEntry {
                name: "grid".into(),
                relay_id: "test-relay".into(),
                relay_url: "http://elsewhere:8080".into(),
                server_id: None,
                server_instance_id: None,
                ipc_address: None,
                crm_ns: "test.ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                max_payload_size: 1024,
                locality: Locality::Peer,
                registered_at: 1000.0,
            },
        });
        assert!(matches!(result, Err(ControlError::OwnerMismatch)));

        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(
            routes[0].ipc_address.as_deref(),
            Some("ipc://grid"),
            "echoed peer route must not overwrite our LOCAL route"
        );
    }

    #[test]
    fn unregister_peer_route_does_not_remove_local_route() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        register_local(&state, "grid", "server-grid", "ipc://grid", client);

        let result = RouteAuthority::new(&state).execute(RouteCommand::WithdrawPeer {
            sender_relay_id: "test-relay".into(),
            name: "grid".into(),
            relay_id: "test-relay".into(),
            removed_at: 1001.0,
        });
        assert!(matches!(result, Err(ControlError::OwnerMismatch)));

        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
    }

    #[test]
    fn remove_routes_by_relay_does_not_remove_local_routes() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        register_local(&state, "grid", "server-grid", "ipc://grid", client);

        remove_peer_routes(&state, "test-relay");

        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
    }

    #[test]
    fn route_authority_preflight_uses_route_table_when_connection_entry_is_missing() {
        let state = RelayState::new(test_config(), null_disseminator());
        state.with_route_table_mut(|rt| {
            rt.register_route(RouteEntry {
                name: "grid".into(),
                relay_id: "test-relay".into(),
                relay_url: "http://localhost:9999".into(),
                server_id: Some("server-old".into()),
                server_instance_id: Some("instance-old".into()),
                ipc_address: Some("ipc://grid-old".into()),
                crm_ns: TEST_CRM_NS.to_string(),
                crm_name: TEST_CRM_NAME.to_string(),
                crm_ver: TEST_CRM_VER.to_string(),
                abi_hash: TEST_ABI_HASH.to_string(),
                signature_hash: TEST_SIGNATURE_HASH.to_string(),
                max_payload_size: 1024,
                locality: Locality::Local,
                registered_at: 1000.0,
            });
        });

        assert!(matches!(
            RouteAuthority::new(&state).register_local_preflight(
                "grid",
                "server-old",
                "instance-old",
                "ipc://grid-old",
            ),
            Ok(crate::relay::authority::RegisterPreflight::SameOwner)
        ));
        assert!(matches!(
            RouteAuthority::new(&state).register_local_preflight(
                "grid",
                "server-new",
                "instance-new",
                "ipc://grid-new",
            ),
            Err(ControlError::DuplicateRoute { .. })
        ));
    }

    #[test]
    fn register_commit_rechecks_owner_after_preflight_no_owner() {
        let state = RelayState::new(test_config(), null_disseminator());
        let first = Arc::new(IpcClient::new("ipc://first"));
        first.force_connected(true);
        let second = Arc::new(IpcClient::new("ipc://second"));
        second.force_connected(true);

        let first_result = state.commit_register_upstream(
            "grid".into(),
            "server-first".into(),
            "instance-first".into(),
            "ipc://first".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );
        assert!(matches!(
            first_result,
            RegisterCommitResult::Registered { .. }
        ));

        let second_result = state.commit_register_upstream(
            "grid".into(),
            "server-second".into(),
            "instance-second".into(),
            "ipc://second".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );
        assert!(matches!(
            second_result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://first"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://first"));
    }

    #[tokio::test]
    async fn candidate_preparation_does_not_grant_registration_right_after_race() {
        let state = RelayState::new(test_config(), null_disseminator());
        let replacement = RouteAuthority::new(&state)
            .prepare_candidate_registration("grid", "server-candidate", "ipc://candidate")
            .await
            .unwrap();
        assert!(replacement.is_none());

        let racer = Arc::new(IpcClient::new("ipc://racer"));
        racer.force_connected(true);
        let racer_result = state.commit_register_upstream(
            "grid".into(),
            "server-racer".into(),
            "instance-racer".into(),
            "ipc://racer".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );
        assert!(matches!(
            racer_result,
            RegisterCommitResult::Registered { .. }
        ));

        let candidate = Arc::new(IpcClient::new("ipc://candidate"));
        candidate.force_connected(true);
        let candidate_result = state.commit_register_upstream(
            "grid".into(),
            "server-candidate".into(),
            "instance-candidate".into(),
            "ipc://candidate".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(
            candidate_result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://racer"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://racer"));
    }

    #[tokio::test]
    async fn replacement_proof_can_replace_same_slot_only_while_still_evicted() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://old"));
        old.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-old",
            "server-old-instance",
            "ipc://old",
            TEST_CRM_NS,
            TEST_CRM_NAME,
            TEST_CRM_VER,
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            old,
        );
        state.evict_connection("grid");
        let replacement_proof =
            confirmed_dead_replacement(&state, "grid", "server-new", "instance-new", "ipc://new")
                .await;

        let replacement = Arc::new(IpcClient::new("ipc://new"));
        replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-new".into(),
            "instance-new".into(),
            "ipc://new".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            Some(replacement_proof),
        );

        assert!(matches!(result, RegisterCommitResult::Registered { .. }));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://new"));
    }

    #[tokio::test]
    async fn replacement_proof_does_not_match_re_registered_same_address_owner() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://same"));
        old.force_connected(true);
        register_local(&state, "grid", "server-old", "ipc://same", old);
        state.evict_connection("grid");
        let replacement_proof = confirmed_dead_replacement(
            &state,
            "grid",
            "server-racer",
            "instance-racer",
            "ipc://replacement",
        )
        .await;
        assert!(matches!(
            state.unregister_upstream("grid", "server-old"),
            UnregisterResult::Removed { .. }
        ));

        let new_same_address = Arc::new(IpcClient::new("ipc://same"));
        new_same_address.force_connected(true);
        register_local(
            &state,
            "grid",
            "server-new-same-address",
            "ipc://same",
            new_same_address,
        );

        let stale_replacement = Arc::new(IpcClient::new("ipc://replacement"));
        stale_replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-racer".into(),
            "instance-racer".into(),
            "ipc://replacement".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            Some(replacement_proof),
        );

        assert!(matches!(
            result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://same"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://same"));
    }

    #[tokio::test]
    async fn replacement_proof_is_rejected_after_same_owner_lease_renewal() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://old"));
        old.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-old",
            "server-old-instance",
            "ipc://old",
            TEST_CRM_NS,
            TEST_CRM_NAME,
            TEST_CRM_VER,
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            old,
        );
        state.evict_connection("grid");
        let replacement_proof = confirmed_dead_replacement(
            &state,
            "grid",
            "server-new",
            "server-new-instance",
            "ipc://replacement",
        )
        .await;

        let same_owner = Arc::new(IpcClient::new("ipc://old"));
        same_owner.force_connected(true);
        let same_owner_result = state.commit_register_upstream(
            "grid".into(),
            "server-old".into(),
            "server-old-instance".into(),
            "ipc://old".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );
        assert!(matches!(
            same_owner_result,
            RegisterCommitResult::SameOwner { .. }
        ));

        let replacement = Arc::new(IpcClient::new("ipc://replacement"));
        replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-new".into(),
            "server-new-instance".into(),
            "ipc://replacement".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            Some(replacement_proof),
        );

        assert!(matches!(
            result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://old"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://old"));
    }

    #[tokio::test]
    async fn replacement_proof_is_rejected_after_successful_upstream_acquire() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://old"));
        old.force_connected(true);
        register_local(&state, "grid", "server-old", "ipc://old", old);
        state.evict_connection("grid");
        let replacement_proof = confirmed_dead_replacement(
            &state,
            "grid",
            "server-new",
            "server-new-instance",
            "ipc://replacement",
        )
        .await;
        let reconnected = Arc::new(IpcClient::new("ipc://old"));
        reconnected.force_connected(true);
        state.reconnect("grid", reconnected);

        let (lease, route) = match state.acquire_upstream("grid").await {
            Ok(acquired) => acquired,
            Err(_) => panic!("expected test upstream acquire to succeed"),
        };
        assert_eq!(route.name, "grid");
        drop(lease);

        state.evict_connection("grid");
        let replacement = Arc::new(IpcClient::new("ipc://replacement"));
        replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-new".into(),
            "server-new-instance".into(),
            "ipc://replacement".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            Some(replacement_proof),
        );

        assert!(matches!(
            result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://old"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://old"));
    }

    #[test]
    fn owner_lease_renewal_is_not_peer_visible_route_state() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://old"));
        old.force_connected(true);
        register_local(&state, "grid", "server-old", "ipc://old", old);

        let before_snapshot =
            serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap();
        let before_digest = state.route_digest();
        assert!(state.conn_pool.renew_current_owner_lease("grid"));

        assert_eq!(
            serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap(),
            before_snapshot
        );
        assert_eq!(state.route_digest(), before_digest);
    }

    #[test]
    fn stale_unreachable_removal_does_not_delete_new_same_address_owner_with_different_tag() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://same"));
        old.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-old",
            "instance-old",
            "ipc://same",
            "test.old",
            "OldGrid",
            "0.1.0",
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            old,
        );
        state.evict_connection("grid");
        assert!(matches!(
            state.unregister_upstream("grid", "server-old"),
            UnregisterResult::Removed { .. }
        ));

        let new_same_address = Arc::new(IpcClient::new("ipc://same"));
        new_same_address.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-new",
            "instance-new",
            "ipc://same",
            "test.new",
            "NewGrid",
            "0.1.0",
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            new_same_address,
        );

        let stale_snapshot = RouteEntry {
            name: "grid".into(),
            relay_id: "test-relay".into(),
            relay_url: "http://localhost:9999".into(),
            server_id: Some("server-old".into()),
            server_instance_id: Some("instance-old".into()),
            ipc_address: Some("ipc://same".into()),
            crm_ns: "test.old".into(),
            crm_name: "OldGrid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 1024,
            locality: Locality::Local,
            registered_at: 0.0,
        };
        assert!(
            state
                .remove_unreachable_local_upstream_if_matches(&stale_snapshot)
                .is_none(),
            "stale failure for the old owner must not remove a new same-address owner"
        );
        let current = state.local_route("grid").expect("new route should remain");
        assert_eq!(current.server_id.as_deref(), Some("server-new"));
        assert_eq!(current.crm_name, "NewGrid");
    }

    #[test]
    fn replacement_proof_cannot_replace_reconnected_owner_slot() {
        let state = RelayState::new(test_config(), null_disseminator());
        let old = Arc::new(IpcClient::new("ipc://same-slot"));
        old.force_connected(true);
        register_local(&state, "grid", "server-old", "ipc://same-slot", old);
        state.evict_connection("grid");
        let replacement_proof = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(confirmed_dead_replacement(
                &state,
                "grid",
                "server-new",
                "instance-new",
                "ipc://replacement",
            ));

        let reconnected_old = Arc::new(IpcClient::new("ipc://same-slot"));
        reconnected_old.force_connected(true);
        state.reconnect("grid", reconnected_old);

        let replacement = Arc::new(IpcClient::new("ipc://replacement"));
        replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-new".into(),
            "instance-new".into(),
            "ipc://replacement".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            Some(replacement_proof),
        );

        assert!(matches!(
            result,
            RegisterCommitResult::Duplicate {
                existing_address
            } if existing_address == "ipc://same-slot"
        ));
        assert_eq!(
            state.get_address("grid").as_deref(),
            Some("ipc://same-slot")
        );
    }

    #[test]
    fn same_server_registration_is_idempotent_without_repairing_evicted_client() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://grid"));
        original.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-grid",
            "server-grid-instance",
            "ipc://grid",
            TEST_CRM_NS,
            TEST_CRM_NAME,
            TEST_CRM_VER,
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            original,
        );
        state.evict_connection("grid");

        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::SameOwner { .. }));
        assert!(matches!(
            state.conn_pool.lookup("grid"),
            CachedClient::OwnerOnly { .. }
        ));
    }

    #[test]
    fn same_owner_registration_rejects_contract_change_without_new_instance() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://grid"));
        original.force_connected(true);
        register_local(&state, "grid", "server-grid", "ipc://grid", original);

        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://grid".into(),
            "test.other".into(),
            "OtherGrid".into(),
            "0.1.0".into(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(
            result,
            RegisterCommitResult::Invalid { reason }
                if reason.contains("CRM contract mismatch")
        ));
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].crm_ns, "test.echo");
        assert_eq!(routes[0].crm_name, "Echo");
    }

    #[test]
    fn same_server_new_instance_refreshes_local_route_owner_slot() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://grid"));
        original.force_connected(true);
        register_local_with_instance(
            &state,
            "grid",
            "server-grid",
            "instance-old",
            "ipc://grid",
            original,
        );

        assert!(matches!(
            RouteAuthority::new(&state).register_local_preflight(
                "grid",
                "server-grid",
                "instance-new",
                "ipc://grid",
            ),
            Ok(crate::relay::authority::RegisterPreflight::Available { .. })
        ));

        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "instance-new".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::Registered { .. }));
        let routes = state.resolve("grid");
        assert_eq!(
            routes[0].server_instance_id.as_deref(),
            Some("instance-new")
        );
        assert!(matches!(
            state.conn_pool.lookup("grid"),
            CachedClient::OwnerOnly { .. }
        ));
    }

    #[test]
    fn same_server_registration_with_different_address_conflicts() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://old"));
        original.force_connected(true);
        register_local(&state, "grid", "server-grid", "ipc://old", original);

        let moved = Arc::new(IpcClient::new("ipc://new"));
        moved.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "instance-grid".into(),
            "ipc://new".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(
            result,
            RegisterCommitResult::ConflictingOwner {
                existing_address
            } if existing_address == "ipc://old"
        ));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://old"));
    }

    #[tokio::test]
    async fn acquire_after_unregister_reports_not_found() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        client.force_connected(true);
        register_local(&state, "grid", "server-grid", "ipc://grid", client);

        assert!(matches!(
            state.unregister_upstream("grid", "server-grid"),
            UnregisterResult::Removed { .. }
        ));

        assert!(matches!(
            state.acquire_upstream("grid").await,
            Err(UpstreamAcquireError::NotFound)
        ));
    }

    #[tokio::test]
    async fn same_server_register_does_not_repair_evicted_slot() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://grid"));
        original.force_connected(true);
        register_local_with_contract(
            &state,
            "grid",
            "server-grid",
            "server-grid-instance",
            "ipc://grid",
            TEST_CRM_NS,
            TEST_CRM_NAME,
            TEST_CRM_VER,
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            original,
        );
        state.evict_connection("grid");

        let replacement = Arc::new(IpcClient::new("ipc://grid"));
        replacement.force_connected(true);
        let result = state.commit_register_upstream(
            "grid".into(),
            "server-grid".into(),
            "server-grid-instance".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::SameOwner { .. }));
        assert!(matches!(
            state.acquire_upstream("grid").await,
            Err(UpstreamAcquireError::Unreachable { .. })
        ));
    }
}
