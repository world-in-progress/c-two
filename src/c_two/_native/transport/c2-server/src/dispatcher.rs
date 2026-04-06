use std::collections::HashMap;
use std::sync::Arc;

use c2_mem::{MemHandle, MemPool};

use crate::scheduler::Scheduler;

/// Error from CRM method invocation
#[derive(Debug)]
pub enum CrmError {
    /// CRM method raised a user-visible error (serialized error bytes)
    UserError(Vec<u8>),
    /// Internal error (method not found, type mismatch, etc.)
    InternalError(String),
}

impl std::fmt::Display for CrmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrmError::UserError(bytes) => write!(f, "CrmUserError({} bytes)", bytes.len()),
            CrmError::InternalError(msg) => write!(f, "CrmInternalError: {}", msg),
        }
    }
}

impl std::error::Error for CrmError {}

/// Metadata describing how the CRM callback produced its response.
///
/// Returned by `CrmCallback::invoke()`. The server reads these coordinates
/// to build the reply frame — no payload data crosses the FFI boundary.
#[derive(Debug)]
pub enum ResponseMeta {
    /// CRM wrote result into response SHM (via pool.alloc + pool.write).
    ShmAlloc {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Small result returned as inline bytes (< shm_threshold).
    Inline(Vec<u8>),
    /// Method returned None / empty.
    Empty,
}

/// Input data for a CRM method call.
///
/// Pure Rust types — no PyO3 dependency. The `PyCrmCallback` impl in c2-ffi
/// converts these into Python objects (ShmBuffer/bytes) under GIL.
pub enum RequestData {
    /// SHM coordinates from peer (buddy or dedicated).
    /// ShmBuffer.release() frees via pool.free_at().
    Shm {
        pool: Arc<parking_lot::RwLock<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Inline bytes from UDS frame.
    Inline(Vec<u8>),
    /// Reassembled MemHandle from chunked transfer.
    /// ShmBuffer.release() returns handle to pool.
    Handle {
        handle: MemHandle,
        pool: Arc<parking_lot::RwLock<MemPool>>,
    },
}

/// Explicitly release SHM resources held by a RequestData.
/// Must be called on error paths where the request won't be consumed by a CRM callback.
pub fn cleanup_request(request: RequestData) {
    match request {
        RequestData::Shm { pool, seg_idx, offset, data_size, is_dedicated } => {
            let mut p = pool.write();
            let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
        }
        RequestData::Handle { handle, pool } => {
            let mut p = pool.write();
            p.release_handle(handle);
        }
        RequestData::Inline(_) => {}
    }
}

impl std::fmt::Debug for RequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestData::Shm { seg_idx, offset, data_size, is_dedicated, .. } =>
                write!(f, "RequestData::Shm(seg={seg_idx}, off={offset}, size={data_size}, ded={is_dedicated})"),
            RequestData::Inline(v) =>
                write!(f, "RequestData::Inline({} bytes)", v.len()),
            RequestData::Handle { .. } =>
                write!(f, "RequestData::Handle"),
        }
    }
}

/// Trait for calling CRM methods from Rust.
///
/// `invoke()` acquires the GIL internally (in PyCrmCallback impl),
/// so callers must NOT hold the GIL when calling this.
///
/// Uses pure Rust types (RequestData, ResponseMeta) — no PyO3 types
/// in the trait interface. PyCrmCallback converts to/from Python objects.
pub trait CrmCallback: Send + Sync + 'static {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError>;
}

/// Per-CRM route entry in the server.
pub struct CrmRoute {
    /// Route name (e.g., "grid", "solver")
    pub name: String,
    /// Concurrency scheduler for this CRM
    pub scheduler: Arc<Scheduler>,
    /// Callback to invoke CRM methods
    pub callback: Arc<dyn CrmCallback>,
    /// Method names indexed by method_idx
    pub method_names: Vec<String>,
}

