use std::collections::HashMap;
use std::sync::Arc;

use c2_mem::{MemHandle, MemPool};

use crate::scheduler::{
    AccessLevel, ConcurrencyMode, RouteConcurrencyHandle, Scheduler, SchedulerLimits,
    SchedulerSnapshot,
};

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
/// or owned bytes to build the reply frame. For language bindings, large
/// buffer-like outputs should be prepared through Rust response helpers before
/// falling back to owned inline bytes.
#[derive(Debug)]
pub enum ResponseMeta {
    /// CRM wrote result into response SHM (via pool.alloc + pool.write).
    ShmAlloc {
        seg_idx: u16,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Owned response bytes awaiting native transport selection.
    ///
    /// `send_response_meta()` may still choose buddy SHM or chunked transport
    /// for this data based on the server IPC config.
    Inline(Vec<u8>),
    /// Method returned None / empty.
    Empty,
}

/// Input data for a CRM method call.
///
/// Pure Rust types with no language-binding dependency. Binding-specific
/// callback implementations convert these into SDK-visible objects.
pub enum RequestData {
    /// SHM coordinates from peer (buddy or dedicated).
    /// ShmBuffer.release() frees via pool.free_at().
    Shm {
        pool: Arc<parking_lot::RwLock<MemPool>>,
        seg_idx: u16,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Inline bytes from a local IPC frame.
    Inline(Vec<u8>),
    /// Reassembled MemHandle from chunked transfer.
    /// ShmBuffer.release() returns handle to pool.
    Handle {
        handle: MemHandle,
        pool: Arc<parking_lot::RwLock<MemPool>>,
    },
}

impl RequestData {
    fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::Inline(data) => Ok(data.clone()),
            Self::Shm {
                pool,
                seg_idx,
                generation,
                offset,
                data_size,
                is_dedicated,
            } => pool
                .read()
                .copy_data_at(
                    u32::from(*seg_idx),
                    *generation,
                    *offset,
                    *data_size,
                    *is_dedicated,
                )
                .map_err(|error| format!("request SHM copy failed: {error}")),
            Self::Handle { handle, pool } => pool
                .read()
                .copy_handle_data(handle)
                .map_err(|error| format!("request handle copy failed: {error}")),
        }
    }

    fn release(self) -> Result<(), String> {
        match self {
            Self::Inline(_) => Ok(()),
            Self::Shm {
                pool,
                seg_idx,
                generation,
                offset,
                data_size,
                is_dedicated,
            } => {
                let mut pool = pool.write();
                pool.validate_data_at(
                    u32::from(seg_idx),
                    generation,
                    offset,
                    data_size,
                    is_dedicated,
                )
                .map_err(|error| format!("request SHM release validation failed: {error}"))?;
                pool.free_at(
                    u32::from(seg_idx),
                    generation,
                    offset,
                    data_size,
                    is_dedicated,
                )
                .map_err(|error| format!("request SHM release failed: {error}"))?;
                Ok(())
            }
            Self::Handle { handle, pool } => {
                let mut pool = pool.write();
                pool.validate_handle(&handle).map_err(|error| {
                    format!("request handle release validation failed: {error}")
                })?;
                release_request_handle(&mut pool, handle)
            }
        }
    }
}

fn release_request_handle(pool: &mut MemPool, handle: MemHandle) -> Result<(), String> {
    match handle {
        MemHandle::Buddy {
            seg_idx,
            generation,
            offset,
            allocation_size,
            ..
        } => {
            pool.free_at(
                u32::from(seg_idx),
                generation,
                offset,
                allocation_size,
                false,
            )
            .map_err(|error| format!("request handle release failed: {error}"))?;
        }
        MemHandle::Dedicated { seg_idx, len } => {
            let data_size = u32::try_from(len)
                .map_err(|_| "request dedicated handle length exceeds the wire address space")?;
            pool.free_at(u32::from(seg_idx), 0, 0, data_size, true)
                .map_err(|error| format!("request handle release failed: {error}"))?;
        }
        MemHandle::FileSpill { .. } => {}
    }
    Ok(())
}

