//! Synchronous IPC client — embeds a tokio runtime handle.
//!
//! Wraps [`IpcClient`] for blocking calls from SDK bindings.
//! Multiple `SyncClient` instances share a single tokio runtime.

use parking_lot::{Mutex, RwLock};
use std::sync::{Arc, OnceLock};

use c2_mem::{MemPool, PoolAllocation};

use crate::client::{
    ClientIpcConfig, IpcClient, IpcError, MethodTable, RequestTransportKind, ServerPoolState,
    choose_request_transport,
};
use crate::response::ResponseData;

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

    /// Synchronous CRM call — blocks until reply.
    pub fn call(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        self.rt
            .block_on(self.inner.call(route_name, method_name, data))
    }

    /// Whether the client has a SHM pool and data exceeds the threshold.
    pub fn should_use_shm(&self, data_len: usize) -> bool {
        choose_request_transport(&self.inner.config, self.inner.pool.is_some(), data_len)
            == RequestTransportKind::Buddy
    }

    /// Allocate from the client SHM pool and write data in a single lock scope.
    ///
    /// Returns the allocation coordinates. On error, the caller should fall
    /// back to the canonical `call()` path.
    pub fn pool_alloc_and_write(&self, data: &[u8]) -> Result<PoolAllocation, IpcError> {
        let pool_arc = self
            .inner
            .pool
            .as_ref()
            .ok_or_else(|| IpcError::Pool("no client pool".into()))?;
        let mut pool = pool_arc.lock();
        let alloc = pool
            .alloc(data.len())
            .map_err(|e| IpcError::Pool(format!("alloc failed: {e}")))?;
        let ptr = pool.data_ptr(&alloc).map_err(|e| {
            let _ = pool.free(&alloc);
            IpcError::Pool(format!("data_ptr failed: {e}"))
        })?;
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }
        Ok(alloc)
    }

    /// Allocate from the client SHM pool and let the caller fill the block.
    ///
    /// The fill callback receives exactly `data_size` bytes. On callback
    /// failure the allocation is freed before the error is returned.
    pub fn pool_alloc_and_fill<F>(
        &self,
        data_size: usize,
        fill: F,
    ) -> Result<PoolAllocation, IpcError>
    where
        F: FnOnce(&mut [u8]) -> Result<(), String>,
    {
        let pool_arc = self
            .inner
            .pool
            .as_ref()
            .ok_or_else(|| IpcError::Pool("no client pool".into()))?;
        let alloc = {
            let mut pool = pool_arc.lock();
            pool.alloc(data_size)
                .map_err(|e| IpcError::Pool(format!("alloc failed: {e}")))?
        };
        let ptr = {
            let pool = pool_arc.lock();
            match pool.data_ptr(&alloc) {
                Ok(ptr) => ptr,
                Err(e) => {
                    drop(pool);
                    self.pool_free(&alloc);
                    return Err(IpcError::Pool(format!("data_ptr failed: {e}")));
                }
            }
        };
        let destination = unsafe { std::slice::from_raw_parts_mut(ptr, data_size) };
        if let Err(err) = fill(destination) {
            self.pool_free(&alloc);
            return Err(IpcError::Pool(format!("fill failed: {err}")));
        }
        Ok(alloc)
    }

    /// Free a pool allocation (used on send failure for cleanup).
    pub fn pool_free(&self, alloc: &PoolAllocation) {
        if let Some(ref pool_arc) = self.inner.pool {
            let mut pool = pool_arc.lock();
            let _ = pool.free(alloc);
        }
    }

    /// Synchronous CRM call with pre-allocated SHM data — blocks until reply.
    pub fn call_prealloc(
        &self,
        route_name: &str,
        method_name: &str,
        alloc: &PoolAllocation,
        data_size: usize,
    ) -> Result<ResponseData, IpcError> {
        let method_idx = match (|| {
            let table = self
                .inner
                .route_tables
                .get(route_name)
                .ok_or_else(|| IpcError::Handshake(format!("unknown route: {route_name}")))?;
            let method_idx = table
                .index_of(method_name)
                .ok_or_else(|| IpcError::Handshake(format!("unknown method: {method_name}")))?;
            let max_payload_size = table.max_payload_size();
            let data_size_u64 = u64::try_from(data_size).unwrap_or(u64::MAX);
            if data_size_u64 > max_payload_size {
                return Err(IpcError::Config(format!(
                    "request payload size {data_size_u64} exceeds route '{route_name}' max_payload_size {max_payload_size}"
                )));
            }
            Ok(method_idx)
        })() {
            Ok(method_idx) => method_idx,
            Err(err) => {
                self.inner.free_prealloc(alloc);
                return Err(err);
            }
        };
        self.rt.block_on(
            self.inner
                .call_with_prealloc(route_name, method_idx, alloc, data_size),
        )
    }

    /// Get a reference to the server SHM pool (for FFI layer).
    pub fn server_pool_arc(&self) -> Arc<Mutex<Option<ServerPoolState>>> {
        self.inner.server_pool.clone()
    }

    /// Get a reference to the client reassembly pool (for FFI layer).
    pub fn reassembly_pool_arc(&self) -> Arc<RwLock<MemPool>> {
        self.inner.reassembly_pool_arc()
    }

    /// Synchronous close.
    pub fn close(&mut self) {
        self.rt.block_on(self.inner.close());
    }

    /// Whether the client is connected.
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Get the route table for a named route.
    pub fn route_table(&self, name: &str) -> Option<&MethodTable> {
        self.inner.route_table(name)
    }

    /// Get all route names.
    pub fn route_names(&self) -> Vec<&str> {
        self.inner.route_names()
    }

    /// Validate the connected route against an expected CRM contract.
    pub fn validate_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        self.inner.validate_route_contract(expected)
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
    /// `is_connected()` returns `false` and `call()` will fail.
    pub(crate) fn new_unconnected(address: &str) -> Self {
        let rt = get_or_create_runtime();
        let inner = IpcClient::new(address);
        Self {
            inner,
            rt: rt.handle().clone(),
        }
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
        };
        let pool = Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig::default())));
        let client = SyncClient {
            inner: IpcClient::with_pool("ipc://sync_selector", pool, config),
            rt: get_or_create_runtime().handle().clone(),
        };

        assert!(!client.should_use_shm(50));
        assert!(client.should_use_shm(200));
        assert_eq!(
            choose_request_transport(&client.inner.config, client.inner.pool.is_some(), 200),
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
        let mut inner = IpcClient::with_pool("ipc://sync_payload_limit", pool.clone(), config);
        inner.route_tables.insert(
            "grid".to_string(),
            MethodTable::from_entries(
                &[c2_wire::handshake::MethodEntry {
                    name: "ping".to_string(),
                    index: 0,
                }],
                "cc.test".to_string(),
                "Grid".to_string(),
                "0.1.0".to_string(),
                "abi".to_string(),
                "sig".to_string(),
                4,
            ),
        );
        let client = SyncClient {
            inner,
            rt: get_or_create_runtime().handle().clone(),
        };

        let alloc = client.pool_alloc_and_write(&[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(pool.lock().stats().alloc_count, 1);
        let err = client.call_prealloc("grid", "ping", &alloc, 5).unwrap_err();

        assert!(err.to_string().contains("max_payload_size"));
        assert_eq!(pool.lock().stats().alloc_count, 0);
    }
}
