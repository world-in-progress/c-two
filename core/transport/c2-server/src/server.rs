//! UDS server — accept loop, per-connection frame handler, CRM dispatch.
//!
//! CRM method execution is delegated to [`CrmCallback`] implementations
//! supplied by language bindings or native runtime adapters.
//!
//! ## Lock conventions
//!
//! This module uses **two different RwLock types**:
//! - `tokio::sync::RwLock` — async lock for `Dispatcher` (must `.await`)
//! - `parking_lot::RwLock` — sync lock for `MemPool` (blocking, no `.await`)

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, mpsc, watch};
use tracing::{debug, info, warn};

/// Monotonic counter ensuring each `Server` instance gets a unique SHM prefix,
/// even when multiple servers are created within the same PID (e.g. benchmarks
/// or tests that reset the registry).
///
/// Uses 32-bit range (~4 billion unique prefixes).  Response pool prefix
/// format: `/cc3r{pid:08x}{gen:08x}` (21 chars); reassembly pool prefix
/// format: `/cc3s{pid:08x}{gen:08x}` (21 chars).  With `_b{idx:04x}`
/// suffix the max SHM name is 27 chars (within macOS 31-char limit).
static RESPONSE_POOL_GEN: AtomicU64 = AtomicU64::new(0);

use c2_error::{C2Error, ErrorCode};
use c2_mem::MemPool;
use c2_mem::config::PoolConfig;
use c2_wire::buddy::{
    BUDDY_PAYLOAD_SIZE, BuddyPayload, decode_buddy_payload, encode_buddy_payload,
};
use c2_wire::chunk::{REPLY_CHUNK_META_SIZE, decode_chunk_header, encode_reply_chunk_meta};
use c2_wire::control::{ReplyControl, decode_call_control, try_encode_reply_control};
use c2_wire::flags::{
    FLAG_BUDDY, FLAG_CHUNK_LAST, FLAG_CHUNKED, FLAG_CTRL, FLAG_HANDSHAKE, FLAG_REPLY_V2,
    FLAG_RESPONSE, FLAG_SIGNAL,
};
use c2_wire::frame::{self, decode_frame_body, encode_frame};
pub use c2_wire::handshake::ServerIdentity;
use c2_wire::handshake::{
    CAP_CALL_V2, CAP_CHUNKED, CAP_METHOD_IDX, MAX_METHODS, MAX_ROUTES, MethodEntry, RouteInfo,
    decode_handshake, encode_server_handshake,
};
use c2_wire::msg_type::{DISCONNECT_ACK_BYTES, MsgType, PONG_BYTES};
use c2_wire::registration_control::{
    PENDING_ROUTE_REJECT_INVALID, PENDING_ROUTE_REJECT_NOT_FOUND,
    PENDING_ROUTE_REJECT_TOKEN_MISMATCH, PendingRouteAttestation, PendingRouteAttestationResponse,
    decode_pending_route_attestation_request, encode_pending_route_attestation_response,
};
use c2_wire::shutdown_control::{DirectShutdownAck, decode_shutdown_initiate, encode_shutdown_ack};

use crate::config::ServerIpcConfig;
use crate::connection::Connection;
use crate::dispatcher::{
    BuiltRoute, CrmCallback, CrmError, CrmRoute, Dispatcher, RequestData, ResponseMeta,
    RouteBuildSpec, cleanup_request,
};
use crate::heartbeat::run_heartbeat;
use crate::response::buddy_response_data_size;
use crate::scheduler::{
    RouteConcurrencyHandle, Scheduler, SchedulerAcquireError, SchedulerLimits,
    SchedulerPendingPermit, SchedulerSnapshot,
};

const IPC_SOCK_DIR: &str = "/tmp/c_two_ipc";
const FRAME_FIXED_BODY_LEN: u32 = 12;
const SHUTDOWN_INITIATE_FRAME_BODY_LEN: u32 =
    FRAME_FIXED_BODY_LEN + c2_wire::msg_type::SHUTDOWN_CLIENT_BYTES.len() as u32;
const POST_SHUTDOWN_DUPLICATE_INITIATE_READ_TIMEOUT_MS: u64 = 100;

fn error_wire(code: ErrorCode, message: impl Into<String>) -> Vec<u8> {
    C2Error::new(code, message).to_wire_bytes()
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors produced by the server.
#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    Config(String),
    Protocol(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Native lifecycle state for the IPC server accept loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLifecycleState {
    Initialized,
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed(String),
}

impl ServerLifecycleState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Starting | Self::Ready | Self::Stopping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRouteCloseOutcome {
    pub route_name: String,
    pub active_drained: bool,
    pub closed_reason: String,
}

/// The main IPC server.
///
/// Binds a UDS socket, accepts connections, and dispatches CRM calls through
/// the [`Dispatcher`].  Each connection runs in its own tokio task with a
/// dedicated heartbeat probe.
pub struct Server {
    identity: ServerIdentity,
    config: ServerIpcConfig,
    ipc_address: String,
    socket_path: PathBuf,
    /// **tokio async RwLock** — guards CRM dispatch table; requires `.read().await`
    /// / `.write().await`.  Do NOT confuse with `parking_lot::RwLock` below.
    dispatcher: RwLock<Dispatcher>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    lifecycle_tx: watch::Sender<ServerLifecycleState>,
    socket_bound: AtomicBool,
    conn_counter: AtomicU64,
    route_registration: Mutex<()>,
    /// Sharded chunk reassembly lifecycle manager.
    chunk_registry: Arc<c2_wire::chunk::ChunkRegistry>,
    /// **parking_lot sync RwLock** — guards SHM memory pool; blocking `.read()`
    /// / `.write()` (no `.await`).  Safe to hold briefly inside tokio tasks.
    response_pool: Arc<parking_lot::RwLock<MemPool>>,
    pending_routes: Arc<parking_lot::Mutex<HashMap<String, PendingRouteInfo>>>,
    pending_requests: Arc<Semaphore>,
    chunk_processing_permits: Arc<Semaphore>,
    chunk_route_pending: parking_lot::Mutex<HashMap<(u64, u64), ChunkRouteAdmission>>,
    active_connections: parking_lot::Mutex<HashMap<u64, Arc<Connection>>>,
    active_connections_notify: Notify,
    shutdown_route_outcomes: parking_lot::Mutex<Vec<ServerRouteCloseOutcome>>,
    shutdown_generation: AtomicU64,
    execution_scheduler: Scheduler,
}

pub struct PendingRouteReservation {
    route_name: String,
    registration_token: String,
    route: Option<CrmRoute>,
    pending_routes: Arc<parking_lot::Mutex<HashMap<String, PendingRouteInfo>>>,
    shutdown_generation: u64,
    resolved: bool,
}

pub struct RouteAdmissionToken {
    route_name: String,
    shutdown_generation: u64,
}

#[derive(Debug, Clone)]
struct PendingRouteInfo {
    registration_token: String,
    contract: c2_contract::ExpectedRouteContract,
    method_names: Vec<String>,
    max_payload_size: u64,
}

impl PendingRouteReservation {
    fn route_name(&self) -> &str {
        &self.route_name
    }

    pub fn registration_token(&self) -> &str {
        &self.registration_token
    }

    fn into_route(mut self) -> CrmRoute {
        self.route
            .take()
            .expect("pending route reservation must still own the route")
    }

    fn resolve(&mut self) {
        if !self.resolved {
            self.pending_routes.lock().remove(&self.route_name);
            self.resolved = true;
        }
    }

    fn close_scheduler_for_abort(&self) {
        if let Some(route) = self.route.as_ref() {
            route.scheduler.close();
        }
    }
}

impl Drop for PendingRouteReservation {
    fn drop(&mut self) {
        if !self.resolved {
            self.pending_routes.lock().remove(&self.route_name);
        }
    }
}

impl Server {
    /// Create a new server for the given IPC address.
    ///
    /// Address format: `ipc://region_id`
    /// → socket at `/tmp/c_two_ipc/region_id.sock`
    pub fn new(address: &str, config: ServerIpcConfig) -> Result<Self, ServerError> {
        let identity = ServerIdentity {
            server_id: server_id_from_ipc_address(address)?,
            server_instance_id: uuid::Uuid::new_v4().simple().to_string(),
        };
        Self::new_with_identity(address, config, identity)
    }

    /// Create a new server with an explicit identity.
    pub fn new_with_identity(
        address: &str,
        config: ServerIpcConfig,
        identity: ServerIdentity,
    ) -> Result<Self, ServerError> {
        config.validate().map_err(ServerError::Config)?;
        validate_server_identity(&identity)?;
        let socket_path = parse_socket_path(address)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let reassembly_cfg = PoolConfig {
            segment_size: config.reassembly_segment_size as usize,
            min_block_size: 4096,
            max_segments: config.reassembly_max_segments as usize,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 5.0,
            buddy_idle_decay_secs: config.pool_decay_seconds,
            spill_threshold: 0.8,
            spill_dir: PathBuf::from("/tmp/c_two_reassembly"),
        };
        let reassembly_pool = {
            let pid = std::process::id();
            let ra_gen = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
            let prefix = format!("/cc3s{:08x}{:08x}", pid, ra_gen);
            MemPool::new_with_prefix(reassembly_cfg, prefix)
        };
        let chunk_config = c2_wire::chunk::ChunkConfig::from_base(&config);
        let chunk_registry = Arc::new(c2_wire::chunk::ChunkRegistry::new(
            Arc::new(parking_lot::RwLock::new(reassembly_pool)),
            chunk_config,
        ));
        let response_cfg = PoolConfig {
            segment_size: config.pool_segment_size as usize,
            min_block_size: 4096,
            max_segments: config.max_pool_segments as usize,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 5.0,
            buddy_idle_decay_secs: config.pool_decay_seconds,
            spill_threshold: 0.8,
            spill_dir: PathBuf::from("/tmp/c_two_response_spill"),
        };
        let mut response_pool = {
            let pid = std::process::id();
            let generation = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("/cc3r{:08x}{:08x}", pid, generation as u32);
            MemPool::new_with_prefix(response_cfg, prefix)
        };
        response_pool
            .ensure_ready()
            .map_err(|e| ServerError::Config(format!("response pool init: {e}")))?;
        let (lifecycle_tx, _lifecycle_rx) = watch::channel(ServerLifecycleState::Initialized);
        let pending_requests = Arc::new(Semaphore::new(config.max_pending_requests as usize));
        let chunk_processing_permits = Arc::new(Semaphore::new(config.max_total_chunks as usize));
        let execution_scheduler = Scheduler::with_limits(
            crate::scheduler::ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits::try_from_usize(None, Some(config.max_execution_workers as usize))
                .expect("validated server config must provide max_execution_workers >= 1"),
        );
        Ok(Self {
            identity,
            config,
            ipc_address: address.to_string(),
            socket_path,
            dispatcher: RwLock::new(Dispatcher::new()),
            shutdown_tx,
            shutdown_rx,
            lifecycle_tx,
            socket_bound: AtomicBool::new(false),
            conn_counter: AtomicU64::new(0),
            route_registration: Mutex::new(()),
            chunk_registry,
            response_pool: Arc::new(parking_lot::RwLock::new(response_pool)),
            pending_routes: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            pending_requests,
            chunk_processing_permits,
            chunk_route_pending: parking_lot::Mutex::new(HashMap::new()),
            active_connections: parking_lot::Mutex::new(HashMap::new()),
            active_connections_notify: Notify::new(),
            shutdown_route_outcomes: parking_lot::Mutex::new(Vec::new()),
            shutdown_generation: AtomicU64::new(0),
            execution_scheduler,
        })
    }

    fn try_acquire_pending_request(&self) -> Result<OwnedSemaphorePermit, u32> {
        self.pending_requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.config.max_pending_requests)
    }

    fn try_acquire_chunk_processing_permit(&self) -> Result<OwnedSemaphorePermit, u32> {
        self.chunk_processing_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.config.max_total_chunks)
    }

    fn store_chunk_route_pending(
        &self,
        conn_id: u64,
        request_id: u64,
        admission: ChunkRouteAdmission,
    ) -> Result<(), ChunkRouteAdmission> {
        let mut pending = self.chunk_route_pending.lock();
        if pending.contains_key(&(conn_id, request_id)) {
            return Err(admission);
        }
        pending.insert((conn_id, request_id), admission);
        Ok(())
    }

    fn take_chunk_route_pending(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Option<ChunkRouteAdmission> {
        self.chunk_route_pending
            .lock()
            .remove(&(conn_id, request_id))
    }

    fn abort_chunk_request(&self, conn_id: u64, request_id: u64) {
        self.chunk_registry.abort(conn_id, request_id);
        self.take_chunk_route_pending(conn_id, request_id);
    }

    fn cleanup_chunk_requests_for_connection(&self, conn_id: u64) {
        self.chunk_registry.cleanup_connection(conn_id);
        self.chunk_route_pending
            .lock()
            .retain(|(pending_conn_id, _), _| *pending_conn_id != conn_id);
    }

    fn sweep_stale_chunk_route_pending(&self) -> usize {
        let stale_keys = {
            let pending = self.chunk_route_pending.lock();
            pending
                .keys()
                .filter(|(conn_id, request_id)| {
                    !self.chunk_registry.contains(*conn_id, *request_id)
                })
                .copied()
                .collect::<Vec<_>>()
        };
        if stale_keys.is_empty() {
            return 0;
        }
        let mut pending = self.chunk_route_pending.lock();
        for key in &stale_keys {
            pending.remove(key);
        }
        stale_keys.len()
    }

    fn register_active_connection(&self, conn: Arc<Connection>) {
        self.active_connections.lock().insert(conn.conn_id(), conn);
    }

    fn unregister_active_connection(&self, conn_id: u64) {
        self.active_connections.lock().remove(&conn_id);
        self.active_connections_notify.notify_waiters();
    }

    async fn close_registered_routes_for_shutdown(
        &self,
        closed_reason: &str,
    ) -> Vec<ServerRouteCloseOutcome> {
        let routes = {
            let _guard = self.route_registration.lock().await;
            let mut dispatcher = self.dispatcher.write().await;
            let routes = dispatcher.routes_snapshot();
            for route in &routes {
                route.scheduler.close();
            }
            dispatcher.take_all()
        };
        Self::wait_closed_routes_drained(routes, closed_reason).await
    }

    async fn wait_closed_routes_drained(
        routes: Vec<Arc<CrmRoute>>,
        closed_reason: &str,
    ) -> Vec<ServerRouteCloseOutcome> {
        let mut outcomes = Vec::with_capacity(routes.len());
        for route in routes {
            route.scheduler.wait_drained_async().await;
            outcomes.push(ServerRouteCloseOutcome {
                route_name: route.name.clone(),
                active_drained: true,
                closed_reason: closed_reason.to_string(),
            });
        }
        outcomes
    }

    async fn wait_for_active_connections_drained(&self) {
        loop {
            let notified = self.active_connections_notify.notified();
            if self.active_connections.lock().is_empty() {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    fn active_connection_ids(&self) -> Vec<u64> {
        self.active_connections.lock().keys().copied().collect()
    }

    #[cfg(test)]
    fn active_connection(&self, conn_id: u64) -> Option<Arc<Connection>> {
        self.active_connections.lock().get(&conn_id).cloned()
    }

    fn take_shutdown_route_outcomes(&self) -> Vec<ServerRouteCloseOutcome> {
        std::mem::take(&mut *self.shutdown_route_outcomes.lock())
    }

    /// Identity announced in server→client handshake ACKs.
    pub fn identity(&self) -> &ServerIdentity {
        &self.identity
    }

    /// Stable logical server identity.
    pub fn server_id(&self) -> &str {
        &self.identity.server_id
    }

    /// Per-server-incarnation identity.
    pub fn server_instance_id(&self) -> &str {
        &self.identity.server_instance_id
    }

    pub fn config(&self) -> &ServerIpcConfig {
        &self.config
    }

    pub fn ipc_address(&self) -> &str {
        &self.ipc_address
    }

    pub fn build_route(
        &self,
        spec: RouteBuildSpec,
        callback: Arc<dyn CrmCallback>,
    ) -> Result<BuiltRoute, ServerError> {
        let scheduler =
            Scheduler::with_limits(spec.concurrency_mode, spec.access_map.clone(), spec.limits);
        let route_handle = RouteConcurrencyHandle::new(scheduler.clone());
        let route = CrmRoute::new(spec, scheduler, callback);
        validate_route_for_wire(&route)?;
        Ok(BuiltRoute::new(route, route_handle))
    }

    pub async fn reserve_route(
        &self,
        route: BuiltRoute,
    ) -> Result<PendingRouteReservation, ServerError> {
        let route = route.into_route();
        validate_route_for_wire(&route)?;
        let route_name = route.name.clone();
        let _guard = self.route_registration.lock().await;
        let dispatcher = self.dispatcher.read().await;
        if dispatcher.resolve(&route_name).is_some() {
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                route_name
            )));
        }
        let mut pending_routes = self.pending_routes.lock();
        if pending_routes.contains_key(&route_name) {
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                route_name
            )));
        }
        if dispatcher.len() + pending_routes.len() >= MAX_ROUTES {
            return Err(ServerError::Protocol(format!(
                "route count exceeds wire limit: {} > {}",
                dispatcher.len() + pending_routes.len() + 1,
                MAX_ROUTES
            )));
        }
        if matches!(self.lifecycle_state(), ServerLifecycleState::Stopping) {
            return Err(ServerError::Protocol(format!(
                "server is shutting down; cannot reserve route {}",
                route_name
            )));
        }
        drop(dispatcher);
        let registration_token = uuid::Uuid::new_v4().simple().to_string();
        pending_routes.insert(
            route_name.clone(),
            PendingRouteInfo {
                registration_token: registration_token.clone(),
                contract: c2_contract::ExpectedRouteContract {
                    route_name: route_name.clone(),
                    crm_ns: route.crm_ns.clone(),
                    crm_name: route.crm_name.clone(),
                    crm_ver: route.crm_ver.clone(),
                    abi_hash: route.abi_hash.clone(),
                    signature_hash: route.signature_hash.clone(),
                },
                method_names: route.method_names.clone(),
                max_payload_size: self.config.max_payload_size,
            },
        );
        Ok(PendingRouteReservation {
            route_name,
            registration_token,
            route: Some(route),
            pending_routes: Arc::clone(&self.pending_routes),
            shutdown_generation: self.shutdown_generation.load(Ordering::Acquire),
            resolved: false,
        })
    }

    #[cfg(test)]
    async fn register_route(&self, route: CrmRoute) -> Result<(), ServerError> {
        let route_handle = RouteConcurrencyHandle::new(route.scheduler.as_ref().clone());
        let reservation = self
            .reserve_route(BuiltRoute::new(route, route_handle))
            .await?;
        self.commit_reserved_route(reservation).await
    }

    pub async fn commit_reserved_route(
        &self,
        reservation: PendingRouteReservation,
    ) -> Result<(), ServerError> {
        self.commit_reserved_route_with_admission(reservation, true)
            .await
            .map(|_| ())
    }

    pub async fn commit_reserved_route_closed(
        &self,
        reservation: PendingRouteReservation,
    ) -> Result<RouteAdmissionToken, ServerError> {
        self.commit_reserved_route_with_admission(reservation, false)
            .await
    }

    async fn commit_reserved_route_with_admission(
        &self,
        mut reservation: PendingRouteReservation,
        admission_open: bool,
    ) -> Result<RouteAdmissionToken, ServerError> {
        let _guard = self.route_registration.lock().await;
        if reservation.shutdown_generation != self.shutdown_generation.load(Ordering::Acquire)
            || matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
        {
            reservation.close_scheduler_for_abort();
            reservation.resolve();
            return Err(ServerError::Protocol(format!(
                "server is shutting down; cannot commit route {}",
                reservation.route_name()
            )));
        }
        let mut dispatcher = self.dispatcher.write().await;
        if dispatcher.resolve(reservation.route_name()).is_some() {
            reservation.close_scheduler_for_abort();
            reservation.resolve();
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                reservation.route_name()
            )));
        }
        let token = RouteAdmissionToken {
            route_name: reservation.route_name().to_string(),
            shutdown_generation: reservation.shutdown_generation,
        };
        if !admission_open {
            reservation.close_scheduler_for_abort();
        }
        reservation.resolve();
        dispatcher.register(reservation.into_route());
        Ok(token)
    }

    pub async fn open_route_admission(
        &self,
        token: RouteAdmissionToken,
    ) -> Result<(), ServerError> {
        let _guard = self.route_registration.lock().await;
        let name = token.route_name;
        if token.shutdown_generation != self.shutdown_generation.load(Ordering::Acquire)
            || matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
        {
            return Err(ServerError::Protocol(format!(
                "server is shutting down; cannot open route {name}"
            )));
        }
        let dispatcher = self.dispatcher.read().await;
        let Some(route) = dispatcher.resolve(&name) else {
            return Err(ServerError::Protocol(format!(
                "route not registered: {name}"
            )));
        };
        route.scheduler.open_admission_for_registration();
        Ok(())
    }

    pub async fn abort_reserved_route(&self, mut reservation: PendingRouteReservation) {
        let _guard = self.route_registration.lock().await;
        reservation.close_scheduler_for_abort();
        reservation.resolve();
    }

    fn attest_pending_route(
        &self,
        route_name: &str,
        registration_token: &str,
    ) -> Result<PendingRouteInfo, PendingRouteAttestationResponse> {
        let pending_routes = self.pending_routes.lock();
        let Some(info) = pending_routes.get(route_name) else {
            return Err(PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_NOT_FOUND.to_string(),
                message: format!("pending route '{route_name}' not found"),
            });
        };
        if info.registration_token != registration_token {
            return Err(PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_TOKEN_MISMATCH.to_string(),
                message: format!("pending route '{route_name}' registration token mismatch"),
            });
        }
        Ok(info.clone())
    }

    /// Remove a CRM route. Returns `true` if it existed.
    ///
    /// The route handle is marked closed under the same dispatcher write lock
    /// that removes the route, so local handle clones and remote route lookup
    /// cannot observe an open-but-unregistered window.
    pub async fn unregister_route(&self, name: &str) -> bool {
        let _guard = self.route_registration.lock().await;
        let mut dispatcher = self.dispatcher.write().await;
        if let Some(route) = dispatcher.resolve(name) {
            route.scheduler.close();
        }
        let removed = dispatcher.unregister(name);
        drop(dispatcher);
        if let Some(route) = removed {
            route.scheduler.wait_drained_async().await;
            true
        } else {
            false
        }
    }

    /// Remove multiple CRM routes as one shutdown transaction.
    ///
    /// All target route handles are closed before any route waits for drain, so
    /// shutdown cannot keep later routes open while an earlier route is draining.
    pub async fn unregister_routes_for_shutdown(
        &self,
        names: &[String],
        closed_reason: &str,
    ) -> Vec<ServerRouteCloseOutcome> {
        let routes = {
            let _guard = self.route_registration.lock().await;
            let mut dispatcher = self.dispatcher.write().await;
            let mut seen = HashSet::new();
            let mut routes = Vec::new();
            for name in names {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                if let Some(route) = dispatcher.resolve(name) {
                    route.scheduler.close();
                    routes.push(route);
                }
            }
            for route in &routes {
                dispatcher.unregister(&route.name);
            }
            routes
        };
        Self::wait_closed_routes_drained(routes, closed_reason).await
    }

    /// Filesystem path of the bound UDS socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn remove_owned_socket_file(&self) {
        if self.socket_bound.swap(false, Ordering::AcqRel) {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    fn set_lifecycle_state(&self, state: ServerLifecycleState) {
        self.lifecycle_tx.send_replace(state);
    }

    pub fn lifecycle_state(&self) -> ServerLifecycleState {
        self.lifecycle_tx.borrow().clone()
    }

    pub fn is_ready(&self) -> bool {
        self.lifecycle_state().is_ready()
    }

    pub fn is_running(&self) -> bool {
        self.lifecycle_state().is_running()
    }

    /// Fence a new native start attempt.
    ///
    /// This resets the one-shot shutdown signal and moves stale terminal
    /// lifecycle states (`Stopped` / `Failed`) back to `Starting` before the
    /// async accept loop is spawned. Callers that start `run()` on a background
    /// runtime should invoke this synchronously before they begin waiting for
    /// readiness, otherwise a waiter can observe the previous attempt's
    /// terminal state before the spawned task is polled.
    pub fn begin_start_attempt(&self) -> Result<(), ServerError> {
        match self.lifecycle_state() {
            ServerLifecycleState::Initialized
            | ServerLifecycleState::Stopped
            | ServerLifecycleState::Failed(_) => {
                self.shutdown_tx.send_replace(false);
                self.shutdown_route_outcomes.lock().clear();
                self.set_lifecycle_state(ServerLifecycleState::Starting);
                Ok(())
            }
            state => Err(ServerError::Config(format!(
                "server cannot start while lifecycle state is {state:?}",
            ))),
        }
    }

    pub async fn wait_until_ready(&self, timeout: Duration) -> Result<(), ServerError> {
        let mut rx = self.lifecycle_tx.subscribe();
        let wait = async {
            loop {
                let state = rx.borrow().clone();
                match state {
                    ServerLifecycleState::Ready => return Ok(()),
                    ServerLifecycleState::Failed(message) => {
                        return Err(ServerError::Config(format!(
                            "server failed to start: {message}",
                        )));
                    }
                    ServerLifecycleState::Stopped => {
                        return Err(ServerError::Config(
                            "server stopped before becoming ready".to_string(),
                        ));
                    }
                    ServerLifecycleState::Initialized
                    | ServerLifecycleState::Starting
                    | ServerLifecycleState::Stopping => {}
                }
                rx.changed().await.map_err(|_| {
                    ServerError::Config("server readiness channel closed".to_string())
                })?;
            }
        };

        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            ServerError::Config(format!(
                "server did not become ready within {:.3}s",
                timeout.as_secs_f64(),
            ))
        })?
    }

    pub async fn wait_until_responsive(&self, timeout: Duration) -> Result<(), ServerError> {
        let started = std::time::Instant::now();
        self.wait_until_ready(timeout).await?;
        loop {
            if started.elapsed() >= timeout {
                return Err(ServerError::Config(format!(
                    "server became ready but did not answer direct IPC ping within {:.3}s",
                    timeout.as_secs_f64(),
                )));
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            let probe_timeout = remaining.min(Duration::from_millis(100));
            if tokio::time::timeout(probe_timeout, ping_server_socket(self.socket_path())).await
                == Ok(true)
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn wait_until_stopped(&self, timeout: Duration) -> Result<(), ServerError> {
        let mut rx = self.lifecycle_tx.subscribe();
        let wait = async {
            loop {
                let state = rx.borrow().clone();
                match state {
                    ServerLifecycleState::Initialized
                    | ServerLifecycleState::Stopped
                    | ServerLifecycleState::Failed(_) => return Ok(()),
                    ServerLifecycleState::Starting
                    | ServerLifecycleState::Ready
                    | ServerLifecycleState::Stopping => {}
                }
                rx.changed().await.map_err(|_| {
                    ServerError::Config("server lifecycle channel closed".to_string())
                })?;
            }
        };

        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            ServerError::Config(format!(
                "server did not stop within {:.3}s",
                timeout.as_secs_f64(),
            ))
        })?
    }

    /// Mark runtime-backed server work as stopped after its runtime is gone.
    ///
    /// This is a shutdown cleanup fence for runtime owners. It does not erase
    /// startup failure diagnostics, but it prevents a force-dropped runtime from
    /// leaving `Starting`, `Ready`, or `Stopping` as a stale non-terminal state.
    pub fn finalize_runtime_stopped(&self) {
        self.remove_owned_socket_file();
        match self.lifecycle_state() {
            ServerLifecycleState::Starting
            | ServerLifecycleState::Ready
            | ServerLifecycleState::Stopping => {
                self.set_lifecycle_state(ServerLifecycleState::Stopped);
            }
            ServerLifecycleState::Initialized
            | ServerLifecycleState::Stopped
            | ServerLifecycleState::Failed(_) => {}
        }
    }

    /// Get a shared reference to the response pool (for zero-copy dispatch).
    pub fn response_pool_arc(&self) -> Arc<parking_lot::RwLock<MemPool>> {
        Arc::clone(&self.response_pool)
    }

    /// Return the configured response SHM threshold.
    pub fn response_shm_threshold(&self) -> u64 {
        self.config.shm_threshold
    }

    /// Return the configured maximum logical response payload size.
    pub fn response_max_payload_size(&self) -> u64 {
        self.config.max_payload_size
    }

    /// Return true if a route is currently registered.
    pub async fn contains_route(&self, name: &str) -> bool {
        self.dispatcher.read().await.resolve(name).is_some()
    }

    /// Return the current route scheduler state, if the route is registered.
    pub async fn route_scheduler_snapshot(&self, name: &str) -> Option<SchedulerSnapshot> {
        self.dispatcher
            .read()
            .await
            .resolve(name)
            .map(|route| route.scheduler.snapshot())
    }

    /// Run the accept loop.  Blocks until [`shutdown`](Self::shutdown) is called.
    pub async fn run(self: &Arc<Self>) -> Result<(), ServerError> {
        if !matches!(self.lifecycle_state(), ServerLifecycleState::Starting) {
            self.begin_start_attempt()?;
        }

        let startup = async {
            std::fs::create_dir_all(IPC_SOCK_DIR)?;
            remove_stale_socket_file(&self.socket_path)?;
            let listener = UnixListener::bind(&self.socket_path)?;
            self.socket_bound.store(true, Ordering::Release);
            Ok::<UnixListener, ServerError>(listener)
        }
        .await;

        let listener = match startup {
            Ok(listener) => listener,
            Err(err) => {
                self.set_lifecycle_state(ServerLifecycleState::Failed(err.to_string()));
                return Err(err);
            }
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600));
        }

        self.set_lifecycle_state(ServerLifecycleState::Ready);
        info!(path = %self.socket_path.display(), "server listening");

        // Spawn periodic GC sweep for expired chunk assemblies.
        let gc_server = Arc::clone(self);
        let gc_interval = self.chunk_registry.config().gc_interval;
        let mut gc_shutdown_rx = self.shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gc_interval);
            interval.tick().await; // skip first immediate tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = gc_server.chunk_registry.gc_sweep();
                        let released_route_pending = gc_server.sweep_stale_chunk_route_pending();
                        if stats.expired > 0 {
                            info!(
                                expired = stats.expired,
                                freed = stats.freed_bytes,
                                released_route_pending,
                                "chunk GC sweep"
                            );
                        }
                    }
                    _ = gc_shutdown_rx.changed() => break,
                }
            }
        });

        let mut shutdown_rx = self.shutdown_rx.clone();
        let (shutdown_done_tx, mut shutdown_done_rx) = mpsc::unbounded_channel();
        let mut shutdown_close_started = false;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let server = Arc::clone(self);
                            tokio::spawn(handle_connection(server, stream));
                        }
                        Err(e) => warn!("accept error: {e}"),
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() && !shutdown_close_started {
                        info!("server shutting down");
                        self.set_lifecycle_state(ServerLifecycleState::Stopping);
                        shutdown_close_started = true;
                        let server = Arc::clone(self);
                        let done_tx = shutdown_done_tx.clone();
                        tokio::spawn(async move {
                            let outcomes = server
                                .close_registered_routes_for_shutdown("direct_ipc_shutdown")
                                .await;
                            let _ = done_tx.send(outcomes);
                        });
                    }
                }
                shutdown_route_outcomes = shutdown_done_rx.recv(), if shutdown_close_started => {
                    let shutdown_route_outcomes = shutdown_route_outcomes.unwrap_or_default();
                    self.wait_for_active_connections_drained().await;
                    if !shutdown_route_outcomes.is_empty() {
                        self.shutdown_route_outcomes
                            .lock()
                            .extend(shutdown_route_outcomes);
                    }
                    self.remove_owned_socket_file();
                    self.set_lifecycle_state(ServerLifecycleState::Stopped);
                        break;
                }
            }
        }
        Ok(())
    }

    /// Observe route-close outcomes from an already initiated external shutdown.
    ///
    /// This is the reconciliation path for owners that did not initiate the
    /// shutdown, for example a runtime bridge observing a direct IPC admin stop.
    /// It never starts shutdown for a ready server.
    pub async fn observe_external_shutdown_outcomes(
        &self,
        timeout: Duration,
    ) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        if matches!(
            self.lifecycle_state(),
            ServerLifecycleState::Stopping | ServerLifecycleState::Stopped
        ) {
            self.wait_until_stopped(timeout).await?;
            Ok(self.take_shutdown_route_outcomes())
        } else {
            Ok(Vec::new())
        }
    }

    /// Initiate shutdown and wait for the runtime-owned close transaction.
    pub async fn shutdown_and_wait(
        &self,
        timeout: Duration,
    ) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        self.request_shutdown_signal();
        self.wait_until_stopped(timeout).await?;
        Ok(self.take_shutdown_route_outcomes())
    }

    fn request_shutdown_signal(&self) {
        match self.lifecycle_state() {
            ServerLifecycleState::Starting | ServerLifecycleState::Ready => {
                self.shutdown_generation.fetch_add(1, Ordering::AcqRel);
                self.set_lifecycle_state(ServerLifecycleState::Stopping);
            }
            ServerLifecycleState::Stopping
            | ServerLifecycleState::Initialized
            | ServerLifecycleState::Stopped
            | ServerLifecycleState::Failed(_) => {}
        }
        if self.is_running() {
            self.set_lifecycle_state(ServerLifecycleState::Stopping);
        }
        let shutdown_already_requested = *self.shutdown_tx.borrow();
        if !shutdown_already_requested {
            let _ = self.shutdown_tx.send(true);
        }
    }
}