/// Owns one request transport allocation until it is explicitly released.
///
/// `copy_bytes` validates and copies without changing transport ownership.
/// `release` is idempotent and validates public coordinates before deriving a
/// buddy allocation level. Dropping an unreleased lease performs the same
/// best-effort cleanup.
pub struct RequestLease {
    request: Option<RequestData>,
}

impl RequestLease {
    pub fn new(request: RequestData) -> Self {
        Self {
            request: Some(request),
        }
    }

    pub fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        self.request
            .as_ref()
            .ok_or_else(|| "request lease is already released".to_string())?
            .copy_bytes()
    }

    pub fn release(&mut self) -> Result<(), String> {
        let Some(request) = self.request.take() else {
            return Ok(());
        };
        request.release()
    }

    pub fn into_owned_bytes(mut self) -> Result<Vec<u8>, String> {
        let Some(request) = self.request.take() else {
            return Err("request lease is already released".to_string());
        };
        self.request = match request {
            RequestData::Inline(data) => return Ok(data),
            transport_request => Some(transport_request),
        };
        let copy_result = self.copy_bytes();
        let release_result = self.release();
        combine_copy_and_release("request", copy_result, release_result)
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn combine_copy_and_release(
    kind: &str,
    copy_result: Result<Vec<u8>, String>,
    release_result: Result<(), String>,
) -> Result<Vec<u8>, String> {
    match (copy_result, release_result) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(copy_error), Ok(())) => Err(copy_error),
        (Ok(_), Err(release_error)) => Err(release_error),
        (Err(copy_error), Err(release_error)) => Err(format!(
            "{kind} copy failed: {copy_error}; {kind} release also failed: {release_error}"
        )),
    }
}

/// Explicitly release SHM resources held by a RequestData.
/// Must be called on error paths where the request won't be consumed by a CRM callback.
pub fn cleanup_request(request: RequestData) {
    let mut lease = RequestLease::new(request);
    let _ = lease.release();
}

impl std::fmt::Debug for RequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestData::Shm {
                seg_idx,
                generation,
                offset,
                data_size,
                is_dedicated,
                ..
            } => write!(
                f,
                "RequestData::Shm(seg={seg_idx}, gen={generation}, off={offset}, size={data_size}, ded={is_dedicated})"
            ),
            RequestData::Inline(v) => write!(f, "RequestData::Inline({} bytes)", v.len()),
            RequestData::Handle { .. } => write!(f, "RequestData::Handle"),
        }
    }
}

/// Trait for calling CRM methods from Rust.
///
/// Implementations may enter language runtimes internally, so callers should
/// not hold unrelated runtime locks while invoking this callback.
///
/// Uses pure Rust types (`RequestData`, `ResponseMeta`) in the trait
/// interface; language bindings handle conversion at the boundary.
pub trait CrmCallback: Send + Sync + 'static {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError>;
}

#[derive(Debug)]
pub struct RouteBuildSpec {
    pub name: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub method_names: Vec<String>,
    pub access_map: HashMap<u16, AccessLevel>,
    pub concurrency_mode: ConcurrencyMode,
    pub limits: SchedulerLimits,
}

pub struct BuiltRoute {
    route: CrmRoute,
    route_handle: RouteConcurrencyHandle,
}

impl BuiltRoute {
    pub(crate) fn new(route: CrmRoute, route_handle: RouteConcurrencyHandle) -> Self {
        Self {
            route,
            route_handle,
        }
    }

    pub(crate) fn into_route(self) -> CrmRoute {
        self.route
    }

    pub fn route_handle(&self) -> RouteConcurrencyHandle {
        self.route_handle.clone()
    }

    pub fn name(&self) -> &str {
        self.route.name()
    }

    pub fn route_uid(&self) -> &str {
        self.route.route_uid()
    }

    pub fn route_revision(&self) -> u64 {
        self.route.route_revision()
    }

