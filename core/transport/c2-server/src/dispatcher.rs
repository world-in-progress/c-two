use std::collections::HashMap;
use std::sync::Arc;

use c2_mem::{MemHandle, MemPool};
use c2_wire::buddy::BuddyPayload;
use c2_wire::chunk::ReassemblyBacking;
use c2_wire::connection::{ConnectionCore, PreparedLoan, RemoteLoan};
use c2_wire::handshake::MAX_METHODS;

use crate::server::ServerError;

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
    /// A backing prepared in the server response pool by the allocation and
    /// fill helper ([`crate::response::try_prepare_shm_response`]). Carries
    /// the unique write owner; the send path consumes it exactly once.
    ShmAlloc(PreparedShmResponse),
    /// Owned response bytes awaiting native transport selection.
    ///
    /// `send_response_meta()` may still choose buddy SHM or chunked transport
    /// for this data based on the server IPC config.
    Inline(Vec<u8>),
    /// Method returned None / empty.
    Empty,
}

/// A response SHM backing that exists, has been filled, and is owned by the
/// server's response pool.
///
/// The only producer is the real allocation/fill helper; a `ShmAlloc`
/// response cannot be hand-assembled from wire coordinates. The send path
/// consumes the owner exactly once: it either releases it locally or moves
/// the original handle into the physical connection's loan book. Dropping an
/// unpublished carrier cleans up through its original pool.
pub struct PreparedShmResponse {
    pool: Arc<parking_lot::RwLock<MemPool>>,
    handle: Option<MemHandle>,
    data_size: u32,
}

impl std::fmt::Debug for PreparedShmResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedShmResponse")
            .field("handle", &self.handle)
            .field("data_size", &self.data_size)
            .finish()
    }
}

impl PreparedShmResponse {
    pub(crate) fn new(
        pool: Arc<parking_lot::RwLock<MemPool>>,
        handle: MemHandle,
        data_size: u32,
    ) -> Self {
        Self {
            pool,
            handle: Some(handle),
            data_size,
        }
    }

    pub fn data_size(&self) -> u32 {
        self.data_size
    }

    pub fn shm_allocation(&self) -> Option<c2_mem::PoolAllocation> {
        self.handle.as_ref().and_then(MemHandle::shm_allocation)
    }

    pub fn is_dedicated(&self) -> bool {
        self.handle.as_ref().is_some_and(MemHandle::is_dedicated)
    }

    pub(crate) fn fill(
        &mut self,
        write_into: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let pool = self.pool.read();
        let handle = self.handle.as_mut().expect("live prepared owner");
        write_into(pool.handle_slice_mut(handle))
    }

    pub(crate) fn copy_logical_bytes(&self) -> Result<Vec<u8>, String> {
        let pool = self.pool.read();
        let handle = self.handle.as_ref().expect("live prepared owner");
        Ok(pool.handle_slice(handle)[..self.data_size as usize].to_vec())
    }

    pub(crate) fn release(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.pool.write().release_handle(handle);
        }
    }

    pub(crate) fn into_loan(mut self, core: &ConnectionCore) -> Result<PreparedLoan, String> {
        let handle = self.handle.take().expect("live prepared owner");
        core.prepare_loan(Arc::clone(&self.pool), handle)
            .map_err(|error| error.to_string())
    }
}

impl Drop for PreparedShmResponse {
    fn drop(&mut self) {
        self.release();
    }
}

/// Move-only receive carrier for one admitted peer SHM request block.
///
/// Minted only at this crate's frame-admission path from a decoded descriptor,
/// its original peer mapping, and a read interval on that physical connection.
pub(crate) struct PeerShmRequest {
    pool: Arc<parking_lot::RwLock<MemPool>>,
    payload: BuddyPayload,
    loan: RemoteLoan,
}

impl std::fmt::Debug for PeerShmRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerShmRequest")
            .field("pool", &Arc::as_ptr(&self.pool))
            .field("payload", &self.payload)
            .finish()
    }
}