async fn ping_server_socket(socket_path: &Path) -> bool {
    let mut stream = match UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let frame = encode_frame(0, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES);
    if stream.write_all(&frame).await.is_err() {
        return false;
    }
    let mut header = [0u8; frame::HEADER_SIZE];
    if stream.read_exact(&mut header).await.is_err() {
        return false;
    }
    let (total_len, body) = match frame::decode_total_len(&header) {
        Ok(decoded) => decoded,
        Err(_) => return false,
    };
    let (frame_header, payload_prefix) = match frame::decode_frame_body(body, total_len) {
        Ok(decoded) => decoded,
        Err(_) => return false,
    };
    let payload_len = frame_header.payload_len();
    let mut payload = payload_prefix.to_vec();
    if payload.len() < payload_len {
        let mut tail = vec![0u8; payload_len - payload.len()];
        if stream.read_exact(&mut tail).await.is_err() {
            return false;
        }
        payload.extend_from_slice(&tail);
    }
    frame_header.flags & FLAG_SIGNAL != 0
        && frame_header.flags & FLAG_RESPONSE != 0
        && payload == c2_wire::msg_type::PONG_BYTES
}

fn validate_route_for_wire(route: &CrmRoute) -> Result<(), ServerError> {
    c2_contract::validate_named_route_name("route name", &route.name)
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
    Ok(())
}

fn validate_server_identity(identity: &ServerIdentity) -> Result<(), ServerError> {
    c2_config::validate_server_id(&identity.server_id).map_err(ServerError::Config)?;
    validate_identity_wire_len("server_id", &identity.server_id).map_err(ServerError::Config)?;
    validate_identity_component("server_instance_id", &identity.server_instance_id)
        .map_err(ServerError::Config)
}

fn validate_identity_wire_len(label: &str, value: &str) -> Result<(), String> {
    let actual = value.len();
    if actual > c2_contract::MAX_WIRE_TEXT_BYTES {
        return Err(format!(
            "{label} cannot exceed {} bytes",
            c2_contract::MAX_WIRE_TEXT_BYTES
        ));
    }
    Ok(())
}

