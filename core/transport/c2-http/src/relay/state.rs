//! Central relay state — thread-safe wrapper around RouteTable + ConnectionPool.
//!
//! Lock ordering (must be followed everywhere):
//!   1. route_table (RwLock)
//!   2. conn_pool (internal slot mutexes)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use c2_config::RelayConfig;
use c2_ipc::{ClientIpcConfig, IpcClient, RouteBinding};
use parking_lot::RwLock;
use parking_lot::RwLockWriteGuard;

use crate::relay::authority::{
    ControlError, LocalRegistration, RouteAuthority, RouteCommand, RouteCommandResult,
};
use crate::relay::conn_pool::{
    AcquireError as PoolAcquireError, CachedClient, ConnectionPool, OwnerReplaceError,
    OwnerReplacementEvidence, OwnerToken, UpstreamLease,
};
use crate::relay::route_table::{RouteTable, TombstoneGcEntry};
use crate::relay::types::*;
use crate::relay::upstream_control::{self, UpstreamControlTask, UpstreamOwnerKey};

pub struct RelayState {
    route_table: RwLock<RouteTable>,
    conn_pool: ConnectionPool,
    upstream_controls: RwLock<HashMap<UpstreamOwnerKey, UpstreamControlTask>>,
    upstream_watch_unavailable: RwLock<HashMap<UpstreamOwnerKey, String>>,
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
        entry: Box<RouteEntry>,
        removed_at: f64,
        removed_revision: u64,
        client: Option<Arc<IpcClient>>,
    },
    AlreadyRemoved,
    NotFound,
    OwnerMismatch,
}

pub enum UpstreamAcquireError {
    NotFound,
    Stale {
        route: RouteEntry,
    },
    WatchUnavailable {
        route: RouteEntry,
        reason: String,
    },
    Unreachable {
        route: RouteEntry,
        address: String,
        error: c2_ipc::IpcError,
    },
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

fn should_treat_as_semantic_route_failure(error: &c2_ipc::IpcError) -> bool {
    matches!(
        error,
        c2_ipc::IpcError::IdentityMismatch { .. }
            | c2_ipc::IpcError::ContractMismatch(_)
            | c2_ipc::IpcError::RouteNotFound(_)
            | c2_ipc::IpcError::RouteRemoved { .. }
            | c2_ipc::IpcError::RouteClosed { .. }
    )
}

async fn verify_route_after_watch_unavailable(
    lease: &UpstreamLease,
    expected: &c2_contract::ExpectedRouteContract,
) -> Result<(), c2_ipc::IpcError> {
    match lease.client().lookup_route(expected).await {
        Err(c2_ipc::IpcError::CatalogCompacted { .. })
        | Err(c2_ipc::IpcError::WatchUnavailable(_)) => {
            lease.client().rebuild_route_catalog().await?;
            lease.client().lookup_route(expected).await
        }
        other => other,
    }
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
            upstream_controls: RwLock::new(HashMap::new()),
            upstream_watch_unavailable: RwLock::new(HashMap::new()),
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

    pub(crate) fn commit_register_upstream(
        &self,
        registration: LocalRegistration,
    ) -> RegisterCommitResult {
        match RouteAuthority::new(self).execute(RouteCommand::RegisterLocal(Box::new(registration)))
        {
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
            | Err(ControlError::ContractMismatch { reason })
            | Err(ControlError::UpstreamUnavailable { reason }) => {
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
                removed_revision,
                client,
            }) => {
                self.stop_upstream_control_if_owner_idle_for_route(&entry);
                UnregisterResult::Removed {
                    entry: Box::new(entry),
                    removed_at,
                    removed_revision,
                    client,
                }
            }
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
            | Err(ControlError::ContractMismatch { .. })
            | Err(ControlError::UpstreamUnavailable { .. }) => UnregisterResult::OwnerMismatch,
            Err(ControlError::DuplicateRoute { .. }) => UnregisterResult::OwnerMismatch,
        }
    }

