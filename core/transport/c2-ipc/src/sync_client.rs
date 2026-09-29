//! Synchronous IPC client — embeds a tokio runtime handle.
//!
//! Wraps [`IpcClient`] for blocking calls from SDK bindings.
//! Multiple `SyncClient` instances share a single tokio runtime.

use parking_lot::Mutex;
use std::sync::{Arc, OnceLock};

use c2_mem::MemPool;

use crate::client::{
    ClientIpcConfig, IpcClient, IpcError, MethodTable, RequestBlock, RequestTransportKind,
    RouteBinding, ServerPoolState, choose_request_transport,
};
use crate::response::{ResponseData, ResponseLease};

/// Whether an IPC call failure is proven to precede CRM dispatch.
///
/// This is intentionally a transport-owned fact. Core retry policy consumes
/// it directly and never infers dispatch safety from display text or an OS
/// error kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportPhase {
    PreDispatch,
    DispatchUncertain,
}

/// An IPC call failure paired with its authoritative dispatch phase.
#[derive(Debug)]
pub struct IpcCallError {
    phase: TransportPhase,
    source: IpcError,
}

impl IpcCallError {
    pub(crate) const fn new(phase: TransportPhase, source: IpcError) -> Self {
        Self { phase, source }
    }

    pub const fn phase(&self) -> TransportPhase {
        self.phase
    }

    pub const fn is_retry_safe(&self) -> bool {
        matches!(self.phase, TransportPhase::PreDispatch)
    }

    pub const fn source_error(&self) -> &IpcError {
        &self.source
    }

    pub fn into_source(self) -> IpcError {
        self.source
    }
}

impl std::fmt::Display for IpcCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "IPC call failed during {:?}: {}",
            self.phase, self.source
        )
    }
}

impl std::error::Error for IpcCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn call_error_phase(error: &IpcError) -> TransportPhase {
    match error {
        IpcError::Config(_)
        | IpcError::Handshake(_)
        | IpcError::Protocol(_)
        | IpcError::IdentityMismatch { .. }
        | IpcError::ContractMismatch(_)
        | IpcError::RouteNotFound(_)
        | IpcError::RouteRemoved { .. }
        | IpcError::RouteClosed { .. }
        | IpcError::RouteStale { .. }
        | IpcError::CatalogCompacted { .. }
        | IpcError::WatchUnavailable(_)
        | IpcError::MethodNotFound { .. }
        | IpcError::Shm(_)
        | IpcError::Pool(_) => TransportPhase::PreDispatch,
        IpcError::Io(_)
        | IpcError::Decode(_)
        | IpcError::Chunk(_)
        | IpcError::CrmError(_)
        | IpcError::Closed => TransportPhase::DispatchUncertain,
    }
}

// ── Global shared runtime ────────────────────────────────────────────────

static GLOBAL_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Return the shared tokio runtime, creating it on first call.
///
/// The runtime uses 2 worker threads — sufficient for client I/O.
fn get_or_create_runtime() -> &'static tokio::runtime::Runtime {
    GLOBAL_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("c2-client-io")
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
}

// ── SyncClient ───────────────────────────────────────────────────────────

/// Synchronous IPC client — embeds a tokio runtime handle.
///
/// Wraps `IpcClient` for blocking calls from SDK bindings.
/// Multiple SyncClients share a single tokio runtime.
pub struct SyncClient {
    inner: IpcClient,
    rt: tokio::runtime::Handle,
}

// Compile-time assertion: SyncClient must be Send+Sync for binding wrappers
// that may be shared across threads.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<SyncClient>();
        _assert_sync::<SyncClient>();
    }
};

impl SyncClient {
    /// Connect to a server with optional pool for SHM transfers.
    pub fn connect(
        address: &str,
        pool: Option<Arc<Mutex<MemPool>>>,
        config: ClientIpcConfig,
    ) -> Result<Self, IpcError> {
        let rt = get_or_create_runtime();
        let mut client = match pool {
            Some(p) => IpcClient::with_pool(address, p, config),
            None => IpcClient::with_config(address, config),
        };
        rt.block_on(client.connect())?;
        Ok(Self {
            inner: client,
            rt: rt.handle().clone(),
        })
    }