impl PeerShmRequest {
    /// The loan is admitted before mapping, so even a mapping failure ends
    /// the remote read interval and queues its release.
    pub(crate) fn admit(
        pool: Arc<parking_lot::RwLock<MemPool>>,
        payload: BuddyPayload,
        loan: RemoteLoan,
    ) -> Result<Self, String> {
        let end = (payload.offset as usize)
            .checked_add(payload.data_size as usize)
            .ok_or_else(|| "request SHM span overflow".to_string())?;
        {
            let mut mapped = pool.write();
            if payload.is_dedicated {
                mapped.ensure_peer_dedicated(
                    u32::from(payload.seg_idx),
                    payload.generation,
                    end,
                )?;
            } else {
                mapped.ensure_peer_segment(u32::from(payload.seg_idx), payload.generation, end)?;
            }
            mapped.validate_data_at(
                u32::from(payload.seg_idx),
                payload.generation,
                payload.offset,
                payload.data_size,
                payload.is_dedicated,
            )?;
        }
        Ok(Self {
            pool,
            payload,
            loan,
        })
    }

    /// Copy the request bytes out of the peer's published block.
    fn into_owned_bytes(self) -> Result<Vec<u8>, String> {
        // Frame admission established the mapping before dispatch. The live
        // carrier keeps it in the original peer cache until release.
        let pool = self.pool.read();
        // SAFETY: these coordinates were admitted from a frame received on
        // this exact connection pool; the writing peer completed and
        // published the block (write-then-publish protocol) and the carrier
        // is the only local authority for it, so no conflicting access or
        // release can race this copy.
        let copied = unsafe {
            pool.copy_data_at(
                u32::from(self.payload.seg_idx),
                self.payload.generation,
                self.payload.offset,
                self.payload.data_size,
                self.payload.is_dedicated,
            )
        }
        .map_err(|error| format!("request SHM copy failed: {error}"));
        drop(pool);
        self.loan.finish();
        copied
    }
}

/// Input data for a CRM method call.
///
/// Pure Rust types with no language-binding dependency. Binding-specific
/// callback implementations convert these into SDK-visible objects.
pub(crate) enum RequestData {
    /// SHM block received from the peer. The carrier binds the descriptor
    /// to the original connection and ends its read interval after copying.
    Shm(PeerShmRequest),
    /// Inline bytes from a local IPC frame.
    Inline(Vec<u8>),
    /// Reassembled chunked transfer. Consuming it copies logical bytes and
    /// releases the local backing and its budget charge before callback entry.
    Handle(ReassemblyBacking),
}

impl RequestData {
    pub(crate) fn into_owned_bytes(self) -> Result<Vec<u8>, String> {
        match self {
            Self::Inline(data) => Ok(data),
            Self::Shm(carrier) => carrier.into_owned_bytes(),
            Self::Handle(mut backing) => {
                let copied = backing
                    .copy_bytes()
                    .map_err(|error| format!("request handle copy failed: {error}"));
                backing.release();
                copied
            }
        }
    }
}

impl std::fmt::Debug for RequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestData::Shm(carrier) => write!(f, "RequestData::Shm({carrier:?})"),
            RequestData::Inline(v) => write!(f, "RequestData::Inline({} bytes)", v.len()),
            RequestData::Handle(_) => write!(f, "RequestData::Handle"),
        }
    }
}