/// Route dispatcher — resolves (route_name, method_idx) to CrmRoute.
pub struct Dispatcher {
    routes: HashMap<String, Arc<CrmRoute>>,
    /// Default route name (first registered, used when client sends empty route key)
    default_route: Option<String>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            default_route: None,
        }
    }

    /// Register a CRM route.
    pub fn register(&mut self, route: CrmRoute) {
        let name = route.name.clone();
        if self.default_route.is_none() {
            self.default_route = Some(name.clone());
        }
        self.routes.insert(name, Arc::new(route));
    }

    /// Remove a CRM route. Returns true if it existed.
    pub fn unregister(&mut self, name: &str) -> bool {
        let removed = self.routes.remove(name).is_some();
        if self.default_route.as_deref() == Some(name) {
            self.default_route = self.routes.keys().next().cloned();
        }
        removed
    }

    /// Resolve a route by name. Empty name uses default route.
    pub fn resolve(&self, route_name: &str) -> Option<Arc<CrmRoute>> {
        if route_name.is_empty() {
            self.default_route
                .as_ref()
                .and_then(|name| self.routes.get(name))
                .cloned()
        } else {
            self.routes.get(route_name).cloned()
        }
    }

    /// Get a snapshot of all routes (for handshake response).
    pub fn routes_snapshot(&self) -> Vec<Arc<CrmRoute>> {
        self.routes.values().cloned().collect()
    }

    /// Number of registered routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::PoolConfig;
    use crate::scheduler::ConcurrencyMode;
    use std::collections::HashMap;

    struct MockCallback;

    impl CrmCallback for MockCallback {
        fn invoke(
            &self,
            _route: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"echo".to_vec()))
        }
    }

    fn make_route(name: &str) -> CrmRoute {
        CrmRoute {
            name: name.to_string(),
            scheduler: Arc::new(Scheduler::new(ConcurrencyMode::ReadParallel, HashMap::new())),
            callback: Arc::new(MockCallback),
            method_names: vec!["method_a".into(), "method_b".into()],
        }
    }

    #[test]
    fn register_and_resolve() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));

        let route = d.resolve("grid").expect("route should exist");
        assert_eq!(route.name, "grid");
        assert_eq!(route.method_names.len(), 2);

        // invoke the callback through the route
        let pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new(PoolConfig::default())
        ));
        let result = route.callback.invoke(
            "grid", 0,
            RequestData::Inline(b"test".to_vec()),
            pool.clone(),
        ).unwrap();
        assert!(matches!(result, ResponseMeta::Inline(ref v) if v == b"echo"));
    }

    #[test]
    fn empty_name_resolves_to_default() {
        let mut d = Dispatcher::new();
        d.register(make_route("first"));
        d.register(make_route("second"));

        let route = d.resolve("").expect("empty name should resolve to default");
        assert_eq!(route.name, "first");
    }

    #[test]
    fn unregister_and_re_resolve_returns_none() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));

        assert!(d.unregister("grid"));
        assert!(d.resolve("grid").is_none());
        assert!(d.is_empty());
    }

    #[test]
    fn unregister_default_promotes_next() {
        let mut d = Dispatcher::new();
        d.register(make_route("alpha"));
        d.register(make_route("beta"));

        // default is "alpha" (first registered)
        assert_eq!(d.resolve("").unwrap().name, "alpha");

        d.unregister("alpha");

        // default should now be "beta"
        let route = d.resolve("").expect("should have a new default");
        assert_eq!(route.name, "beta");
    }

    #[test]
    fn unregister_nonexistent_returns_false() {
        let mut d = Dispatcher::new();
        assert!(!d.unregister("nope"));
    }

    #[test]
    fn routes_snapshot_returns_all() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));
        d.register(make_route("solver"));
        d.register(make_route("mesh"));

        let snap = d.routes_snapshot();
        assert_eq!(snap.len(), 3);

        let mut names: Vec<&str> = snap.iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["grid", "mesh", "solver"]);
    }

    #[test]
    fn empty_dispatcher() {
        let d = Dispatcher::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert!(d.resolve("").is_none());
        assert!(d.resolve("anything").is_none());
        assert!(d.routes_snapshot().is_empty());
    }

    #[test]
    fn crm_error_display() {
        let user_err = CrmError::UserError(vec![1, 2, 3]);
        assert_eq!(format!("{}", user_err), "CrmUserError(3 bytes)");

        let internal_err = CrmError::InternalError("not found".into());
        assert_eq!(format!("{}", internal_err), "CrmInternalError: not found");
    }

    #[test]
    fn mock_callback_error() {
        struct FailCallback;
        impl CrmCallback for FailCallback {
            fn invoke(
                &self,
                _route: &str,
                _method_idx: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                Err(CrmError::InternalError("method not found".into()))
            }
        }

        let pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new(PoolConfig::default())
        ));
        let cb: Arc<dyn CrmCallback> = Arc::new(FailCallback);
        let err = cb.invoke(
            "grid", 99,
            RequestData::Inline(b"test".to_vec()),
            pool,
        ).unwrap_err();
        assert!(matches!(err, CrmError::InternalError(_)));
    }
}