    /// Test-only connect that arms a probe before the receive task is spawned.
    #[cfg(test)]
    pub(crate) fn connect_with_partial_header_probe_for_test(
        address: &str,
        config: ClientIpcConfig,
        ready: std::sync::mpsc::Sender<()>,
        receiver_drop_gate: Option<crate::client::ReceiverDropGateForTest>,
    ) -> Result<Self, IpcError> {
        let rt = get_or_create_runtime();
        let mut client = IpcClient::with_config(address, config);
        client.set_partial_header_pending_for_test(ready);
        if let Some(gate) = receiver_drop_gate {
            client.set_receiver_drop_gate_for_test(gate);
        }
        rt.block_on(client.connect())?;
        Ok(Self {
            inner: client,
            rt: rt.handle().clone(),
        })
    }

    /// Connect with a transport-internal pool (pooled-client acquire path).
    ///
    /// The pool was built from `config` by the caller inside this crate and is
    /// owned solely by the resulting client, so it is not subject to the
    /// injected-pool policy gate. `budget` is the owning cache's shared domain
    /// context: the pool already charges it and the client's reassembly pool
    /// charges the same context.
    pub(crate) fn connect_transport_pool(
        address: &str,
        pool: Arc<Mutex<MemPool>>,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
    ) -> Result<Self, IpcError> {
        let rt = get_or_create_runtime();
        let mut client = IpcClient::with_transport_pool(address, pool, config, budget);
        rt.block_on(client.connect())?;
        Ok(Self {
            inner: client,
            rt: rt.handle().clone(),
        })
    }

    /// Synchronous CRM call through an immutable route binding.
    pub fn call_bound(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        self.rt
            .block_on(self.inner.call_bound(binding, method_name, data))
    }

    /// Synchronous CRM call with an explicit dispatch-safety phase on failure.
    pub fn call_bound_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcCallError> {
        self.call_bound(binding, method_name, data)
            .map_err(|source| IpcCallError::new(call_error_phase(&source), source))
    }

    /// Whether the client has a SHM pool and data exceeds the threshold.
    pub fn should_use_shm(&self, data_len: usize) -> bool {
        choose_request_transport(&self.inner.config, self.inner.has_request_pool(), data_len)
            == RequestTransportKind::Buddy
    }