    pub fn remove_unreachable_local_upstream_if_matches(
        &self,
        expected: &RouteEntry,
    ) -> Option<(RouteEntry, f64, u64, Option<Arc<IpcClient>>)> {
        let (entry, removed_at, removed_revision, client) = {
            let mut route_table = self.route_table.write();
            let (entry, removed_at, removed_revision) =
                route_table.unregister_local_route_if_matches(expected);
            let client = entry
                .as_ref()
                .and_then(UpstreamEndpointKey::from_route)
                .and_then(|key| {
                    if route_table.has_local_route_for_endpoint(&key) {
                        None
                    } else {
                        self.conn_pool.remove(&key)
                    }
                });
            (entry, removed_at, removed_revision, client)
        };
        entry.map(|entry| (entry, removed_at, removed_revision, client))
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

    #[cfg(test)]
    pub async fn acquire_upstream(
        &self,
        name: &str,
    ) -> Result<(UpstreamLease, RouteEntry), UpstreamAcquireError> {
        let expected = self
            .route_table
            .read()
            .local_route(name)
            .ok_or(UpstreamAcquireError::NotFound)?;
        let (lease, route, _binding) = self.acquire_upstream_for_route(&expected).await?;
        Ok((lease, route))
    }

    pub async fn acquire_upstream_for_route(
        &self,
        expected: &RouteEntry,
    ) -> Result<(UpstreamLease, RouteEntry, RouteBinding), UpstreamAcquireError> {
        if !self
            .route_table
            .read()
            .local_route(&expected.name)
            .is_some_and(|current| local_route_matches(&current, expected))
        {
            return Err(UpstreamAcquireError::Stale {
                route: expected.clone(),
            });
        }

        let Some(endpoint_key) = UpstreamEndpointKey::from_route(expected) else {
            return Err(UpstreamAcquireError::NotFound);
        };
        let route_name = expected.name.clone();
        let expected_for_connect = expected.clone();

        let lease = match self
            .conn_pool
            .acquire_with(&endpoint_key, move |endpoint| {
                let expected = expected_for_connect.clone();
                let route_name = route_name.clone();
                async move {
                    if expected.ipc_address.as_deref() != Some(endpoint.address()) {
                        return Err(c2_ipc::IpcError::Protocol(format!(
                            "relay upstream address mismatch for route {route_name}: expected {:?}, got {}",
                            expected.ipc_address,
                            endpoint.address()
                        )));
                    }
                    if expected.server_id.as_deref() != Some(endpoint.server_id())
                        || expected.server_instance_id.as_deref()
                            != Some(endpoint.server_instance_id())
                    {
                        return Err(c2_ipc::IpcError::Protocol(format!(
                            "relay upstream endpoint mismatch for route {route_name}: expected server_id={:?} server_instance_id={:?} address={:?}, got {endpoint}",
                            expected.server_id,
                            expected.server_instance_id,
                            expected.ipc_address
                        )));
                    }
                    let mut client =
                        IpcClient::with_config(endpoint.address(), ClientIpcConfig::default());
                    client.connect().await?;
                    if client.server_id() != expected.server_id.as_deref()
                        || client.server_instance_id() != expected.server_instance_id.as_deref()
                    {
                        let got_server_id = client.server_id().unwrap_or("").to_string();
                        let got_server_instance_id =
                            client.server_instance_id().unwrap_or("").to_string();
                        client.close().await;
                        return Err(c2_ipc::IpcError::IdentityMismatch {
                            expected_server_id: expected.server_id.clone().unwrap_or_default(),
                            expected_server_instance_id: expected
                                .server_instance_id
                                .clone()
                                .unwrap_or_default(),
                            actual_server_id: got_server_id,
                            actual_server_instance_id: got_server_instance_id,
                        });
                    }
                    let expected_contract = expected_contract_for_route(&expected);
                    if let Err(err) = client.acquire_route(&expected_contract).await {
                        client.close().await;
                        return Err(err);
                    }
                    Ok(Arc::new(client))
                }
            })
            .await
        {
            Ok(lease) => lease,
            Err(PoolAcquireError::NotFound) => return Err(UpstreamAcquireError::NotFound),
            Err(PoolAcquireError::Unreachable { endpoint, error }) => {
                if !self
                    .route_table
                    .read()
                    .local_route(&expected.name)
                    .is_some_and(|current| local_route_matches(&current, expected))
                {
                    return Err(UpstreamAcquireError::Stale {
                        route: expected.clone(),
                    });
                }
                return Err(UpstreamAcquireError::Unreachable {
                    route: expected.clone(),
                    address: endpoint.address().to_string(),
                    error,
                });
            }
        };

        let lease_address = lease.address();
        let expected_contract = expected_contract_for_route(expected);
        if let Some(reason) = self.upstream_control_watch_unavailable_for_route(expected) {
            match verify_route_after_watch_unavailable(&lease, &expected_contract).await {
                Ok(()) => {}
                Err(error) if should_treat_as_semantic_route_failure(&error) => {
                    if let Some(old_client) = lease.evict_current_client() {
                        old_client.close_shared().await;
                    }
                    drop(lease);
                    return Err(UpstreamAcquireError::Unreachable {
                        route: expected.clone(),
                        address: lease_address,
                        error,
                    });
                }
                Err(error) => {
                    if let Some(old_client) = lease.evict_current_client() {
                        old_client.close_shared().await;
                    }
                    drop(lease);
                    return Err(UpstreamAcquireError::WatchUnavailable {
                        route: expected.clone(),
                        reason: format!("{reason}; route lookup unavailable: {error}"),
                    });
                }
            }
        }
        let binding = match lease
            .client()
            .acquire_route_token(
                &expected_contract,
                &expected.route_uid,
                expected.route_revision,
            )
            .await
        {
            Ok(binding) => binding,
            Err(error) => {
                if let Some(old_client) = lease.evict_current_client() {
                    old_client.close_shared().await;
                }
                drop(lease);
                return match error {
                    c2_ipc::IpcError::RouteStale { .. } => Err(UpstreamAcquireError::Stale {
                        route: expected.clone(),
                    }),
                    error => Err(UpstreamAcquireError::Unreachable {
                        route: expected.clone(),
                        address: lease_address,
                        error,
                    }),
                };
            }
        };

        let lease_endpoint = lease.endpoint();
        let route_matches_lease =
            self.renew_owner_lease_if_current_route(expected, &lease_endpoint);

        if route_matches_lease {
            Ok((lease, expected.clone(), binding))
        } else {
            let client = lease.client();
            drop(lease);
            client.close_shared().await;
            Err(UpstreamAcquireError::Stale {
                route: expected.clone(),
            })
        }
    }

    fn renew_owner_lease_if_current_route(
        &self,
        expected: &RouteEntry,
        lease_endpoint: &UpstreamEndpointKey,
    ) -> bool {
        let route_table = self.route_table.read();
        let Some(entry) = route_table.local_route(&expected.name) else {
            return false;
        };
        if !local_route_matches(&entry, expected)
            || UpstreamEndpointKey::from_route(&entry).as_ref() != Some(lease_endpoint)
        {
            return false;
        }
        self.conn_pool.renew_current_owner_lease(lease_endpoint)
    }

    #[cfg(test)]
    pub(crate) fn get_address(&self, name: &str) -> Option<String> {
        let key = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))?;
        self.conn_pool.get_address(&key)
    }

    pub(crate) fn owner_token(&self, name: &str) -> Option<OwnerToken> {
        let key = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))?;
        self.conn_pool.owner_token(&key)
    }

    pub(crate) fn owner_token_for_endpoint(&self, key: &UpstreamEndpointKey) -> Option<OwnerToken> {
        self.conn_pool.owner_token(key)
    }

    pub(crate) fn matches_owner_token(&self, name: &str, token: &OwnerToken) -> bool {
        let Some(key) = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))
        else {
            return false;
        };
        self.conn_pool.matches_owner_token(&key, token)
    }

    #[cfg(test)]
    pub(crate) fn renew_owner_lease(&self, name: &str, token: &OwnerToken) -> bool {
        let Some(key) = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))
        else {
            return false;
        };
        self.conn_pool.renew_owner_lease(&key, token)
    }

    pub(crate) fn renew_owner_lease_for_endpoint(
        &self,
        key: &UpstreamEndpointKey,
        token: &OwnerToken,
    ) -> bool {
        self.conn_pool.renew_owner_lease(key, token)
    }

    pub(crate) fn validate_replaceable_owner_token(
        &self,
        key: &UpstreamEndpointKey,
        token: &OwnerToken,
        evidence: OwnerReplacementEvidence,
    ) -> Result<(), OwnerReplaceError> {
        self.conn_pool
            .validate_replaceable_owner_token(key, token, evidence)
    }

    pub(crate) fn connection_lookup(&self, name: &str) -> CachedClient {
        let Some(key) = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))
        else {
            return CachedClient::Missing;
        };
        self.conn_pool.lookup(&key)
    }

    pub(crate) fn insert_owner_slot(&self, entry: &RouteEntry) {
        if let Some(key) = UpstreamEndpointKey::from_route(entry) {
            self.conn_pool.insert_owner(key);
        }
    }

    pub(crate) fn start_upstream_control(self: &Arc<Self>, entry: &RouteEntry) {
        let Some(key) = upstream_control::owner_key_for_route(entry) else {
            return;
        };
        {
            let controls = self.upstream_controls.read();
            if controls.contains_key(&key) {
                return;
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task = upstream_control::spawn(Arc::clone(self), key.clone());
        let old_task = self.upstream_controls.write().insert(key, task);
        if let Some(old_task) = old_task {
            old_task.abort();
        }
    }

    pub(crate) fn mark_upstream_control_watch_unavailable(
        &self,
        key: &UpstreamOwnerKey,
        reason: impl Into<String>,
    ) {
        self.upstream_watch_unavailable
            .write()
            .insert(key.clone(), reason.into());
    }

    pub(crate) fn clear_upstream_control_watch_unavailable(&self, key: &UpstreamOwnerKey) {
        self.upstream_watch_unavailable.write().remove(key);
    }

    pub(crate) fn upstream_control_watch_unavailable_for_route(
        &self,
        route: &RouteEntry,
    ) -> Option<String> {
        let key = upstream_control::owner_key_for_route(route)?;
        self.upstream_watch_unavailable.read().get(&key).cloned()
    }

    pub(crate) fn local_routes_for_owner(&self, key: &UpstreamOwnerKey) -> Vec<RouteEntry> {
        self.route_table
            .read()
            .list_routes()
            .into_iter()
            .filter(|entry| upstream_control::owner_key_for_route(entry).as_ref() == Some(key))
            .collect()
    }

    pub(crate) fn clear_upstream_control_if_matches(
        &self,
        key: &UpstreamOwnerKey,
        token: &Arc<()>,
    ) {
        let mut controls = self.upstream_controls.write();
        if controls
            .get(key)
            .is_some_and(|task| task.token_matches(token))
        {
            controls.remove(key);
            drop(controls);
            self.clear_upstream_control_watch_unavailable(key);
        }
    }

    pub(crate) fn stop_upstream_control_if_owner_idle_for_route(&self, entry: &RouteEntry) {
        let Some(key) = upstream_control::owner_key_for_route(entry) else {
            return;
        };
        if !self.local_routes_for_owner(&key).is_empty() {
            return;
        }
        if let Some(task) = self.upstream_controls.write().remove(&key) {
            task.abort();
        }
        self.clear_upstream_control_watch_unavailable(&key);
    }

    pub(crate) fn remove_connection_if_endpoint_unused(
        &self,
        key: &UpstreamEndpointKey,
    ) -> Option<Arc<IpcClient>> {
        if self.route_table.read().has_local_route_for_endpoint(key) {
            None
        } else {
            self.conn_pool.remove(key)
        }
    }

    pub(crate) fn route_table_write(&self) -> RwLockWriteGuard<'_, RouteTable> {
        self.route_table.write()
    }

    pub(crate) fn evict_idle(
        &self,
        idle_timeout_ms: u64,
    ) -> Vec<(UpstreamEndpointKey, Option<Arc<IpcClient>>)> {
        self.conn_pool.evict_idle(idle_timeout_ms)
    }

    #[cfg(test)]
    pub(crate) fn evict_connection(&self, name: &str) -> Option<Arc<IpcClient>> {
        let key = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))?;
        self.conn_pool.evict(&key)
    }

    #[cfg(test)]
    pub(crate) fn reconnect(&self, name: &str, client: Arc<IpcClient>) {
        if let Some(key) = self
            .route_table
            .read()
            .local_route(name)
            .and_then(|entry| UpstreamEndpointKey::from_route(&entry))
        {
            self.conn_pool.reconnect(&key, client);
        }
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

    pub(crate) fn route_catalog_revisions(&self) -> (u64, u64) {
        self.with_route_table(|rt| (rt.catalog_revision(), rt.compaction_revision()))
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
    use crate::relay::authority::{OwnerReplacement, RegisterPreparation};

    const TEST_CRM_NS: &str = "test.relay";
    const TEST_CRM_NAME: &str = "RelayGrid";
    const TEST_CRM_VER: &str = "0.1.0";
    const TEST_ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_SIGNATURE_HASH: &str =
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    #[derive(Clone, Copy)]
    struct TestRouteContract<'a> {
        crm_ns: &'a str,
        crm_name: &'a str,
        crm_ver: &'a str,
        abi_hash: &'a str,
        signature_hash: &'a str,
    }

    const TEST_ROUTE_CONTRACT: TestRouteContract<'static> = TestRouteContract {
        crm_ns: TEST_CRM_NS,
        crm_name: TEST_CRM_NAME,
        crm_ver: TEST_CRM_VER,
        abi_hash: TEST_ABI_HASH,
        signature_hash: TEST_SIGNATURE_HASH,
    };

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
            TestRouteContract {
                crm_ns: "test.echo",
                crm_name: "Echo",
                crm_ver: "0.1.0",
                abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            },
            client,
        )
    }

    fn register_local_with_contract(
        state: &RelayState,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        address: &str,
        contract: TestRouteContract<'_>,
        _client: Arc<IpcClient>,
    ) -> RouteEntry {
        match test_commit_registration!(
            &state,
            name.to_string(),
            server_id.to_string(),
            server_instance_id.to_string(),
            address.to_string(),
            contract.crm_ns.to_string(),
            contract.crm_name.to_string(),
            contract.crm_ver.to_string(),
            contract.abi_hash.to_string(),
            contract.signature_hash.to_string(),
            1024,
            format!("{name}-{server_id}-uid"),
            1,
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
            "pub(crate) fn commit_register_upstream(",
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
                entry: Box::new(entry),
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
                removed_revision: 1,
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

        let result = test_commit_registration!(
            &state,
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
            "grid-server-old-uid".into(),
            1,
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

        let result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
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

        match test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
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
    fn local_routes_on_same_endpoint_share_one_owner_slot() {
        let state = RelayState::new(test_config(), null_disseminator());
        let manager = Arc::new(IpcClient::new("ipc://shared"));
        manager.force_connected(true);
        register_local_with_contract(
            &state,
            "manager",
            "server-grid",
            "server-grid-instance",
            "ipc://shared",
            TEST_ROUTE_CONTRACT,
            manager,
        );
        let builder = Arc::new(IpcClient::new("ipc://shared"));
        builder.force_connected(true);
        register_local_with_contract(
            &state,
            "builder",
            "server-grid",
            "server-grid-instance",
            "ipc://shared",
            TEST_ROUTE_CONTRACT,
            builder,
        );

        assert_eq!(state.local_route_count(), 2);
        assert_eq!(
            state.conn_pool.list_connections().len(),
            1,
            "multiple local routes on one server instance must share one endpoint slot"
        );
        assert!(
            state.evict_idle(0).is_empty(),
            "registering multiple routes must not install a data-plane client"
        );
    }

    #[test]
    fn unregister_one_route_keeps_shared_endpoint_for_remaining_routes() {
        let state = RelayState::new(test_config(), null_disseminator());
        let manager = Arc::new(IpcClient::new("ipc://shared"));
        manager.force_connected(true);
        register_local(&state, "manager", "server-grid", "ipc://shared", manager);
        let builder = Arc::new(IpcClient::new("ipc://shared"));
        builder.force_connected(true);
        register_local(&state, "builder", "server-grid", "ipc://shared", builder);

        assert!(matches!(
            state.unregister_upstream("manager", "server-grid"),
            UnregisterResult::Removed { client: None, .. }
        ));

        assert!(state.resolve("manager").is_empty());
        assert_eq!(state.resolve("builder").len(), 1);
        assert_eq!(state.conn_pool.list_connections().len(), 1);
        assert!(matches!(
            state.connection_lookup("builder"),
            CachedClient::OwnerOnly { .. }
        ));

        assert!(matches!(
            state.unregister_upstream("builder", "server-grid"),
            UnregisterResult::Removed { .. }
        ));
        assert!(state.conn_pool.list_connections().is_empty());
    }

    #[test]
    fn upstream_watch_unavailable_marks_owner_without_withdrawing_route() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        let entry = register_local(&state, "grid", "server-grid", "ipc://grid", client);
        let key = upstream_control::owner_key_for_route(&entry).expect("local route owner key");

        state.mark_upstream_control_watch_unavailable(&key, "watch stream closed");

        let reason = state
            .upstream_control_watch_unavailable_for_route(&entry)
            .expect("watch-unavailable reason recorded");
        assert!(reason.contains("watch stream closed"));
        assert_eq!(state.resolve("grid").len(), 1);
        assert_eq!(state.with_route_table(|rt| rt.list_tombstones().len()), 0);

        state.clear_upstream_control_watch_unavailable(&key);

        assert!(
            state
                .upstream_control_watch_unavailable_for_route(&entry)
                .is_none()
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
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
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
            entry: Box::new(RouteEntry {
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
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                locality: Locality::Peer,
                registered_at: 1000.0,
            }),
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
            removed_revision: 1,
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
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
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

        let first_result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
            None,
        );
        assert!(matches!(
            first_result,
            RegisterCommitResult::Registered { .. }
        ));

        let second_result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
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
        let racer_result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
            None,
        );
        assert!(matches!(
            racer_result,
            RegisterCommitResult::Registered { .. }
        ));

        let candidate = Arc::new(IpcClient::new("ipc://candidate"));
        candidate.force_connected(true);
        let candidate_result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
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
            TEST_ROUTE_CONTRACT,
            old,
        );
        state.evict_connection("grid");
        let replacement_proof =
            confirmed_dead_replacement(&state, "grid", "server-new", "instance-new", "ipc://new")
                .await;

        let replacement = Arc::new(IpcClient::new("ipc://new"));
        replacement.force_connected(true);
        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
            Some(replacement_proof),
        );

        assert!(matches!(result, RegisterCommitResult::Registered { .. }));
        assert_eq!(state.get_address("grid").as_deref(), Some("ipc://new"));
    }

    #[tokio::test]
    async fn route_replacement_keeps_old_endpoint_when_other_routes_still_reference_it() {
        let state = RelayState::new(test_config(), null_disseminator());
        let manager = Arc::new(IpcClient::new("ipc://old"));
        manager.force_connected(true);
        register_local_with_contract(
            &state,
            "manager",
            "server-old",
            "server-old-instance",
            "ipc://old",
            TEST_ROUTE_CONTRACT,
            manager,
        );
        let builder = Arc::new(IpcClient::new("ipc://old"));
        builder.force_connected(true);
        register_local_with_contract(
            &state,
            "builder",
            "server-old",
            "server-old-instance",
            "ipc://old",
            TEST_ROUTE_CONTRACT,
            builder,
        );
        let replacement_proof = confirmed_dead_replacement(
            &state,
            "manager",
            "server-new",
            "server-new-instance",
            "ipc://new",
        )
        .await;

        let result = test_commit_registration!(
            &state,
            "manager".into(),
            "server-new".into(),
            "server-new-instance".into(),
            "ipc://new".into(),
            TEST_CRM_NS.to_string(),
            TEST_CRM_NAME.to_string(),
            TEST_CRM_VER.to_string(),
            TEST_ABI_HASH.to_string(),
            TEST_SIGNATURE_HASH.to_string(),
            1024,
            "manager-server-new-uid".into(),
            1,
            Some(replacement_proof),
        );

        assert!(matches!(result, RegisterCommitResult::Registered { .. }));
        assert_eq!(state.get_address("manager").as_deref(), Some("ipc://new"));
        assert_eq!(state.get_address("builder").as_deref(), Some("ipc://old"));
        assert_eq!(
            state.conn_pool.list_connections().len(),
            2,
            "new owner endpoint and still-referenced old endpoint must coexist"
        );
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
        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
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
            TEST_ROUTE_CONTRACT,
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
        let same_owner_result = test_commit_registration!(
            &state,
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
            "grid-server-old-uid".into(),
            1,
            None,
        );
        assert!(matches!(
            same_owner_result,
            RegisterCommitResult::SameOwner { .. }
        ));

        let replacement = Arc::new(IpcClient::new("ipc://replacement"));
        replacement.force_connected(true);
        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
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
        let token = state.owner_token("grid").expect("owner token");
        assert!(state.renew_owner_lease("grid", &token));

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
            TestRouteContract {
                crm_ns: "test.old",
                crm_name: "OldGrid",
                crm_ver: "0.1.0",
                abi_hash: TEST_ABI_HASH,
                signature_hash: TEST_SIGNATURE_HASH,
            },
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
            TestRouteContract {
                crm_ns: "test.new",
                crm_name: "NewGrid",
                crm_ver: "0.1.0",
                abi_hash: TEST_ABI_HASH,
                signature_hash: TEST_SIGNATURE_HASH,
            },
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
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
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
        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
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
            TEST_ROUTE_CONTRACT,
            original,
        );
        state.evict_connection("grid");

        let result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::SameOwner { .. }));
        assert!(matches!(
            state.connection_lookup("grid"),
            CachedClient::OwnerOnly { .. }
        ));
    }

    #[test]
    fn same_owner_registration_rejects_contract_change_without_new_instance() {
        let state = RelayState::new(test_config(), null_disseminator());
        let original = Arc::new(IpcClient::new("ipc://grid"));
        original.force_connected(true);
        register_local(&state, "grid", "server-grid", "ipc://grid", original);

        let result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
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

        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::Registered { .. }));
        let routes = state.resolve("grid");
        assert_eq!(
            routes[0].server_instance_id.as_deref(),
            Some("instance-new")
        );
        assert!(matches!(
            state.connection_lookup("grid"),
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
        let result = test_commit_registration!(
            &state,
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
            "grid-test-route-uid".into(),
            1,
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
            TEST_ROUTE_CONTRACT,
            original,
        );
        state.evict_connection("grid");

        let replacement = Arc::new(IpcClient::new("ipc://grid"));
        replacement.force_connected(true);
        let result = test_commit_registration!(
            &state,
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
            "grid-server-grid-uid".into(),
            1,
            None,
        );

        assert!(matches!(result, RegisterCommitResult::SameOwner { .. }));
        assert!(matches!(
            state.acquire_upstream("grid").await,
            Err(UpstreamAcquireError::Unreachable { .. })
        ));
    }
}