    pub fn crm_ns(&self) -> &str {
        self.route.crm_ns()
    }

    pub fn crm_name(&self) -> &str {
        self.route.crm_name()
    }

    pub fn crm_ver(&self) -> &str {
        self.route.crm_ver()
    }

    pub fn abi_hash(&self) -> &str {
        self.route.abi_hash()
    }

    pub fn signature_hash(&self) -> &str {
        self.route.signature_hash()
    }

    pub fn method_names(&self) -> &[String] {
        self.route.method_names()
    }

    pub fn scheduler_snapshot(&self) -> SchedulerSnapshot {
        self.route.scheduler_snapshot()
    }

    pub fn access_map_snapshot(&self) -> HashMap<u16, AccessLevel> {
        self.route.scheduler.access_map_snapshot()
    }
}

/// Per-CRM route entry in the server.
pub(crate) struct CrmRoute {
    /// Route name (e.g., "grid", "solver")
    pub(crate) name: String,
    /// Unique identity for this committed route registration.
    pub(crate) route_uid: String,
    /// Monotonic revision for this route identity. Starts at 1 on creation.
    pub(crate) route_revision: u64,
    /// CRM namespace from the language-neutral contract descriptor.
    pub(crate) crm_ns: String,
    /// CRM contract class/model name from the language-neutral contract descriptor.
    pub(crate) crm_name: String,
    /// CRM semantic version from the language-neutral contract descriptor.
    pub(crate) crm_ver: String,
    /// Canonical ABI descriptor hash for the route contract.
    pub(crate) abi_hash: String,
    /// Canonical method signature descriptor hash for the route contract.
    pub(crate) signature_hash: String,
    /// Concurrency scheduler for this CRM
    pub(crate) scheduler: Arc<Scheduler>,
    /// Callback to invoke CRM methods
    pub(crate) callback: Arc<dyn CrmCallback>,
    /// Method names indexed by method_idx
    pub(crate) method_names: Vec<String>,
}

impl CrmRoute {
    pub(crate) fn new(
        spec: RouteBuildSpec,
        scheduler: Scheduler,
        callback: Arc<dyn CrmCallback>,
    ) -> Self {
        Self {
            name: spec.name,
            route_uid: uuid::Uuid::new_v4().simple().to_string(),
            route_revision: 1,
            crm_ns: spec.crm_ns,
            crm_name: spec.crm_name,
            crm_ver: spec.crm_ver,
            abi_hash: spec.abi_hash,
            signature_hash: spec.signature_hash,
            scheduler: Arc::new(scheduler),
            callback,
            method_names: spec.method_names,
        }
    }

    /// Return true when `method_idx` points at a registered CRM method.
    ///
    /// This is intentionally an O(1) bounds check for the remote dispatch hot
    /// path. Full method-name and contract validation happens at registration
    /// and client binding time.
    pub(crate) fn has_method_index(&self, method_idx: u16) -> bool {
        (method_idx as usize) < self.method_names.len()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn route_uid(&self) -> &str {
        &self.route_uid
    }

    pub(crate) fn route_revision(&self) -> u64 {
        self.route_revision
    }

    pub(crate) fn crm_ns(&self) -> &str {
        &self.crm_ns
    }

    pub(crate) fn crm_name(&self) -> &str {
        &self.crm_name
    }

    pub(crate) fn crm_ver(&self) -> &str {
        &self.crm_ver
    }

    pub(crate) fn abi_hash(&self) -> &str {
        &self.abi_hash
    }

    pub(crate) fn signature_hash(&self) -> &str {
        &self.signature_hash
    }

    pub(crate) fn method_names(&self) -> &[String] {
        &self.method_names
    }

    pub(crate) fn scheduler_snapshot(&self) -> SchedulerSnapshot {
        self.scheduler.snapshot()
    }
}

/// Route dispatcher — resolves (route_name, method_idx) to CrmRoute.
pub(crate) struct Dispatcher {
    routes: HashMap<String, Arc<CrmRoute>>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    /// Register a CRM route.
    pub fn register(&mut self, route: CrmRoute) {
        let name = route.name.clone();
        self.routes.insert(name, Arc::new(route));
    }