    /// Allocate from the client SHM pool and write data in a single lock scope.
    ///
    /// The returned [`RequestBlock`] carries the exact pool that owns the
    /// allocation. Later writes and every release path go through that owner,
    /// so a confirmed close that detaches the client's pool (or a reconnect
    /// that installs a fresh incarnation) can never redirect the coordinates
    /// into a replacement pool.
    ///
    /// On error, the caller should fall back to the canonical route-bound call
    /// path.
    pub fn pool_alloc_and_write(&self, data: &[u8]) -> Result<RequestBlock, IpcError> {
        let pool_arc = self
            .inner
            .select_request_pool()
            .ok_or_else(|| IpcError::Pool("no client pool".into()))?;
        let mut pool = pool_arc.lock();
        let alloc = pool
            .alloc(data.len())
            .map_err(|e| IpcError::Pool(format!("alloc failed: {e}")))?;
        let ptr = match pool.data_ptr(&alloc) {
            Ok(ptr) => ptr,
            Err(e) => {
                let _ = pool.free(&alloc);
                return Err(IpcError::Pool(format!("data_ptr failed: {e}")));
            }
        };
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }
        drop(pool);
        Ok(RequestBlock::new(pool_arc, alloc))
    }

    /// Allocate from the client SHM pool and let the caller fill the block.
    ///
    /// The fill callback receives exactly `data_size` bytes and runs without
    /// any pool lock held; the returned [`RequestBlock`] keeps the owning pool
    /// alive. On callback failure the allocation is released through that same
    /// owner before the error is returned, and a panicking callback unwinds
    /// through the block's armed `Drop` release, so neither path can strand
    /// the allocation or its shared-domain charge.
    pub fn pool_alloc_and_fill<F>(
        &self,
        data_size: usize,
        fill: F,
    ) -> Result<RequestBlock, IpcError>
    where
        F: FnOnce(&mut [u8]) -> Result<(), String>,
    {
        let pool_arc = self
            .inner
            .select_request_pool()
            .ok_or_else(|| IpcError::Pool("no client pool".into()))?;
        let (alloc, ptr) = {
            let mut pool = pool_arc.lock();
            let alloc = pool
                .alloc(data_size)
                .map_err(|e| IpcError::Pool(format!("alloc failed: {e}")))?;
            match pool.data_ptr(&alloc) {
                Ok(ptr) => (alloc, ptr),
                Err(e) => {
                    let _ = pool.free(&alloc);
                    return Err(IpcError::Pool(format!("data_ptr failed: {e}")));
                }
            }
        };
        let block = RequestBlock::new(pool_arc, alloc);
        let destination = unsafe { std::slice::from_raw_parts_mut(ptr, data_size) };
        if let Err(err) = fill(destination) {
            let _ = block.release();
            return Err(IpcError::Pool(format!("fill failed: {err}")));
        }
        Ok(block)
    }

    /// Release a request block through the exact pool that owns it.
    ///
    /// Used on send failure for cleanup. Never resolves the client's current
    /// pool slot: the block already carries its owner.
    pub fn pool_free(&self, block: &RequestBlock) {
        let _ = block.release();
    }

    /// Synchronous CRM call with pre-allocated SHM data through an immutable route binding.
    pub fn call_bound_prealloc(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        block: &RequestBlock,
        data_size: usize,
    ) -> Result<ResponseData, IpcError> {
        let (method_idx, identity) = match (|| {
            let (method_idx, identity, max_payload_size) = binding.call_target_for(method_name)?;
            let data_size_u64 = u64::try_from(data_size).unwrap_or(u64::MAX);
            if data_size_u64 > max_payload_size {
                return Err(IpcError::Config(format!(
                    "request payload size {data_size_u64} exceeds route '{}' max_payload_size {max_payload_size}",
                    binding.route_name()
                )));
            }
            Ok((method_idx, identity))
        })() {
            Ok(target) => target,
            Err(err) => {
                let _ = block.release();
                return Err(err);
            }
        };
        self.rt.block_on(
            self.inner
                .call_with_prealloc(&identity, method_idx, block, data_size),
        )
    }

    /// Get a reference to the server SHM pool (for FFI layer).
    pub fn server_pool_arc(&self) -> Arc<Mutex<Option<ServerPoolState>>> {
        self.inner.server_pool.clone()
    }

    /// The transport-owned request pool this client currently holds, if any.
    ///
    /// `None` for clients without a config-owned pool and after a confirmed
    /// close detached an idle pool (`connect` recreates a fresh incarnation).
    #[cfg(test)]
    pub(crate) fn request_pool(&self) -> Option<Arc<Mutex<MemPool>>> {
        self.inner.request_pool()
    }

    /// The complete resolved client configuration this connection was created
    /// from. Used by the owning cache to reject a same-address hit whose
    /// requested policy differs from the cached connection's policy.
    pub(crate) fn config(&self) -> &ClientIpcConfig {
        self.inner.config()
    }

    /// Bind a response to the exact transport pool that owns its backing.
    ///
    /// Reassembled handles carry their own pool and budget charge inside the
    /// [`c2_wire::chunk::ReassemblyBacking`] carrier.
    pub fn lease_response(&self, response: ResponseData) -> ResponseLease {
        ResponseLease::new(response, self.server_pool_arc())
    }

    /// Synchronous close.
    pub fn close(&mut self) {
        self.rt.block_on(self.inner.close_shared());
    }

    /// Synchronous shared-ownership close with a bounded close barrier.
    ///
    /// Usable through `Arc<SyncClient>` when unique ownership cannot be
    /// proven. Returns `true` only when the receive task finished and the
    /// writer slot cleared within `timeout`; `false` reports an unconfirmed
    /// close honestly — nothing is force-stopped or force-released beyond the
    /// bounded abort attempts made inside the barrier.
    pub fn close_shared(&self, timeout: std::time::Duration) -> bool {
        self.rt.block_on(self.inner.close_shared_bounded(timeout))
    }

    /// Whether the client is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Get the route table for a named route.
    pub fn route_table(&self, name: &str) -> Option<MethodTable> {
        self.inner.route_table(name)
    }

    /// Get all route names.
    pub fn route_names(&self) -> Vec<String> {
        self.inner.route_names()
    }

    /// Validate the connected route against an expected CRM contract.
    pub fn validate_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        self.inner.validate_route_contract(expected)
    }

    /// Ensure the connected server currently exports a route matching an expected CRM contract.
    pub fn ensure_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        self.rt.block_on(self.inner.ensure_route_contract(expected))
    }

    /// Authoritatively acquire and bind a route against an expected CRM contract.
    pub fn acquire_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteBinding, IpcError> {
        self.rt.block_on(self.inner.acquire_route(expected))
    }

    /// Authoritatively acquire one exact route token.
    pub fn acquire_route_token(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        route_uid: &str,
        route_revision: u64,
    ) -> Result<RouteBinding, IpcError> {
        self.rt.block_on(
            self.inner
                .acquire_route_token(expected, route_uid, route_revision),
        )
    }

    /// CRM tag advertised by a route, if present.
    pub fn route_contract(&self, route_name: &str) -> Option<c2_contract::ExpectedRouteContract> {
        self.inner.route_contract(route_name)
    }

    /// Identity announced by the connected IPC server handshake.
    pub fn server_identity(&self) -> Option<&c2_wire::handshake::ServerIdentity> {
        self.inner.server_identity()
    }

    /// Stable logical server ID announced by the connected IPC server.
    pub fn server_id(&self) -> Option<&str> {
        self.inner.server_id()
    }

    /// Per-server-incarnation ID announced by the connected IPC server.
    pub fn server_instance_id(&self) -> Option<&str> {
        self.inner.server_instance_id()
    }
}