fn validate_identity_component(label: &str, value: &str) -> Result<(), String> {
    validate_identity_wire_len(label, value)?;
    if value.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if value.trim() != value {
        return Err(format!(
            "{label} cannot contain leading or trailing whitespace"
        ));
    }
    if value == "." || value == ".." || value.contains('/') || value.contains('\\') {
        return Err(format!("{label} cannot contain path separators"));
    }
    if !value.is_ascii() {
        return Err(format!("{label} must be ASCII"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} cannot contain control characters"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Address helpers
// ---------------------------------------------------------------------------

fn server_id_from_ipc_address(address: &str) -> Result<String, ServerError> {
    let region = address
        .strip_prefix("ipc://")
        .ok_or_else(|| ServerError::Config(format!("invalid IPC address: {address}")))?;
    validate_region_id(region).map_err(ServerError::Config)?;
    Ok(region.to_string())
}

fn parse_socket_path(address: &str) -> Result<PathBuf, ServerError> {
    let region = address
        .strip_prefix("ipc://")
        .ok_or_else(|| ServerError::Config(format!("invalid IPC address: {address}")))?;
    validate_region_id(region).map_err(ServerError::Config)?;
    Ok(PathBuf::from(IPC_SOCK_DIR).join(format!("{region}.sock")))
}

fn remove_stale_socket_file(socket_path: &Path) -> Result<(), ServerError> {
    if !socket_path.exists() {
        return Ok(());
    }

    match StdUnixStream::connect(socket_path) {
        Ok(_) => Err(ServerError::Config(format!(
            "IPC socket {} already has an active listener",
            socket_path.display(),
        ))),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            let _ = std::fs::remove_file(socket_path);
            Ok(())
        }
        Err(err) => Err(ServerError::Io(err)),
    }
}

fn validate_region_id(region: &str) -> Result<(), String> {
    c2_config::validate_ipc_region_id(region)
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

enum SignalAction {
    Continue,
    Disconnect,
}

async fn handle_post_shutdown_connection(server: Arc<Server>, stream: UnixStream) {
    let conn_id = server.conn_counter.fetch_add(1, Ordering::Relaxed);
    let conn = Connection::new(conn_id);
    let (mut reader, write_half) = stream.into_split();
    let writer = Arc::new(Mutex::new(write_half));
    let read_timeout = Duration::from_millis(POST_SHUTDOWN_DUPLICATE_INITIATE_READ_TIMEOUT_MS);

    let mut len_buf = [0u8; 4];
    if !matches!(
        tokio::time::timeout(read_timeout, reader.read_exact(&mut len_buf)).await,
        Ok(Ok(_))
    ) {
        return;
    }
    let total_len = u32::from_le_bytes(len_buf);
    if total_len != SHUTDOWN_INITIATE_FRAME_BODY_LEN {
        warn!(
            conn_id,
            total_len, "non-shutdown frame received after shutdown requested"
        );
        return;
    }

    let mut body = vec![0u8; total_len as usize];
    if !matches!(
        tokio::time::timeout(read_timeout, reader.read_exact(&mut body)).await,
        Ok(Ok(_))
    ) {
        return;
    }
    let (header, payload) = match decode_frame_body(&body, total_len) {
        Ok(value) => value,
        Err(err) => {
            warn!(conn_id, ?err, "post-shutdown duplicate frame decode error");
            return;
        }
    };
    if header.is_signal()
        && matches!(
            payload.first().and_then(|&b| MsgType::from_byte(b)),
            Some(MsgType::ShutdownClient)
        )
    {
        handle_shutdown_signal(&server, &conn, payload, header.request_id, &writer).await;
    }
}

async fn handle_connection(server: Arc<Server>, stream: UnixStream) {
    let mut shutdown_rx = server.shutdown_rx.clone();
    if *shutdown_rx.borrow() {
        handle_post_shutdown_connection(server, stream).await;
        return;
    }

    let conn_id = server.conn_counter.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(Connection::new(conn_id));
    server.register_active_connection(Arc::clone(&conn));

    let (mut reader, write_half) = stream.into_split();
    let writer = Arc::new(Mutex::new(write_half));

    // Start heartbeat task.
    let hb_handle = {
        let c = Arc::clone(&conn);
        let w = Arc::clone(&writer);
        let cfg = server.config.clone();
        tokio::spawn(async move { run_heartbeat(c, w, &cfg).await })
    };

    debug!(conn_id, "connection accepted");

    let max_frame = server.config.max_frame_size;

    loop {
        // 1. Read 4-byte total_len prefix.
        let mut len_buf = [0u8; 4];
        tokio::select! {
            read_result = reader.read_exact(&mut len_buf) => {
                if read_result.is_err() {
                    break; // EOF or broken pipe
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        }
        let total_len = u32::from_le_bytes(len_buf);

        if *shutdown_rx.borrow() && total_len != SHUTDOWN_INITIATE_FRAME_BODY_LEN {
            warn!(
                conn_id,
                total_len, "non-shutdown frame received after shutdown requested"
            );
            break;
        }
        if total_len < 12 || (total_len as u64) > max_frame {
            warn!(conn_id, total_len, "invalid frame length");
            break;
        }

        // 2. Read body (request_id + flags + payload).
        let mut body = vec![0u8; total_len as usize];
        tokio::select! {
            read_result = reader.read_exact(&mut body) => {
                if read_result.is_err() {
                    break;
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        }

        // 3. Decode header + payload.
        let (header, payload) = match decode_frame_body(&body, total_len) {
            Ok(v) => v,
            Err(e) => {
                warn!(conn_id, ?e, "frame decode error");
                break;
            }
        };

        conn.touch();
        let flags = header.flags;
        let request_id = header.request_id;

        if *shutdown_rx.borrow() {
            if header.is_signal()
                && matches!(
                    payload.first().and_then(|&b| MsgType::from_byte(b)),
                    Some(MsgType::ShutdownClient)
                )
            {
                handle_shutdown_signal(&server, &conn, payload, request_id, &writer).await;
            }
            break;
        }

        // 4. Dispatch by frame type.
        if header.is_handshake() {
            if let Err(e) = handle_handshake(&server, &conn, payload, request_id, &writer).await {
                warn!(conn_id, %e, "handshake failed");
                break;
            }
        } else if header.is_signal() {
            if matches!(
                payload.first().and_then(|&b| MsgType::from_byte(b)),
                Some(MsgType::ShutdownClient)
            ) {
                handle_shutdown_signal(&server, &conn, payload, request_id, &writer).await;
                break;
            }
            match handle_signal(payload, request_id, &writer).await {
                SignalAction::Continue => {}
                SignalAction::Disconnect => break,
            }
        } else if header.is_ctrl() {
            handle_ctrl(&server, payload, request_id, &writer).await;
        } else if header.is_call_v2() {
            if c2_wire::flags::is_chunked(flags) {
                let chunk_processing_permit = match server.try_acquire_chunk_processing_permit() {
                    Ok(permit) => permit,
                    Err(limit) => {
                        if c2_wire::flags::is_buddy(flags) {
                            cleanup_buddy_request_block(&conn, payload);
                        }
                        write_chunk_processing_capacity_error(&writer, request_id, limit).await;
                        continue;
                    }
                };
                let srv = Arc::clone(&server);
                let cn = Arc::clone(&conn);
                let wr = Arc::clone(&writer);
                let pl = payload.to_vec();
                tokio::spawn(async move {
                    dispatch_chunked_call(
                        &srv,
                        &cn,
                        request_id,
                        flags,
                        &pl,
                        &wr,
                        chunk_processing_permit,
                    )
                    .await;
                });
                continue;
            }
            if c2_wire::flags::is_buddy(flags) {
                let (ctrl, ctrl_consumed) = match decode_call_control(payload, BUDDY_PAYLOAD_SIZE) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            conn_id = conn.conn_id(),
                            ?e,
                            "buddy call control decode error"
                        );
                        cleanup_buddy_request_block(&conn, payload);
                        continue;
                    }
                };
                let route_admission =
                    match reserve_route_execution(&server, &ctrl.route_name, ctrl.method_idx).await
                    {
                        Ok(admission) => admission,
                        Err(err) => {
                            cleanup_buddy_request_block(&conn, payload);
                            write_route_admission_error(&writer, request_id, err).await;
                            continue;
                        }
                    };
                let pending_permit = match server.try_acquire_pending_request() {
                    Ok(permit) => permit,
                    Err(limit) => {
                        drop(route_admission.pending_permit);
                        cleanup_buddy_request_block(&conn, payload);
                        write_server_pending_capacity_error(&writer, request_id, limit).await;
                        continue;
                    }
                };
                let srv = Arc::clone(&server);
                let cn = Arc::clone(&conn);
                let wr = Arc::clone(&writer);
                let pl = payload.to_vec();
                let route = route_admission.route;
                let route_pending_permit = route_admission.pending_permit;
                let method_idx = ctrl.method_idx;
                tokio::spawn(async move {
                    dispatch_admitted_buddy_call(
                        &srv,
                        &cn,
                        request_id,
                        &pl,
                        ctrl_consumed,
                        route,
                        method_idx,
                        &wr,
                        pending_permit,
                        route_pending_permit,
                    )
                    .await;
                });
                continue;
            }

            let (ctrl, ctrl_consumed) = match decode_call_control(payload, 0) {
                Ok(v) => v,
                Err(e) => {
                    warn!(conn_id = conn.conn_id(), ?e, "call control decode error");
                    continue;
                }
            };
            let route_admission =
                match reserve_route_execution(&server, &ctrl.route_name, ctrl.method_idx).await {
                    Ok(admission) => admission,
                    Err(err) => {
                        write_route_admission_error(&writer, request_id, err).await;
                        continue;
                    }
                };
            let pending_permit = match server.try_acquire_pending_request() {
                Ok(permit) => permit,
                Err(limit) => {
                    drop(route_admission.pending_permit);
                    write_server_pending_capacity_error(&writer, request_id, limit).await;
                    continue;
                }
            };
            let srv = Arc::clone(&server);
            let cn = Arc::clone(&conn);
            let wr = Arc::clone(&writer);
            let pl = payload.to_vec();
            let route = route_admission.route;
            let route_pending_permit = route_admission.pending_permit;
            let method_idx = ctrl.method_idx;
            tokio::spawn(async move {
                dispatch_admitted_call(
                    &srv,
                    &cn,
                    request_id,
                    &pl,
                    ctrl_consumed,
                    route,
                    method_idx,
                    &wr,
                    pending_permit,
                    route_pending_permit,
                )
                .await;
            });
        } else {
            warn!(conn_id, flags, "unknown frame type");
        }
    }

    debug!(conn_id, "connection closing, draining in-flight");
    hb_handle.abort();
    conn.cancel_queued_work();
    conn.wait_idle().await;
    server.cleanup_chunk_requests_for_connection(conn_id);
    server.unregister_active_connection(conn_id);
    debug!(conn_id, "connection closed");
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

async fn handle_handshake(
    server: &Server,
    conn: &Connection,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> Result<(), ServerError> {
    let client_hs = decode_handshake(payload)
        .map_err(|e| ServerError::Protocol(format!("handshake decode: {e:?}")))?;

    // Store client SHM metadata for buddy frame resolution.
    conn.init_peer_shm(client_hs.prefix, client_hs.segments);
    conn.set_handshake_done(true);

    if client_hs.capability_flags & CAP_CHUNKED != 0 {
        conn.set_chunked_capable(true);
    }

    // Collect response pool segment info for handshake.
    let (server_segments, server_prefix) = {
        let pool = server.response_pool.read();
        let count = pool.segment_count();
        let mut segs = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(name) = pool.segment_name(i) {
                if let Some(seg) = pool.segment(i) {
                    segs.push((name.to_string(), seg.allocator().data_size() as u32));
                }
            }
        }
        let prefix = pool.prefix().to_string();
        (segs, prefix)
    };

    let dispatcher = server.dispatcher.read().await;
    let routes: Vec<RouteInfo> = dispatcher
        .routes_snapshot()
        .iter()
        .map(|r| RouteInfo {
            name: r.name.clone(),
            crm_ns: r.crm_ns.clone(),
            crm_name: r.crm_name.clone(),
            crm_ver: r.crm_ver.clone(),
            abi_hash: r.abi_hash.clone(),
            signature_hash: r.signature_hash.clone(),
            max_payload_size: server.config.max_payload_size,
            methods: r
                .method_names
                .iter()
                .enumerate()
                .map(|(i, n)| MethodEntry {
                    name: n.clone(),
                    index: i as u16,
                })
                .collect(),
        })
        .collect();
    drop(dispatcher);

    let cap = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED;
    let hs_bytes = encode_server_handshake(
        &server_segments,
        cap,
        &routes,
        &server_prefix,
        server.identity(),
    )
    .map_err(|e| ServerError::Protocol(e.to_string()))?;
    let frame = encode_frame(request_id, FLAG_HANDSHAKE | FLAG_RESPONSE, &hs_bytes);

    writer.lock().await.write_all(&frame).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

async fn handle_shutdown_signal(
    server: &Server,
    conn: &Connection,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) {
    if decode_shutdown_initiate(payload).is_err() {
        let outcome = DirectShutdownAck {
            acknowledged: false,
            shutdown_started: false,
            server_stopped: false,
            route_outcomes: Vec::new(),
        };
        let payload = encode_shutdown_ack(&outcome)
            .expect("shutdown control outcome serialization should not fail");
        let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, &payload);
        let _ = writer.lock().await.write_all(&frame).await;
        return;
    }
    server.unregister_active_connection(conn.conn_id());
    server.request_shutdown_signal();
    let outcome = DirectShutdownAck {
        acknowledged: true,
        shutdown_started: true,
        server_stopped: false,
        route_outcomes: Vec::new(),
    };
    let payload = encode_shutdown_ack(&outcome)
        .expect("shutdown control outcome serialization should not fail");
    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
}

async fn handle_signal(
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> SignalAction {
    let sig = match payload.first().and_then(|&b| MsgType::from_byte(b)) {
        Some(s) => s,
        None => return SignalAction::Continue,
    };

    let (reply, action) = match sig {
        MsgType::Ping => (&PONG_BYTES[..], SignalAction::Continue),
        MsgType::Disconnect => (&DISCONNECT_ACK_BYTES[..], SignalAction::Disconnect),
        _ => return SignalAction::Continue,
    };

    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, reply);
    let _ = writer.lock().await.write_all(&frame).await;
    action
}

async fn handle_ctrl(
    server: &Server,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) {
    let Some(msg_type) = payload.first().and_then(|&b| MsgType::from_byte(b)) else {
        return;
    };
    if msg_type != MsgType::PendingRouteAttest {
        debug!(request_id, "unknown ctrl frame ignored");
        return;
    }

    let response = match decode_pending_route_attestation_request(payload) {
        Ok(request) => {
            match server.attest_pending_route(&request.route_name, &request.registration_token) {
                Ok(info) => PendingRouteAttestationResponse::Attested {
                    contract: PendingRouteAttestation {
                        route_name: info.contract.route_name,
                        crm_ns: info.contract.crm_ns,
                        crm_name: info.contract.crm_name,
                        crm_ver: info.contract.crm_ver,
                        abi_hash: info.contract.abi_hash,
                        signature_hash: info.contract.signature_hash,
                        method_names: info.method_names,
                        max_payload_size: info.max_payload_size,
                    },
                },
                Err(response) => response,
            }
        }
        Err(err) => PendingRouteAttestationResponse::Rejected {
            code: PENDING_ROUTE_REJECT_INVALID.to_string(),
            message: err,
        },
    };

    match encode_pending_route_attestation_response(&response) {
        Ok(payload) => write_ctrl_response(writer, request_id, &payload).await,
        Err(err) => {
            let fallback = PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_INVALID.to_string(),
                message: err,
            };
            if let Ok(payload) = encode_pending_route_attestation_response(&fallback) {
                write_ctrl_response(writer, request_id, &payload).await;
            }
        }
    }
}

#[derive(Debug)]
enum RouteExecutionError {
    Acquire(SchedulerAcquireError),
    Crm(CrmError),
}

struct RouteExecutionAdmission {
    route: Arc<CrmRoute>,
    pending_permit: SchedulerPendingPermit,
}

struct ChunkRouteAdmission {
    route: Arc<CrmRoute>,
    method_idx: u16,
    pending_permit: SchedulerPendingPermit,
}

enum RouteAdmissionError {
    RouteNotFound(String),
    UnknownMethod { route_name: String, method_idx: u16 },
    Acquire(SchedulerAcquireError),
}

async fn execute_route_request<F>(
    execution_scheduler: Scheduler,
    route_pending_permit: SchedulerPendingPermit,
    conn: &Connection,
    request: RequestData,
    f: F,
) -> Result<ResponseMeta, RouteExecutionError>
where
    F: FnOnce(RequestData) -> Result<ResponseMeta, CrmError> + Send + 'static,
{
    let route_guard = tokio::select! {
        result = route_pending_permit.async_activate() => match result {
            Ok(guard) => guard,
            Err(err) => {
                cleanup_request(request);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        _ = conn.wait_cancelled() => {
            cleanup_request(request);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    let execution_guard = tokio::select! {
        result = execution_scheduler.async_acquire(0) => match result {
            Ok(guard) => guard,
            Err(err) => {
                cleanup_request(request);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        _ = route_guard.wait_closed() => {
            cleanup_request(request);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        },
        _ = conn.wait_cancelled() => {
            cleanup_request(request);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    if route_guard.is_closed() || conn.is_cancelled() {
        cleanup_request(request);
        return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
    }

    tokio::task::spawn_blocking(move || {
        let _route_guard = route_guard;
        let _execution_guard = execution_guard;
        f(request).map_err(RouteExecutionError::Crm)
    })
    .await
    .expect("route execution task panicked")
}

async fn reserve_route_execution(
    server: &Server,
    route_name: &str,
    method_idx: u16,
) -> Result<RouteExecutionAdmission, RouteAdmissionError> {
    let route = match server.dispatcher.read().await.resolve(route_name) {
        Some(route) => route,
        None => return Err(RouteAdmissionError::RouteNotFound(route_name.to_string())),
    };
    if !route.has_method_index(method_idx) {
        return Err(RouteAdmissionError::UnknownMethod {
            route_name: route.name.clone(),
            method_idx,
        });
    }
    let pending_permit = route
        .scheduler
        .reserve_pending(method_idx)
        .map_err(RouteAdmissionError::Acquire)?;
    Ok(RouteExecutionAdmission {
        route,
        pending_permit,
    })
}

async fn send_route_execution_result(
    server: &Server,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    result: Result<ResponseMeta, RouteExecutionError>,
) {
    match result {
        Ok(meta) => {
            if let Err(err) = send_response_meta(
                &server.response_pool,
                writer,
                request_id,
                meta,
                server.config.shm_threshold,
                server.config.chunk_size as usize,
                server.config.max_payload_size,
            )
            .await
            {
                match err {
                    ResponseSendError::UserVisible(message) => {
                        write_reply(
                            writer,
                            request_id,
                            &ReplyControl::Error(error_wire(
                                ErrorCode::ResourceOutputSerializing,
                                message,
                            )),
                        )
                        .await;
                    }
                    ResponseSendError::Transport(message) => {
                        warn!(request_id, error = %message, "response send failed");
                    }
                }
            }
        }
        Err(RouteExecutionError::Crm(CrmError::UserError(b))) => {
            write_reply(writer, request_id, &ReplyControl::Error(b)).await;
        }
        Err(RouteExecutionError::Crm(CrmError::InternalError(s))) => {
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceFunctionExecuting, s)),
            )
            .await;
        }
        Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed)) => {
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, "route closed")),
            )
            .await;
        }
        Err(RouteExecutionError::Acquire(SchedulerAcquireError::Capacity { field, limit })) => {
            let msg = format!("route concurrency capacity exceeded: {field}={limit}");
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
            )
            .await;
        }
    }
}

async fn write_route_admission_error(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    err: RouteAdmissionError,
) {
    match err {
        RouteAdmissionError::RouteNotFound(route_name) => {
            write_reply(writer, request_id, &ReplyControl::RouteNotFound(route_name)).await;
        }
        RouteAdmissionError::UnknownMethod {
            route_name,
            method_idx,
        } => {
            write_unknown_method_index(writer, request_id, &route_name, method_idx).await;
        }
        RouteAdmissionError::Acquire(SchedulerAcquireError::Closed) => {
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, "route closed")),
            )
            .await;
        }
        RouteAdmissionError::Acquire(SchedulerAcquireError::Capacity { field, limit }) => {
            let msg = format!("route concurrency capacity exceeded: {field}={limit}");
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
            )
            .await;
        }
    }
}

async fn write_unknown_method_index(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    route_name: &str,
    method_idx: u16,
) {
    let message = format!("unknown method index {method_idx} for route {route_name}");
    write_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceFunctionExecuting, message)),
    )
    .await;
}

// ---------------------------------------------------------------------------
// CRM call dispatch (inline, non-buddy, non-chunked)
// ---------------------------------------------------------------------------

