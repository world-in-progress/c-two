//! Synchronous IPC client — embeds a tokio runtime handle.
//!
//! Wraps [`IpcClient`] for blocking calls from Python (via PyO3).
//! Multiple `SyncClient` instances share a single tokio runtime.

use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use c2_mem::MemPool;

use crate::client::{IpcClient, IpcConfig, IpcError, MethodTable};

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
/// Wraps `IpcClient` for blocking calls from Python (via PyO3).
/// Multiple SyncClients share a single tokio runtime.
pub struct SyncClient {
    inner: IpcClient,
    rt: tokio::runtime::Handle,
}

// Compile-time assertion: SyncClient must be Send+Sync for PyO3
// `#[pyclass(frozen)]`.
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
        pool: Option<Arc<StdMutex<MemPool>>>,
        config: IpcConfig,
    ) -> Result<Self, IpcError> {
        let rt = get_or_create_runtime();
        let mut client = match pool {
            Some(p) => IpcClient::with_pool(address, p, config),
            None => IpcClient::new(address),
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
    ) -> Result<Vec<u8>, IpcError> {
        self.rt
            .block_on(self.inner.call_full(route_name, method_name, data))
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
}