// ── Test-only helpers ────────────────────────────────────────────────────

#[cfg(test)]
impl SyncClient {
    /// Create an unconnected `SyncClient` for pool bookkeeping tests.
    ///
    /// The resulting client is **not** connected to any server —
    /// `is_connected()` returns `false` and route-bound calls will fail.
    pub(crate) fn new_unconnected(address: &str) -> Self {
        let rt = get_or_create_runtime();
        let inner = IpcClient::new(address);
        Self {
            inner,
            rt: rt.handle().clone(),
        }
    }

    /// Deterministically occupy the writer slot exactly like a bulk write
    /// stuck on a non-reading peer would, signalling `acquired` once the
    /// slot is held and releasing it when `release` resolves.
    ///
    /// This replaces timing-dependent blocked-pipe fixtures: the close
    /// barrier's writer phase observes an unavailable writer slot without
    /// any 32 MiB stack buffers or kernel pipe-buffer pressure.
    pub(crate) fn hold_writer_slot_for_test(
        &self,
        acquired: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        let writer = self.inner.writer_slot_for_test();
        self.rt.spawn(async move {
            let _guard = writer.lock().await;
            let _ = acquired.send(());
            let _ = release.await;
        });
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Expose runtime pointer for cross-module test assertions.
    pub fn runtime_ptr() -> *const tokio::runtime::Runtime {
        get_or_create_runtime() as *const _
    }

    #[test]
    fn test_global_runtime_returns_same_instance() {
        let rt1 = get_or_create_runtime();
        let rt2 = get_or_create_runtime();
        // OnceLock guarantees the same pointer — verify via handle equality.
        let h1 = rt1.handle();
        let h2 = rt2.handle();
        // Both handles should be able to spawn; identity check via pointer.
        assert!(std::ptr::eq(rt1, rt2));
        // Extra: verify the handles are functional.
        let result = h1.block_on(async { 42 });
        assert_eq!(result, 42);
        let result2 = h2.block_on(async { 43 });
        assert_eq!(result2, 43);
    }

    #[test]
    fn call_phase_is_transport_owned_and_dispatch_uncertainty_is_not_retry_safe() {
        let before_dispatch = IpcCallError::new(
            TransportPhase::PreDispatch,
            IpcError::MethodNotFound {
                route_name: "route".to_string(),
                method_name: "missing".to_string(),
            },
        );
        let uncertain = IpcCallError::new(TransportPhase::DispatchUncertain, IpcError::Closed);

        assert!(before_dispatch.is_retry_safe());
        assert!(!uncertain.is_retry_safe());
    }

    #[test]
    fn sync_client_projects_server_identity() {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "identity-server".to_string(),
            server_instance_id: "identity-instance".to_string(),
        };
        let mut client = SyncClient::new_unconnected("ipc://identity_projection_sync");
        client.inner.server_identity = Some(identity.clone());

        assert_eq!(client.server_identity(), Some(&identity));
        assert_eq!(client.server_id(), Some("identity-server"));
        assert_eq!(client.server_instance_id(), Some("identity-instance"));
    }