    /// Remove a CRM route. Returns the removed route if it existed.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<CrmRoute>> {
        self.routes.remove(name)
    }

    /// Resolve a route by explicit name.
    pub fn resolve(&self, route_name: &str) -> Option<Arc<CrmRoute>> {
        self.routes.get(route_name).cloned()
    }

    /// Get a snapshot of all routes (for handshake response).
    pub fn routes_snapshot(&self) -> Vec<Arc<CrmRoute>> {
        self.routes.values().cloned().collect()
    }

    /// Remove and return all registered routes.
    pub fn take_all(&mut self) -> Vec<Arc<CrmRoute>> {
        self.routes.drain().map(|(_, route)| route).collect()
    }

    /// Number of registered routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    #[cfg(test)]
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
    use crate::scheduler::ConcurrencyMode;
    use c2_mem::PoolConfig;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

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
            route_uid: format!("{name}-uid-0001"),
            route_revision: 1,
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            scheduler: Arc::new(Scheduler::new(
                ConcurrencyMode::ReadParallel,
                HashMap::new(),
            )),
            callback: Arc::new(MockCallback),
            method_names: vec!["method_a".into(), "method_b".into()],
        }
    }

    fn request_test_pool() -> MemPool {
        static NEXT_POOL: AtomicU64 = AtomicU64::new(0);
        MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 2,
                dedicated_crash_timeout_secs: 5.0,
                buddy_idle_decay_secs: 1.0,
                spill_threshold: 0.8,
                spill_dir: std::env::temp_dir().join("c_two_request_data_test_spill"),
            },
            format!(
                "/c2rq{:08x}{:04x}",
                std::process::id(),
                NEXT_POOL.fetch_add(1, Ordering::Relaxed),
            ),
        )
    }

    #[test]
    fn request_data_inline_materializes_owned_bytes() {
        assert_eq!(
            RequestLease::new(RequestData::Inline(b"inline".to_vec()))
                .into_owned_bytes()
                .unwrap(),
            b"inline",
        );
    }

    #[test]
    fn request_lease_copy_retains_backing_until_idempotent_release() {
        let payload = b"borrowed request".repeat(128);
        let mut pool = request_test_pool();
        let allocation = pool.alloc(payload.len()).unwrap();
        let pointer = pool.data_ptr(&allocation).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, payload.len());
        }
        let pool = Arc::new(parking_lot::RwLock::new(pool));
        let mut lease = RequestLease::new(RequestData::Shm {
            pool: Arc::clone(&pool),
            seg_idx: u16::try_from(allocation.seg_idx).unwrap(),
            generation: allocation.generation,
            offset: allocation.offset,
            data_size: u32::try_from(payload.len()).unwrap(),
            is_dedicated: allocation.is_dedicated,
        });

        assert_eq!(lease.copy_bytes().unwrap(), payload);
        assert_eq!(pool.read().stats().alloc_count, 1);
        lease.release().unwrap();
        lease.release().unwrap();
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn request_data_shm_materializes_and_releases_buddy_and_dedicated_allocations() {
        for payload in [b"buddy".repeat(32), b"dedicated".repeat(5000)] {
            let mut pool = request_test_pool();
            let allocation = pool.alloc(payload.len()).unwrap();
            let ptr = pool.data_ptr(&allocation).unwrap();
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
            }
            let pool = Arc::new(parking_lot::RwLock::new(pool));
            let request = RequestData::Shm {
                pool: Arc::clone(&pool),
                seg_idx: u16::try_from(allocation.seg_idx).unwrap(),
                generation: allocation.generation,
                offset: allocation.offset,
                data_size: u32::try_from(payload.len()).unwrap(),
                is_dedicated: allocation.is_dedicated,
            };

            assert_eq!(
                RequestLease::new(request).into_owned_bytes().unwrap(),
                payload
            );
            assert_eq!(pool.read().stats().alloc_count, 0);
        }
    }

    #[test]
    fn request_data_shm_rejects_invalid_span_without_freeing_unproven_coordinates() {
        let mut pool = request_test_pool();
        let allocation = pool.alloc(4096).unwrap();
        let capacity = pool
            .segment(allocation.seg_idx as usize)
            .unwrap()
            .allocator()
            .data_size();
        let pool = Arc::new(parking_lot::RwLock::new(pool));
        let request = RequestData::Shm {
            pool: Arc::clone(&pool),
            seg_idx: u16::try_from(allocation.seg_idx).unwrap(),
            generation: allocation.generation,
            offset: u32::try_from(capacity - 1).unwrap(),
            data_size: 2,
            is_dedicated: false,
        };

        assert!(
            RequestLease::new(request)
                .into_owned_bytes()
                .unwrap_err()
                .contains("outside buddy segment")
        );
        assert_eq!(pool.read().stats().alloc_count, 1);
        pool.write().free(&allocation).unwrap();
    }

    #[test]
    fn request_data_handle_materializes_and_releases_reassembly_allocation() {
        let payload = b"reassembled".repeat(512);
        for logical_len in [payload.len(), 4096, 1, 0] {
            let mut pool = request_test_pool();
            let mut handle = pool.alloc_handle(payload.len()).unwrap();
            pool.handle_slice_mut(&mut handle).copy_from_slice(&payload);
            handle.set_len(logical_len);
            let pool = Arc::new(parking_lot::RwLock::new(pool));
            let request = RequestData::Handle {
                handle,
                pool: Arc::clone(&pool),
            };

            assert_eq!(
                RequestLease::new(request).into_owned_bytes().unwrap(),
                payload[..logical_len]
            );
            assert_eq!(pool.read().stats().alloc_count, 0);
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
            MemPool::new(PoolConfig::default()),
        ));
        let result = route
            .callback
            .invoke(
                "grid",
                0,
                RequestData::Inline(b"test".to_vec()),
                pool.clone(),
            )
            .unwrap();
        assert!(matches!(result, ResponseMeta::Inline(ref v) if v == b"echo"));
    }

    #[test]
    fn route_contract_metadata_and_method_index_bounds_are_available() {
        let route = make_route("grid");

        assert_eq!(route.crm_ns, "test.grid");
        assert_eq!(route.crm_name, "Grid");
        assert_eq!(route.crm_ver, "0.1.0");
        assert!(route.has_method_index(0));
        assert!(route.has_method_index(1));
        assert!(!route.has_method_index(2));
        assert!(!route.has_method_index(u16::MAX));
    }

    #[test]
    fn empty_name_does_not_resolve_to_default() {
        let mut d = Dispatcher::new();
        d.register(make_route("first"));
        d.register(make_route("second"));

        assert!(d.resolve("").is_none());
    }

    #[test]
    fn unregister_and_re_resolve_returns_none() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));

        assert!(d.unregister("grid").is_some());
        assert!(d.resolve("grid").is_none());
        assert!(d.is_empty());
    }

    #[test]
    fn unregister_does_not_create_default_route() {
        let mut d = Dispatcher::new();
        d.register(make_route("alpha"));
        d.register(make_route("beta"));

        let removed = d.unregister("alpha");
        assert!(removed.is_some());

        assert!(d.resolve("").is_none());
        assert_eq!(d.resolve("beta").unwrap().name, "beta");
    }

    #[test]
    fn unregister_nonexistent_returns_false() {
        let mut d = Dispatcher::new();
        assert!(d.unregister("nope").is_none());
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
            MemPool::new(PoolConfig::default()),
        ));
        let cb: Arc<dyn CrmCallback> = Arc::new(FailCallback);
        let err = cb
            .invoke("grid", 99, RequestData::Inline(b"test".to_vec()), pool)
            .unwrap_err();
        assert!(matches!(err, CrmError::InternalError(_)));
    }
}