async fn dispatch_admitted_call(
    server: &Server,
    conn: &Connection,
    request_id: u64,
    payload: &[u8],
    control_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    _pending_permit: OwnedSemaphorePermit,
    route_pending_permit: SchedulerPendingPermit,
) {
    let _flight = crate::connection::FlightGuard::new(conn);

    let callback = Arc::clone(&route.callback);
    let name = route.name.clone();
    let args = payload[control_consumed..].to_vec();
    let resp_pool = Arc::clone(&server.response_pool);

    let execution_scheduler = server.execution_scheduler.clone();
    let request = RequestData::Inline(args);
    let result = execute_route_request(
        execution_scheduler,
        route_pending_permit,
        conn,
        request,
        move |request| callback.invoke(&name, method_idx, request, resp_pool),
    )
    .await;

    send_route_execution_result(server, writer, request_id, result).await;
}

#[cfg(test)]
async fn dispatch_call(
    server: &Server,
    conn: &Connection,
    request_id: u64,
    payload: &[u8],
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    pending_permit: OwnedSemaphorePermit,
) {
    let (ctrl, consumed) = match decode_call_control(payload, 0) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "call control decode error");
            return;
        }
    };

    let admission = match reserve_route_execution(server, &ctrl.route_name, ctrl.method_idx).await {
        Ok(admission) => admission,
        Err(err) => {
            write_route_admission_error(writer, request_id, err).await;
            return;
        }
    };

    dispatch_admitted_call(
        server,
        conn,
        request_id,
        payload,
        consumed,
        admission.route,
        ctrl.method_idx,
        writer,
        pending_permit,
        admission.pending_permit,
    )
    .await;
}

// ---------------------------------------------------------------------------
// CRM call dispatch — buddy SHM
// ---------------------------------------------------------------------------

fn schedule_peer_buddy_gc_if_idle(conn: &Arc<Connection>, free_result: c2_mem::FreeResult) {
    if let c2_mem::FreeResult::SegmentIdle { .. } = free_result {
        let conn2 = Arc::clone(conn);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            conn2.gc_peer_buddy();
        });
    }
}

fn cleanup_buddy_request_block(conn: &Arc<Connection>, payload: &[u8]) {
    let (bp, _) = match decode_buddy_payload(payload) {
        Ok(decoded) => decoded,
        Err(e) => {
            warn!(
                conn_id = conn.conn_id(),
                ?e,
                "buddy request cleanup decode error"
            );
            return;
        }
    };
    let free_result = conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
    schedule_peer_buddy_gc_if_idle(conn, free_result);
}

async fn dispatch_admitted_buddy_call(
    server: &Server,
    conn: &Arc<Connection>,
    request_id: u64,
    payload: &[u8],
    ctrl_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    _pending_permit: OwnedSemaphorePermit,
    route_pending_permit: SchedulerPendingPermit,
) {
    let _flight = crate::connection::FlightGuard::new(conn.as_ref());

    // 1. Decode buddy pointer (11 bytes).
    let (bp, _bp_consumed) = match decode_buddy_payload(payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "buddy payload decode error");
            return;
        }
    };

    // 2. Ensure peer SHM segment is mapped and get pool Arc (single lock).
    let peer_pool = match conn.ensure_and_get_peer_pool(bp.seg_idx, bp.data_size, bp.is_dedicated) {
        Ok(p) => p,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), %e, "ensure_and_get_peer_pool failed");
            let msg = format!("buddy SHM segment open: {e}");
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceInputDeserializing, msg)),
            )
            .await;
            return;
        }
    };

    // 3. Check for extra inline args appended after control header.
    let inline_start = BUDDY_PAYLOAD_SIZE + ctrl_consumed;
    let extra_args = if inline_start < payload.len() {
        &payload[inline_start..]
    } else {
        &[]
    };

    // 4. Build request — zero-copy SHM or fallback to inline if extra args present.
    let request = if extra_args.is_empty() {
        RequestData::Shm {
            pool: peer_pool,
            seg_idx: bp.seg_idx,
            offset: bp.offset,
            data_size: bp.data_size,
            is_dedicated: bp.is_dedicated,
        }
    } else {
        // Rare edge case: extra inline args after buddy payload — fall back to copy.
        warn!(
            conn_id = conn.conn_id(),
            extra_len = extra_args.len(),
            "buddy call has trailing inline args, falling back to copy"
        );
        let args = match conn.read_peer_data(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated) {
            Ok(data) => data,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "buddy SHM read failed (fallback)");
                let msg = format!("buddy SHM read: {e}");
                write_reply(
                    writer,
                    request_id,
                    &ReplyControl::Error(error_wire(ErrorCode::ResourceInputDeserializing, msg)),
                )
                .await;
                return;
            }
        };
        let free_result =
            conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
        schedule_peer_buddy_gc_if_idle(conn, free_result);
        let mut combined = args;
        combined.extend_from_slice(extra_args);
        RequestData::Inline(combined)
    };

    let callback = Arc::clone(&route.callback);
    let name = route.name.clone();
    let resp_pool = Arc::clone(&server.response_pool);

    let execution_scheduler = server.execution_scheduler.clone();
    let result = execute_route_request(
        execution_scheduler,
        route_pending_permit,
        conn,
        request,
        move |request| callback.invoke(&name, method_idx, request, resp_pool),
    )
    .await;

    send_route_execution_result(server, writer, request_id, result).await;
}

// ---------------------------------------------------------------------------
// CRM call dispatch — chunked reassembly
// ---------------------------------------------------------------------------

async fn dispatch_chunked_call(
    server: &Server,
    conn: &Arc<Connection>,
    request_id: u64,
    flags: u32,
    payload: &[u8],
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    chunk_processing_permit: OwnedSemaphorePermit,
) {
    let is_buddy = c2_wire::flags::is_buddy(flags);
    let mut offset: usize = 0;

    // 1. If buddy-backed chunk, read data from SHM first.
    // NOTE: Per-chunk data must be copied into the reassembly buffer — zero-copy
    // is applied only to the final assembled result (RequestData::Handle above).
    let shm_data: Option<Vec<u8>>;
    if is_buddy {
        let (bp, bp_consumed) = match decode_buddy_payload(payload) {
            Ok(v) => v,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), ?e, "chunked buddy decode error");
                server.abort_chunk_request(conn.conn_id(), request_id);
                return;
            }
        };
        offset = bp_consumed;
        match conn.read_peer_data(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated) {
            Ok(data) => {
                let free_result =
                    conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
                schedule_peer_buddy_gc_if_idle(conn, free_result);
                shm_data = Some(data);
            }
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunked SHM read failed");
                server.abort_chunk_request(conn.conn_id(), request_id);
                return;
            }
        }
    } else {
        shm_data = None;
    }

    // 2. Decode chunk header.
    let (chunk_idx, total_chunks, ch_consumed) = match decode_chunk_header(payload, offset) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "chunk header decode error");
            server.abort_chunk_request(conn.conn_id(), request_id);
            return;
        }
    };
    offset += ch_consumed;

    // 3. On first chunk, decode call control and register with ChunkRegistry.
    if chunk_idx == 0 {
        let (ctrl, ctrl_consumed) = match decode_call_control(payload, offset) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    conn_id = conn.conn_id(),
                    ?e,
                    "chunked call control decode error"
                );
                return;
            }
        };
        let route_admission =
            match reserve_route_execution(server, &ctrl.route_name, ctrl.method_idx).await {
                Ok(admission) => admission,
                Err(err) => {
                    write_route_admission_error(writer, request_id, err).await;
                    return;
                }
            };
        offset += ctrl_consumed;

        // Determine chunk_size from this first chunk's data length.
        let first_data = if let Some(ref sd) = shm_data {
            sd.as_slice()
        } else {
            &payload[offset..]
        };
        let chunk_size = first_data.len();
        if chunk_size == 0 {
            warn!(
                conn_id = conn.conn_id(),
                "chunked: first chunk has zero data"
            );
            return;
        }

        if let Err(e) = server.chunk_registry.insert(
            conn.conn_id(),
            request_id,
            total_chunks as usize,
            chunk_size,
        ) {
            warn!(conn_id = conn.conn_id(), %e, "chunk insert failed");
            return;
        }
        server.chunk_registry.set_route_info(
            conn.conn_id(),
            request_id,
            ctrl.route_name,
            ctrl.method_idx,
        );
        if server
            .store_chunk_route_pending(
                conn.conn_id(),
                request_id,
                ChunkRouteAdmission {
                    route: route_admission.route,
                    method_idx: ctrl.method_idx,
                    pending_permit: route_admission.pending_permit,
                },
            )
            .is_err()
        {
            server.chunk_registry.abort(conn.conn_id(), request_id);
            warn!(
                conn_id = conn.conn_id(),
                request_id, "chunk route pending permit unexpectedly duplicated"
            );
            return;
        }
    }

    // 4. Get chunk data.
    let chunk_data: &[u8] = if let Some(ref sd) = shm_data {
        sd.as_slice()
    } else {
        &payload[offset..]
    };

    // 5. Feed chunk to registry.
    let complete =
        match server
            .chunk_registry
            .feed(conn.conn_id(), request_id, chunk_idx as usize, chunk_data)
        {
            Ok(complete) => complete,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunk feed error");
                server.abort_chunk_request(conn.conn_id(), request_id);
                return;
            }
        };

    // 6. If complete, finish and dispatch.
    if complete {
        let chunk_admission = match server.take_chunk_route_pending(conn.conn_id(), request_id) {
            Some(admission) => admission,
            None => {
                server.abort_chunk_request(conn.conn_id(), request_id);
                warn!(
                    conn_id = conn.conn_id(),
                    request_id, "chunked call missing stored route admission"
                );
                write_reply(
                    writer,
                    request_id,
                    &ReplyControl::Error(error_wire(
                        ErrorCode::ResourceUnavailable,
                        "chunked call missing route admission",
                    )),
                )
                .await;
                return;
            }
        };
        let finished = match server.chunk_registry.finish(conn.conn_id(), request_id) {
            Ok(f) => f,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunk finish failed");
                return;
            }
        };
        let c2_wire::chunk::FinishedChunk {
            handle,
            route_name,
            method_idx,
        } = finished;
        let pool_arc = server.chunk_registry.pool().clone();
        let request = RequestData::Handle {
            handle,
            pool: pool_arc,
        };
        let _pending_permit = match server.try_acquire_pending_request() {
            Ok(permit) => permit,
            Err(limit) => {
                cleanup_request(request);
                write_server_pending_capacity_error(writer, request_id, limit).await;
                return;
            }
        };
        drop(chunk_processing_permit);
        let (route_name, method_idx) = match (route_name, method_idx) {
            (Some(route_name), Some(method_idx)) => (route_name, method_idx),
            (route_name, method_idx) => {
                cleanup_request(request);
                warn!(
                    conn_id = conn.conn_id(),
                    request_id,
                    has_route_name = route_name.is_some(),
                    has_method_idx = method_idx.is_some(),
                    "chunked call missing route metadata"
                );
                write_reply(
                    writer,
                    request_id,
                    &ReplyControl::Error(error_wire(
                        ErrorCode::ResourceInputDeserializing,
                        "chunked call missing route metadata",
                    )),
                )
                .await;
                return;
            }
        };
        if route_name != chunk_admission.route.name || method_idx != chunk_admission.method_idx {
            cleanup_request(request);
            warn!(
                conn_id = conn.conn_id(),
                request_id,
                expected_route = chunk_admission.route.name,
                expected_method_idx = chunk_admission.method_idx,
                actual_route = route_name,
                actual_method_idx = method_idx,
                "chunked call route metadata drifted from stored admission"
            );
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(
                    ErrorCode::ResourceInputDeserializing,
                    "chunked call route metadata drifted from admission",
                )),
            )
            .await;
            return;
        }

        // FlightGuard: increments on create, decrements on drop.
        let _flight = crate::connection::FlightGuard::new(conn);

        let callback = Arc::clone(&chunk_admission.route.callback);
        let name = chunk_admission.route.name.clone();
        let resp_pool = Arc::clone(&server.response_pool);
        let execution_scheduler = server.execution_scheduler.clone();
        let result = execute_route_request(
            execution_scheduler,
            chunk_admission.pending_permit,
            conn,
            request,
            move |request| callback.invoke(&name, method_idx, request, resp_pool),
        )
        .await;

        send_route_execution_result(server, writer, request_id, result).await;
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

const REPLY_FLAGS: u32 = FLAG_RESPONSE | FLAG_REPLY_V2;

fn checked_frame_total_len(payload_len: usize, context: &str) -> Result<u32, String> {
    let total_len = 12usize
        .checked_add(payload_len)
        .ok_or_else(|| format!("{context} length overflow"))?;
    u32::try_from(total_len)
        .map_err(|_| format!("{context} length {total_len} exceeds u32 frame limit"))
}

fn inline_reply_total_len(data_len: usize) -> Result<u32, String> {
    let ctrl_len = try_encode_reply_control(&ReplyControl::Success)
        .map_err(|err| err.to_string())?
        .len();
    let payload_len = ctrl_len
        .checked_add(data_len)
        .ok_or_else(|| "inline reply frame length overflow".to_string())?;
    checked_frame_total_len(payload_len, "inline reply frame")
}

fn reply_chunk_count(data_len: usize, chunk_size: usize) -> Result<u32, String> {
    if chunk_size == 0 {
        return Err("chunk_size must be > 0".to_string());
    }
    if data_len == 0 {
        return Ok(0);
    }
    let chunks = data_len.div_ceil(chunk_size);
    u32::try_from(chunks).map_err(|_| {
        format!(
            "chunk count {chunks} exceeds reply chunk metadata limit {}",
            u32::MAX
        )
    })
}

fn ensure_response_meta_len(
    data_len: usize,
    max_payload_size: u64,
) -> Result<(), ResponseSendError> {
    let data_len_u64 = u64::try_from(data_len).unwrap_or(u64::MAX);
    if data_len_u64 > max_payload_size {
        return Err(ResponseSendError::UserVisible(format!(
            "response payload size {data_len} exceeds max_payload_size {max_payload_size}"
        )));
    }
    Ok(())
}

#[derive(Debug)]
enum BuddyReplyError {
    Fallback(String),
    Fatal(String),
}

impl std::fmt::Display for BuddyReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fallback(message) | Self::Fatal(message) => f.write_str(message),
        }
    }
}

#[derive(Debug)]
enum ResponseSendError {
    UserVisible(String),
    Transport(String),
}

impl std::fmt::Display for ResponseSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserVisible(message) | Self::Transport(message) => f.write_str(message),
        }
    }
}

async fn write_server_pending_capacity_error(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    limit: u32,
) {
    let msg = format!("server execution capacity exceeded: max_pending_requests={limit}");
    write_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
    )
    .await;
}

async fn write_chunk_processing_capacity_error(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    limit: u32,
) {
    let msg = format!("server chunk processing capacity exceeded: max_total_chunks={limit}");
    write_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
    )
    .await;
}

async fn write_reply(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, ctrl: &ReplyControl) {
    let payload = match try_encode_reply_control(ctrl) {
        Ok(payload) => payload,
        Err(err) => {
            let fallback = ReplyControl::Error(error_wire(
                ErrorCode::ResourceFunctionExecuting,
                err.to_string(),
            ));
            match try_encode_reply_control(&fallback) {
                Ok(payload) => payload,
                Err(fallback_err) => {
                    warn!(
                        request_id,
                        error = %fallback_err,
                        "failed to encode fallback error reply"
                    );
                    return;
                }
            }
        }
    };
    let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
    if let Err(err) = writer.lock().await.write_all(&frame).await {
        warn!(request_id, error = %err, "failed to write reply frame");
    }
}

async fn write_ctrl_response(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, payload: &[u8]) {
    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_CTRL, payload);
    if let Err(err) = writer.lock().await.write_all(&frame).await {
        warn!(request_id, error = %err, "failed to write ctrl response frame");
    }
}

/// Write a success reply: control header (STATUS_SUCCESS) + result data.
/// Uses stack buffer for small responses (≤1024B total frame) to avoid heap allocation.
async fn write_reply_with_data(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
) -> Result<(), ResponseSendError> {
    let ctrl_bytes = try_encode_reply_control(&ReplyControl::Success)
        .map_err(|err| ResponseSendError::UserVisible(err.to_string()))?;
    let payload_len = ctrl_bytes.len().checked_add(data.len()).ok_or_else(|| {
        ResponseSendError::UserVisible("inline reply frame length overflow".to_string())
    })?;
    let total_len = inline_reply_total_len(data.len()).map_err(ResponseSendError::UserVisible)?;
    let frame_size = frame::HEADER_SIZE + payload_len;

    if frame_size <= 1024 {
        // Stack buffer: single write_all, zero heap allocation
        let mut buf = [0u8; 1024];
        buf[0..4].copy_from_slice(&total_len.to_le_bytes());
        buf[4..12].copy_from_slice(&request_id.to_le_bytes());
        buf[12..16].copy_from_slice(&REPLY_FLAGS.to_le_bytes());
        let mut off = frame::HEADER_SIZE;
        buf[off..off + ctrl_bytes.len()].copy_from_slice(&ctrl_bytes);
        off += ctrl_bytes.len();
        buf[off..off + data.len()].copy_from_slice(data);
        off += data.len();
        writer
            .lock()
            .await
            .write_all(&buf[..off])
            .await
            .map_err(|e| ResponseSendError::Transport(format!("inline reply write failed: {e}")))?;
    } else {
        // Large response: heap Vec (existing path)
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&ctrl_bytes);
        payload.extend_from_slice(data);
        let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
        writer
            .lock()
            .await
            .write_all(&frame)
            .await
            .map_err(|e| ResponseSendError::Transport(format!("inline reply write failed: {e}")))?;
    }
    Ok(())
}

/// Write a success reply via buddy SHM: allocate from response pool, write
/// data, send 11-byte pointer frame. The caller chooses inline or chunked
/// fallback when SHM is unavailable.
async fn write_buddy_reply_with_data(
    response_pool: &parking_lot::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
) -> Result<(), BuddyReplyError> {
    let data_size = buddy_response_data_size(data.len()).ok_or_else(|| {
        BuddyReplyError::Fallback(format!(
            "response payload size {} exceeds buddy response wire limit {}",
            data.len(),
            u32::MAX
        ))
    })?;

    // 1. Allocate from response pool.
    let alloc = {
        let mut pool = response_pool.write();
        pool.alloc(data.len())
    };
    let alloc = match alloc {
        Ok(a) => a,
        Err(e) => {
            return Err(BuddyReplyError::Fallback(format!("alloc failed: {e}")));
        }
    };

    // 2. Write data to SHM (single lock scope).
    let write_ok = {
        let pool = response_pool.read();
        match pool.data_ptr(&alloc) {
            Ok(ptr) => {
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                }
                true
            }
            Err(_) => false,
        }
    };
    if !write_ok {
        {
            let mut pool = response_pool.write();
            let _ = pool.free(&alloc);
        }
        return Err(BuddyReplyError::Fallback("data_ptr failed".into()));
    }

    // 3. Encode buddy payload + reply control.
    let bp = BuddyPayload {
        seg_idx: alloc.seg_idx as u16,
        offset: alloc.offset,
        data_size,
        is_dedicated: alloc.is_dedicated,
    };
    let buddy_bytes = encode_buddy_payload(&bp);
    let ctrl_bytes = try_encode_reply_control(&ReplyControl::Success)
        .map_err(|err| BuddyReplyError::Fatal(err.to_string()))?;

    let mut payload = Vec::with_capacity(BUDDY_PAYLOAD_SIZE + ctrl_bytes.len());
    payload.extend_from_slice(&buddy_bytes);
    payload.extend_from_slice(&ctrl_bytes);

    // 4. Send frame with FLAG_BUDDY.
    let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY;
    let frame = encode_frame(request_id, flags, &payload);
    if let Err(err) = writer.lock().await.write_all(&frame).await {
        let mut pool = response_pool.write();
        let _ = pool.free(&alloc);
        return Err(BuddyReplyError::Fatal(format!(
            "buddy reply write failed: {err}"
        )));
    }

    // 5. Server-side free for dedicated segments: the client will lazy-open
    //    and read from SHM before gc_delay expires.  Buddy allocs use SHM
    //    atomics so the client's free_at is already cross-process visible.
    if alloc.is_dedicated {
        let mut pool = response_pool.write();
        let _ = pool.free(&alloc);
    }

    Ok(())
}