    #[test]
    fn sync_client_shm_projection_reuses_canonical_selector() {
        let config = ClientIpcConfig {
            shm_threshold: 100,
            base: c2_config::BaseIpcConfig {
                chunk_size: 500,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };
        let pool = Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig::default())));
        let client = SyncClient {
            inner: IpcClient::with_pool("ipc://sync_selector", pool, config),
            rt: get_or_create_runtime().handle().clone(),
        };

        assert!(!client.should_use_shm(50));
        assert!(client.should_use_shm(200));
        assert_eq!(
            choose_request_transport(&client.inner.config, client.inner.has_request_pool(), 200),
            RequestTransportKind::Buddy
        );
    }

    #[test]
    fn sync_client_connect_without_external_pool_preserves_config() {
        let source = include_str!("sync_client.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("sync_client.rs must contain a production section");
        assert!(
            !production.contains("None => IpcClient::new(address)"),
            "SyncClient::connect must not discard ClientIpcConfig when no external pool is supplied"
        );
        assert!(
            production.contains("None => IpcClient::with_config(address, config)"),
            "SyncClient::connect must preserve ClientIpcConfig in the no-external-pool branch"
        );
    }

    #[test]
    fn call_prealloc_rejects_route_payload_limit_and_frees_alloc() {
        let config = ClientIpcConfig {
            shm_threshold: 1,
            ..ClientIpcConfig::default()
        };
        let pool = Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig::default())));
        let inner = IpcClient::with_pool("ipc://sync_payload_limit", pool.clone(), config);
        inner.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable::from_entries(
                &[c2_wire::handshake::MethodEntry {
                    name: "ping".to_string(),
                    index: 0,
                }],
                c2_wire::control::RouteCallIdentity {
                    route_name: "grid".to_string(),
                    route_uid: "grid-route-uid-0001".to_string(),
                    observed_route_revision: 1,
                    crm_ns: "cc.test".to_string(),
                    crm_name: "Grid".to_string(),
                    crm_ver: "0.1.0".to_string(),
                    abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_string(),
                    signature_hash:
                        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                            .to_string(),
                },
                4,
            ),
        );
        let client = SyncClient {
            inner,
            rt: get_or_create_runtime().handle().clone(),
        };

        let alloc = client.pool_alloc_and_write(&[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(pool.lock().stats().alloc_count, 1);
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "cc.test".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
        };
        let binding = client
            .inner
            .bind_cached_route(&expected)
            .expect("cached test route should bind");
        let err = client
            .call_bound_prealloc(&binding, "ping", &alloc, 5)
            .unwrap_err();

        assert!(err.to_string().contains("max_payload_size"));
        assert_eq!(pool.lock().stats().alloc_count, 0);
    }

    #[test]
    fn production_sync_client_api_is_route_acquire_and_bound_call_only() {
        let source = include_str!("sync_client.rs");
        let production = source
            .split("// ── Test-only helpers")
            .next()
            .expect("sync_client.rs must contain a production section");
        assert!(production.contains("pub fn acquire_route("));
        assert!(production.contains("pub fn acquire_route_token("));
        assert!(!production.contains(concat!("pub fn ", "call(\n")));
        assert!(!production.contains(concat!("pub fn ", "call_prealloc(")));
        assert!(production.contains("pub fn call_bound("));
        assert!(production.contains("pub fn call_bound_prealloc("));
    }

    // ── Owner affinity across close/reconnect ────────────────────────────

    use c2_config::BaseIpcConfig;
    use c2_server::{Server, ServerIpcConfig};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::Duration;

    static RACE_ADDRESS_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn race_client_config() -> ClientIpcConfig {
        ClientIpcConfig {
            shm_threshold: 1,
            base: BaseIpcConfig {
                pool_segment_size: 65_536,
                max_pool_segments: 1,
                max_pool_memory: 65_536,
                ..BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        }
    }

    /// Route-less IPC server plus the private runtime that keeps it running.
    fn start_race_server(label: &str) -> (tokio::runtime::Runtime, Arc<Server>, String) {
        let address = format!(
            "ipc://{label}_{}_{}",
            std::process::id(),
            RACE_ADDRESS_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("race test runtime");
        let server = runtime.block_on(async {
            let server = Arc::new(
                Server::new(&address, ServerIpcConfig::default()).expect("bind race test server"),
            );
            let running = Arc::clone(&server);
            tokio::spawn(async move {
                let _ = running.run().await;
            });
            server
                .wait_until_ready(Duration::from_secs(5))
                .await
                .expect("race test server ready");
            server
        });
        (runtime, server, address)
    }

    /// Deterministically race one preallocation against a confirmed close and
    /// a fresh pool incarnation in the client slot.
    ///
    /// The selection hook pauses the calling thread after it has cloned the
    /// old pool and before it allocates. The companion thread then confirms a
    /// close (which detaches the idle old pool), installs the returned fresh
    /// pool exactly as a reconnect would, and allocates a live canary in it.
    /// `MemPool` frees are explicit, so the canary stays allocated while the
    /// caller asserts on the block under test.
    fn install_replacement_race(
        client: &Arc<SyncClient>,
        old_pool: &Arc<Mutex<MemPool>>,
        canary_size: usize,
    ) -> (Arc<Mutex<MemPool>>, std::thread::JoinHandle<()>) {
        let fresh_config = old_pool.lock().config().clone();
        let fresh_budget = old_pool
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let fresh = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            fresh_config,
            format!(
                "/cc3crace{:08x}{:08x}",
                std::process::id(),
                RACE_ADDRESS_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            ),
            fresh_budget,
        )));
        let (selected_tx, selected_rx) = std::sync::mpsc::channel::<()>();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = std::sync::Mutex::new(resume_rx);
        client
            .inner
            .set_prealloc_selection_hook_for_test(Some(Box::new(move || {
                let _ = selected_tx.send(());
                // Bounded so a panicking companion fails the test instead of
                // hanging the suite in this hook.
                let _ = resume_rx
                    .lock()
                    .expect("resume lock")
                    .recv_timeout(Duration::from_secs(10));
            })));
        let companion_client = Arc::clone(client);
        let companion_fresh = Arc::clone(&fresh);
        let join = std::thread::spawn(move || {
            selected_rx.recv().expect("selection hook must fire");
            assert!(
                companion_client.close_shared(Duration::from_secs(5)),
                "the race requires a confirmed close"
            );
            companion_client
                .inner
                .replace_request_pool_for_test(Some(Arc::clone(&companion_fresh)));
            let _canary = companion_fresh
                .lock()
                .alloc(canary_size)
                .expect("fresh-pool canary");
            resume_tx.send(()).expect("main is paused in the hook");
        });
        (fresh, join)
    }

    #[test]
    fn prealloc_selection_race_frees_only_the_owning_pool() {
        let (_runtime, _server, address) = start_race_server("prealloc_race_pool_free");
        let client = Arc::new(
            SyncClient::connect(&address, None, race_client_config()).expect("connect race client"),
        );
        let old_pool = client
            .inner
            .request_pool()
            .expect("transport-owned request pool");
        let (fresh_pool, companion) = install_replacement_race(&client, &old_pool, 64);

        let data = vec![7u8; 64];
        let block = client
            .pool_alloc_and_write(&data)
            .expect("the originally selected pool serves the allocation");
        assert_eq!(
            old_pool.lock().stats().alloc_count,
            1,
            "the paused allocation must be charged to the originally selected pool"
        );
        assert_eq!(
            fresh_pool.lock().stats().alloc_count,
            1,
            "the replacement pool must hold only its own live canary"
        );

        client.pool_free(&block);

        assert_eq!(
            old_pool.lock().stats().alloc_count,
            0,
            "pool_free must release through the owning pool, not leak the old coordinates"
        );
        assert_eq!(
            fresh_pool.lock().stats().alloc_count,
            1,
            "the replacement pool's live canary must never be freed by stale coordinates"
        );
        companion.join().expect("companion thread");
    }

    #[test]
    fn prealloc_selection_race_fill_failure_frees_only_the_owning_pool() {
        let (_runtime, _server, address) = start_race_server("prealloc_race_fill");
        let client = Arc::new(
            SyncClient::connect(&address, None, race_client_config()).expect("connect race client"),
        );
        let old_pool = client
            .inner
            .request_pool()
            .expect("transport-owned request pool");
        let (fresh_pool, companion) = install_replacement_race(&client, &old_pool, 64);

        let error = client
            .pool_alloc_and_fill(64, |_buffer| Err("injected fill failure".to_string()))
            .expect_err("a failing fill must fail the allocation");
        assert!(error.to_string().contains("fill failed"), "{error}");

        assert_eq!(
            old_pool.lock().stats().alloc_count,
            0,
            "fill failure must release through the owning pool"
        );
        assert_eq!(
            fresh_pool.lock().stats().alloc_count,
            1,
            "the replacement pool's live canary must never be freed by stale coordinates"
        );
        companion.join().expect("companion thread");
    }

    /// A panicking fill callback must not leak the allocation.
    ///
    /// The callback runs between the allocation and any frame write, so the
    /// block is purely local when the panic unwinds through
    /// `pool_alloc_and_fill`; unwinding must release it through the owning
    /// pool instead of stranding the charge until the client is dropped.
    #[test]
    fn pool_alloc_and_fill_panic_releases_the_allocation() {
        let pool = Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig {
            segment_size: 65_536,
            max_segments: 1,
            ..c2_mem::PoolConfig::default()
        })));
        let inner = IpcClient::with_pool(
            "ipc://fill_panic_release",
            Arc::clone(&pool),
            ClientIpcConfig {
                shm_threshold: 1,
                ..ClientIpcConfig::default()
            },
        );
        let client = SyncClient {
            inner,
            rt: get_or_create_runtime().handle().clone(),
        };

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.pool_alloc_and_fill(64, |_| panic!("injected fill callback panic"))
        }));
        std::panic::set_hook(previous_hook);

        assert!(panic.is_err(), "the injected fill panic must unwind");
        assert_eq!(
            pool.lock().stats().alloc_count,
            0,
            "a panicking fill callback must release the allocation through the owning pool"
        );
    }
}