/// Trait for calling CRM methods from Rust.
///
/// Implementations may enter language runtimes internally, so callers should
/// not hold unrelated runtime locks while invoking this callback.
///
/// Uses owned bytes and pure Rust response metadata in the trait
/// interface; language bindings handle conversion at the boundary.
pub trait CrmCallback: Send + Sync + 'static {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: Vec<u8>,
        response_pool: Arc<crate::ResponsePool>,
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
    pub(crate) fn new(route: CrmRoute) -> Result<Self, ServerError> {
        c2_contract::validate_call_route_key("route name", &route.name)
            .map_err(|e| ServerError::Protocol(e.to_string()))?;
        if route.method_names.len() > MAX_METHODS {
            return Err(ServerError::Protocol(format!(
                "method count exceeds wire limit: {} > {}",
                route.method_names.len(),
                MAX_METHODS
            )));
        }
        for method_name in &route.method_names {
            c2_contract::validate_contract_text_field("method name", method_name)
                .map_err(|e| ServerError::Protocol(e.to_string()))?;
        }
        c2_contract::validate_crm_tag(&route.crm_ns, &route.crm_name, &route.crm_ver)
            .map_err(|e| ServerError::Protocol(e.to_string()))?;
        c2_contract::validate_contract_hash("abi_hash", &route.abi_hash)
            .map_err(|e| ServerError::Protocol(e.to_string()))?;
        c2_contract::validate_contract_hash("signature_hash", &route.signature_hash)
            .map_err(|e| ServerError::Protocol(e.to_string()))?;
        let route_handle = RouteConcurrencyHandle::new(route.scheduler.as_ref().clone());
        Ok(Self {
            route,
            route_handle,
        })
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
    removed: HashMap<String, String>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            removed: HashMap::new(),
        }
    }

    /// Register a CRM route.
    pub fn register(&mut self, route: CrmRoute) {
        let name = route.name.clone();
        self.removed.remove(&name);
        self.routes.insert(name, Arc::new(route));
    }

    /// Remove a CRM route. Returns the removed route if it existed.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<CrmRoute>> {
        let route = self.routes.remove(name)?;
        self.removed
            .insert(name.to_owned(), route.route_uid.clone());
        Some(route)
    }

    /// Identity of the latest removed registration for a route name.
    pub(crate) fn removed_uid(&self, name: &str) -> Option<String> {
        self.removed.get(name).cloned()
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
        let routes: Vec<_> = self.routes.drain().map(|(_, route)| route).collect();
        for route in &routes {
            self.removed
                .insert(route.name.clone(), route.route_uid.clone());
        }
        routes
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
    use c2_wire::connection::WriteRequest;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct MockCallback;

    impl CrmCallback for MockCallback {
        fn invoke(
            &self,
            _route: &str,
            _method_idx: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"echo".to_vec()))
        }
    }

    fn make_route(name: &str) -> CrmRoute {
        CrmRoute {
            name: name.to_string(),
            route_uid: format!("{name}-uid-0001"),
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

    static REQUEST_POOL_SEQ: AtomicU64 = AtomicU64::new(0);

    fn request_pool_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            spill_threshold: 0.8,
            ..PoolConfig::default()
        }
    }

    fn request_test_pool() -> MemPool {
        let suffix = REQUEST_POOL_SEQ.fetch_add(1, Ordering::Relaxed);
        MemPool::new_with_prefix(
            request_pool_config(),
            format!("/c2rq{:08x}{suffix:04x}", std::process::id()),
        )
        .unwrap()
    }

    fn server_core() -> ConnectionCore {
        let core = ConnectionCore::new(c2_wire::connection::ConnectionSide::Server, || {});
        core.finish_handshake().unwrap();
        core
    }

    #[test]
    fn inline_request_moves_owned_bytes() {
        let bytes = b"inline".to_vec();
        let pointer = bytes.as_ptr();
        let received = RequestData::Inline(bytes).into_owned_bytes().unwrap();
        assert_eq!(received, b"inline");
        assert_eq!(received.as_ptr(), pointer);
    }

    #[test]
    fn peer_request_copy_ends_remote_read_and_queues_release() {
        let mut owner = request_test_pool();
        let payload = b"borrowed request".repeat(128);
        let mut handle = owner.alloc_shm_handle(payload.len()).unwrap();
        owner
            .handle_slice_mut(&mut handle)
            .copy_from_slice(&payload);
        let allocation = handle.shm_allocation().unwrap();
        let peer = Arc::new(parking_lot::RwLock::new(
            MemPool::open_peer(request_pool_config(), owner.prefix().to_owned()).unwrap(),
        ));
        let core = server_core();
        let loan = core.admit_remote_loan(7, Box::new(())).unwrap();
        let descriptor = BuddyPayload {
            seg_idx: allocation.seg_idx as u16,
            generation: allocation.generation,
            offset: allocation.offset,
            data_size: payload.len() as u32,
            is_dedicated: allocation.is_dedicated,
            loan_id: 7,
        };
        let carrier = PeerShmRequest::admit(peer, descriptor, loan).unwrap();
        assert_eq!(
            RequestData::Shm(carrier).into_owned_bytes().unwrap(),
            payload
        );
        assert_eq!(owner.stats().alloc_count, 1);
        assert!(
            !core
                .begin_write(WriteRequest::Releases)
                .unwrap()
                .unwrap()
                .release_bytes()
                .is_empty()
        );
        owner.release_handle(handle);
    }

    #[test]
    fn invalid_peer_span_still_queues_release() {
        let mut owner = request_test_pool();
        let handle = owner.alloc_shm_handle(4096).unwrap();
        let allocation = handle.shm_allocation().unwrap();
        let peer = Arc::new(parking_lot::RwLock::new(
            MemPool::open_peer(request_pool_config(), owner.prefix().to_owned()).unwrap(),
        ));
        let core = server_core();
        let loan = core.admit_remote_loan(9, Box::new(())).unwrap();
        let descriptor = BuddyPayload {
            seg_idx: allocation.seg_idx as u16,
            generation: allocation.generation,
            offset: u32::MAX,
            data_size: 2,
            is_dedicated: allocation.is_dedicated,
            loan_id: 9,
        };
        assert!(PeerShmRequest::admit(peer, descriptor, loan).is_err());
        assert!(
            !core
                .begin_write(WriteRequest::Releases)
                .unwrap()
                .unwrap()
                .release_bytes()
                .is_empty()
        );
        owner.release_handle(handle);
    }

    #[test]
    fn reassembly_request_copies_then_releases_local_backing() {
        let payload = b"reassembled".repeat(512);
        let pool = Arc::new(parking_lot::RwLock::new(request_test_pool()));
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, payload.len()).unwrap();
        backing.write_at(0, &payload).unwrap();
        backing.trim_to(4096).unwrap();
        let bytes = RequestData::Handle(backing).into_owned_bytes().unwrap();
        assert_eq!(bytes, payload[..4096]);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn register_and_resolve() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));

        let route = d.resolve("grid").expect("route should exist");
        assert_eq!(route.name, "grid");
        assert_eq!(route.method_names.len(), 2);

        // invoke the callback through the route
        let pool = Arc::new(crate::ResponsePool::new(
            MemPool::new(PoolConfig::default()).unwrap(),
        ));
        let result = route
            .callback
            .invoke("grid", 0, b"test".to_vec(), pool.clone())
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
        assert_eq!(d.removed_uid("grid"), Some("grid-uid-0001".into()));
        assert!(d.is_empty());
    }

    #[test]
    fn registering_again_clears_removed_identity() {
        let mut d = Dispatcher::new();
        d.register(make_route("grid"));
        d.unregister("grid");
        assert!(d.removed_uid("grid").is_some());
        d.register(make_route("grid"));
        assert!(d.removed_uid("grid").is_none());
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
                _request: Vec<u8>,
                _response_pool: Arc<crate::ResponsePool>,
            ) -> Result<ResponseMeta, CrmError> {
                Err(CrmError::InternalError("method not found".into()))
            }
        }

        let pool = Arc::new(crate::ResponsePool::new(
            MemPool::new(PoolConfig::default()).unwrap(),
        ));
        let cb: Arc<dyn CrmCallback> = Arc::new(FailCallback);
        let err = cb.invoke("grid", 99, b"test".to_vec(), pool).unwrap_err();
        assert!(matches!(err, CrmError::InternalError(_)));
    }
}