/// Write a success reply via chunked transfer: split data into chunks,
/// each with reply chunk meta header. Uses FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_CHUNKED.
/// Last chunk also sets FLAG_CHUNK_LAST.
async fn write_chunked_reply(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    chunk_size: usize,
) -> Result<(), ResponseSendError> {
    let total_chunks =
        reply_chunk_count(data.len(), chunk_size).map_err(ResponseSendError::UserVisible)?;
    let total_size = data.len() as u64;

    for (idx, chunk) in data.chunks(chunk_size).enumerate() {
        let chunk_idx = u32::try_from(idx).map_err(|_| {
            ResponseSendError::UserVisible(format!(
                "chunk index {idx} exceeds reply chunk metadata limit"
            ))
        })?;
        let meta = encode_reply_chunk_meta(total_size, total_chunks, chunk_idx);
        let mut flags = REPLY_FLAGS | FLAG_CHUNKED;
        if chunk_idx == total_chunks - 1 {
            flags |= FLAG_CHUNK_LAST;
        }

        // Build frame: header + meta + chunk data
        let payload_len = REPLY_CHUNK_META_SIZE + chunk.len();
        let total_len = checked_frame_total_len(payload_len, "chunked reply frame")
            .map_err(ResponseSendError::UserVisible)?;

        let mut frame = Vec::with_capacity(frame::HEADER_SIZE + payload_len);
        frame.extend_from_slice(&total_len.to_le_bytes());
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&flags.to_le_bytes());
        frame.extend_from_slice(&meta);
        frame.extend_from_slice(chunk);

        writer.lock().await.write_all(&frame).await.map_err(|e| {
            ResponseSendError::Transport(format!("chunked reply write failed: {e}"))
        })?;
    }
    Ok(())
}

/// Dispatch a `ResponseMeta` to the appropriate reply path.
async fn send_response_meta(
    response_pool: &parking_lot::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    meta: ResponseMeta,
    shm_threshold: u64,
    chunk_size: usize,
    max_payload_size: u64,
) -> Result<(), ResponseSendError> {
    match meta {
        ResponseMeta::Inline(data) => {
            ensure_response_meta_len(data.len(), max_payload_size)?;
            smart_reply_with_data(
                response_pool,
                writer,
                request_id,
                &data,
                shm_threshold,
                chunk_size,
            )
            .await?;
        }
        ResponseMeta::Empty => {
            write_reply_with_data(writer, request_id, &[]).await?;
        }
        ResponseMeta::ShmAlloc {
            seg_idx,
            offset,
            data_size,
            is_dedicated,
        } => {
            if u64::from(data_size) > max_payload_size {
                let mut pool = response_pool.write();
                let _ = pool.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                return Err(ResponseSendError::UserVisible(format!(
                    "response payload size {data_size} exceeds max_payload_size {max_payload_size}"
                )));
            }
            // CRM already wrote into our response pool — send buddy pointer.
            let bp = BuddyPayload {
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            };
            let buddy_bytes = encode_buddy_payload(&bp);
            let ctrl_bytes = try_encode_reply_control(&ReplyControl::Success)
                .map_err(|err| ResponseSendError::UserVisible(err.to_string()))?;
            let mut payload = Vec::with_capacity(BUDDY_PAYLOAD_SIZE + ctrl_bytes.len());
            payload.extend_from_slice(&buddy_bytes);
            payload.extend_from_slice(&ctrl_bytes);
            let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY;
            let frame = encode_frame(request_id, flags, &payload);
            if let Err(err) = writer.lock().await.write_all(&frame).await {
                let mut pool = response_pool.write();
                let _ = pool.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                return Err(ResponseSendError::Transport(format!(
                    "prepared SHM reply write failed: {err}"
                )));
            }

            // Server-side free for dedicated segments (same as write_buddy_reply_with_data).
            if is_dedicated {
                let mut pool = response_pool.write();
                let _ = pool.free_at(seg_idx as u32, offset, data_size, true);
            }
        }
    }
    Ok(())
}

/// Choose buddy SHM or inline reply based on data size and threshold.
async fn smart_reply_with_data(
    response_pool: &parking_lot::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    shm_threshold: u64,
    chunk_size: usize,
) -> Result<(), ResponseSendError> {
    if data.len() as u64 > shm_threshold {
        // Try buddy SHM first only when the buddy wire metadata can represent
        // the payload. Larger responses must go straight to chunked fallback.
        if buddy_response_data_size(data.len()).is_some() {
            match write_buddy_reply_with_data(response_pool, writer, request_id, data).await {
                Ok(()) => return Ok(()),
                Err(BuddyReplyError::Fallback(_)) => {}
                Err(BuddyReplyError::Fatal(message)) => {
                    return Err(ResponseSendError::Transport(message));
                }
            }
        }

        // SHM failed or is not representable. Use chunked for large data and
        // for any data that cannot fit in a single inline frame.
        if data.len() > chunk_size || inline_reply_total_len(data.len()).is_err() {
            return write_chunked_reply(writer, request_id, data, chunk_size).await;
        }
    }

    if inline_reply_total_len(data.len()).is_err() {
        return write_chunked_reply(writer, request_id, data, chunk_size).await;
    }
    write_reply_with_data(writer, request_id, data).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::dispatcher::{CrmCallback, CrmError, CrmRoute};
    use crate::scheduler::{ConcurrencyMode, Scheduler};

    // -- address parsing --

    #[test]
    fn parse_ipc_address() {
        let p = parse_socket_path("ipc://my_region").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/c_two_ipc/my_region.sock"));
    }

    #[test]
    fn parse_legacy_v3_rejected() {
        assert!(parse_socket_path("ipc-v3://region42").is_err());
    }

    #[test]
    fn parse_invalid_scheme() {
        assert!(parse_socket_path("tcp://host").is_err());
    }

    #[test]
    fn parse_empty_region() {
        assert!(parse_socket_path("ipc://").is_err());
    }

    #[test]
    fn parse_rejects_path_like_region() {
        for address in [
            "ipc://../escape",
            "ipc://bad/name",
            "ipc://bad\\name",
            "ipc://.",
            "ipc://..",
            "ipc:// leading",
            "ipc://trailing ",
            "ipc://bad\nname",
        ] {
            assert!(
                parse_socket_path(address).is_err(),
                "address should be rejected: {address:?}"
            );
        }
    }

    // -- server construction --

    #[test]
    fn server_new_default_config() {
        let s = Server::new("ipc://test_srv", ServerIpcConfig::default()).unwrap();
        assert_eq!(s.socket_path(), Path::new("/tmp/c_two_ipc/test_srv.sock"));
    }

    #[test]
    fn server_new_derives_stable_server_id_and_instance_identity() {
        let first = Server::new("ipc://identity_srv", ServerIpcConfig::default()).unwrap();
        let second = Server::new("ipc://identity_srv", ServerIpcConfig::default()).unwrap();

        assert_eq!(first.server_id(), "identity_srv");
        assert_eq!(first.identity().server_id, "identity_srv");
        assert_eq!(first.server_instance_id().len(), 32);
        assert_ne!(first.server_instance_id(), second.server_instance_id());
    }

    #[test]
    fn server_new_with_identity_uses_validated_identity() {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "server-explicit".to_string(),
            server_instance_id: "instance-explicit".to_string(),
        };

        let server = Server::new_with_identity(
            "ipc://identity_explicit",
            ServerIpcConfig::default(),
            identity.clone(),
        )
        .unwrap();

        assert_eq!(server.identity(), &identity);
        assert_eq!(server.server_id(), "server-explicit");
        assert_eq!(server.server_instance_id(), "instance-explicit");
    }

    #[test]
    fn server_new_with_identity_rejects_invalid_instance_id() {
        let too_long = "a".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        for server_instance_id in ["../bad", "实例", too_long.as_str()] {
            let identity = c2_wire::handshake::ServerIdentity {
                server_id: "server-explicit".to_string(),
                server_instance_id: server_instance_id.to_string(),
            };

            let err = Server::new_with_identity(
                "ipc://identity_bad",
                ServerIpcConfig::default(),
                identity,
            )
            .err()
            .expect("invalid identity should be rejected");

            assert!(err.to_string().contains("server_instance_id"));
        }
    }

    #[test]
    fn server_new_with_identity_rejects_server_id_too_long_for_wire() {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "s".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
            server_instance_id: "instance-explicit".to_string(),
        };

        let err = Server::new_with_identity(
            "ipc://identity_bad_server_id",
            ServerIpcConfig::default(),
            identity,
        )
        .err()
        .expect("overlong server_id should be rejected");

        assert!(err.to_string().contains("server_id"));
    }

    #[test]
    fn server_new_bad_address() {
        assert!(Server::new("http://bad", ServerIpcConfig::default()).is_err());
    }

    #[test]
    fn server_new_bad_config() {
        let cfg = ServerIpcConfig {
            max_payload_size: 0,
            ..ServerIpcConfig::default()
        };
        assert!(Server::new("ipc://x", cfg).is_err());
    }

    fn unique_readiness_address(prefix: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        format!("ipc://{prefix}_{}_{}", std::process::id(), n)
    }

    fn unique_response_pool_prefix(label: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        format!("/c2sw{:04x}{:04x}", std::process::id() as u16, n as u16) + label
    }

    fn small_response_pool(label: &str) -> parking_lot::RwLock<MemPool> {
        parking_lot::RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                ..PoolConfig::default()
            },
            unique_response_pool_prefix(label),
        ))
    }

    fn closed_writer() -> Arc<Mutex<OwnedWriteHalf>> {
        let (client, server) = StdUnixStream::pair().unwrap();
        client.shutdown(std::net::Shutdown::Both).unwrap();
        server.set_nonblocking(true).unwrap();
        let server = UnixStream::from_std(server).unwrap();
        let (_read_half, write_half) = server.into_split();
        Arc::new(Mutex::new(write_half))
    }

    #[tokio::test]
    async fn wait_until_ready_times_out_before_start() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("ready_timeout"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );

        let err = server
            .wait_until_ready(Duration::from_millis(1))
            .await
            .expect_err("server that was never started must time out");

        assert!(err.to_string().contains("did not become ready"));
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Initialized);
        assert!(!server.is_ready());
        assert!(!server.is_running());
    }

    #[tokio::test]
    async fn run_sets_ready_then_shutdown_sets_stopped() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("ready_state"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };

        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
        assert!(server.is_ready());
        assert!(server.is_running());
        assert!(StdUnixStream::connect(server.socket_path()).is_ok());

        server.request_shutdown_signal();
        runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
        assert!(!server.is_ready());
        assert!(!server.is_running());
    }

    #[test]
    fn wait_until_responsive_only_returns_after_ping_round_trip() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("responsive_ready"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let runner = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(server.run())
            })
        };
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(server.wait_until_responsive(Duration::from_secs(2)))
            .expect("server should answer control ping before readiness returns");

        server.request_shutdown_signal();
        runner.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn active_socket_is_not_unlinked_by_second_server() {
        let address = unique_readiness_address("active_socket");
        let first = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let first_runner = {
            let first = Arc::clone(&first);
            tokio::spawn(async move { first.run().await })
        };
        first
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(StdUnixStream::connect(first.socket_path()).is_ok());

        let second = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let second_result = tokio::time::timeout(Duration::from_millis(200), {
            let second = Arc::clone(&second);
            async move { second.run().await }
        })
        .await;

        match second_result {
            Ok(Err(err)) => {
                let message = err.to_string();
                assert!(
                    message.contains("already has an active listener")
                        || message.contains("address already in use"),
                    "unexpected error: {message}",
                );
            }
            Ok(Ok(())) => panic!("second server unexpectedly started and stopped cleanly"),
            Err(_) => {
                second.request_shutdown_signal();
                panic!("second server hung instead of rejecting the active socket");
            }
        }

        assert!(first.is_ready());
        assert!(StdUnixStream::connect(first.socket_path()).is_ok());
        first.request_shutdown_signal();
        first_runner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_after_failed_bind_does_not_unlink_active_socket() {
        let address = unique_readiness_address("failed_bind_shutdown");
        let first = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let first_runner = {
            let first = Arc::clone(&first);
            tokio::spawn(async move { first.run().await })
        };
        first
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(StdUnixStream::connect(first.socket_path()).is_ok());

        let second = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let second_result = tokio::time::timeout(Duration::from_millis(200), {
            let second = Arc::clone(&second);
            async move { second.run().await }
        })
        .await;
        let err = second_result
            .expect("second server hung instead of rejecting the active socket")
            .expect_err("second bind must fail");
        assert!(
            err.to_string().contains("active listener")
                || err.to_string().contains("address already in use"),
            "unexpected error: {err}",
        );
        second.request_shutdown_signal();

        assert!(first.is_ready());
        assert!(StdUnixStream::connect(first.socket_path()).is_ok());
        first.request_shutdown_signal();
        first_runner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn same_server_can_start_again_after_shutdown() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("restart_after_shutdown"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );

        server.begin_start_attempt().unwrap();
        let first_runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(StdUnixStream::connect(server.socket_path()).is_ok());

        server.request_shutdown_signal();
        first_runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);

        server.begin_start_attempt().unwrap();
        let second_runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(StdUnixStream::connect(server.socket_path()).is_ok());

        server.request_shutdown_signal();
        second_runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    }

    #[tokio::test]
    async fn failed_bind_can_retry_after_socket_released() {
        let address = unique_readiness_address("retry_after_failed_bind");
        let first = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        first.begin_start_attempt().unwrap();
        let first_runner = {
            let first = Arc::clone(&first);
            tokio::spawn(async move { first.run().await })
        };
        first
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();

        let second = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        second.begin_start_attempt().unwrap();
        let failed = second
            .run()
            .await
            .expect_err("active socket should reject the first attempt");
        assert!(
            failed.to_string().contains("active listener")
                || failed.to_string().contains("address already in use"),
            "unexpected error: {failed}",
        );
        assert!(matches!(
            second.lifecycle_state(),
            ServerLifecycleState::Failed(_)
        ));

        first.request_shutdown_signal();
        first_runner.await.unwrap().unwrap();

        second.begin_start_attempt().unwrap();
        let second_runner = {
            let second = Arc::clone(&second);
            tokio::spawn(async move { second.run().await })
        };
        second
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(StdUnixStream::connect(second.socket_path()).is_ok());

        second.request_shutdown_signal();
        second_runner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn wait_until_stopped_fences_restart_after_shutdown() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("wait_stop_restart"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );

        server.begin_start_attempt().unwrap();
        let runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();

        server.request_shutdown_signal();
        server
            .wait_until_stopped(Duration::from_secs(2))
            .await
            .unwrap();
        server
            .begin_start_attempt()
            .expect("stopped lifecycle should permit a new start attempt");

        runner.await.unwrap().unwrap();
    }

    #[test]
    fn finalize_runtime_stopped_terminalizes_running_states_but_preserves_failed() {
        let server = Server::new(
            &unique_readiness_address("finalize_runtime"),
            ServerIpcConfig::default(),
        )
        .unwrap();

        server.set_lifecycle_state(ServerLifecycleState::Stopping);
        server.finalize_runtime_stopped();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);

        server.set_lifecycle_state(ServerLifecycleState::Failed("bind failed".to_string()));
        server.finalize_runtime_stopped();
        assert_eq!(
            server.lifecycle_state(),
            ServerLifecycleState::Failed("bind failed".to_string()),
        );
    }

    // -- route registration --

    struct Echo;
    impl CrmCallback for Echo {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"echo".to_vec()))
        }
    }

    fn make_route(name: &str) -> CrmRoute {
        CrmRoute {
            name: name.into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
            scheduler: Arc::new(Scheduler::new(
                ConcurrencyMode::ReadParallel,
                HashMap::new(),
            )),
            callback: Arc::new(Echo),
            method_names: vec!["step".into(), "query".into()],
        }
    }

    #[tokio::test]
    async fn register_unregister_route() {
        let s = Arc::new(Server::new("ipc://reg_test", ServerIpcConfig::default()).unwrap());

        let route = make_route("grid");
        let scheduler = route.scheduler.as_ref().clone();
        s.register_route(route).await.unwrap();
        assert!(s.dispatcher.read().await.resolve("grid").is_some());
        assert!(s.contains_route("grid").await);

        assert!(s.unregister_route("grid").await);
        assert!(s.dispatcher.read().await.resolve("grid").is_none());
        assert!(!s.contains_route("grid").await);
        assert!(scheduler.snapshot().closed);
        assert_eq!(
            scheduler.try_acquire(0).unwrap_err(),
            crate::scheduler::SchedulerAcquireError::Closed,
        );
    }

    #[tokio::test]
    async fn reserved_route_cannot_commit_after_shutdown_generation_advances() {
        let address = unique_readiness_address("stale_reservation_after_shutdown");
        let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .expect("server ready");

        let route = make_route("late");
        let route_handle = RouteConcurrencyHandle::new(route.scheduler.as_ref().clone());
        let reservation = server
            .reserve_route(BuiltRoute::new(route, route_handle))
            .await
            .unwrap();
        server.request_shutdown_signal();
        server
            .wait_until_stopped(Duration::from_secs(2))
            .await
            .expect("server stopped");

        let err = server
            .commit_reserved_route(reservation)
            .await
            .expect_err("stale pre-shutdown reservation must not commit after stop");
        assert!(
            err.to_string().contains("shutting down"),
            "unexpected error: {err}",
        );
        assert!(
            !server.contains_route("late").await,
            "stale reservation committed a route after shutdown completed",
        );

        runner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn duplicate_route_registration_is_rejected() {
        let s = Arc::new(Server::new("ipc://dup_route_test", ServerIpcConfig::default()).unwrap());

        s.register_route(make_route("grid")).await.unwrap();
        let err = s.register_route(make_route("grid")).await.unwrap_err();

        assert!(err.to_string().contains("already registered"));
    }

    #[tokio::test]
    async fn rejected_acquire_cleans_materialized_request() {
        use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
        use c2_mem::PoolConfig;
        use std::num::NonZeroUsize;

        let pool = Arc::new(parking_lot::RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 4096,
                min_block_size: 128,
                max_segments: 1,
                max_dedicated_segments: 0,
                dedicated_crash_timeout_secs: 0.0,
                ..PoolConfig::default()
            },
            format!("/cc2s{:04x}{:04x}", std::process::id() as u16, 0xaceu16,),
        )));
        pool.write().ensure_ready().unwrap();
        let alloc = pool.write().alloc(128).unwrap();
        let request = RequestData::Shm {
            pool: Arc::clone(&pool),
            seg_idx: alloc.seg_idx as u16,
            offset: alloc.offset,
            data_size: 128,
            is_dedicated: alloc.is_dedicated,
        };
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        let execution_scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits::default(),
        );
        let reserved = scheduler
            .reserve_pending(0)
            .expect("pending reservation should succeed");
        scheduler.close();

        let conn = Connection::new(99);
        let err =
            execute_route_request(execution_scheduler, reserved, &conn, request, |_request| {
                panic!("callback must not run when acquire fails")
            })
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            RouteExecutionError::Acquire(SchedulerAcquireError::Closed)
        ));
        let reused = pool.write().alloc(128).unwrap();
        assert_eq!(reused.offset, alloc.offset);
    }

    #[tokio::test]
    async fn register_route_rejects_wire_invalid_route_name() {
        let s = Arc::new(Server::new("ipc://long_route_test", ServerIpcConfig::default()).unwrap());
        let route = make_route(&"x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1));

        let err = s.register_route(route).await.unwrap_err();

        assert!(err.to_string().contains("route name"));
        assert!(s.dispatcher.read().await.is_empty());
    }

    #[tokio::test]
    async fn register_route_rejects_invalid_crm_tag_fields() {
        let s = Server::new("ipc://invalid_crm_tag_route", ServerIpcConfig::default()).unwrap();
        let mut route = make_route("grid");
        route.crm_name = "Grid\0Injected".to_string();

        let err = s
            .register_route(route)
            .await
            .expect_err("invalid CrmTag must fail before route registration");

        assert!(err.to_string().contains("control characters"));
    }

    #[tokio::test]
    async fn register_route_rejects_too_many_methods() {
        let s =
            Arc::new(Server::new("ipc://method_count_test", ServerIpcConfig::default()).unwrap());
        let mut route = make_route("grid");
        route.method_names = (0..=c2_wire::handshake::MAX_METHODS)
            .map(|i| format!("m{i}"))
            .collect();

        let err = s.register_route(route).await.unwrap_err();

        assert!(err.to_string().contains("method count"));
        assert!(s.dispatcher.read().await.is_empty());
    }

    #[tokio::test]
    async fn register_route_rejects_route_count_overflow_under_write_lock() {
        let s =
            Arc::new(Server::new("ipc://route_count_test", ServerIpcConfig::default()).unwrap());

        for i in 0..c2_wire::handshake::MAX_ROUTES {
            s.register_route(make_route(&format!("route_{i}")))
                .await
                .unwrap();
        }

        let err = s.register_route(make_route("overflow")).await.unwrap_err();

        assert!(err.to_string().contains("route count"));
        assert!(s.dispatcher.read().await.resolve("overflow").is_none());
    }

    #[tokio::test]
    async fn inline_dispatch_rejects_unknown_method_index_before_callback() {
        use c2_wire::control::{decode_reply_control, encode_call_control};
        use c2_wire::frame::decode_frame;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingCallback {
            calls: Arc<AtomicUsize>,
        }

        impl CrmCallback for CountingCallback {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ResponseMeta::Inline(b"should-not-run".to_vec()))
            }
        }

        let server =
            Arc::new(Server::new("ipc://unknown_method_idx", ServerIpcConfig::default()).unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let conn = Connection::new(1);
        let (mut client_stream, server_stream) = UnixStream::pair().unwrap();
        let (_read_half, write_half) = server_stream.into_split();
        let writer = Arc::new(Mutex::new(write_half));
        let payload = encode_call_control("grid", 99).unwrap();

        let pending_permit = server.try_acquire_pending_request().unwrap();
        dispatch_call(&server, &conn, 42, &payload, &writer, pending_permit).await;

        let mut total_len_buf = [0u8; 4];
        client_stream.read_exact(&mut total_len_buf).await.unwrap();
        let total_len = u32::from_le_bytes(total_len_buf);
        let mut body = vec![0u8; total_len as usize];
        client_stream.read_exact(&mut body).await.unwrap();
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&total_len_buf);
        frame.extend_from_slice(&body);
        let (header, reply_payload) = decode_frame(&frame).unwrap();

        assert_eq!(header.request_id, 42);
        match decode_reply_control(reply_payload, 0).unwrap().0 {
            ReplyControl::Error(err) => {
                let message = String::from_utf8_lossy(&err);
                assert!(message.contains("unknown method index 99"));
            }
            other => panic!("expected method-index error reply, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remote_callbacks_respect_server_wide_execution_limit_across_routes() {
        use c2_wire::control::encode_call_control;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep, timeout};

        struct BlockingCallback {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            started: Arc<AtomicUsize>,
            release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(current, Ordering::SeqCst);
                self.started.fetch_add(1, Ordering::SeqCst);

                let (lock, cvar) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }

                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_execution_workers = 1;
        let server = Arc::new(Server::new("ipc://server_execution_limit", config).unwrap());

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        for name in ["grid_a", "grid_b"] {
            let mut route = make_route(name);
            route.callback = Arc::new(BlockingCallback {
                active: Arc::clone(&active),
                peak: Arc::clone(&peak),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            });
            server.register_route(route).await.unwrap();
        }

        let conn_a = Connection::new(1);
        let conn_b = Connection::new(2);
        let writer_a = closed_writer();
        let writer_b = closed_writer();
        let payload_a = encode_call_control("grid_a", 0).unwrap();
        let payload_b = encode_call_control("grid_b", 0).unwrap();

        let first = {
            let server = Arc::clone(&server);
            let writer = Arc::clone(&writer_a);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_a, 1, &payload_a, &writer, pending_permit).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first callback should start");

        let second = {
            let server = Arc::clone(&server);
            let writer = Arc::clone(&writer_b);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_b, 2, &payload_b, &writer, pending_permit).await;
            })
        };

        sleep(Duration::from_millis(50)).await;
        let started_while_blocked = started.load(Ordering::SeqCst);
        let peak_while_blocked = peak.load(Ordering::SeqCst);

        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(
            started_while_blocked, 1,
            "second route started executing while first callback still held the server-wide execution slot",
        );
        assert_eq!(peak_while_blocked, 1);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn route_waiters_do_not_occupy_blocking_execution_threads() {
        use crate::runtime::ServerRuntimeBuilder;
        use c2_wire::control::encode_call_control;
        use std::num::NonZeroUsize;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep, timeout};

        struct BlockingCallback {
            route_a_started: Arc<AtomicUsize>,
            route_b_started: Arc<AtomicUsize>,
            release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                route_name: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                if route_name == "grid_a" {
                    self.route_a_started.fetch_add(1, Ordering::SeqCst);
                    let (lock, cvar) = &*self.release_route_a;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = cvar.wait(released).unwrap();
                    }
                } else {
                    self.route_b_started.fetch_add(1, Ordering::SeqCst);
                }
                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_execution_workers = 2;
        config.max_pending_requests = 8;
        let rt = ServerRuntimeBuilder::build(&config).unwrap();

        rt.block_on(async move {
            let server = Arc::new(Server::new("ipc://route_waiter_starvation", config).unwrap());
            let route_a_started = Arc::new(AtomicUsize::new(0));
            let route_b_started = Arc::new(AtomicUsize::new(0));
            let release_route_a =
                Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

            let route_a_scheduler = Scheduler::with_limits(
                ConcurrencyMode::Parallel,
                HashMap::new(),
                SchedulerLimits {
                    max_pending: Some(NonZeroUsize::new(3).unwrap()),
                    max_workers: Some(NonZeroUsize::new(1).unwrap()),
                },
            );
            let route_b_scheduler = Scheduler::with_limits(
                ConcurrencyMode::Parallel,
                HashMap::new(),
                SchedulerLimits {
                    max_pending: Some(NonZeroUsize::new(1).unwrap()),
                    max_workers: Some(NonZeroUsize::new(1).unwrap()),
                },
            );

            for (name, scheduler) in [
                ("grid_a", route_a_scheduler.clone()),
                ("grid_b", route_b_scheduler),
            ] {
                let mut route = make_route(name);
                route.scheduler = Arc::new(scheduler);
                route.callback = Arc::new(BlockingCallback {
                    route_a_started: Arc::clone(&route_a_started),
                    route_b_started: Arc::clone(&route_b_started),
                    release_route_a: Arc::clone(&release_route_a),
                });
                server.register_route(route).await.unwrap();
            }

            let payload_a = encode_call_control("grid_a", 0).unwrap();
            let payload_b = encode_call_control("grid_b", 0).unwrap();
            let writer_a = closed_writer();
            let writer_a_waiter = closed_writer();
            let writer_b = closed_writer();

            let first_a = {
                let server = Arc::clone(&server);
                let payload = payload_a.clone();
                tokio::spawn(async move {
                    let pending_permit = server.try_acquire_pending_request().unwrap();
                    dispatch_call(
                        &server,
                        &Connection::new(101),
                        1,
                        &payload,
                        &writer_a,
                        pending_permit,
                    )
                    .await;
                })
            };

            timeout(Duration::from_secs(1), async {
                while route_a_started.load(Ordering::SeqCst) < 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first route A callback should start");

            let second_a = {
                let server = Arc::clone(&server);
                let payload = payload_a.clone();
                tokio::spawn(async move {
                    let pending_permit = server.try_acquire_pending_request().unwrap();
                    dispatch_call(
                        &server,
                        &Connection::new(102),
                        2,
                        &payload,
                        &writer_a_waiter,
                        pending_permit,
                    )
                    .await;
                })
            };

            timeout(Duration::from_secs(1), async {
                loop {
                    let snapshot = route_a_scheduler.snapshot();
                    if snapshot.pending >= 2 && snapshot.active_workers == 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("second route A request should be waiting on route capacity");
            sleep(Duration::from_millis(50)).await;

            let route_b = {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let pending_permit = server.try_acquire_pending_request().unwrap();
                    dispatch_call(
                        &server,
                        &Connection::new(201),
                        3,
                        &payload_b,
                        &writer_b,
                        pending_permit,
                    )
                    .await;
                })
            };

            let route_b_ready = timeout(Duration::from_millis(250), async {
                while route_b_started.load(Ordering::SeqCst) < 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

            {
                let (lock, cvar) = &*release_route_a;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }

            first_a.await.unwrap();
            second_a.await.unwrap();
            route_b.await.unwrap();

            route_b_ready.expect("ready route B callback should not be blocked by route A waiter");
        });
    }

    #[test]
    fn unregister_closes_admission_before_waiting_on_blocking_pool_drain() {
        use crate::runtime::ServerRuntimeBuilder;
        use c2_wire::control::encode_call_control;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep, timeout};

        struct BlockingCallback {
            route_a_started: Arc<AtomicUsize>,
            route_b_started: Arc<AtomicUsize>,
            release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                route_name: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                if route_name == "grid_a" {
                    self.route_a_started.fetch_add(1, Ordering::SeqCst);
                    let (lock, cvar) = &*self.release_route_a;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = cvar.wait(released).unwrap();
                    }
                } else {
                    self.route_b_started.fetch_add(1, Ordering::SeqCst);
                }
                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_execution_workers = 1;
        config.max_pending_requests = 8;
        let rt = ServerRuntimeBuilder::build(&config).unwrap();

        rt.block_on(async move {
            let server = Arc::new(
                Server::new("ipc://unregister_closes_before_blocking_wait", config).unwrap(),
            );
            let route_a_started = Arc::new(AtomicUsize::new(0));
            let route_b_started = Arc::new(AtomicUsize::new(0));
            let release_route_a =
                Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let route_b_scheduler = Scheduler::new(ConcurrencyMode::Parallel, HashMap::new());

            for (name, scheduler) in [
                (
                    "grid_a",
                    Scheduler::new(ConcurrencyMode::Parallel, HashMap::new()),
                ),
                ("grid_b", route_b_scheduler.clone()),
            ] {
                let mut route = make_route(name);
                route.scheduler = Arc::new(scheduler);
                route.callback = Arc::new(BlockingCallback {
                    route_a_started: Arc::clone(&route_a_started),
                    route_b_started: Arc::clone(&route_b_started),
                    release_route_a: Arc::clone(&release_route_a),
                });
                server.register_route(route).await.unwrap();
            }

            let payload_a = encode_call_control("grid_a", 0).unwrap();
            let payload_b = encode_call_control("grid_b", 0).unwrap();
            let writer_a = closed_writer();
            let writer_b = closed_writer();

            let first = {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let pending_permit = server.try_acquire_pending_request().unwrap();
                    dispatch_call(
                        &server,
                        &Connection::new(501),
                        1,
                        &payload_a,
                        &writer_a,
                        pending_permit,
                    )
                    .await;
                })
            };

            timeout(Duration::from_secs(1), async {
                while route_a_started.load(Ordering::SeqCst) < 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("route A callback should occupy the only blocking execution thread");

            let second = {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    let pending_permit = server.try_acquire_pending_request().unwrap();
                    dispatch_call(
                        &server,
                        &Connection::new(502),
                        2,
                        &payload_b,
                        &writer_b,
                        pending_permit,
                    )
                    .await;
                })
            };

            let route_b_admitted = timeout(Duration::from_secs(1), async {
                loop {
                    let snapshot = route_b_scheduler.snapshot();
                    if snapshot.active_workers == 1 && route_b_started.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            sleep(Duration::from_millis(50)).await;

            let mut unregister = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.unregister_route("grid_b").await })
            };
            let mut removed_before_release = None;
            let unregister_finished_before_release =
                match timeout(Duration::from_millis(100), &mut unregister).await {
                    Ok(result) => {
                        removed_before_release = Some(result.unwrap());
                        true
                    }
                    Err(_) => false,
                };
            assert_eq!(
                route_b_started.load(Ordering::SeqCst),
                0,
                "route B callback must not start while unregister waits for drain",
            );

            {
                let (lock, cvar) = &*release_route_a;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }

            first.await.unwrap();
            second.await.unwrap();
            let removed = match removed_before_release {
                Some(removed) => removed,
                None => unregister.await.unwrap(),
            };
            assert!(
                route_b_admitted,
                "route B request should wait after route admission and before callback start",
            );
            assert!(
                unregister_finished_before_release,
                "unregister must close route admission without waiting for blocking pool capacity",
            );
            assert!(removed);
            assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
        });
    }

    #[tokio::test]
    async fn shutdown_signal_closes_all_route_admissions_before_waiting_for_drain() {
        use std::collections::HashSet;
        use tokio::time::{Duration, timeout};

        let server = Arc::new(
            Server::new(
                "ipc://shutdown_closes_all_before_drain",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let scheduler_a = Scheduler::new(ConcurrencyMode::Parallel, HashMap::new());
        let scheduler_b = Scheduler::new(ConcurrencyMode::Parallel, HashMap::new());
        for (name, scheduler) in [
            ("grid_a", scheduler_a.clone()),
            ("grid_b", scheduler_b.clone()),
        ] {
            let mut route = make_route(name);
            route.scheduler = Arc::new(scheduler);
            server.register_route(route).await.unwrap();
        }

        let guard_a = scheduler_a
            .blocking_acquire(0)
            .expect("route A guard should enter");
        let guard_b = scheduler_b
            .blocking_acquire(0)
            .expect("route B guard should enter");
        let close_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                server
                    .close_registered_routes_for_shutdown("direct_ipc_shutdown")
                    .await
            })
        };

        timeout(Duration::from_secs(1), async {
            while !scheduler_a.snapshot().closed && !scheduler_b.snapshot().closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown should close at least one route before waiting for drain");
        assert!(
            scheduler_a.snapshot().closed && scheduler_b.snapshot().closed,
            "direct shutdown must close every route admission before waiting for any route to drain"
        );

        drop(guard_a);
        drop(guard_b);
        let outcomes = close_task.await.unwrap();
        let route_names: HashSet<_> = outcomes
            .iter()
            .map(|outcome| outcome.route_name.as_str())
            .collect();
        assert_eq!(route_names, HashSet::from(["grid_a", "grid_b"]));
    }

    #[tokio::test]
    async fn shutdown_signal_waits_for_active_connection_drain_before_stopped() {
        use c2_wire::control::encode_call_control;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, timeout};

        struct BlockingCallback {
            started: Arc<AtomicUsize>,
            release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                self.started.fetch_add(1, Ordering::SeqCst);

                let (lock, cvar) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }

                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let address = unique_readiness_address("shutdown_connection_drain");
        let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut route = make_route("grid");
        route.callback = Arc::new(BlockingCallback {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        server.register_route(route).await.unwrap();

        let runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .expect("server ready");

        let _client = StdUnixStream::connect(server.socket_path()).expect("connect idle client");
        let conn_id = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(conn_id) = server.active_connection_ids().into_iter().next() {
                    return conn_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection should register");
        let conn = server
            .active_connection(conn_id)
            .expect("tracked connection should be accessible");

        let payload = encode_call_control("grid", 0).unwrap();
        let writer = closed_writer();
        let request = {
            let server = Arc::clone(&server);
            let conn = Arc::clone(&conn);
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn, 1, &payload, &writer, pending_permit).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("callback should start");

        server.request_shutdown_signal();
        server
            .wait_until_stopped(Duration::from_millis(50))
            .await
            .expect_err("server must not report stopped while connection callback is still active");

        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        request.await.unwrap();
        runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    }

    #[tokio::test]
    async fn duplicate_shutdown_signal_does_not_rebroadcast_watch() {
        use tokio::time::{Duration, timeout};

        let server = Server::new(
            "ipc://duplicate_shutdown_signal_no_rebroadcast",
            ServerIpcConfig::default(),
        )
        .unwrap();
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        let mut shutdown_rx = server.shutdown_rx.clone();

        server.request_shutdown_signal();
        timeout(Duration::from_secs(1), shutdown_rx.changed())
            .await
            .expect("first shutdown signal should notify")
            .expect("shutdown watch should remain open");
        assert!(*shutdown_rx.borrow_and_update());

        server.request_shutdown_signal();
        timeout(Duration::from_millis(50), shutdown_rx.changed())
            .await
            .expect_err("duplicate shutdown signal must not wake active connection reads again");
    }

    #[tokio::test]
    async fn unregister_cancels_request_waiting_for_server_execution_slot() {
        use c2_wire::control::encode_call_control;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep, timeout};

        struct BlockingCallback {
            route_a_started: Arc<AtomicUsize>,
            route_b_started: Arc<AtomicUsize>,
            release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                route_name: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                if route_name == "grid_a" {
                    self.route_a_started.fetch_add(1, Ordering::SeqCst);
                    let (lock, cvar) = &*self.release_route_a;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = cvar.wait(released).unwrap();
                    }
                } else {
                    self.route_b_started.fetch_add(1, Ordering::SeqCst);
                }
                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_execution_workers = 1;
        let server =
            Arc::new(Server::new("ipc://unregister_cancels_global_waiter", config).unwrap());

        let route_a_started = Arc::new(AtomicUsize::new(0));
        let route_b_started = Arc::new(AtomicUsize::new(0));
        let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        for name in ["grid_a", "grid_b"] {
            let mut route = make_route(name);
            route.callback = Arc::new(BlockingCallback {
                route_a_started: Arc::clone(&route_a_started),
                route_b_started: Arc::clone(&route_b_started),
                release_route_a: Arc::clone(&release_route_a),
            });
            server.register_route(route).await.unwrap();
        }

        let payload_a = encode_call_control("grid_a", 0).unwrap();
        let payload_b = encode_call_control("grid_b", 0).unwrap();
        let writer_a = closed_writer();
        let writer_b = closed_writer();
        let conn_a = Connection::new(301);
        let conn_b = Connection::new(302);

        let first = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_a, 1, &payload_a, &writer_a, pending_permit).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while route_a_started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("route A callback should occupy the only execution slot");

        let second = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_b, 2, &payload_b, &writer_b, pending_permit).await;
            })
        };

        sleep(Duration::from_millis(50)).await;
        let unregister_result = timeout(
            Duration::from_millis(100),
            server.unregister_route("grid_b"),
        )
        .await;
        assert_eq!(
            route_b_started.load(Ordering::SeqCst),
            0,
            "route B callback must not run after route unregister cancels the queued request",
        );

        {
            let (lock, cvar) = &*release_route_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first.await.unwrap();
        second.await.unwrap();
        let removed = unregister_result
            .expect("unregister must cancel a not-started request waiting for server execution");
        assert!(removed);
        assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connection_close_cancels_request_waiting_for_server_execution_slot() {
        use c2_wire::control::encode_call_control;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep, timeout};

        struct BlockingCallback {
            route_a_started: Arc<AtomicUsize>,
            route_b_started: Arc<AtomicUsize>,
            release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                route_name: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                if route_name == "grid_a" {
                    self.route_a_started.fetch_add(1, Ordering::SeqCst);
                    let (lock, cvar) = &*self.release_route_a;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = cvar.wait(released).unwrap();
                    }
                } else {
                    self.route_b_started.fetch_add(1, Ordering::SeqCst);
                }
                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_execution_workers = 1;
        let server =
            Arc::new(Server::new("ipc://connection_cancels_global_waiter", config).unwrap());

        let route_a_started = Arc::new(AtomicUsize::new(0));
        let route_b_started = Arc::new(AtomicUsize::new(0));
        let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        for name in ["grid_a", "grid_b"] {
            let mut route = make_route(name);
            route.callback = Arc::new(BlockingCallback {
                route_a_started: Arc::clone(&route_a_started),
                route_b_started: Arc::clone(&route_b_started),
                release_route_a: Arc::clone(&release_route_a),
            });
            server.register_route(route).await.unwrap();
        }

        let payload_a = encode_call_control("grid_a", 0).unwrap();
        let payload_b = encode_call_control("grid_b", 0).unwrap();
        let writer_a = closed_writer();
        let writer_b = closed_writer();
        let conn_a = Connection::new(401);
        let conn_b = Arc::new(Connection::new(402));

        let first = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_a, 1, &payload_a, &writer_a, pending_permit).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while route_a_started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("route A callback should occupy the only execution slot");

        let second = {
            let server = Arc::clone(&server);
            let conn_b = Arc::clone(&conn_b);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_b, 2, &payload_b, &writer_b, pending_permit).await;
            })
        };

        sleep(Duration::from_millis(50)).await;
        conn_b.cancel_queued_work();
        timeout(Duration::from_millis(100), conn_b.wait_idle())
            .await
            .expect("connection idle wait must not wait for a cancelled queued callback");
        assert_eq!(
            route_b_started.load(Ordering::SeqCst),
            0,
            "route B callback must not run after its connection cancels queued work",
        );

        {
            let (lock, cvar) = &*release_route_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_signal_interrupts_partial_frame_body_reads() {
        use std::io::Write;
        use tokio::time::timeout;

        let address = unique_readiness_address("shutdown_partial_frame");
        let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let runner = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run().await })
        };
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .expect("server ready");

        let mut client = StdUnixStream::connect(server.socket_path()).expect("connect client");
        client
            .write_all(&12u32.to_le_bytes())
            .expect("write partial frame header");

        let conn_id = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(conn_id) = server.active_connection_ids().into_iter().next() {
                    return conn_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection should register");
        assert!(
            server.active_connection(conn_id).is_some(),
            "tracked connection should remain visible while body read is pending",
        );

        server.request_shutdown_signal();
        server
            .wait_until_stopped(Duration::from_secs(1))
            .await
            .expect("shutdown should interrupt partial body reads");
        runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    }

    #[tokio::test]
    async fn inline_dispatch_rejects_when_server_pending_capacity_is_exhausted() {
        use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
        use c2_wire::frame::decode_frame;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::oneshot;
        use tokio::time::{Duration, timeout};

        struct BlockingCallback {
            started: Arc<AtomicUsize>,
            release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl CrmCallback for BlockingCallback {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                self.started.fetch_add(1, Ordering::SeqCst);

                let (lock, cvar) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }

                Ok(ResponseMeta::Inline(b"done".to_vec()))
            }
        }

        let mut config = ServerIpcConfig::default();
        config.max_pending_requests = 1;
        config.max_execution_workers = 2;
        let server = Arc::new(Server::new("ipc://server_pending_limit", config).unwrap());

        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        for name in ["grid_a", "grid_b"] {
            let mut route = make_route(name);
            route.callback = Arc::new(BlockingCallback {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            });
            server.register_route(route).await.unwrap();
        }

        let conn_a = Connection::new(11);
        let conn_b = Connection::new(22);
        let writer_a = closed_writer();
        let payload_a = encode_call_control("grid_a", 0).unwrap();
        let payload_b = encode_call_control("grid_b", 0).unwrap();

        let first = {
            let server = Arc::clone(&server);
            let writer = Arc::clone(&writer_a);
            tokio::spawn(async move {
                let pending_permit = server.try_acquire_pending_request().unwrap();
                dispatch_call(&server, &conn_a, 1, &payload_a, &writer, pending_permit).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first callback should start");

        let (reply_tx, reply_rx) = oneshot::channel();
        let second = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let (mut client_stream, server_stream) = UnixStream::pair().unwrap();
                let (_read_half, write_half) = server_stream.into_split();
                let writer = Arc::new(Mutex::new(write_half));

                match server.try_acquire_pending_request() {
                    Ok(pending_permit) => {
                        dispatch_call(&server, &conn_b, 2, &payload_b, &writer, pending_permit)
                            .await;
                    }
                    Err(limit) => {
                        write_server_pending_capacity_error(&writer, 2, limit).await;
                    }
                }

                let mut total_len_buf = [0u8; 4];
                client_stream.read_exact(&mut total_len_buf).await.unwrap();
                let total_len = u32::from_le_bytes(total_len_buf);
                let mut body = vec![0u8; total_len as usize];
                client_stream.read_exact(&mut body).await.unwrap();
                let mut frame = Vec::with_capacity(4 + body.len());
                frame.extend_from_slice(&total_len_buf);
                frame.extend_from_slice(&body);
                let (_header, reply_payload) = decode_frame(&frame).unwrap();
                let message = match decode_reply_control(reply_payload, 0).unwrap().0 {
                    ReplyControl::Error(err) => String::from_utf8_lossy(&err).to_string(),
                    other => panic!("expected pending-capacity error reply, got {other:?}"),
                };
                let _ = reply_tx.send(message);
            })
        };

        let reply = match timeout(Duration::from_millis(200), reply_rx).await {
            Ok(Ok(message)) => message,
            Ok(Err(_)) => panic!("second reply channel dropped unexpectedly"),
            Err(_) => {
                {
                    let (lock, cvar) = &*release;
                    *lock.lock().unwrap() = true;
                    cvar.notify_all();
                }
                first.await.unwrap();
                second.await.unwrap();
                panic!("second call waited instead of rejecting at server pending admission");
            }
        };

        assert!(reply.contains("max_pending_requests=1"), "{reply}");
        assert_eq!(started.load(Ordering::SeqCst), 1);

        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn chunked_handle_connection_path_uses_chunk_processing_permit() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let start = source
            .find("if c2_wire::flags::is_chunked(flags)")
            .expect("chunked branch must exist");
        let rest = &source[start..];
        let end = rest
            .find("if c2_wire::flags::is_buddy(flags)")
            .expect("buddy branch must follow chunked branch");
        let chunked_branch = &rest[..end];

        assert!(
            chunked_branch.contains("try_acquire_chunk_processing_permit()"),
            "chunked call spawn path must be bounded by a chunk-processing permit",
        );
    }

    #[test]
    fn raw_server_shutdown_surface_is_transaction_owned() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let raw_shutdown_fn = concat!("pub fn request_", "shutdown_signal(&self) {");
        let raw_journal_drain_fn = concat!(
            "pub fn take_shutdown_",
            "route_outcomes(&self) -> Vec<ServerRouteCloseOutcome> {"
        );

        assert!(
            !source
                .lines()
                .any(|line| line.trim() == "pub fn shutdown(&self) {")
        );
        assert!(!source.lines().any(|line| line.trim() == raw_shutdown_fn));
        assert!(
            !source
                .lines()
                .any(|line| line.trim() == raw_journal_drain_fn)
        );
    }

    #[test]
    fn relay_route_admission_open_is_token_gated() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let open_route_start = source
            .find("pub async fn open_route_admission")
            .expect("open_route_admission should exist");
        let open_route_signature = &source[open_route_start
            ..source[open_route_start..]
                .find(") -> Result<(), ServerError>")
                .expect("open_route_admission signature should return ServerError")
                + open_route_start];

        assert!(
            !source.lines().any(|line| {
                line.trim() == "pub async fn open_route_admission(&self, name: &str) -> Result<(), ServerError> {"
            }),
            "closed relay-backed route admission must not be reopened by route name alone",
        );
        assert!(
            open_route_signature.contains("token: RouteAdmissionToken"),
            "closed relay-backed route admission must consume the token returned by closed commit",
        );
    }

    #[test]
    fn raw_route_construction_and_scheduler_surfaces_are_not_public() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let lib_source = std::fs::read_to_string(root.join("lib.rs")).unwrap();
        let dispatcher_source = std::fs::read_to_string(root.join("dispatcher.rs")).unwrap();
        let server_source = std::fs::read_to_string(root.join("server.rs")).unwrap();
        let crm_route_start = dispatcher_source
            .find("struct CrmRoute")
            .expect("CrmRoute should exist");
        let crm_route_rest = &dispatcher_source[crm_route_start..];
        let crm_route_end = crm_route_rest
            .find("impl CrmRoute")
            .expect("CrmRoute impl should follow struct");
        let crm_route_source = &crm_route_rest[..crm_route_end];

        assert!(
            !lib_source.contains("pub mod dispatcher;"),
            "dispatcher internals must not be a public construction surface",
        );
        assert!(
            !lib_source.contains("pub mod scheduler;"),
            "raw Scheduler must not be a public SDK-facing concurrency authority",
        );
        assert!(
            !lib_source.contains("pub use scheduler::{AccessLevel, ConcurrencyMode, Scheduler};"),
            "raw Scheduler must not be re-exported",
        );
        assert!(
            !lib_source.contains("CrmRoute"),
            "raw CrmRoute type must not be re-exported",
        );
        assert!(
            !lib_source.contains("Dispatcher"),
            "raw Dispatcher type must not be re-exported",
        );
        for field in [
            "pub name:",
            "pub crm_ns:",
            "pub crm_name:",
            "pub crm_ver:",
            "pub abi_hash:",
            "pub signature_hash:",
            "pub scheduler:",
            "pub callback:",
            "pub method_names:",
        ] {
            assert!(
                !crm_route_source.contains(field),
                "CrmRoute field remained public: {field}",
            );
        }
        assert!(
            !server_source.lines().any(|line| {
                line.trim()
                    == "pub async fn register_route(&self, route: CrmRoute) -> Result<(), ServerError> {"
            }),
            "raw CrmRoute registration must not be a public Server API",
        );
    }

    #[test]
    fn generic_signal_handler_does_not_keep_legacy_shutdown_ack_path() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let start = source
            .find("async fn handle_signal(")
            .expect("generic signal handler must exist");
        let rest = &source[start..];
        let end = rest
            .find("#[derive(Debug)]\nenum RouteExecutionError")
            .expect("route execution error enum should follow signal handler");
        let handle_signal_source = &rest[..end];

        assert!(
            !handle_signal_source.contains("MsgType::ShutdownClient"),
            "shutdown control must flow through handle_shutdown_signal, not the legacy generic signal ack path",
        );
    }

    #[test]
    fn shutdown_signal_handler_ack_path_does_not_wait_for_drain_budget() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let start = source
            .find("async fn handle_shutdown_signal(")
            .expect("shutdown signal handler must exist");
        let rest = &source[start..];
        let end = rest
            .find("\nasync fn handle_signal(")
            .expect("generic signal handler should follow shutdown signal handler");
        let handler_source = &rest[..end];

        assert!(handler_source.contains("decode_shutdown_initiate(payload)"));
        for forbidden in [
            concat!("decode_shutdown_request_", "wait_", "budget"),
            concat!("wait_", "budget"),
            concat!("wait_for_shutdown_", "control_outcome"),
            "tokio::time::timeout",
            "wait_for_active_connections_drained",
        ] {
            assert!(
                !handler_source.contains(forbidden),
                "shutdown initiate ack path must not wait for drain or accept a server-side wait budget: {forbidden}",
            );
        }

        let start_ack = handler_source
            .find("let outcome = DirectShutdownAck {\n        acknowledged: true,")
            .expect("success ack outcome should be constructed directly in the handler");
        let success_ack = &handler_source[start_ack..];
        assert!(success_ack.contains("shutdown_started: true"));
        assert!(success_ack.contains("server_stopped: false"));
        assert!(success_ack.contains("route_outcomes: Vec::new()"));
    }

    #[tokio::test]
    async fn malformed_shutdown_signal_does_not_stop_server() {
        let server = Server::new(
            "ipc://malformed_shutdown_signal",
            ServerIpcConfig::default(),
        )
        .unwrap();
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        let conn = Connection::new(12);
        let writer = closed_writer();
        let malformed = [MsgType::ShutdownClient.as_byte(), 0x01, 0x02];

        handle_shutdown_signal(&server, &conn, &malformed, 99, &writer).await;

        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
        assert!(!*server.shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn connection_accepted_after_shutdown_reads_duplicate_shutdown_signal() {
        use c2_wire::shutdown_control::{decode_shutdown_ack, encode_shutdown_initiate};

        let server = Arc::new(
            Server::new(
                "ipc://duplicate_shutdown_after_signal",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        server.request_shutdown_signal();

        let (mut client, server_stream) = UnixStream::pair().expect("unix stream pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                handle_connection(server, server_stream).await;
            })
        };

        let request = encode_frame(7, FLAG_SIGNAL, &encode_shutdown_initiate());
        client.write_all(&request).await.expect("write request");
        let mut header = [0u8; frame::HEADER_SIZE];
        client
            .read_exact(&mut header)
            .await
            .expect("read ack header");
        let (total_len, body) = frame::decode_total_len(&header).unwrap();
        let (frame_header, payload_prefix) = decode_frame_body(body, total_len).unwrap();
        let payload_len = frame_header.payload_len();
        let mut payload = payload_prefix.to_vec();
        if payload.len() < payload_len {
            let mut tail = vec![0u8; payload_len - payload.len()];
            client.read_exact(&mut tail).await.expect("read ack body");
            payload.extend_from_slice(&tail);
        }
        let ack = decode_shutdown_ack(&payload).expect("structured shutdown ack");

        assert_eq!(frame_header.request_id, 7);
        assert!(frame_header.flags & FLAG_SIGNAL != 0);
        assert!(frame_header.flags & FLAG_RESPONSE != 0);
        assert!(ack.acknowledged);
        assert!(ack.shutdown_started);
        assert!(!ack.server_stopped);
        assert!(ack.route_outcomes.is_empty());
        handler.await.expect("handler completes");
    }

    #[tokio::test]
    async fn connection_accepted_after_shutdown_idle_peer_does_not_block_handler() {
        use tokio::time::{Duration, timeout};

        let server = Arc::new(
            Server::new(
                "ipc://duplicate_shutdown_idle_peer",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        server.request_shutdown_signal();

        let (_client, server_stream) = UnixStream::pair().expect("unix stream pair");

        timeout(
            Duration::from_millis(250),
            handle_connection(server, server_stream),
        )
        .await
        .expect("post-shutdown idle peer must not park the handler");
    }

    #[tokio::test]
    async fn connection_accepted_after_shutdown_partial_shutdown_frame_does_not_wait_for_body() {
        use tokio::time::{Duration, timeout};

        let server = Arc::new(
            Server::new(
                "ipc://duplicate_shutdown_partial_frame",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        server.request_shutdown_signal();

        let (mut client, server_stream) = UnixStream::pair().expect("unix stream pair");
        let handler = tokio::spawn(async move {
            handle_connection(server, server_stream).await;
        });

        client
            .write_all(&SHUTDOWN_INITIATE_FRAME_BODY_LEN.to_le_bytes())
            .await
            .expect("write shutdown initiate frame length only");

        timeout(Duration::from_millis(250), handler)
            .await
            .expect("post-shutdown partial shutdown frame must not wait for body")
            .expect("handler completes");
    }

    #[tokio::test]
    async fn connection_accepted_after_shutdown_rejects_non_shutdown_frame_without_body_wait() {
        use tokio::time::{Duration, timeout};

        let server = Arc::new(
            Server::new(
                "ipc://duplicate_shutdown_rejects_non_shutdown",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        server.set_lifecycle_state(ServerLifecycleState::Ready);
        server.request_shutdown_signal();

        let (mut client, server_stream) = UnixStream::pair().expect("unix stream pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                handle_connection(server, server_stream).await;
            })
        };

        let non_shutdown_body_len = SHUTDOWN_INITIATE_FRAME_BODY_LEN + 1;
        client
            .write_all(&non_shutdown_body_len.to_le_bytes())
            .await
            .expect("write oversized post-shutdown frame prefix");

        timeout(Duration::from_millis(100), handler)
            .await
            .expect("post-shutdown non-shutdown frame must close without waiting for body")
            .expect("handler completes");
    }

    #[test]
    fn inline_and_buddy_handle_connection_paths_reserve_route_pending_before_spawn() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();

        let buddy_start = source
            .find(
                "if c2_wire::flags::is_buddy(flags) {\n                let (ctrl, ctrl_consumed) = match decode_call_control(payload, BUDDY_PAYLOAD_SIZE)",
            )
            .expect("buddy branch must exist");
        let buddy_rest = &source[buddy_start..];
        let buddy_end = buddy_rest
            .find("continue;\n            }\n\n            let (ctrl, ctrl_consumed)")
            .expect("inline branch must follow buddy branch");
        let buddy_branch = &buddy_rest[..buddy_end];
        let buddy_reserve = buddy_branch
            .find("reserve_route_execution(&server, &ctrl.route_name, ctrl.method_idx)")
            .expect("buddy branch must reserve route pending before spawn");
        let buddy_spawn = buddy_branch
            .find("tokio::spawn(async move")
            .expect("buddy branch must spawn dispatch task");
        assert!(
            buddy_reserve < buddy_spawn,
            "buddy branch must reserve route pending before spawning dispatch",
        );

        let inline_start = source
            .find("let (ctrl, ctrl_consumed) = match decode_call_control(payload, 0)")
            .expect("inline call branch must decode control");
        let inline_rest = &source[inline_start..];
        let inline_end = inline_rest
            .find("} else {\n            warn!(conn_id, flags, \"unknown frame type\");")
            .expect("unknown-frame branch must follow inline call branch");
        let inline_branch = &inline_rest[..inline_end];
        let inline_reserve = inline_branch
            .find("reserve_route_execution(&server, &ctrl.route_name, ctrl.method_idx)")
            .expect("inline branch must reserve route pending before spawn");
        let inline_spawn = inline_branch
            .find("tokio::spawn(async move")
            .expect("inline branch must spawn dispatch task");
        assert!(
            inline_reserve < inline_spawn,
            "inline branch must reserve route pending before spawning dispatch",
        );
    }

    #[test]
    fn dispatch_paths_use_flight_guard_instead_of_manual_connection_counters() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs")).unwrap();
        let tests_start = source
            .find("\n#[cfg(test)]\nmod tests")
            .expect("server source must end with test module");
        let production_source = &source[..tests_start];
        let flight_inc = concat!(".flight_", "inc(");
        let flight_dec = concat!(".flight_", "dec(");

        assert!(
            !production_source.contains(flight_inc),
            "server dispatch paths must use FlightGuard rather than manual flight increment"
        );
        assert!(
            !production_source.contains(flight_dec),
            "server dispatch paths must use FlightGuard rather than manual flight decrement"
        );
        assert!(
            production_source.matches("FlightGuard::new").count() >= 3,
            "inline, buddy, and chunked dispatch should all use FlightGuard"
        );
    }

    #[test]
    fn chunk_processing_permit_is_bounded_by_max_total_chunks() {
        let config = ServerIpcConfig {
            base: c2_config::BaseIpcConfig {
                max_total_chunks: 1,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ServerIpcConfig::default()
        };
        let server = Server::new("ipc://chunk_processing_limit", config).unwrap();

        let first = server.try_acquire_chunk_processing_permit().unwrap();
        assert_eq!(server.try_acquire_chunk_processing_permit().unwrap_err(), 1);
        drop(first);
        assert!(server.try_acquire_chunk_processing_permit().is_ok());
    }

    // -- shutdown --

    #[tokio::test]
    async fn shutdown_sets_signal() {
        let s = Server::new("ipc://shut_test", ServerIpcConfig::default()).unwrap();
        let mut rx = s.shutdown_rx.clone();
        s.request_shutdown_signal();
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    // -- error display --

    #[test]
    fn server_error_display() {
        let e = ServerError::Config("bad".into());
        assert!(format!("{e}").contains("bad"));

        let e2 = ServerError::Protocol("oops".into());
        assert!(format!("{e2}").contains("oops"));
    }

    // -- buddy payload decode + call control --

    #[test]
    fn decode_buddy_then_call_control() {
        use c2_wire::buddy::{BUDDY_PAYLOAD_SIZE, BuddyPayload, encode_buddy_payload};
        use c2_wire::control::encode_call_control;

        let bp = BuddyPayload {
            seg_idx: 0,
            offset: 4096,
            data_size: 256,
            is_dedicated: false,
        };
        let bp_bytes = encode_buddy_payload(&bp);
        let ctrl_bytes = encode_call_control("grid", 1).unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(&bp_bytes);
        payload.extend_from_slice(&ctrl_bytes);

        // Decode buddy part.
        let (decoded_bp, bp_consumed) = decode_buddy_payload(&payload).unwrap();
        assert_eq!(decoded_bp, bp);
        assert_eq!(bp_consumed, BUDDY_PAYLOAD_SIZE);

        // Decode call control after buddy header.
        let (ctrl, _) = decode_call_control(&payload, BUDDY_PAYLOAD_SIZE).unwrap();
        assert_eq!(ctrl.route_name, "grid");
        assert_eq!(ctrl.method_idx, 1);
    }

    #[tokio::test]
    async fn cleanup_buddy_request_block_frees_unconsumed_peer_block() {
        use c2_mem::MemHandle;
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};

        let mut peer_pool = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                ..PoolConfig::default()
            },
            unique_response_pool_prefix("rq"),
        );
        let handle = peer_pool.try_alloc_shm(128).unwrap();
        let (seg_idx, offset, len) = match handle {
            MemHandle::Buddy {
                seg_idx,
                offset,
                len,
            } => (seg_idx, offset, len),
            other => panic!("expected buddy request block, got {other:?}"),
        };
        let segment_name = peer_pool
            .segment_name(seg_idx as usize)
            .unwrap()
            .to_string();
        let segment_size = peer_pool
            .segment(seg_idx as usize)
            .unwrap()
            .allocator()
            .data_size() as u32;
        assert_eq!(peer_pool.stats().alloc_count, 1);

        let conn = Arc::new(Connection::new(7));
        conn.init_peer_shm(
            peer_pool.prefix().to_string(),
            vec![(segment_name, segment_size)],
        );
        let payload = encode_buddy_payload(&BuddyPayload {
            seg_idx,
            offset,
            data_size: len as u32,
            is_dedicated: false,
        });

        cleanup_buddy_request_block(&conn, &payload);

        assert_eq!(peer_pool.stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn buddy_dispatch_passes_shm_request_to_callback_without_inline_materialization() {
        use c2_mem::MemHandle;
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
        use c2_wire::control::encode_call_control;
        use std::sync::Mutex as StdMutex;

        #[derive(Clone, Debug, PartialEq, Eq)]
        struct SeenShmRequest {
            seg_idx: u16,
            offset: u32,
            data_size: u32,
            is_dedicated: bool,
        }

        struct InspectingCallback {
            seen: Arc<StdMutex<Option<SeenShmRequest>>>,
        }

        impl CrmCallback for InspectingCallback {
            fn invoke(
                &self,
                route_name: &str,
                method_idx: u16,
                request: RequestData,
                _response_pool: Arc<parking_lot::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                assert_eq!(route_name, "grid");
                assert_eq!(method_idx, 0);
                match request {
                    RequestData::Shm {
                        pool,
                        seg_idx,
                        offset,
                        data_size,
                        is_dedicated,
                    } => {
                        *self.seen.lock().unwrap() = Some(SeenShmRequest {
                            seg_idx,
                            offset,
                            data_size,
                            is_dedicated,
                        });
                        cleanup_request(RequestData::Shm {
                            pool,
                            seg_idx,
                            offset,
                            data_size,
                            is_dedicated,
                        });
                        Ok(ResponseMeta::Inline(b"ok".to_vec()))
                    }
                    other => {
                        panic!("expected buddy dispatch to preserve SHM request, got {other:?}")
                    }
                }
            }
        }

        let server = Arc::new(
            Server::new(
                &unique_readiness_address("buddy_callback_shm"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let seen = Arc::new(StdMutex::new(None));
        let mut route = make_route("grid");
        route.callback = Arc::new(InspectingCallback {
            seen: Arc::clone(&seen),
        });
        server.register_route(route).await.unwrap();

        let mut peer_pool = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                ..PoolConfig::default()
            },
            unique_response_pool_prefix("rqcb"),
        );
        let handle = peer_pool.try_alloc_shm(128).unwrap();
        let (seg_idx, offset, len) = match handle {
            MemHandle::Buddy {
                seg_idx,
                offset,
                len,
            } => (seg_idx, offset, len),
            other => panic!("expected buddy request block, got {other:?}"),
        };
        let segment_name = peer_pool
            .segment_name(seg_idx as usize)
            .unwrap()
            .to_string();
        let segment_size = peer_pool
            .segment(seg_idx as usize)
            .unwrap()
            .allocator()
            .data_size() as u32;
        assert_eq!(peer_pool.stats().alloc_count, 1);

        let conn = Arc::new(Connection::new(70));
        conn.init_peer_shm(
            peer_pool.prefix().to_string(),
            vec![(segment_name, segment_size)],
        );

        let mut payload = encode_buddy_payload(&BuddyPayload {
            seg_idx,
            offset,
            data_size: len as u32,
            is_dedicated: false,
        })
        .to_vec();
        let call_control = encode_call_control("grid", 0).unwrap();
        let ctrl_consumed = call_control.len();
        payload.extend_from_slice(&call_control);

        let admission = match reserve_route_execution(&server, "grid", 0).await {
            Ok(admission) => admission,
            Err(_) => panic!("route admission should succeed"),
        };
        let pending_permit = server.try_acquire_pending_request().unwrap();
        dispatch_admitted_buddy_call(
            &server,
            &conn,
            77,
            &payload,
            ctrl_consumed,
            admission.route,
            0,
            &closed_writer(),
            pending_permit,
            admission.pending_permit,
        )
        .await;

        let observed = seen
            .lock()
            .unwrap()
            .clone()
            .expect("callback should receive the buddy request");
        assert_eq!(
            observed,
            SeenShmRequest {
                seg_idx,
                offset,
                data_size: len as u32,
                is_dedicated: false,
            }
        );
        assert_eq!(peer_pool.stats().alloc_count, 0);
    }

    // -- chunked reassembly via ChunkRegistry --

    #[test]
    fn chunked_reassembly_via_registry() {
        use parking_lot::RwLock;
        use std::sync::Arc;

        let reassembly_cfg = c2_mem::config::PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_srv_chunk_test"),
        };
        let pool = Arc::new(RwLock::new(c2_mem::MemPool::new(reassembly_cfg)));
        let registry = c2_wire::chunk::ChunkRegistry::new(
            pool.clone(),
            c2_wire::chunk::ChunkConfig::default(),
        );

        let conn_id = 99u64;
        let request_id = 42u64;
        let total_chunks = 3usize;
        let chunk_size = 8usize;

        registry
            .insert(conn_id, request_id, total_chunks, chunk_size)
            .unwrap();
        registry.set_route_info(conn_id, request_id, "grid".into(), 0);

        // Feed chunks.
        assert!(!registry.feed(conn_id, request_id, 0, b"aaaaaaaa").unwrap());
        assert!(!registry.feed(conn_id, request_id, 1, b"bbbbbbbb").unwrap());
        assert!(registry.feed(conn_id, request_id, 2, b"cc").unwrap());

        // Finish.
        let finished = registry.finish(conn_id, request_id).unwrap();
        assert_eq!(finished.route_name.as_deref(), Some("grid"));
        assert_eq!(finished.method_idx, Some(0));
        assert_eq!(finished.handle.len(), 18); // 8+8+2
        let p = pool.read();
        let slice = p.handle_slice(&finished.handle);
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        drop(p);
        pool.write().release_handle(finished.handle);
    }

    #[tokio::test]
    async fn first_chunked_chunk_holds_route_pending_until_feed_error_aborts() {
        use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::encode_call_control;
        use std::num::NonZeroUsize;

        let server = Arc::new(
            Server::new(
                "ipc://chunk_route_pending_abort",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        route.scheduler = Arc::new(scheduler.clone());
        server.register_route(route).await.unwrap();

        let conn = Arc::new(Connection::new(77));
        let writer = closed_writer();
        let mut first_payload = Vec::new();
        first_payload.extend_from_slice(&encode_chunk_header(0, 2));
        first_payload.extend_from_slice(&encode_call_control("grid", 0).unwrap());
        first_payload.extend_from_slice(b"abcd");

        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            42,
            FLAG_CHUNKED,
            &first_payload,
            &writer,
            chunk_permit,
        )
        .await;

        assert!(server.chunk_registry.contains(conn.conn_id(), 42));
        assert!(matches!(
            scheduler.try_acquire(0),
            Err(SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            })
        ));

        let mut bad_second_payload = Vec::new();
        bad_second_payload.extend_from_slice(&encode_chunk_header(7, 2));
        bad_second_payload.extend_from_slice(b"zzzz");
        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            42,
            FLAG_CHUNKED,
            &bad_second_payload,
            &writer,
            chunk_permit,
        )
        .await;

        assert!(!server.chunk_registry.contains(conn.conn_id(), 42));
        let guard = scheduler
            .try_acquire(0)
            .expect("feed error abort should release route pending capacity");
        drop(guard);
    }

    #[tokio::test]
    async fn chunked_connection_cleanup_releases_route_pending_capacity() {
        use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::encode_call_control;
        use std::num::NonZeroUsize;

        let server = Arc::new(
            Server::new(
                "ipc://chunk_route_pending_cleanup",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        route.scheduler = Arc::new(scheduler.clone());
        server.register_route(route).await.unwrap();

        let conn = Arc::new(Connection::new(88));
        let writer = closed_writer();
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control("grid", 0).unwrap());
        payload.extend_from_slice(b"abcd");

        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            9,
            FLAG_CHUNKED,
            &payload,
            &writer,
            chunk_permit,
        )
        .await;

        assert!(server.chunk_registry.contains(conn.conn_id(), 9));
        assert!(matches!(
            scheduler.try_acquire(0),
            Err(SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            })
        ));

        server.cleanup_chunk_requests_for_connection(conn.conn_id());

        assert!(!server.chunk_registry.contains(conn.conn_id(), 9));
        let guard = scheduler
            .try_acquire(0)
            .expect("connection cleanup should release route pending capacity");
        drop(guard);
    }

    #[tokio::test]
    async fn chunked_gc_sweep_releases_stale_route_pending_capacity() {
        use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::encode_call_control;
        use std::num::NonZeroUsize;

        let server = Arc::new(
            Server::new("ipc://chunk_route_pending_gc", ServerIpcConfig::default()).unwrap(),
        );
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        route.scheduler = Arc::new(scheduler.clone());
        server.register_route(route).await.unwrap();

        let conn = Arc::new(Connection::new(99));
        let writer = closed_writer();
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control("grid", 0).unwrap());
        payload.extend_from_slice(b"abcd");

        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            11,
            FLAG_CHUNKED,
            &payload,
            &writer,
            chunk_permit,
        )
        .await;

        assert!(server.chunk_registry.contains(conn.conn_id(), 11));
        assert!(matches!(
            scheduler.try_acquire(0),
            Err(SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            })
        ));

        server.chunk_registry.abort(conn.conn_id(), 11);
        assert_eq!(server.sweep_stale_chunk_route_pending(), 1);

        let guard = scheduler
            .try_acquire(0)
            .expect("GC stale sweep should release route pending capacity");
        drop(guard);
    }

    #[test]
    fn buddy_response_wire_limit_rejects_oversized_payloads() {
        assert_eq!(
            crate::response::buddy_response_data_size(u32::MAX as usize),
            Some(u32::MAX)
        );
        assert_eq!(
            crate::response::buddy_response_data_size(u32::MAX as usize + 1),
            None
        );
    }

    #[test]
    fn reply_chunk_count_rejects_unrepresentable_chunk_counts() {
        assert!(reply_chunk_count(0, 0).unwrap_err().contains("chunk_size"));
        assert_eq!(reply_chunk_count(0, 128).unwrap(), 0);
        assert_eq!(reply_chunk_count(1025, 512).unwrap(), 3);

        let err = reply_chunk_count(u32::MAX as usize + 1, 1).unwrap_err();
        assert!(err.contains("chunk count"));
    }

    #[test]
    fn inline_reply_frame_len_rejects_unrepresentable_frames() {
        assert!(inline_reply_total_len(16).is_ok());

        let err = inline_reply_total_len(u32::MAX as usize).unwrap_err();
        assert!(err.contains("inline reply frame"));
    }

    #[tokio::test]
    async fn buddy_reply_write_failure_frees_allocated_response_block() {
        let pool = small_response_pool("a");
        let writer = closed_writer();
        let payload = b"x".repeat(8192);

        let err = write_buddy_reply_with_data(&pool, &writer, 7, payload.as_slice())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("buddy reply write failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn prepared_shm_reply_write_failure_frees_allocated_response_block() {
        let pool = small_response_pool("b");
        let alloc = pool.write().alloc(8192).unwrap();
        let writer = closed_writer();

        let err = send_response_meta(
            &pool,
            &writer,
            7,
            ResponseMeta::ShmAlloc {
                seg_idx: alloc.seg_idx as u16,
                offset: alloc.offset,
                data_size: 8192,
                is_dedicated: alloc.is_dedicated,
            },
            1024,
            4096,
            16 * 1024,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("prepared SHM reply write failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn inline_response_over_max_payload_is_rejected_before_transport() {
        let pool = small_response_pool("c");
        let writer = closed_writer();

        let err = send_response_meta(
            &pool,
            &writer,
            7,
            ResponseMeta::Inline(b"x".repeat(1025)),
            1024,
            4096,
            1024,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("response payload size 1025 exceeds max_payload_size 1024")
        );
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn prepared_shm_response_over_max_payload_is_rejected_and_freed() {
        let pool = small_response_pool("d");
        let alloc = pool.write().alloc(8192).unwrap();
        let writer = closed_writer();

        let err = send_response_meta(
            &pool,
            &writer,
            7,
            ResponseMeta::ShmAlloc {
                seg_idx: alloc.seg_idx as u16,
                offset: alloc.offset,
                data_size: 8192,
                is_dedicated: alloc.is_dedicated,
            },
            1024,
            4096,
            4096,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("response payload size 8192 exceeds max_payload_size 4096")
        );
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn smart_reply_treats_buddy_write_failure_as_fatal() {
        let pool = small_response_pool("e");
        let writer = closed_writer();
        let payload = b"x".repeat(8192);

        let err = smart_reply_with_data(&pool, &writer, 7, payload.as_slice(), 1024, 4096)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("buddy reply write failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    // -- handshake extraction --

    #[test]
    fn handshake_extracts_client_info() {
        use c2_wire::handshake::{
            CAP_CALL_V2, CAP_CHUNKED, decode_handshake, encode_client_handshake,
        };

        let segments = vec![("seg0".into(), 4096u32), ("seg1".into(), 8192u32)];
        let cap = CAP_CALL_V2 | CAP_CHUNKED;
        let hs_bytes = encode_client_handshake(&segments, cap, "/cc3b_test").unwrap();

        let decoded = decode_handshake(&hs_bytes).unwrap();
        assert_eq!(decoded.prefix, "/cc3b_test");
        assert_eq!(decoded.segments.len(), 2);
        assert_eq!(decoded.segments[0].0, "seg0");
        assert_eq!(decoded.segments[0].1, 4096);
        assert_eq!(decoded.capability_flags & CAP_CHUNKED, CAP_CHUNKED);
    }
}
