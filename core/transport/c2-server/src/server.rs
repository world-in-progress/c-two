//! Local IPC server — accept loop, per-connection frame handler, CRM dispatch.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use c2_local::{
    DEFAULT_CONNECT_TIMEOUT, LocalEndpoint, LocalListener, LocalReadHalf, LocalStream,
    LocalWriteHalf,
};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, mpsc, watch};
use tracing::{debug, info, warn};

/// Label counter for server pools. MemPool adds its incarnation and owns
/// platform segment-name derivation.
static RESPONSE_POOL_GEN: AtomicU64 = AtomicU64::new(0);

use c2_error::{C2Error, ErrorCode};
#[cfg(test)]
use c2_mem::config::PoolConfig;
use c2_mem::{MemPool, MemoryBudget};
use c2_wire::buddy::{
    BUDDY_PAYLOAD_SIZE, BuddyPayload, decode_buddy_payload, encode_buddy_payload,
};
use c2_wire::chunk::{
    ChunkAdmissionError, REPLY_CHUNK_META_SIZE, decode_chunk_header, encode_reply_chunk_meta,
};
use c2_wire::control::{
    ReplyControl, RouteCallIdentity, decode_call_control, try_encode_reply_control,
};
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
    ROUTE_CONTRACT_REJECT_INVALID, ROUTE_CONTRACT_REJECT_NOT_FOUND, RouteContractResponse,
    decode_pending_route_attestation_request, decode_route_contract_request,
    encode_pending_route_attestation_response, encode_route_contract_response,
};
use c2_wire::route_catalog_control::{
    RouteContractWire, RouteLookupRequest, RouteLookupResponse, RouteNack, RouteRecordWire,
    RouteSelector, RouteStateReasonWire, RouteWatchEvent, decode_route_list_request,
    decode_route_lookup_request, decode_route_watch_request, encode_route_list_response,
    encode_route_lookup_response, encode_route_nack, encode_route_watch_event,
};
use c2_wire::shutdown_control::{DirectShutdownAck, decode_shutdown_initiate, encode_shutdown_ack};

use crate::catalog::{RouteCatalog, RouteWatchBatch};
use crate::chunk_ordering::{
    ChunkAdmissionGate, ChunkAdmissionOutcome, ChunkAdmissionOwner, ChunkAdmissionWaiter,
    ChunkOrderingError,
};
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

const FRAME_FIXED_BODY_LEN: u32 = 12;
const SHUTDOWN_INITIATE_FRAME_BODY_LEN: u32 =
    FRAME_FIXED_BODY_LEN + c2_wire::msg_type::SHUTDOWN_CLIENT_BYTES.len() as u32;
const POST_SHUTDOWN_DUPLICATE_INITIATE_READ_TIMEOUT_MS: u64 = 100;
const CONTROL_ACK_PEER_CLOSE_TIMEOUT_MS: u64 = 50;
/// Default journal reason for an explicit direct IPC shutdown transaction.
pub const DIRECT_IPC_SHUTDOWN_REASON: &str = "direct_ipc_shutdown";
/// Journal reason for the shutdown transaction an owner-bound host starts
/// after its controller's grace window expired. The owner control EOF is a
/// separate channel from business stream EOF, idle eviction, ping failures,
/// cancellation, and relay reconnects, and must stay distinguishable here.
pub const OWNER_BOUND_SHUTDOWN_REASON: &str = "owner_bound_grace_expired";
/// Journal reason when an owner-bound host stops because its own control
/// watcher failed locally and the owner relationship can no longer be
/// observed. This is not a business-stream failure and not a peer EOF.
pub const OWNER_WATCHER_ERROR_SHUTDOWN_REASON: &str = "owner_bound_watcher_error";
/// Admission-closure reason when the controller closed the control channel.
pub const OWNER_MISSING_ADMISSION_REASON: &str = "owner_missing";
/// Admission-closure reason when the control watcher itself failed.
pub const OWNER_WATCHER_ERROR_ADMISSION_REASON: &str = "owner_watcher_error";

fn error_wire(code: ErrorCode, message: impl Into<String>) -> Vec<u8> {
    C2Error::new(code, message).to_wire_bytes()
}

fn close_reason_to_route_state_reason(closed_reason: &str) -> RouteStateReasonWire {
    match closed_reason {
        "shutdown" | "direct_ipc_shutdown" | OWNER_BOUND_SHUTDOWN_REASON
        | OWNER_WATCHER_ERROR_SHUTDOWN_REASON => RouteStateReasonWire::Shutdown,
        OWNER_MISSING_ADMISSION_REASON | OWNER_WATCHER_ERROR_ADMISSION_REASON => {
            RouteStateReasonWire::OwnerWatchDisconnected
        }
        _ => RouteStateReasonWire::ExplicitUnregister,
    }
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

/// Read-only snapshot of the server direction's shared memory budget.
///
/// The three cells are C-Two-owned accounting scopes — owner-created SHM
/// backing, owner-created file backing, and live reassembly capacity — not
/// process RSS. Their sum must never be presented as physical memory usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerMemorySnapshot {
    /// Resolved limits the server budget enforces.
    pub limits: c2_config::MemoryBudgetLimits,
    /// Consistent view of the three budget cells.
    pub budget: c2_mem::BudgetSnapshot,
}

impl ServerMemorySnapshot {
    /// Scope label for the server direction.
    ///
    /// Use this to distinguish the server response/reassembly role from a
    /// Runtime's outgoing client role when reporting more than one scope.
    pub const SCOPE: &'static str = "server";
}

/// The main IPC server.
///
/// Binds a local OS endpoint, accepts connections, and dispatches CRM calls through
/// the [`Dispatcher`].  Each connection runs in its own tokio task with a
/// dedicated heartbeat probe.
pub struct Server {
    identity: ServerIdentity,
    config: ServerIpcConfig,
    ipc_address: String,
    endpoint: LocalEndpoint,
    /// **tokio async RwLock** — guards CRM dispatch table; requires `.read().await`
    /// / `.write().await`.  Do NOT confuse with `parking_lot::RwLock` below.
    dispatcher: RwLock<Dispatcher>,
    /// **parking_lot sync RwLock** — guards authoritative route metadata and
    /// revisioned watch history. It must not be held across awaits.
    route_catalog: parking_lot::RwLock<RouteCatalog>,
    shutdown_tx: watch::Sender<bool>,
    lifecycle_tx: watch::Sender<ServerLifecycleState>,
    conn_counter: AtomicU64,
    route_registration: Mutex<()>,
    /// Sharded chunk reassembly lifecycle manager.
    chunk_registry: Arc<c2_wire::chunk::ChunkRegistry>,
    /// **parking_lot sync RwLock** — guards SHM memory pool; blocking `.read()`
    /// / `.write()` (no `.await`).  Safe to hold briefly inside tokio tasks.
    response_pool: Arc<parking_lot::RwLock<MemPool>>,
    /// Shared transport memory context charged by the response pool, the
    /// reassembly pool inside `chunk_registry`, and response prewarm. One
    /// server direction, one budget; it owns only accounting counters and is
    /// never reset by shutdown.
    memory_budget: MemoryBudget,
    pending_routes: Arc<parking_lot::Mutex<HashMap<String, PendingRouteInfo>>>,
    pending_requests: Arc<Semaphore>,
    chunk_processing_permits: Arc<Semaphore>,
    chunk_route_pending: parking_lot::Mutex<HashMap<(u64, u64), ChunkRouteAdmission>>,
    /// Bounded per-request first-chunk admission ordering (see
    /// [`crate::chunk_ordering`]). Created at frame dispatch and resolved by the
    /// first chunk's task, its abort paths, or connection cleanup.
    chunk_admission_gate: ChunkAdmissionGate,
    active_connections: parking_lot::Mutex<HashMap<u64, Arc<Connection>>>,
    active_connections_notify: Notify,
    shutdown_route_outcomes: parking_lot::Mutex<Vec<ServerRouteCloseOutcome>>,
    shutdown_generation: AtomicU64,
    /// Close reason stamped into the run loop's shutdown route journal.
    /// `direct_ipc_shutdown` is the default; owner-bound owners select their
    /// own reason so a control-EOF shutdown is distinguishable in outcomes.
    shutdown_close_reason: parking_lot::Mutex<&'static str>,
    /// Set when an owner-bound controller closes and new business admission
    /// must stop before the shutdown transaction itself begins. Existing work
    /// keeps draining under the original rules; the listener stays open.
    business_admission_closed: AtomicBool,
    run_entered: AtomicBool,
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
    route_uid: String,
    route_revision: u64,
    contract: c2_contract::ExpectedRouteContract,
    method_names: Vec<String>,
    max_payload_size: u64,
}

impl PendingRouteInfo {
    fn into_attestation(self) -> PendingRouteAttestation {
        PendingRouteAttestation {
            route_name: self.contract.route_name,
            route_uid: self.route_uid,
            route_revision: self.route_revision,
            crm_ns: self.contract.crm_ns,
            crm_name: self.contract.crm_name,
            crm_ver: self.contract.crm_ver,
            abi_hash: self.contract.abi_hash,
            signature_hash: self.contract.signature_hash,
            method_names: self.method_names,
            max_payload_size: self.max_payload_size,
        }
    }
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
        parse_local_endpoint_with_protocol(address, config.base.endpoint_protocol)?;
        // One server direction, one finite budget: the response pool, the
        // chunk-reassembly pool, and response prewarm all charge the same
        // context, so the server cannot double its configured cap by owning
        // two pools. The context owns only accounting counters and is never
        // reset by shutdown; charges retained by outstanding response data
        // stay observable.
        let memory_budget = MemoryBudget::from_limits(&config.memory_budget_limits());
        let reassembly_pool = {
            let pid = std::process::id();
            let ra_gen = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
            let prefix = format!("/cc3s{:08x}{:08x}", pid, ra_gen);
            // Centralized projection: reassembly follows the same buddy policy
            // as every other pool role (disabled buddy → dedicated/file storage).
            MemPool::new_with_prefix_and_budget(
                config
                    .base
                    .reassembly_pool_config(&config.reassembly_pool_tuning()),
                prefix,
                memory_budget.clone(),
            )
        };
        Self::with_reassembly_pool(
            address,
            config,
            identity,
            Arc::new(parking_lot::RwLock::new(reassembly_pool)),
        )
    }

    /// Build a server around an explicitly owned reassembly pool.
    ///
    /// The pool's `MemPool::budget()` reassembly cell is the server-side
    /// admission authority for chunked request reassembly. Production callers
    /// pass the config-derived pool through [`Server::new_with_identity`];
    /// tests use this seam to inject pools with tiny finite budgets.
    fn with_reassembly_pool(
        address: &str,
        config: ServerIpcConfig,
        identity: ServerIdentity,
        reassembly_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<Self, ServerError> {
        config.validate().map_err(ServerError::Config)?;
        validate_server_identity(&identity)?;
        // The resolved config alone selects the endpoint protocol; bind and
        // every restart bind reuse this one derivation.
        let endpoint =
            parse_local_endpoint_with_protocol(address, config.base.endpoint_protocol)?;
        let (shutdown_tx, _) = watch::channel(false);
        // The injected test pool and the production reassembly pool both
        // carry the server's accounting authority. Derive the response-pool
        // budget from that exact owner so both directions share one context.
        let memory_budget = reassembly_pool
            .read()
            .budget()
            .cloned()
            .ok_or_else(|| ServerError::Config("reassembly pool has no owner budget".into()))?;
        let chunk_config = c2_wire::chunk::ChunkConfig::from_base(&config);
        let chunk_registry = Arc::new(c2_wire::chunk::ChunkRegistry::new(
            reassembly_pool,
            chunk_config,
        ));
        let response_pool = {
            let pid = std::process::id();
            let generation = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("/cc3r{:08x}{:08x}", pid, generation as u32);
            // Centralized projection: the response pool keeps dedicated SHM
            // replies available even when the buddy tiers are disabled.
            MemPool::new_with_prefix_and_budget(
                config
                    .base
                    .primary_pool_config(&config.response_pool_tuning()),
                prefix,
                memory_budget.clone(),
            )
        };
        // Explicit prewarm only. The response pool stays unmapped at
        // construction; buddy segments are created lazily by the first large
        // reply (or mapped here when `pool_prewarm_segments` asks for them).
        let prewarm_segments = config.pool_prewarm_segments as usize;
        let mut response_pool = response_pool;
        if prewarm_segments > 0 {
            response_pool
                .ensure_buddy_segments(prewarm_segments)
                .map_err(|e| ServerError::Config(format!("response pool prewarm: {e}")))?;
        }
        let chunk_admission_gate = ChunkAdmissionGate::new(config.max_total_chunks as usize);
        let (lifecycle_tx, _lifecycle_rx) = watch::channel(ServerLifecycleState::Initialized);
        let pending_requests = Arc::new(Semaphore::new(config.max_pending_requests as usize));
        let chunk_processing_permits = Arc::new(Semaphore::new(config.max_total_chunks as usize));
        let route_catalog = RouteCatalog::new(identity.clone(), config.max_payload_size);
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
            endpoint,
            dispatcher: RwLock::new(Dispatcher::new()),
            route_catalog: parking_lot::RwLock::new(route_catalog),
            shutdown_tx,
            lifecycle_tx,
            conn_counter: AtomicU64::new(0),
            route_registration: Mutex::new(()),
            chunk_registry,
            response_pool: Arc::new(parking_lot::RwLock::new(response_pool)),
            memory_budget,
            pending_routes: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            pending_requests,
            chunk_processing_permits,
            chunk_route_pending: parking_lot::Mutex::new(HashMap::new()),
            chunk_admission_gate,
            active_connections: parking_lot::Mutex::new(HashMap::new()),
            active_connections_notify: Notify::new(),
            shutdown_route_outcomes: parking_lot::Mutex::new(Vec::new()),
            shutdown_generation: AtomicU64::new(0),
            shutdown_close_reason: parking_lot::Mutex::new(DIRECT_IPC_SHUTDOWN_REASON),
            business_admission_closed: AtomicBool::new(false),
            run_entered: AtomicBool::new(false),
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

    /// Tear down a failed/aborted chunk request.
    ///
    /// Order is the correctness argument:
    /// 1. Publish the terminal ordering outcome and take a gate fence. A
    ///    first-chunk task still parked in route admission wakes here and can
    ///    no longer win the admission commit; the fence keeps the gate entry in
    ///    place so a same-key successor generation cannot begin mid-teardown.
    /// 2. Release the stored route admission, then abort exactly the assembly
    ///    generation that stored it (identity-checked, so this cannot touch a
    ///    successor's same-key assembly).
    /// 3. Drop the fence, allowing a successor generation to begin.
    fn abort_chunk_request(&self, conn_id: u64, request_id: u64) {
        let reclaim = self.chunk_admission_gate.abort_key((conn_id, request_id));
        if let Some(admission) = self.take_chunk_route_pending(conn_id, request_id) {
            self.chunk_registry.abort_id(admission.assembly);
        }
        drop(reclaim);
    }

    /// Claim first-chunk admission ordering for an incoming chunked request.
    fn begin_chunk_admission(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Result<ChunkAdmissionOwner, ChunkOrderingError> {
        self.chunk_admission_gate.begin((conn_id, request_id))
    }

    /// Ordering waiter for a later chunk whose request is still being admitted.
    fn chunk_admission_waiter(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Option<ChunkAdmissionWaiter> {
        self.chunk_admission_gate.waiter((conn_id, request_id))
    }

    fn cleanup_chunk_requests_for_connection(&self, conn_id: u64) {
        // Publish the terminal ordering outcome first and wake every later-chunk
        // waiter (and any first chunk parked in route admission) of this
        // connection, so a reservation that completes during this cleanup can
        // no longer win the admission commit. The registry/route cleanup below
        // then removes whatever was already published; a generation that loses
        // the commit rolls itself back through its publication guard.
        let aborted_ordering = self.chunk_admission_gate.abort_connection(conn_id);
        self.chunk_registry.cleanup_connection(conn_id);
        self.chunk_route_pending
            .lock()
            .retain(|(pending_conn_id, _), _| *pending_conn_id != conn_id);
        if aborted_ordering > 0 {
            debug!(
                conn_id,
                aborted_ordering, "aborted pending chunk admission ordering on disconnect"
            );
        }
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
        let reason = close_reason_to_route_state_reason(closed_reason);
        let routes = {
            let _guard = self.route_registration.lock().await;
            let mut dispatcher = self.dispatcher.write().await;
            let routes = dispatcher.routes_snapshot();
            let mut catalog = self.route_catalog.write();
            for route in &routes {
                let _ = catalog.close_route(&route.name, reason.clone());
                route.scheduler.close();
            }
            dispatcher.take_all()
        };
        let outcomes = Self::wait_closed_routes_drained(routes, closed_reason).await;
        self.remove_catalog_routes(&outcomes, reason);
        outcomes
    }

    fn remove_catalog_routes(
        &self,
        outcomes: &[ServerRouteCloseOutcome],
        reason: RouteStateReasonWire,
    ) {
        let mut catalog = self.route_catalog.write();
        for outcome in outcomes {
            let _ = catalog.remove_route(&outcome.route_name, reason.clone());
        }
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

    /// Read-only count of native connections still owned by this server.
    /// Connection count does not select the server lifecycle policy.
    pub fn active_connection_count(&self) -> usize {
        self.active_connections.lock().len()
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
        if self
            .route_catalog
            .read()
            .contains_current_route(&route_name)
        {
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
        if self.business_admission_closed() {
            return Err(ServerError::Protocol(format!(
                "server business admission is closed; cannot reserve route {}",
                route_name
            )));
        }
        drop(dispatcher);
        let registration_token = uuid::Uuid::new_v4().simple().to_string();
        pending_routes.insert(
            route_name.clone(),
            PendingRouteInfo {
                registration_token: registration_token.clone(),
                route_uid: route.route_uid.clone(),
                route_revision: route.route_revision,
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
            || self.business_admission_closed()
        {
            let refusal = if self.business_admission_closed()
                && !matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
            {
                "server business admission is closed"
            } else {
                "server is shutting down"
            };
            reservation.close_scheduler_for_abort();
            reservation.resolve();
            return Err(ServerError::Protocol(format!(
                "{refusal}; cannot commit route {}",
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
        let route = reservation.into_route();
        if let Err(err) = self
            .route_catalog
            .write()
            .register_ready(&route, admission_open)
        {
            route.scheduler.close();
            return Err(ServerError::Protocol(err));
        }
        dispatcher.register(route);
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
        self.route_catalog
            .write()
            .open_route(&name)
            .map_err(ServerError::Protocol)?;
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
    ) -> Result<PendingRouteInfo, Box<PendingRouteAttestationResponse>> {
        let pending_routes = self.pending_routes.lock();
        let Some(info) = pending_routes.get(route_name) else {
            return Err(Box::new(PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_NOT_FOUND.to_string(),
                message: format!("pending route '{route_name}' not found"),
            }));
        };
        if info.registration_token != registration_token {
            return Err(Box::new(PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_TOKEN_MISMATCH.to_string(),
                message: format!("pending route '{route_name}' registration token mismatch"),
            }));
        }
        Ok(info.clone())
    }

    /// Remove a CRM route. Returns `true` if it existed.
    ///
    /// The route handle is marked closed under the same dispatcher write lock
    /// that removes the route, so local handle clones and remote route lookup
    /// cannot observe an open-but-unregistered window.
    pub async fn unregister_route(&self, name: &str) -> bool {
        let removed = {
            let _guard = self.route_registration.lock().await;
            let mut dispatcher = self.dispatcher.write().await;
            if let Some(route) = dispatcher.resolve(name) {
                let _ = self
                    .route_catalog
                    .write()
                    .close_route(name, RouteStateReasonWire::ExplicitUnregister);
                route.scheduler.close();
            }
            dispatcher.unregister(name)
        };
        if let Some(route) = removed {
            route.scheduler.wait_drained_async().await;
            let _ = self
                .route_catalog
                .write()
                .remove_route(name, RouteStateReasonWire::ExplicitUnregister);
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
        let reason = close_reason_to_route_state_reason(closed_reason);
        let routes = {
            let _guard = self.route_registration.lock().await;
            let mut dispatcher = self.dispatcher.write().await;
            let mut seen = HashSet::new();
            let mut routes = Vec::new();
            let mut catalog = self.route_catalog.write();
            for name in names {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                if let Some(route) = dispatcher.resolve(name) {
                    let _ = catalog.close_route(name, reason.clone());
                    route.scheduler.close();
                    routes.push(route);
                }
            }
            for route in &routes {
                dispatcher.unregister(&route.name);
            }
            routes
        };
        let outcomes = Self::wait_closed_routes_drained(routes, closed_reason).await;
        self.remove_catalog_routes(&outcomes, reason);
        outcomes
    }

    /// The validated local endpoint, independent of the OS transport.
    pub fn local_endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
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
                self.run_entered.store(false, Ordering::Release);
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
            if tokio::time::timeout(probe_timeout, ping_server_endpoint(self.local_endpoint()))
                .await
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

    /// Observe initiation so an owner can cancel control observation and its grace immediately.
    pub async fn wait_for_shutdown_requested(&self) {
        let mut rx = self.shutdown_tx.subscribe();
        wait_for_shutdown(&mut rx).await;
    }

    /// Wait for actual terminal runtime work without imposing a caller deadline.
    /// Pending shutdown owners use this while their callers observe bounded waits separately.
    pub async fn wait_until_terminal(&self) -> Result<(), ServerError> {
        let mut rx = self.lifecycle_tx.subscribe();
        loop {
            if matches!(
                *rx.borrow(),
                ServerLifecycleState::Initialized
                    | ServerLifecycleState::Stopped
                    | ServerLifecycleState::Failed(_)
            ) {
                return Ok(());
            }
            rx.changed()
                .await
                .map_err(|_| ServerError::Config("server lifecycle channel closed".into()))?;
        }
    }

    pub async fn observe_external_shutdown_outcomes_unbounded(
        &self,
    ) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        if matches!(
            self.lifecycle_state(),
            ServerLifecycleState::Stopping | ServerLifecycleState::Stopped
        ) {
            self.wait_until_terminal().await?;
            Ok(self.take_shutdown_route_outcomes())
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn shutdown_and_wait_unbounded(
        &self,
    ) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        self.request_shutdown_signal();
        self.wait_until_terminal().await?;
        Ok(self.take_shutdown_route_outcomes())
    }

    /// Mark runtime-backed server work as stopped after its runtime is gone.
    ///
    /// This is a shutdown cleanup fence for runtime owners. It does not erase
    /// startup failure diagnostics, but it prevents a force-dropped runtime from
    /// leaving `Starting`, `Ready`, or `Stopping` as a stale non-terminal state.
    pub fn finalize_runtime_stopped(&self) {
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

    /// Read-only snapshot of the server direction's shared memory budget.
    ///
    /// This never allocates, maps, connects, or resets accounting: observing
    /// the server memory context does not change its limits or usage. Charges
    /// retained by outstanding response or reassembly data stay visible until
    /// their backing is released.
    pub fn memory_budget_snapshot(&self) -> ServerMemorySnapshot {
        ServerMemorySnapshot {
            limits: self.config.memory_budget_limits(),
            budget: self.memory_budget.snapshot(),
        }
    }

    /// Read-only observer of the server direction's shared budget.
    ///
    /// The handle shares only the accounting counters and resolved limits, so
    /// a stopped server keeps its retained response/reassembly charges
    /// observable without retaining pools, connections, or callbacks.
    pub fn memory_budget_observer(&self) -> c2_mem::BudgetObserver {
        c2_mem::BudgetObserver::new(
            self.config.memory_budget_limits(),
            self.memory_budget.clone(),
        )
    }

    /// Return the configured response SHM threshold.
    pub fn response_shm_threshold(&self) -> u64 {
        self.config.shm_threshold
    }

    /// Return the configured maximum logical response payload size.
    pub fn response_max_payload_size(&self) -> u64 {
        self.config.max_payload_size
    }

    /// Capture a concrete route identity before starting its retirement transaction.
    pub fn registered_route_identity(&self, name: &str) -> Option<(String, u64)> {
        self.route_catalog.read().registered_identity(name)
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

        self.run_entered.store(true, Ordering::Release);
        let startup = LocalListener::bind(&self.endpoint).map_err(ServerError::Io);

        let mut listener = match startup {
            Ok(listener) => listener,
            Err(err) => {
                self.set_lifecycle_state(ServerLifecycleState::Failed(err.to_string()));
                return Err(err);
            }
        };

        self.set_lifecycle_state(ServerLifecycleState::Ready);
        info!(endpoint = ?self.endpoint.os_name(), "server listening");

        // Spawn periodic GC sweep for expired chunk assemblies. The same
        // bounded task reclaims both storage tiers of the server's owner
        // pools: idle buddy segments retire down to the configured minimum
        // retained segments (`gc_buddy`) and dedicated segments whose reader
        // set `read_done` are unmapped (`gc_dedicated`), so dedicated-only
        // traffic releases its mappings without waiting for another
        // allocation. No per-allocation thread or task is ever spawned.
        let gc_server = Arc::clone(self);
        let gc_interval = self.chunk_registry.config().gc_interval;
        let mut gc_shutdown_rx = self.shutdown_tx.subscribe();
        let gc_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(gc_interval);
            interval.tick().await; // skip first immediate tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = gc_server.chunk_registry.gc_sweep();
                        let released_route_pending = gc_server.sweep_stale_chunk_route_pending();
                        {
                            let mut response_pool = gc_server.response_pool.write();
                            response_pool.gc_buddy();
                            response_pool.gc_dedicated();
                        }
                        {
                            let mut reassembly_pool = gc_server.chunk_registry.pool().write();
                            reassembly_pool.gc_buddy();
                            reassembly_pool.gc_dedicated();
                        }
                        if stats.expired > 0 {
                            info!(
                                expired = stats.expired,
                                freed = stats.freed_bytes,
                                released_route_pending,
                                "chunk GC sweep"
                            );
                        }
                    }
                    _ = wait_for_shutdown(&mut gc_shutdown_rx) => break,
                }
            }
        });

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let (shutdown_done_tx, mut shutdown_done_rx) = mpsc::unbounded_channel();
        let mut shutdown_close_started = false;
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok(stream) => {
                            let server = Arc::clone(self);
                            tokio::spawn(handle_connection(server, stream));
                        }
                        Err(e) => warn!("accept error: {e}"),
                    }
                }
                _ = wait_for_shutdown(&mut shutdown_rx), if !shutdown_close_started => {
                    info!("server shutting down");
                    self.set_lifecycle_state(ServerLifecycleState::Stopping);
                    shutdown_close_started = true;
                    let server = Arc::clone(self);
                    let done_tx = shutdown_done_tx.clone();
                    let close_reason = *server.shutdown_close_reason.lock();
                    tokio::spawn(async move {
                        let outcomes = server
                            .close_registered_routes_for_shutdown(close_reason)
                            .await;
                        let _ = done_tx.send(outcomes);
                    });
                }
                shutdown_route_outcomes = shutdown_done_rx.recv(), if shutdown_close_started => {
                    let shutdown_route_outcomes = shutdown_route_outcomes.unwrap_or_default();
                    self.wait_for_active_connections_drained().await;
                    if !shutdown_route_outcomes.is_empty() {
                        self.shutdown_route_outcomes
                            .lock()
                            .extend(shutdown_route_outcomes);
                    }
                    let _ = gc_handle.await;
                    drop(listener);
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

    /// Initiate the shutdown transaction with an explicit journal reason.
    ///
    /// Owner-bound hosts use this so the recorded route-close reason says the
    /// controller relationship ended, not an anonymous direct IPC stop. The
    /// reason must be a static string because it is stored for the lifetime of
    /// the shutdown transaction.
    pub fn request_shutdown_signal_with_reason(&self, close_reason: &'static str) {
        *self.shutdown_close_reason.lock() = close_reason;
        self.request_shutdown_signal();
    }

    /// Suspend new business admission without starting the shutdown
    /// transaction.
    ///
    /// This is the owner-bound `OwnerMissing` action: every registered route
    /// scheduler closes so new requests are rejected, later route
    /// registrations fail, and existing work continues to drain under the
    /// original rules. The listener stays open and the lifecycle state is
    /// unchanged; the full shutdown transaction still has to run separately.
    /// Returns the number of routes whose admission was closed by this call
    /// (already-closed routes are not counted again).
    pub async fn close_business_admission(&self, reason: &str) -> usize {
        self.business_admission_closed
            .store(true, Ordering::Release);
        let route_state_reason = close_reason_to_route_state_reason(reason);
        let routes = {
            let _guard = self.route_registration.lock().await;
            let dispatcher = self.dispatcher.read().await;
            dispatcher.routes_snapshot()
        };
        let mut closed = 0usize;
        for route in &routes {
            let scheduler_closed = !route.scheduler.snapshot().closed;
            if scheduler_closed {
                route.scheduler.close();
            }
            let _ = self
                .route_catalog
                .write()
                .close_route(&route.name, route_state_reason.clone());
            if scheduler_closed {
                closed += 1;
            }
        }
        closed
    }

    /// Whether [`Server::close_business_admission`] suspended new business
    /// admission on this server.
    pub fn business_admission_closed(&self) -> bool {
        self.business_admission_closed.load(Ordering::Acquire)
    }

    /// Reject a start attempt that has not published readiness yet.
    ///
    /// This moves an unpolled `Starting` or cancelled `Stopping` attempt to `Failed`, which makes every
    /// readiness waiter fail fast with `message`. It exists for owners that
    /// must refuse to serve before readiness — for example an owner-bound
    /// host whose controller capability is already gone. It never touches a
    /// server that is running or already terminal.
    pub fn reject_start_attempt(&self, message: String) -> Result<(), ServerError> {
        match self.lifecycle_state() {
            ServerLifecycleState::Starting | ServerLifecycleState::Stopping
                if !self.run_entered.load(Ordering::Acquire) =>
            {
                self.set_lifecycle_state(ServerLifecycleState::Failed(message));
                Ok(())
            }
            state => Err(ServerError::Config(format!(
                "cannot reject a start attempt while lifecycle state is {state:?}",
            ))),
        }
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
            self.shutdown_tx.send_replace(true);
        }
    }
}

async fn ping_server_endpoint(endpoint: &LocalEndpoint) -> bool {
    let mut stream = match LocalStream::connect(endpoint, DEFAULT_CONNECT_TIMEOUT).await {
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

#[cfg(test)]
fn parse_local_endpoint(address: &str) -> Result<LocalEndpoint, ServerError> {
    parse_local_endpoint_with_protocol(address, c2_config::LocalEndpointProtocol::LegacyV1)
}

fn parse_local_endpoint_with_protocol(
    address: &str,
    protocol: c2_config::LocalEndpointProtocol,
) -> Result<LocalEndpoint, ServerError> {
    // The resolved server config selects the endpoint protocol. A managed-v2
    // request on a platform that cannot serve it is a normalized configuration
    // error at construction, never a silent legacy fallback.
    LocalEndpoint::from_address_with_protocol(address, protocol).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
        ) {
            ServerError::Config(error.to_string())
        } else {
            ServerError::Io(error)
        }
    })
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

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    // A start attempt publishes false even when the channel was already false.
    // Only true may cancel a partial read or stop GC; a version change alone
    // is not a shutdown request.
    let _ = receiver.wait_for(|requested| *requested).await;
}

async fn wait_for_control_peer_close(reader: &mut LocalReadHalf) {
    // The control client consumes the complete acknowledgement and closes its
    // one-exchange connection. Retain the pipe until that EOF where possible.
    // A timeout or unexpected byte only ends the wait; neither proves that the
    // peer consumed the acknowledgement.
    let mut byte = [0_u8; 1];
    let _ = tokio::time::timeout(
        Duration::from_millis(CONTROL_ACK_PEER_CLOSE_TIMEOUT_MS),
        reader.read(&mut byte),
    )
    .await;
}

async fn handle_post_shutdown_connection(
    server: Arc<Server>,
    conn: Arc<Connection>,
    stream: LocalStream,
) {
    let conn_id = conn.conn_id();
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
        handle_shutdown_signal(&server, payload, header.request_id, &writer).await;
        wait_for_control_peer_close(&mut reader).await;
    }
}

/// Registration exists before the task is spawned, so shutdown also waits
/// for accepted connections whose handler has not yet been polled.
struct RegisteredConnection {
    server: Arc<Server>,
    conn: Arc<Connection>,
    stream: Option<LocalStream>,
}

impl Drop for RegisteredConnection {
    fn drop(&mut self) {
        // Also close an accepted stream if its future was never polled.
        drop(self.stream.take());
        self.server
            .unregister_active_connection(self.conn.conn_id());
    }
}

fn handle_connection(
    server: Arc<Server>,
    stream: LocalStream,
) -> impl std::future::Future<Output = ()> + Send {
    let conn_id = server.conn_counter.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(Connection::new(conn_id));
    server.register_active_connection(Arc::clone(&conn));
    let registration = RegisteredConnection {
        server,
        conn,
        stream: Some(stream),
    };
    async move {
        let mut registration = registration;
        let server = Arc::clone(&registration.server);
        let conn = Arc::clone(&registration.conn);
        let stream = registration
            .stream
            .take()
            .expect("registered connection stream");
        let mut shutdown_rx = server.shutdown_tx.subscribe();
        if *shutdown_rx.borrow() {
            handle_post_shutdown_connection(server, conn, stream).await;
            return;
        }

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
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
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
                _ = wait_for_shutdown(&mut shutdown_rx) => break,
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
                    handle_shutdown_signal(&server, payload, request_id, &writer).await;
                    wait_for_control_peer_close(&mut reader).await;
                }
                break;
            }

            // 4. Dispatch by frame type.
            if header.is_handshake() {
                if let Err(e) = handle_handshake(&server, &conn, payload, request_id, &writer).await
                {
                    warn!(conn_id, %e, "handshake failed");
                    break;
                }
            } else if header.is_signal() {
                if matches!(
                    payload.first().and_then(|&b| MsgType::from_byte(b)),
                    Some(MsgType::ShutdownClient)
                ) {
                    handle_shutdown_signal(&server, payload, request_id, &writer).await;
                    wait_for_control_peer_close(&mut reader).await;
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
                    let chunk_processing_permit = match server.try_acquire_chunk_processing_permit()
                    {
                        Ok(permit) => permit,
                        Err(limit) => {
                            if c2_wire::flags::is_buddy(flags) {
                                cleanup_buddy_request_block(&conn, payload);
                            }
                            // The frame cannot be processed, so the request is
                            // terminal: publish the abort and release (or fence)
                            // any pending admission, assembly, and route work
                            // for this request before correlating the refusal.
                            // A first-chunk task still parked in route
                            // admission wakes and rolls back; it can never
                            // publish after this terminal outcome.
                            server.abort_chunk_request(conn.conn_id(), request_id);
                            write_chunk_processing_capacity_error(&writer, request_id, limit).await;
                            continue;
                        }
                    };
                    // Frame dispatch is the wire-order point: establish (for a
                    // first chunk) or join (for a later chunk) the request's
                    // admission ordering before spawning the chunk task, so a
                    // later chunk can never race ahead of first-chunk admission.
                    let ordering = match chunk_frame_ordering(
                        &server,
                        conn.conn_id(),
                        request_id,
                        flags,
                        payload,
                    ) {
                        Ok(ordering) => ordering,
                        Err(error) => {
                            if c2_wire::flags::is_buddy(flags) {
                                cleanup_buddy_request_block(&conn, payload);
                            }
                            drop(chunk_processing_permit);
                            warn!(
                                conn_id = conn.conn_id(),
                                request_id, %error, "chunk admission ordering rejected"
                            );
                            // A duplicate first chunk (AlreadyPending) or gate
                            // capacity means this request cannot be admitted:
                            // use the same terminal reasoning as the
                            // chunk-processing capacity refusal above, which
                            // also tears down an already-published assembly
                            // and frees its stored route admission.
                            server.abort_chunk_request(conn.conn_id(), request_id);
                            write_reply(
                                &writer,
                                request_id,
                                &ReplyControl::Error(error_wire(
                                    ErrorCode::ResourceUnavailable,
                                    error.to_string(),
                                )),
                            )
                            .await;
                            continue;
                        }
                    };
                    spawn_chunked_call(
                        &server,
                        &conn,
                        request_id,
                        flags,
                        payload,
                        &writer,
                        chunk_processing_permit,
                        ordering,
                    );
                    continue;
                }
                if c2_wire::flags::is_buddy(flags) {
                    let (ctrl, ctrl_consumed) =
                        match decode_call_control(payload, BUDDY_PAYLOAD_SIZE) {
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
                        match reserve_route_execution(&server, &ctrl.identity, ctrl.method_idx)
                            .await
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
                        dispatch_admitted_buddy_call(AdmittedBuddyCall {
                            server: &srv,
                            conn: &cn,
                            request_id,
                            payload: &pl,
                            ctrl_consumed,
                            route,
                            method_idx,
                            writer: &wr,
                            _pending_permit: pending_permit,
                            route_pending_permit,
                        })
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
                    match reserve_route_execution(&server, &ctrl.identity, ctrl.method_idx).await {
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
                    dispatch_admitted_call(AdmittedCall {
                        server: &srv,
                        conn: &cn,
                        request_id,
                        payload: &pl,
                        control_consumed: ctrl_consumed,
                        route,
                        method_idx,
                        writer: &wr,
                        _pending_permit: pending_permit,
                        route_pending_permit,
                    })
                    .await;
                });
            } else {
                warn!(conn_id, flags, "unknown frame type");
            }
        }

        debug!(conn_id, "connection closing, draining in-flight");
        hb_handle.abort();
        let _ = hb_handle.await;
        conn.cancel_queued_work();
        conn.wait_idle().await;
        server.cleanup_chunk_requests_for_connection(conn_id);
        debug!(conn_id, "connection closed");
        // Halves and their pending I/O are dropped before the registration guard.
        drop(writer);
        drop(reader);
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

async fn handle_handshake(
    server: &Server,
    conn: &Connection,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
            if let Some(name) = pool.segment_name(i)
                && let Some(seg) = pool.segment(i)
            {
                segs.push((name.to_string(), seg.allocator().data_size() as u32));
            }
        }
        let prefix = pool.prefix().to_string();
        (segs, prefix)
    };

    let routes: Vec<RouteInfo> = server
        .route_catalog
        .read()
        .list(&RouteSelector::All)
        .map_err(ServerError::Protocol)?
        .routes
        .into_iter()
        .map(route_info_from_record)
        .collect();

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
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
) {
    let Some(msg_type) = payload.first().and_then(|&b| MsgType::from_byte(b)) else {
        return;
    };
    let responses = match msg_type {
        MsgType::PendingRouteAttest => vec![pending_route_attestation_payload(server, payload)],
        MsgType::RouteContract => vec![route_contract_payload(server, payload).await],
        MsgType::RouteList => vec![route_list_payload(server, payload)],
        MsgType::RouteLookup => vec![route_lookup_payload(server, payload)],
        MsgType::RouteWatch => route_watch_payloads(server, payload),
        _ => {
            debug!(request_id, "unknown ctrl frame ignored");
            return;
        }
    };
    for response in responses {
        write_ctrl_response(writer, request_id, &response).await;
    }
}

fn route_info_from_record(record: RouteRecordWire) -> RouteInfo {
    RouteInfo {
        name: record.route_name,
        route_uid: record.route_uid,
        route_revision: record.route_revision,
        crm_ns: record.contract.crm_ns,
        crm_name: record.contract.crm_name,
        crm_ver: record.contract.crm_ver,
        abi_hash: record.contract.abi_hash,
        signature_hash: record.contract.signature_hash,
        max_payload_size: record.max_payload_size,
        methods: record
            .methods
            .into_iter()
            .map(|method| MethodEntry {
                name: method.name,
                index: method.index,
            })
            .collect(),
    }
}

fn route_attestation_from_record(record: RouteRecordWire) -> PendingRouteAttestation {
    PendingRouteAttestation {
        route_name: record.route_name,
        route_uid: record.route_uid,
        route_revision: record.route_revision,
        crm_ns: record.contract.crm_ns,
        crm_name: record.contract.crm_name,
        crm_ver: record.contract.crm_ver,
        abi_hash: record.contract.abi_hash,
        signature_hash: record.contract.signature_hash,
        method_names: record
            .methods
            .into_iter()
            .map(|method| method.name)
            .collect(),
        max_payload_size: record.max_payload_size,
    }
}

fn pending_route_attestation_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let response = match decode_pending_route_attestation_request(payload) {
        Ok(request) => {
            match server.attest_pending_route(&request.route_name, &request.registration_token) {
                Ok(info) => PendingRouteAttestationResponse::Attested {
                    contract: info.into_attestation(),
                },
                Err(response) => *response,
            }
        }
        Err(err) => PendingRouteAttestationResponse::Rejected {
            code: PENDING_ROUTE_REJECT_INVALID.to_string(),
            message: err,
        },
    };

    match encode_pending_route_attestation_response(&response) {
        Ok(payload) => payload,
        Err(err) => {
            let fallback = PendingRouteAttestationResponse::Rejected {
                code: PENDING_ROUTE_REJECT_INVALID.to_string(),
                message: err,
            };
            encode_pending_route_attestation_response(&fallback).unwrap_or_default()
        }
    }
}

fn route_catalog_nack_payload(server: &Server, rejected_revision: u64, message: String) -> Vec<u8> {
    let current_revision = server.route_catalog.read().catalog_revision();
    let nack = RouteNack {
        nonce: 0,
        rejected_revision: rejected_revision.max(current_revision),
        error: C2Error::new(ErrorCode::ProtocolViolation, message).envelope(),
    };
    encode_route_nack(&nack).unwrap_or_default()
}

fn route_list_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let request = match decode_route_list_request(payload) {
        Ok(request) => request,
        Err(err) => return route_catalog_nack_payload(server, 0, err),
    };
    let response = match server.route_catalog.read().list(&request.selector) {
        Ok(response) => response,
        Err(err) => {
            return route_catalog_nack_payload(server, request.min_revision.unwrap_or(0), err);
        }
    };
    encode_route_list_response(&response)
        .unwrap_or_else(|err| route_catalog_nack_payload(server, response.catalog_revision, err))
}

fn route_lookup_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let request = match decode_route_lookup_request(payload) {
        Ok(request) => request,
        Err(err) => return route_catalog_nack_payload(server, 0, err),
    };
    let response = match server.route_catalog.read().lookup(&request) {
        Ok(response) => response,
        Err(err) => {
            return route_catalog_nack_payload(
                server,
                request.observed_route_revision.unwrap_or(0),
                err,
            );
        }
    };
    encode_route_lookup_response(&response).unwrap_or_else(|err| {
        route_catalog_nack_payload(server, request.observed_route_revision.unwrap_or(0), err)
    })
}

fn route_watch_payloads(server: &Server, payload: &[u8]) -> Vec<Vec<u8>> {
    let request = match decode_route_watch_request(payload) {
        Ok(request) => request,
        Err(err) => return vec![route_catalog_nack_payload(server, 0, err)],
    };
    match server
        .route_catalog
        .read()
        .watch_from(request.from_revision, &request.selector)
    {
        RouteWatchBatch::Compacted {
            compacted_revision,
            current_revision,
        } => vec![
            encode_route_watch_event(&RouteWatchEvent::Compacted {
                compacted_revision,
                current_revision,
            })
            .unwrap_or_else(|err| route_catalog_nack_payload(server, current_revision, err)),
        ],
        RouteWatchBatch::Events {
            current_revision,
            events,
        } => {
            if events.is_empty() {
                if request.allow_heartbeat {
                    return vec![
                        encode_route_watch_event(&RouteWatchEvent::Heartbeat {
                            catalog_revision: current_revision,
                        })
                        .unwrap_or_else(|err| {
                            route_catalog_nack_payload(server, current_revision, err)
                        }),
                    ];
                }
                return Vec::new();
            }
            let mut payloads = events
                .into_iter()
                .map(|event| {
                    let revision = event_revision_for_nack(&event);
                    encode_route_watch_event(&event)
                        .unwrap_or_else(|err| route_catalog_nack_payload(server, revision, err))
                })
                .collect::<Vec<_>>();
            if request.allow_heartbeat {
                payloads.push(
                    encode_route_watch_event(&RouteWatchEvent::Heartbeat {
                        catalog_revision: current_revision,
                    })
                    .unwrap_or_else(|err| {
                        route_catalog_nack_payload(server, current_revision, err)
                    }),
                );
            }
            payloads
        }
    }
}

fn event_revision_for_nack(event: &RouteWatchEvent) -> u64 {
    match event {
        RouteWatchEvent::Added { record } | RouteWatchEvent::Updated { record } => {
            record.catalog_revision
        }
        RouteWatchEvent::Removed {
            catalog_revision, ..
        }
        | RouteWatchEvent::Closed {
            catalog_revision, ..
        }
        | RouteWatchEvent::Heartbeat {
            catalog_revision, ..
        } => *catalog_revision,
        RouteWatchEvent::Compacted {
            current_revision, ..
        } => *current_revision,
    }
}

async fn route_contract_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let response = match decode_route_contract_request(payload) {
        Ok(request) => {
            let catalog_response = server.route_catalog.read().list(&RouteSelector::RouteName {
                route_name: request.route_name.clone(),
            });
            match catalog_response
                .ok()
                .and_then(|response| response.routes.into_iter().next())
            {
                Some(record) => RouteContractResponse::Attested {
                    contract: route_attestation_from_record(record),
                },
                None => RouteContractResponse::Rejected {
                    code: ROUTE_CONTRACT_REJECT_NOT_FOUND.to_string(),
                    message: format!("route not found: {}", request.route_name),
                },
            }
        }
        Err(err) => RouteContractResponse::Rejected {
            code: ROUTE_CONTRACT_REJECT_INVALID.to_string(),
            message: err,
        },
    };

    match encode_route_contract_response(&response) {
        Ok(payload) => payload,
        Err(err) => {
            let fallback = RouteContractResponse::Rejected {
                code: ROUTE_CONTRACT_REJECT_INVALID.to_string(),
                message: err,
            };
            encode_route_contract_response(&fallback).unwrap_or_default()
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
    /// Identity of the assembly generation that stored this admission. Teardown
    /// releases exactly this generation through `ChunkRegistry::abort_id`, so a
    /// stale request teardown can never abort a successor's same-key assembly.
    assembly: c2_wire::chunk::ChunkAssemblyId,
    route: Arc<CrmRoute>,
    method_idx: u16,
    pending_permit: SchedulerPendingPermit,
}

enum RouteAdmissionError {
    RouteNotFound(String),
    RouteRemoved {
        route_name: String,
        route_uid: Option<String>,
    },
    RouteClosed {
        route_name: String,
        route_uid: String,
        reason: RouteStateReasonWire,
    },
    RouteStale {
        route_name: String,
        expected_uid: String,
        expected_revision: u64,
        actual_uid: String,
        actual_revision: u64,
    },
    ContractMismatch {
        route_name: String,
        reason: String,
    },
    UnknownMethod {
        route_name: String,
        method_idx: u16,
    },
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
    identity: &RouteCallIdentity,
    method_idx: u16,
) -> Result<RouteExecutionAdmission, RouteAdmissionError> {
    let lookup_request = RouteLookupRequest {
        expected: RouteContractWire {
            route_name: identity.route_name.clone(),
            crm_ns: identity.crm_ns.clone(),
            crm_name: identity.crm_name.clone(),
            crm_ver: identity.crm_ver.clone(),
            abi_hash: identity.abi_hash.clone(),
            signature_hash: identity.signature_hash.clone(),
        },
        observed_route_uid: Some(identity.route_uid.clone()),
        observed_route_revision: Some(identity.observed_route_revision),
    };
    match server
        .route_catalog
        .read()
        .lookup(&lookup_request)
        .map_err(|reason| RouteAdmissionError::ContractMismatch {
            route_name: identity.route_name.clone(),
            reason,
        })? {
        RouteLookupResponse::Ready { .. } => {}
        RouteLookupResponse::NotFound { route_name } => {
            return Err(RouteAdmissionError::RouteNotFound(route_name));
        }
        RouteLookupResponse::Removed {
            route_name,
            route_uid,
        } => {
            return Err(RouteAdmissionError::RouteRemoved {
                route_name,
                route_uid,
            });
        }
        RouteLookupResponse::Closed {
            route_name,
            route_uid,
            reason,
        } => {
            return Err(RouteAdmissionError::RouteClosed {
                route_name,
                route_uid,
                reason,
            });
        }
        RouteLookupResponse::Stale { current } => {
            return Err(RouteAdmissionError::RouteStale {
                route_name: identity.route_name.clone(),
                expected_uid: identity.route_uid.clone(),
                expected_revision: identity.observed_route_revision,
                actual_uid: current.route_uid,
                actual_revision: current.route_revision,
            });
        }
        RouteLookupResponse::ContractMismatch { .. } => {
            return Err(RouteAdmissionError::ContractMismatch {
                route_name: identity.route_name.clone(),
                reason: "expected route contract does not match current catalog record".to_string(),
            });
        }
    }
    let route = match server.dispatcher.read().await.resolve(&identity.route_name) {
        Some(route) => route,
        None => {
            return Err(RouteAdmissionError::RouteNotFound(
                identity.route_name.clone(),
            ));
        }
    };
    if route.route_uid != identity.route_uid
        || route.route_revision != identity.observed_route_revision
    {
        return Err(RouteAdmissionError::RouteStale {
            route_name: identity.route_name.clone(),
            expected_uid: identity.route_uid.clone(),
            expected_revision: identity.observed_route_revision,
            actual_uid: route.route_uid.clone(),
            actual_revision: route.route_revision,
        });
    }
    if route.crm_ns != identity.crm_ns
        || route.crm_name != identity.crm_name
        || route.crm_ver != identity.crm_ver
        || route.abi_hash != identity.abi_hash
        || route.signature_hash != identity.signature_hash
    {
        return Err(RouteAdmissionError::ContractMismatch {
            route_name: identity.route_name.clone(),
            reason: format!(
                "expected {}/{}/{} abi_hash={} signature_hash={}, got {}/{}/{} abi_hash={} signature_hash={}",
                identity.crm_ns,
                identity.crm_name,
                identity.crm_ver,
                identity.abi_hash,
                identity.signature_hash,
                route.crm_ns,
                route.crm_name,
                route.crm_ver,
                route.abi_hash,
                route.signature_hash,
            ),
        });
    }
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
                &ReplyControl::Error(error_wire(ErrorCode::ResourceClosed, "route closed")),
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
    request_id: u64,
    err: RouteAdmissionError,
) {
    match err {
        RouteAdmissionError::RouteNotFound(route_name) => {
            write_reply(writer, request_id, &ReplyControl::RouteNotFound(route_name)).await;
        }
        RouteAdmissionError::RouteRemoved {
            route_name,
            route_uid,
        } => {
            let message = match route_uid {
                Some(route_uid) => {
                    format!("route {route_name} was removed: route_uid {route_uid}")
                }
                None => format!("route {route_name} was removed"),
            };
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceRemoved, message)),
            )
            .await;
        }
        RouteAdmissionError::RouteClosed {
            route_name,
            route_uid,
            reason,
        } => {
            let message =
                format!("route {route_name} is closed: route_uid {route_uid}, reason {reason:?}");
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceClosed, message)),
            )
            .await;
        }
        RouteAdmissionError::RouteStale {
            route_name,
            expected_uid,
            expected_revision,
            actual_uid,
            actual_revision,
        } => {
            let message = format!(
                "stale route token for {route_name}: expected route_uid {expected_uid} revision {expected_revision}, current route_uid {actual_uid} revision {actual_revision}"
            );
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::RouteStale, message)),
            )
            .await;
        }
        RouteAdmissionError::ContractMismatch { route_name, reason } => {
            let message = format!("contract mismatch for route {route_name}: {reason}");
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ContractMismatch, message)),
            )
            .await;
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
                &ReplyControl::Error(error_wire(ErrorCode::ResourceClosed, "route closed")),
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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

struct AdmittedCall<'a> {
    server: &'a Server,
    conn: &'a Connection,
    request_id: u64,
    payload: &'a [u8],
    control_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &'a Arc<Mutex<LocalWriteHalf>>,
    _pending_permit: OwnedSemaphorePermit,
    route_pending_permit: SchedulerPendingPermit,
}

async fn dispatch_admitted_call(call: AdmittedCall<'_>) {
    let AdmittedCall {
        server,
        conn,
        request_id,
        payload,
        control_consumed,
        route,
        method_idx,
        writer,
        _pending_permit,
        route_pending_permit,
    } = call;
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
    pending_permit: OwnedSemaphorePermit,
) {
    let (ctrl, consumed) = match decode_call_control(payload, 0) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "call control decode error");
            return;
        }
    };

    let admission = match reserve_route_execution(server, &ctrl.identity, ctrl.method_idx).await {
        Ok(admission) => admission,
        Err(err) => {
            write_route_admission_error(writer, request_id, err).await;
            return;
        }
    };

    dispatch_admitted_call(AdmittedCall {
        server,
        conn,
        request_id,
        payload,
        control_consumed: consumed,
        route: admission.route,
        method_idx: ctrl.method_idx,
        writer,
        _pending_permit: pending_permit,
        route_pending_permit: admission.pending_permit,
    })
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
    let free_result = conn.free_peer_block(
        bp.seg_idx,
        bp.generation,
        bp.offset,
        bp.data_size,
        bp.is_dedicated,
    );
    schedule_peer_buddy_gc_if_idle(conn, free_result);
}

struct AdmittedBuddyCall<'a> {
    server: &'a Server,
    conn: &'a Arc<Connection>,
    request_id: u64,
    payload: &'a [u8],
    ctrl_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &'a Arc<Mutex<LocalWriteHalf>>,
    _pending_permit: OwnedSemaphorePermit,
    route_pending_permit: SchedulerPendingPermit,
}

async fn dispatch_admitted_buddy_call(call: AdmittedBuddyCall<'_>) {
    let AdmittedBuddyCall {
        server,
        conn,
        request_id,
        payload,
        ctrl_consumed,
        route,
        method_idx,
        writer,
        _pending_permit,
        route_pending_permit,
    } = call;
    let _flight = crate::connection::FlightGuard::new(conn.as_ref());

    // 1. Decode the versioned buddy backing reference.
    let (bp, _bp_consumed) = match decode_buddy_payload(payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "buddy payload decode error");
            return;
        }
    };

    // 2. Ensure peer SHM segment is mapped and get pool Arc (single lock).
    let peer_pool = match conn.ensure_and_get_peer_pool(
        bp.seg_idx,
        bp.generation,
        bp.data_size,
        bp.is_dedicated,
    ) {
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
            generation: bp.generation,
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
        let args = match conn.read_peer_data(
            bp.seg_idx,
            bp.generation,
            bp.offset,
            bp.data_size,
            bp.is_dedicated,
        ) {
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
        let free_result = conn.free_peer_block(
            bp.seg_idx,
            bp.generation,
            bp.offset,
            bp.data_size,
            bp.is_dedicated,
        );
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

/// How one chunked call frame relates to its request's first-chunk admission.
enum ChunkFrameOrdering {
    /// This frame is the first chunk and owns admission readiness.
    First(ChunkAdmissionOwner),
    /// A later chunk whose request is still being admitted.
    Wait(ChunkAdmissionWaiter),
    /// No ordering applies: feed directly (assembly already published, request
    /// already terminal, or a malformed header the chunk task reports).
    Direct,
}

/// Spawn the chunk task for one frame after dispatch claimed its permit and
/// ordering. Kept separate from [`dispatch_chunked_call`] so the receive loop
/// stays a wire-order dispatcher that never awaits a chunk's admission.
#[allow(clippy::too_many_arguments)] // A chunk frame needs the full dispatch context.
fn spawn_chunked_call(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    request_id: u64,
    flags: u32,
    payload: &[u8],
    writer: &Arc<Mutex<LocalWriteHalf>>,
    chunk_processing_permit: OwnedSemaphorePermit,
    ordering: ChunkFrameOrdering,
) {
    let srv = Arc::clone(server);
    let cn = Arc::clone(conn);
    let wr = Arc::clone(writer);
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
            ordering,
        )
        .await;
    });
}

/// Ordering decision for one chunked frame, made at frame dispatch.
///
/// The receive loop is the only place with wire order, so first-chunk ownership
/// is claimed here — before the chunk task is spawned — and later chunks join
/// the pending admission instead of feeding a not-yet-published assembly.
fn chunk_frame_ordering(
    server: &Server,
    conn_id: u64,
    request_id: u64,
    flags: u32,
    payload: &[u8],
) -> Result<ChunkFrameOrdering, ChunkOrderingError> {
    let offset = if c2_wire::flags::is_buddy(flags) {
        BUDDY_PAYLOAD_SIZE
    } else {
        0
    };
    let (chunk_idx, _, _) = match decode_chunk_header(payload, offset) {
        Ok(header) => header,
        // Malformed chunk header: establish no ordering; the chunk task
        // reports the malformed frame with a correlated error.
        Err(_) => return Ok(ChunkFrameOrdering::Direct),
    };
    if chunk_idx == 0 {
        server
            .begin_chunk_admission(conn_id, request_id)
            .map(ChunkFrameOrdering::First)
    } else {
        Ok(match server.chunk_admission_waiter(conn_id, request_id) {
            Some(waiter) => ChunkFrameOrdering::Wait(waiter),
            None => ChunkFrameOrdering::Direct,
        })
    }
}

/// Fail one chunked request: publish the terminal admission outcome first (so a
/// first-chunk task still parked in route admission wakes and cannot publish
/// afterwards), release the assembly, the stored route admission, and the
/// ordering entry, then write the correlated structured error for the caller
/// that is waiting on this request id. Every malformed/feed/admission failure
/// path funnels through here so a caller can never be left waiting.
async fn fail_chunked_request(
    server: &Server,
    conn: &Arc<Connection>,
    writer: &Arc<Mutex<LocalWriteHalf>>,
    request_id: u64,
    mut admission_owner: Option<ChunkAdmissionOwner>,
    code: ErrorCode,
    message: String,
) {
    warn!(
        conn_id = conn.conn_id(),
        request_id, %message, "chunked call failed"
    );
    // Terminal outcome first: waiters stop, and the owner's late commit loses
    // the atomic admission transition. The owner keeps the gate entry as a
    // teardown fence until its release below.
    if let Some(owner) = admission_owner.as_mut() {
        owner.refuse();
    }
    server.abort_chunk_request(conn.conn_id(), request_id);
    if let Some(owner) = admission_owner.as_mut() {
        owner.release();
    }
    write_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(code, message)),
    )
    .await;
}

/// Unpublished first-chunk admission publication.
///
/// A first chunk publishes two pieces of request state before it can commit its
/// admission ordering entry: the registry assembly and the stored route
/// admission (which owns the route pending permit). This guard owns both until
/// [`ChunkPublicationGuard::commit`]; on any other exit (cancellation, panic, a
/// concurrent terminal abort that wins the commit race, or a publish failure)
/// Drop rolls them back exactly once.
///
/// Rollback order matters: the stored route admission is taken while the
/// assembly is still registered, so the key-scoped permit take cannot reach a
/// successor generation's record (a successor's insert cannot succeed until the
/// assembly it would replace is gone). The assembly abort is identity-checked,
/// so it can never release a successor's same-key assembly.
struct ChunkPublicationGuard<'a> {
    server: &'a Server,
    conn_id: u64,
    request_id: u64,
    assembly: Option<c2_wire::chunk::ChunkAssemblyId>,
    committed: bool,
}

impl<'a> ChunkPublicationGuard<'a> {
    fn new(server: &'a Server, conn_id: u64, request_id: u64) -> Self {
        Self {
            server,
            conn_id,
            request_id,
            assembly: None,
            committed: false,
        }
    }

    /// Note a successful registry admission. Until `commit`, Drop releases it.
    fn publish_assembly(&mut self, assembly: c2_wire::chunk::ChunkAssemblyId) {
        self.assembly = Some(assembly);
    }

    /// The admitted request now owns everything this guard published.
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for ChunkPublicationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // A generation that never admitted an assembly has no published state
        // of its own: leave whatever assembly / stored admission already exists
        // for this key to the request-teardown path (`abort_chunk_request`),
        // which releases it before this owner's gate entry disappears.
        if self.assembly.is_some() {
            // Take the stored route admission first: the assembly is still
            // registered, so a same-key successor cannot have stored its own
            // record yet. Then abort exactly this generation's assembly.
            let _ = self
                .server
                .take_chunk_route_pending(self.conn_id, self.request_id);
        }
        if let Some(assembly) = self.assembly.take() {
            self.server.chunk_registry.abort_id(assembly);
        }
    }
}

#[allow(clippy::too_many_arguments)] // Chunk frames need the full dispatch context.
async fn dispatch_chunked_call(
    server: &Server,
    conn: &Arc<Connection>,
    request_id: u64,
    flags: u32,
    payload: &[u8],
    writer: &Arc<Mutex<LocalWriteHalf>>,
    chunk_processing_permit: OwnedSemaphorePermit,
    ordering: ChunkFrameOrdering,
) {
    // Ordering: a later chunk waits for its request's first-chunk admission
    // (route permit, registry assembly, stored route admission) before it can
    // touch the registry. The wait is async and scoped to one request, so the
    // receive loop, control frames, and other requests keep progressing.
    let mut admission_owner = match ordering {
        ChunkFrameOrdering::First(owner) => Some(owner),
        ChunkFrameOrdering::Wait(waiter) => {
            let discard = match waiter.wait().await {
                ChunkAdmissionOutcome::Admitted => None,
                // Refused: the admission owner already wrote the correlated
                // error. Aborted: the request or connection is terminal.
                // Either way this later chunk stops without a duplicate reply,
                // but it must still release a buddy-backed frame's peer block
                // instead of leaking the SHM allocation.
                ChunkAdmissionOutcome::Refused | ChunkAdmissionOutcome::Aborted => Some(
                    "chunk admission ended before this later chunk could feed",
                ),
                ChunkAdmissionOutcome::Pending => {
                    debug_assert!(false, "chunk admission waiter returned while pending");
                    Some("chunk admission waiter returned while still pending")
                }
            };
            if let Some(reason) = discard {
                if c2_wire::flags::is_buddy(flags) {
                    debug!(
                        conn_id = conn.conn_id(),
                        request_id, reason, "releasing discarded buddy chunk frame"
                    );
                    cleanup_buddy_request_block(conn, payload);
                }
                return;
            }
            None
        }
        ChunkFrameOrdering::Direct => None,
    };

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
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceInputDeserializing,
                    format!("chunked call buddy payload decode failed: {e:?}"),
                )
                .await;
                return;
            }
        };
        offset = bp_consumed;
        match conn.read_peer_data(
            bp.seg_idx,
            bp.generation,
            bp.offset,
            bp.data_size,
            bp.is_dedicated,
        ) {
            Ok(data) => {
                let free_result = conn.free_peer_block(
                    bp.seg_idx,
                    bp.generation,
                    bp.offset,
                    bp.data_size,
                    bp.is_dedicated,
                );
                schedule_peer_buddy_gc_if_idle(conn, free_result);
                shm_data = Some(data);
            }
            Err(e) => {
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceInputDeserializing,
                    format!("chunked call SHM read failed: {e}"),
                )
                .await;
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
            fail_chunked_request(
                server,
                conn,
                writer,
                request_id,
                admission_owner.take(),
                ErrorCode::ResourceInputDeserializing,
                format!("chunked call chunk header decode failed: {e:?}"),
            )
            .await;
            return;
        }
    };
    offset += ch_consumed;

    // 3. On first chunk, decode call control and register with ChunkRegistry.
    if chunk_idx == 0 {
        let (ctrl, ctrl_consumed) = match decode_call_control(payload, offset) {
            Ok(v) => v,
            Err(e) => {
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceInputDeserializing,
                    format!("chunked call control decode failed: {e:?}"),
                )
                .await;
                return;
            }
        };
        let route_admission = {
            // The route/dispatcher gate can stall (a route transaction holds
            // the dispatcher write lock). A disconnect must not leave this
            // task, its chunk permit, or its ordering entry alive behind that
            // gate, so the wait is cancellable by the connection's teardown.
            //
            // The request's admission entry is the second fence: if another
            // path publishes a terminal outcome while this wait is in flight
            // (request abort, chunk-processing capacity refusal, duplicate
            // first chunk, disconnect), the wait wakes immediately. The
            // terminal publisher owns the caller reply, so this task publishes
            // nothing; a reservation that completes at the same instant is
            // rejected by the atomic admission commit below.
            enum AdmissionWait {
                Ready(Result<RouteExecutionAdmission, RouteAdmissionError>),
                Terminal,
                Cancelled,
            }
            let terminal_waiter = admission_owner
                .as_ref()
                .map(ChunkAdmissionOwner::terminal_waiter);
            let terminal = async {
                match terminal_waiter {
                    Some(waiter) => waiter.wait().await,
                    None => std::future::pending::<ChunkAdmissionOutcome>().await,
                }
            };
            let waited = tokio::select! {
                biased;
                admission =
                    reserve_route_execution(server, &ctrl.identity, ctrl.method_idx) => {
                    AdmissionWait::Ready(admission)
                }
                _ = terminal => AdmissionWait::Terminal,
                _ = conn.wait_cancelled() => AdmissionWait::Cancelled,
            };
            match waited {
                AdmissionWait::Ready(Ok(admission)) => admission,
                AdmissionWait::Ready(Err(err)) => {
                    let owner_was_pending = admission_owner
                        .as_mut()
                        .map(|owner| {
                            let refused = owner.refuse();
                            owner.release();
                            refused
                        })
                        .unwrap_or(true);
                    if !owner_was_pending {
                        // A concurrent terminal outcome already owns the
                        // correlated reply; do not write a second one for the
                        // same request id.
                        return;
                    }
                    write_route_admission_error(writer, request_id, err).await;
                    return;
                }
                AdmissionWait::Terminal => return,
                AdmissionWait::Cancelled => {
                    if let Some(owner) = admission_owner.as_mut() {
                        owner.abort();
                        owner.release();
                    }
                    return;
                }
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
            fail_chunked_request(
                server,
                conn,
                writer,
                request_id,
                admission_owner.take(),
                ErrorCode::ProtocolViolation,
                "chunked call rejected: first chunk carries no data".to_string(),
            )
            .await;
            return;
        }

        // Staged publication: the registry assembly and the stored route
        // admission below are owned by this guard until the admission commit
        // succeeds. Cancellation, panic, a losing commit race, or a publish
        // failure rolls back exactly this generation (identity-checked assembly
        // abort, stored route admission returned) so no unpublished work stays
        // charged, and it can never touch a successor's same-key state.
        let mut publication = ChunkPublicationGuard::new(server, conn.conn_id(), request_id);
        let assembly = match server.chunk_registry.insert(
            conn.conn_id(),
            request_id,
            total_chunks as usize,
            chunk_size,
        ) {
            Ok(assembly) => assembly,
            Err(e) => {
                // Correlate the admission failure back to the pending caller
                // instead of logging and leaving it waiting. The message
                // carries the budget cell and size detail from the admission
                // error.
                drop(publication);
                let code = match &e {
                    ChunkAdmissionError::Capacity(_) => ErrorCode::ResourceUnavailable,
                    ChunkAdmissionError::Protocol(_) | ChunkAdmissionError::Duplicate { .. } => {
                        ErrorCode::ProtocolViolation
                    }
                };
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    code,
                    format!("chunked call reassembly admission failed: {e}"),
                )
                .await;
                return;
            }
        };
        publication.publish_assembly(assembly);
        server.chunk_registry.set_route_info(
            conn.conn_id(),
            request_id,
            ctrl.identity.route_name.clone(),
            ctrl.method_idx,
        );
        let stored = server.store_chunk_route_pending(
            conn.conn_id(),
            request_id,
            ChunkRouteAdmission {
                assembly,
                route: route_admission.route,
                method_idx: ctrl.method_idx,
                pending_permit: route_admission.pending_permit,
            },
        );
        if stored.is_err() {
            drop(publication);
            fail_chunked_request(
                server,
                conn,
                writer,
                request_id,
                admission_owner.take(),
                ErrorCode::ResourceUnavailable,
                "chunked call route admission was unexpectedly duplicated".to_string(),
            )
            .await;
            return;
        }
        // Admission commit: the atomic Pending → Admitted transition is the
        // linearization point. Losing it means a terminal outcome was published
        // concurrently; the guard rolls this generation's publications back and
        // the terminal publisher owns the caller's correlated error reply.
        let admitted = admission_owner
            .as_mut()
            .map(ChunkAdmissionOwner::admit)
            .unwrap_or(true);
        if !admitted {
            drop(publication);
            return;
        }
        publication.commit();
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
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceUnavailable,
                    format!("chunked call chunk feed failed: {e}"),
                )
                .await;
                return;
            }
        };

    // 6. If complete, finish and dispatch.
    if complete {
        let chunk_admission = match server.take_chunk_route_pending(conn.conn_id(), request_id) {
            Some(admission) => admission,
            None => {
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceUnavailable,
                    "chunked call missing route admission".to_string(),
                )
                .await;
                return;
            }
        };
        let finished = match server.chunk_registry.finish(conn.conn_id(), request_id) {
            Ok(f) => f,
            Err(e) => {
                fail_chunked_request(
                    server,
                    conn,
                    writer,
                    request_id,
                    admission_owner.take(),
                    ErrorCode::ResourceUnavailable,
                    format!("chunked call reassembly finish failed: {e}"),
                )
                .await;
                return;
            }
        };
        let c2_wire::chunk::FinishedChunk {
            backing,
            route_name,
            method_idx,
        } = finished;
        // The carrier owns the reassembly pool, handle, and budget charge;
        // its release ordering (storage through the pool authority first,
        // charge refund after) applies on every path below.
        let request = RequestData::Handle(backing);
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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

async fn write_reply(writer: &Arc<Mutex<LocalWriteHalf>>, request_id: u64, ctrl: &ReplyControl) {
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

async fn write_ctrl_response(writer: &Arc<Mutex<LocalWriteHalf>>, request_id: u64, payload: &[u8]) {
    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_CTRL, payload);
    if let Err(err) = writer.lock().await.write_all(&frame).await {
        warn!(request_id, error = %err, "failed to write ctrl response frame");
    }
}

/// Write a success reply: control header (STATUS_SUCCESS) + result data.
/// Uses stack buffer for small responses (≤1024B total frame) to avoid heap allocation.
async fn write_reply_with_data(
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
/// data, send 15-byte generation-bearing pointer metadata. The caller chooses inline or chunked
/// fallback when SHM is unavailable.
async fn write_buddy_reply_with_data(
    response_pool: &parking_lot::RwLock<MemPool>,
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
        generation: alloc.generation,
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
            generation,
            offset,
            data_size,
            is_dedicated,
        } => {
            if u64::from(data_size) > max_payload_size {
                let mut pool = response_pool.write();
                let _ = pool.free_at(seg_idx as u32, generation, offset, data_size, is_dedicated);
                return Err(ResponseSendError::UserVisible(format!(
                    "response payload size {data_size} exceeds max_payload_size {max_payload_size}"
                )));
            }
            // CRM already wrote into our response pool — send buddy pointer.
            let bp = BuddyPayload {
                seg_idx,
                generation,
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
                let _ = pool.free_at(seg_idx as u32, generation, offset, data_size, is_dedicated);
                return Err(ResponseSendError::Transport(format!(
                    "prepared SHM reply write failed: {err}"
                )));
            }

            // Server-side free for dedicated segments (same as write_buddy_reply_with_data).
            if is_dedicated {
                let mut pool = response_pool.write();
                let _ = pool.free_at(seg_idx as u32, generation, offset, data_size, true);
            }
        }
    }
    Ok(())
}

/// Choose buddy SHM or inline reply based on data size and threshold.
async fn smart_reply_with_data(
    response_pool: &parking_lot::RwLock<MemPool>,
    writer: &Arc<Mutex<LocalWriteHalf>>,
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
    use crate::RequestLease;

    // -- address parsing --

    #[test]
    fn parse_ipc_address() {
        let p = parse_local_endpoint("ipc://my_region").unwrap();
        assert_eq!(p.address(), "ipc://my_region");
    }

    #[test]
    fn parse_legacy_v3_rejected() {
        assert!(parse_local_endpoint("ipc-v3://region42").is_err());
    }

    #[test]
    fn parse_invalid_scheme() {
        assert!(parse_local_endpoint("tcp://host").is_err());
    }

    #[test]
    fn parse_empty_region() {
        assert!(parse_local_endpoint("ipc://").is_err());
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
                parse_local_endpoint(address).is_err(),
                "address should be rejected: {address:?}"
            );
        }
    }

    // -- server construction --

    #[test]
    fn server_new_default_config() {
        let s = Server::new("ipc://test_srv", ServerIpcConfig::default()).unwrap();
        assert_eq!(s.local_endpoint().address(), "ipc://test_srv");
    }

    #[tokio::test]
    async fn server_restart_reuses_the_resolved_endpoint_protocol() {
        // A restart bind must not drift to another endpoint namespace: the one
        // resolved protocol is the only derivation both binds use.
        let mut config = ServerIpcConfig::default();
        #[cfg(unix)]
        {
            config.base.endpoint_protocol = c2_config::LocalEndpointProtocol::ManagedV2;
        }
        #[cfg(windows)]
        {
            let mut unsupported = config.clone();
            unsupported.base.endpoint_protocol = c2_config::LocalEndpointProtocol::ManagedV2;
            let error = match Server::new("ipc://protocol_restart_unsupported", unsupported) {
                Err(error) => error,
                Ok(_) => panic!("managed-v2 must be rejected before a Windows server is created"),
            };
            assert!(matches!(error, IpcError::Config(message)
                if message == "managed-v2 IPC endpoints are not supported on Windows"));
        }
        let expected_protocol = config.base.endpoint_protocol;
        let address = "ipc://protocol_restart_srv";
        let first = Server::new(address, config.clone()).unwrap();
        let first_endpoint = first.local_endpoint().clone();
        drop(first);

        let second = Server::new(address, config).unwrap();
        assert_eq!(second.local_endpoint(), &first_endpoint);
        assert_eq!(second.local_endpoint().protocol(), expected_protocol);
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

    async fn closed_writer() -> Arc<Mutex<LocalWriteHalf>> {
        let (client, server) = LocalStream::pair().await.unwrap();
        client.abort_handle().abort();
        drop(client);
        let (_reader, writer) = server.into_split();
        Arc::new(Mutex::new(writer))
    }

    async fn endpoint_connects(endpoint: &LocalEndpoint) -> bool {
        LocalStream::connect(endpoint, DEFAULT_CONNECT_TIMEOUT)
            .await
            .is_ok()
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
        assert!(endpoint_connects(server.local_endpoint()).await);

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
    async fn false_shutdown_updates_cannot_truncate_fragmented_handshake() {
        let server = Arc::new(
            Server::new(
                &unique_readiness_address("fragmented_start"),
                ServerIpcConfig {
                    heartbeat_interval_secs: 0.0,
                    ..ServerIpcConfig::default()
                },
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
        let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .unwrap();
        let payload = c2_wire::handshake::encode_client_handshake(&[], CAP_CALL_V2, "").unwrap();
        let frame = encode_frame(0, FLAG_HANDSHAKE, &payload);
        for bytes in frame.chunks(3) {
            client.write_all(bytes).await.unwrap();
            // Startup resets shutdown to false. Repeated observations of that
            // state must never cancel a partly consumed protocol frame.
            server.shutdown_tx.send_replace(false);
            tokio::task::yield_now().await;
        }
        let mut header = [0_u8; frame::HEADER_SIZE];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        let (length, body) = frame::decode_total_len(&header).unwrap();
        let (header, _) = frame::decode_frame_body(body, length).unwrap();
        assert!(header.is_handshake());
        let mut payload = vec![0_u8; header.payload_len()];
        client.read_exact(&mut payload).await.unwrap();
        assert!(
            decode_handshake(&payload)
                .unwrap()
                .server_identity
                .is_some()
        );
        drop(client);
        server
            .shutdown_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        runner.await.unwrap().unwrap();
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
        assert!(endpoint_connects(first.local_endpoint()).await);

        let second = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
        let second_result = tokio::time::timeout(Duration::from_millis(200), {
            let second = Arc::clone(&second);
            async move { second.run().await }
        })
        .await;

        match second_result {
            Ok(Err(err)) => {
                assert!(
                    matches!(err, ServerError::Io(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
                    "unexpected error: {err}",
                );
            }
            Ok(Ok(())) => panic!("second server unexpectedly started and stopped cleanly"),
            Err(_) => {
                second.request_shutdown_signal();
                panic!("second server hung instead of rejecting the active socket");
            }
        }

        assert!(first.is_ready());
        assert!(endpoint_connects(first.local_endpoint()).await);
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
        assert!(endpoint_connects(first.local_endpoint()).await);

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
            matches!(err, ServerError::Io(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
            "unexpected error: {err}",
        );
        second.request_shutdown_signal();

        assert!(first.is_ready());
        assert!(endpoint_connects(first.local_endpoint()).await);
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
        assert!(endpoint_connects(server.local_endpoint()).await);

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
        assert!(endpoint_connects(server.local_endpoint()).await);

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
            matches!(failed, ServerError::Io(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
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
        assert!(endpoint_connects(second.local_endpoint()).await);

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
            route_uid: format!("{name}-uid-0001"),
            route_revision: 1,
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

    fn call_identity(name: &str) -> c2_wire::control::RouteCallIdentity {
        c2_wire::control::RouteCallIdentity {
            route_name: name.into(),
            route_uid: format!("{name}-uid-0001"),
            observed_route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
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
    async fn route_list_ctrl_reads_authoritative_catalog() {
        use c2_wire::route_catalog_control::{
            RouteListRequest, RouteSelector, decode_route_list_response, encode_route_list_request,
        };

        let server =
            Arc::new(Server::new("ipc://route_list_catalog", ServerIpcConfig::default()).unwrap());
        server.register_route(make_route("grid")).await.unwrap();
        let request = RouteListRequest {
            selector: RouteSelector::All,
            min_revision: None,
        };

        let payload = route_list_payload(&server, &encode_route_list_request(&request).unwrap());
        let response = decode_route_list_response(&payload).unwrap();

        assert_eq!(response.catalog_revision, 1);
        assert_eq!(response.min_watch_revision, 1);
        assert_eq!(response.routes.len(), 1);
        assert_eq!(response.routes[0].route_name, "grid");
        assert_eq!(response.routes[0].owner_server_id, "route_list_catalog");
    }

    #[tokio::test]
    async fn route_lookup_ctrl_returns_ready_from_catalog() {
        use c2_wire::route_catalog_control::{
            RouteLookupRequest, RouteLookupResponse, decode_route_lookup_response,
            encode_route_lookup_request,
        };

        let server =
            Arc::new(Server::new("ipc://route_lookup_ready", ServerIpcConfig::default()).unwrap());
        server.register_route(make_route("grid")).await.unwrap();
        let identity = call_identity("grid");
        let request = RouteLookupRequest {
            expected: RouteContractWire {
                route_name: identity.route_name,
                crm_ns: identity.crm_ns,
                crm_name: identity.crm_name,
                crm_ver: identity.crm_ver,
                abi_hash: identity.abi_hash,
                signature_hash: identity.signature_hash,
            },
            observed_route_uid: Some(identity.route_uid),
            observed_route_revision: Some(identity.observed_route_revision),
        };

        let payload =
            route_lookup_payload(&server, &encode_route_lookup_request(&request).unwrap());
        let response = decode_route_lookup_response(&payload).unwrap();

        match response {
            RouteLookupResponse::Ready { current } => {
                assert_eq!(current.route_name, "grid");
                assert_eq!(current.catalog_revision, 1);
            }
            other => panic!("expected ready lookup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_lookup_ctrl_returns_removed_tombstone_after_unregister() {
        use c2_wire::route_catalog_control::{
            RouteLookupRequest, RouteLookupResponse, decode_route_lookup_response,
            encode_route_lookup_request,
        };

        let server = Arc::new(
            Server::new("ipc://route_lookup_removed", ServerIpcConfig::default()).unwrap(),
        );
        server.register_route(make_route("grid")).await.unwrap();
        let identity = call_identity("grid");
        assert!(server.unregister_route("grid").await);
        let request = RouteLookupRequest {
            expected: RouteContractWire {
                route_name: identity.route_name,
                crm_ns: identity.crm_ns,
                crm_name: identity.crm_name,
                crm_ver: identity.crm_ver,
                abi_hash: identity.abi_hash,
                signature_hash: identity.signature_hash,
            },
            observed_route_uid: Some(identity.route_uid),
            observed_route_revision: Some(identity.observed_route_revision),
        };

        let payload =
            route_lookup_payload(&server, &encode_route_lookup_request(&request).unwrap());
        let response = decode_route_lookup_response(&payload).unwrap();

        match response {
            RouteLookupResponse::Removed {
                route_name,
                route_uid,
            } => {
                assert_eq!(route_name, "grid");
                assert_eq!(route_uid.as_deref(), Some("grid-uid-0001"));
            }
            other => panic!("expected removed lookup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_watch_ctrl_terminates_event_batch_with_heartbeat() {
        use c2_wire::route_catalog_control::{
            RouteSelector, RouteWatchEvent, RouteWatchRequest, decode_route_watch_event,
            encode_route_watch_request,
        };

        let server = Arc::new(
            Server::new("ipc://route_watch_heartbeat", ServerIpcConfig::default()).unwrap(),
        );
        server.register_route(make_route("grid")).await.unwrap();
        let request = RouteWatchRequest {
            from_revision: 0,
            selector: RouteSelector::All,
            allow_heartbeat: true,
        };

        let payloads =
            route_watch_payloads(&server, &encode_route_watch_request(&request).unwrap());

        assert_eq!(payloads.len(), 2);
        assert!(matches!(
            decode_route_watch_event(&payloads[0]).unwrap(),
            RouteWatchEvent::Added { .. }
        ));
        assert!(matches!(
            decode_route_watch_event(&payloads[1]).unwrap(),
            RouteWatchEvent::Heartbeat {
                catalog_revision: 1
            }
        ));
    }

    #[tokio::test]
    async fn reserve_rejects_same_name_while_removed_route_is_draining() {
        let server = Arc::new(
            Server::new(
                "ipc://route_reuse_while_draining",
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let route = make_route("grid");
        let scheduler = route.scheduler.as_ref().clone();
        server.register_route(route).await.unwrap();
        let active_guard = scheduler.try_acquire(0).unwrap();
        let unregister_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.unregister_route("grid").await })
        };
        while server.contains_route("grid").await {
            tokio::task::yield_now().await;
        }

        let replacement = make_route("grid");
        let replacement_handle =
            RouteConcurrencyHandle::new(replacement.scheduler.as_ref().clone());
        let err = match server
            .reserve_route(BuiltRoute::new(replacement, replacement_handle))
            .await
        {
            Ok(_) => panic!("catalog must reject same-name replacement while old route drains"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("route already registered"));
        drop(active_guard);
        assert!(unregister_task.await.unwrap());

        let replacement = make_route("grid");
        let replacement_handle =
            RouteConcurrencyHandle::new(replacement.scheduler.as_ref().clone());
        let reservation = server
            .reserve_route(BuiltRoute::new(replacement, replacement_handle))
            .await
            .expect("same-name reserve should succeed after old route is removed");
        server.abort_reserved_route(reservation).await;
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
            generation: alloc.generation,
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
        let (mut client_stream, server_stream) = LocalStream::pair().await.unwrap();
        let (_read_half, write_half) = server_stream.into_split();
        let writer = Arc::new(Mutex::new(write_half));
        let payload = encode_call_control(&call_identity("grid"), 99).unwrap();

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
    async fn inline_dispatch_rejects_stale_route_token_before_callback() {
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
            Arc::new(Server::new("ipc://stale_route_uid", ServerIpcConfig::default()).unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let mut identity = call_identity("grid");
        identity.observed_route_revision = 0;

        let conn = Connection::new(1);
        let (mut client_stream, server_stream) = LocalStream::pair().await.unwrap();
        let (_read_half, write_half) = server_stream.into_split();
        let writer = Arc::new(Mutex::new(write_half));
        let payload = encode_call_control(&identity, 0).unwrap();

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
                let decoded = C2Error::from_wire_bytes(&err)
                    .unwrap()
                    .expect("route stale must be encoded as a C2 error");
                assert_eq!(decoded.code, ErrorCode::RouteStale);
                assert!(decoded.message.contains("stale route token"));
                assert!(decoded.message.contains("route_uid grid-uid-0001"));
                assert!(decoded.message.contains("revision 0"));
                assert!(decoded.message.contains("revision 1"));
            }
            other => panic!("expected route-stale error reply, got {other:?}"),
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

        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        };
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
        let writer_a = closed_writer().await;
        let writer_b = closed_writer().await;
        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

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

        let config = ServerIpcConfig {
            max_execution_workers: 2,
            max_pending_requests: 8,
            ..ServerIpcConfig::default()
        };
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

            let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
            let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();
            let writer_a = closed_writer().await;
            let writer_a_waiter = closed_writer().await;
            let writer_b = closed_writer().await;

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

        let config = ServerIpcConfig {
            max_execution_workers: 1,
            max_pending_requests: 8,
            ..ServerIpcConfig::default()
        };
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

            let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
            let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();
            let writer_a = closed_writer().await;
            let writer_b = closed_writer().await;

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

        let _client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .expect("connect idle client");
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

        let payload = encode_call_control(&call_identity("grid"), 0).unwrap();
        let writer = closed_writer().await;
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
        let mut shutdown_rx = server.shutdown_tx.subscribe();

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

        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        };
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

        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();
        let writer_a = closed_writer().await;
        let writer_b = closed_writer().await;
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

        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        };
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

        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();
        let writer_a = closed_writer().await;
        let writer_b = closed_writer().await;
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

        let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .expect("connect client");
        client
            .write_all(&12u32.to_le_bytes())
            .await
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

        let config = ServerIpcConfig {
            max_pending_requests: 1,
            max_execution_workers: 2,
            ..ServerIpcConfig::default()
        };
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
        let writer_a = closed_writer().await;
        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

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
                let (mut client_stream, server_stream) = LocalStream::pair().await.unwrap();
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
        let writer = closed_writer().await;
        let malformed = [MsgType::ShutdownClient.as_byte(), 0x01, 0x02];

        handle_shutdown_signal(&server, &malformed, 99, &writer).await;

        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
        assert!(!*server.shutdown_tx.borrow());
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

        let (mut client, server_stream) = LocalStream::pair().await.expect("local stream pair");
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

        let (_client, server_stream) = LocalStream::pair().await.expect("local stream pair");

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

        let (mut client, server_stream) = LocalStream::pair().await.expect("local stream pair");
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

        let (mut client, server_stream) = LocalStream::pair().await.expect("local stream pair");
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
        let source = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let buddy_start = source
            .find(
                "if c2_wire::flags::is_buddy(flags) { let (ctrl, ctrl_consumed) = match decode_call_control(payload, BUDDY_PAYLOAD_SIZE)",
            )
            .expect("buddy branch must exist");
        let buddy_rest = &source[buddy_start..];
        let buddy_end = buddy_rest
            .find("continue; } let (ctrl, ctrl_consumed)")
            .expect("inline branch must follow buddy branch");
        let buddy_branch = &buddy_rest[..buddy_end];
        let buddy_reserve = buddy_branch
            .find("reserve_route_execution(&server, &ctrl.identity, ctrl.method_idx)")
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
            .find("} else { warn!(conn_id, flags, \"unknown frame type\");")
            .expect("unknown-frame branch must follow inline call branch");
        let inline_branch = &inline_rest[..inline_end];
        let inline_reserve = inline_branch
            .find("reserve_route_execution(&server, &ctrl.identity, ctrl.method_idx)")
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
        let mut rx = s.shutdown_tx.subscribe();
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
            generation: 1,
            offset: 4096,
            data_size: 256,
            is_dedicated: false,
        };
        let bp_bytes = encode_buddy_payload(&bp);
        let ctrl_bytes = encode_call_control(&call_identity("grid"), 1).unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(&bp_bytes);
        payload.extend_from_slice(&ctrl_bytes);

        // Decode buddy part.
        let (decoded_bp, bp_consumed) = decode_buddy_payload(&payload).unwrap();
        assert_eq!(decoded_bp, bp);
        assert_eq!(bp_consumed, BUDDY_PAYLOAD_SIZE);

        // Decode call control after buddy header.
        let (ctrl, _) = decode_call_control(&payload, BUDDY_PAYLOAD_SIZE).unwrap();
        assert_eq!(ctrl.identity.route_name, "grid");
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
        let (seg_idx, generation, offset, len) = match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                len,
                ..
            } => (seg_idx, generation, offset, len),
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
            generation,
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
            generation: u32,
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
                        generation,
                        offset,
                        data_size,
                        is_dedicated,
                    } => {
                        *self.seen.lock().unwrap() = Some(SeenShmRequest {
                            seg_idx,
                            generation,
                            offset,
                            data_size,
                            is_dedicated,
                        });
                        cleanup_request(RequestData::Shm {
                            pool,
                            seg_idx,
                            generation,
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
        let (seg_idx, generation, offset, len) = match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                len,
                ..
            } => (seg_idx, generation, offset, len),
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
            generation,
            offset,
            data_size: len as u32,
            is_dedicated: false,
        })
        .to_vec();
        let call_control = encode_call_control(&call_identity("grid"), 0).unwrap();
        let ctrl_consumed = call_control.len();
        payload.extend_from_slice(&call_control);

        let admission = match reserve_route_execution(&server, &call_identity("grid"), 0).await {
            Ok(admission) => admission,
            Err(_) => panic!("route admission should succeed"),
        };
        let pending_permit = server.try_acquire_pending_request().unwrap();
        let writer = closed_writer().await;
        dispatch_admitted_buddy_call(AdmittedBuddyCall {
            server: &server,
            conn: &conn,
            request_id: 77,
            payload: &payload,
            ctrl_consumed,
            route: admission.route,
            method_idx: 0,
            writer: &writer,
            _pending_permit: pending_permit,
            route_pending_permit: admission.pending_permit,
        })
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
                generation,
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
            ..PoolConfig::default()
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
        let mut finished = registry.finish(conn_id, request_id).unwrap();
        assert_eq!(finished.route_name.as_deref(), Some("grid"));
        assert_eq!(finished.method_idx, Some(0));
        assert_eq!(finished.backing.len(), 18); // 8+8+2
        let slice = finished.backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        // The carrier owns the pool; releasing it frees the backing.
        finished.backing.release().unwrap();
        assert_eq!(pool.read().stats().alloc_count, 0);
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
        let writer = closed_writer().await;
        let mut first_payload = Vec::new();
        first_payload.extend_from_slice(&encode_chunk_header(0, 2));
        first_payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
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
            chunk_frame_ordering(&server, conn.conn_id(), 42, FLAG_CHUNKED, &first_payload)
                .unwrap(),
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
            chunk_frame_ordering(
                &server,
                conn.conn_id(),
                42,
                FLAG_CHUNKED,
                &bad_second_payload,
            )
            .unwrap(),
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
        let writer = closed_writer().await;
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
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
            chunk_frame_ordering(&server, conn.conn_id(), 9, FLAG_CHUNKED, &payload).unwrap(),
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
    async fn chunked_request_admission_failure_writes_correlated_error_reply() {
        use crate::scheduler::SchedulerLimits;
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
        use std::num::NonZeroUsize;

        // Tiny reassembly budget: a 1024-byte assembly cannot be admitted.
        let budget = c2_mem::MemoryBudget::new(1 << 20, 1 << 20, 512);
        let reassembly_pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    segment_size: 64 * 1024,
                    min_block_size: 4096,
                    max_segments: 2,
                    max_dedicated_segments: 2,
                    dedicated_crash_timeout_secs: 0.0,
                    buddy_idle_decay_secs: 0.0,
                    spill_threshold: 1.0,
                    spill_dir: std::env::temp_dir().join("c2_srv_admission_test"),
                    ..PoolConfig::default()
                },
                "/c2srv_adm_budget".to_string(),
                budget,
            ),
        ));
        let server = Arc::new(
            Server::with_reassembly_pool(
                "ipc://chunk_admission_budget",
                ServerIpcConfig::default(),
                ServerIdentity {
                    server_id: "chunk_admission_budget".into(),
                    server_instance_id: "chunk_admission_budget-instance".into(),
                },
                reassembly_pool,
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

        // Open pair so the error reply can actually be read back.
        let (client, server_side) = c2_local::LocalStream::pair().await.unwrap();
        let (mut probe, _keep) = client.into_split();
        let (_read_half, write_half) = server_side.into_split();
        let writer = Arc::new(Mutex::new(write_half));

        let conn = Arc::new(Connection::new(123));
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
        payload.extend_from_slice(&[0x42u8; 512]);

        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            21,
            FLAG_CHUNKED,
            &payload,
            &writer,
            chunk_permit,
            chunk_frame_ordering(&server, conn.conn_id(), 21, FLAG_CHUNKED, &payload).unwrap(),
        )
        .await;

        // No assembly was published and the route admission permit was
        // released by the correlated rejection.
        assert!(!server.chunk_registry.contains(conn.conn_id(), 21));
        assert!(scheduler.try_acquire(0).is_ok());

        // Read the correlated error reply frame for request 21.
        let mut header = [0u8; c2_wire::frame::HEADER_SIZE];
        tokio::time::timeout(std::time::Duration::from_secs(5), probe.read_exact(&mut header))
            .await
            .expect("error reply within timeout")
            .unwrap();
        // total_len counts everything after the 4-byte prefix: the 12-byte
        // header (already read) plus the payload still to read.
        let (total_len, _) = c2_wire::frame::decode_total_len(&header).unwrap();
        let mut body = vec![0u8; total_len as usize - 12];
        tokio::time::timeout(std::time::Duration::from_secs(5), probe.read_exact(&mut body))
            .await
            .expect("error body within timeout")
            .unwrap();
        let mut frame_bytes = header.to_vec();
        frame_bytes.extend_from_slice(&body);
        let (hdr, payload_out) = c2_wire::frame::decode_frame(&frame_bytes).unwrap();
        assert_eq!(hdr.request_id, 21);
        assert!(hdr.is_response());
        let (control, _) = decode_reply_control(payload_out, 0).unwrap();
        let ReplyControl::Error(err_bytes) = control else {
            panic!("expected correlated error reply, got {control:?}");
        };
        let message = String::from_utf8_lossy(&err_bytes).to_string();
        assert!(
            message.contains("'reassembly'"),
            "error must name the budget cell: {message}"
        );
        assert!(
            message.contains("1024"),
            "error must name the rejected size: {message}"
        );
    }

    // -- Chunked first-chunk admission ordering (frame dispatch) --

    /// Callback that echoes the reassembled request bytes and counts invocations.
    struct EchoRequestBytes {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CrmCallback for EchoRequestBytes {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let bytes = RequestLease::new(request)
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    /// Read one complete reply frame from the client side of a local pair.
    async fn read_reply_frame(client: &mut LocalStream) -> (u64, u32, Vec<u8>) {
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut len_buf))
            .await
            .expect("reply length within timeout")
            .expect("reply length");
        let total_len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; total_len];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut body))
            .await
            .expect("reply body within timeout")
            .expect("reply body");
        let mut frame_bytes = Vec::with_capacity(4 + total_len);
        frame_bytes.extend_from_slice(&len_buf);
        frame_bytes.extend_from_slice(&body);
        let (header, payload) = c2_wire::frame::decode_frame(&frame_bytes).expect("decode reply");
        (header.request_id, header.flags, payload.to_vec())
    }

    /// Poll until `check` holds, with a bounded wait so a regression fails the
    /// test instead of hanging it.
    async fn wait_until(label: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached within timeout: {label}");
    }

    fn chunked_call_frame(
        request_id: u64,
        chunk_idx: u16,
        total_chunks: u16,
        data: &[u8],
        identity: &str,
    ) -> Vec<u8> {
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::encode_call_control;

        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(chunk_idx, total_chunks));
        if chunk_idx == 0 {
            payload.extend_from_slice(
                &encode_call_control(&call_identity(identity), 0).expect("call control"),
            );
        }
        payload.extend_from_slice(data);
        let mut flags = c2_wire::flags::FLAG_CALL_V2 | FLAG_CHUNKED;
        if chunk_idx + 1 == total_chunks {
            flags |= c2_wire::flags::FLAG_CHUNK_LAST;
        }
        encode_frame(request_id, flags, &payload)
    }

    fn ordering_test_server(address: &str, max_total_chunks: u32) -> Arc<Server> {
        let mut config = ServerIpcConfig::default();
        config.base.max_total_chunks = max_total_chunks;
        Arc::new(Server::new(address, config).unwrap())
    }

    /// Wire-order frame dispatch must not let later chunks run ahead of the
    /// first chunk's route admission. Hold the route/dispatcher gate, push a
    /// first chunk plus two later chunks through the real receive loop, prove
    /// nothing was published or discarded while the gate is held, prove control
    /// frames still work, then release the gate and prove every byte was
    /// delivered exactly once with all ordering/registry/permit state cleaned.
    #[tokio::test]
    async fn later_chunks_wait_for_stalled_first_chunk_route_admission() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_ordering_hold", 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        );
        route.scheduler = Arc::new(scheduler.clone());
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // Hold the route gate the first chunk must pass through.
        let dispatcher_guard = server.dispatcher.write().await;

        const TOTAL: usize = 3;
        const CHUNK: usize = 64;
        let data: Vec<u8> = (0..(2 * CHUNK + 7)).map(|i| (i % 251) as u8).collect();
        for idx in 0..TOTAL {
            let start = idx * CHUNK;
            let end = usize::min(start + CHUNK, data.len());
            client
                .write_all(&chunked_call_frame(77, idx as u16, TOTAL as u16, &data[start..end], "grid"))
                .await
                .expect("write chunk frame");
        }

        // Everything is parked: first chunk on the held gate, later chunks on
        // first-chunk admission, all three chunk-processing permits held.
        wait_until("three chunk frames dispatched", || {
            server.chunk_admission_gate.len() == 1
                && server.chunk_processing_permits.available_permits() == 0
        })
        .await;
        let conn_id = server.active_connection_ids()[0];
        assert!(
            !server.chunk_registry.contains(conn_id, 77),
            "later chunks must not publish or discard an assembly before first-chunk admission"
        );
        assert_eq!(server.chunk_registry.active_count(), 0);
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Control liveness while the route gate is stalled: the receive loop
        // still answers a ping on the same connection.
        client
            .write_all(&encode_frame(5, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES))
            .await
            .expect("write ping");
        let (ping_rid, ping_flags, ping_payload) = read_reply_frame(&mut client).await;
        assert_eq!(ping_rid, 5);
        assert!(ping_flags & FLAG_SIGNAL != 0);
        assert_eq!(ping_payload, PONG_BYTES);

        // Release the gate: the first chunk admits, the later chunks feed, and
        // the request is executed once with the exact reassembled bytes.
        drop(dispatcher_guard);
        let (reply_rid, reply_flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 77);
        assert!(reply_flags & FLAG_RESPONSE != 0);
        let (control, consumed) =
            c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
        match control {
            ReplyControl::Success => {}
            other => panic!("expected successful reply, got {other:?}"),
        }
        assert_eq!(&reply_payload[consumed..], data.as_slice());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one execution");

        // No ordering entry, assembly, stored admission, or permit is left.
        assert_eq!(server.chunk_admission_gate.len(), 0);
        assert_eq!(server.chunk_registry.active_count(), 0);
        assert!(!server.chunk_registry.contains(conn_id, 77));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(server.chunk_processing_permits.available_permits(), 3);
        assert!(scheduler.try_acquire(0).is_ok());

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// Route admission refusal while later chunks are parked on the ordering
    /// gate must wake them, complete the caller with the structured canonical
    /// error, and leave no ordering entry, assembly, or permit behind.
    #[tokio::test]
    async fn route_refusal_wakes_parked_later_chunks_and_releases_state() {
        let server = ordering_test_server("ipc://chunked_ordering_refusal", 3);
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        );
        route.scheduler = Arc::new(scheduler.clone());
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // First chunk targets a route that is not in the catalog, so admission
        // is refused immediately; later chunks must not feed anything.
        client
            .write_all(&chunked_call_frame(88, 0, 2, b"first-half", "missing"))
            .await
            .expect("write first chunk");
        client
            .write_all(&chunked_call_frame(88, 1, 2, b"second-half", "missing"))
            .await
            .expect("write later chunk");

        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 88);
        let (control, _) = c2_wire::control::decode_reply_control(&reply_payload, 0)
            .expect("reply control");
        match control {
            ReplyControl::RouteNotFound(route) => assert_eq!(route, "missing"),
            other => panic!("expected structured route-not-found reply, got {other:?}"),
        }

        // The refused request leaves no ordering entry, assembly, or permit.
        let conn_id = server.active_connection_ids()[0];
        wait_until("ordering state released after refusal", || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
        })
        .await;
        assert!(!server.chunk_registry.contains(conn_id, 88));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert!(scheduler.try_acquire(0).is_ok());

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// Disconnect while the first chunk is parked behind the route gate must
    /// tear the ordering state and every chunk permit down (cancellation), and
    /// must not leave the caller's request charged in the registry.
    #[tokio::test]
    async fn disconnect_cancels_parked_first_chunk_and_releases_permits() {
        let server = ordering_test_server("ipc://chunked_ordering_cancel", 3);
        let mut route = make_route("grid");
        route.scheduler = Arc::new(Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        ));
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // Park the first chunk on the route gate and two later chunks on its
        // admission; every chunk-processing permit is then held.
        let dispatcher_guard = server.dispatcher.write().await;
        for idx in 0..3usize {
            client
                .write_all(&chunked_call_frame(
                    99,
                    idx as u16,
                    3,
                    &[0x5au8; 32],
                    "grid",
                ))
                .await
                .expect("write chunk frame");
        }
        wait_until("chunk frames parked behind the route gate", || {
            server.chunk_admission_gate.len() == 1
                && server.chunk_processing_permits.available_permits() == 0
        })
        .await;
        let conn_id = server.active_connection_ids()[0];

        // Client disconnect: the receive loop cancels the connection, which
        // cancels the parked first-chunk task behind the held gate.
        drop(client);
        handler.await.expect("connection handler completes");

        wait_until("disconnect released ordering state and permits", || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
        })
        .await;
        assert!(!server.chunk_registry.contains(conn_id, 99));
        assert!(server.chunk_route_pending.lock().is_empty());
        drop(dispatcher_guard);
    }

    /// An incomplete chunked request that expires on the assembler timeout must
    /// release its assembly charge and its stored route pending permit, leave
    /// no ordering state behind, keep answering control frames, and still serve
    /// a complete request afterwards with every byte delivered once.
    #[tokio::test]
    async fn assembler_timeout_releases_charge_and_route_capacity_then_serves_again() {
        use std::sync::atomic::AtomicUsize;

        let mut config = ServerIpcConfig::default();
        config.base.max_total_chunks = 3;
        config.base.chunk_assembler_timeout_secs = 0.05;
        let server = Arc::new(Server::new("ipc://chunked_ordering_timeout", config).unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        );
        route.scheduler = Arc::new(scheduler.clone());
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // First chunk + middle chunk of a three-chunk request: the assembly
        // stays incomplete and keeps the route pending permit while it waits.
        for idx in 0..2usize {
            client
                .write_all(&chunked_call_frame(
                    55,
                    idx as u16,
                    3,
                    &[0x44u8; 32],
                    "grid",
                ))
                .await
                .expect("write chunk frame");
        }
        wait_until("incomplete assembly registered with route admission", || {
            server.chunk_registry.active_count() == 1
                && server.chunk_route_pending.lock().len() == 1
                && server.chunk_admission_gate.len() == 0
                && server.chunk_processing_permits.available_permits() == 3
        })
        .await;
        let conn_id = server.active_connection_ids()[0];
        assert!(matches!(
            scheduler.try_acquire(0),
            Err(crate::scheduler::SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            })
        ));

        // Control liveness while the incomplete assembly is still charged.
        client
            .write_all(&encode_frame(6, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES))
            .await
            .expect("write ping");
        let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
        assert_eq!(ping_rid, 6);
        assert_eq!(ping_payload, PONG_BYTES);

        // Let the assembler timeout elapse and run the same sweep the
        // background GC task runs.
        tokio::time::sleep(Duration::from_millis(90)).await;
        let stats = server.chunk_registry.gc_sweep();
        assert_eq!(stats.expired, 1);
        let swept = server.sweep_stale_chunk_route_pending();
        assert_eq!(swept, 1);

        assert_eq!(server.chunk_registry.active_count(), 0);
        assert!(!server.chunk_registry.contains(conn_id, 55));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(server.chunk_admission_gate.len(), 0);
        assert_eq!(server.chunk_processing_permits.available_permits(), 3);
        assert!(scheduler.try_acquire(0).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Control liveness after the timeout, and the connection still serves
        // a complete request with every byte delivered once.
        client
            .write_all(&encode_frame(7, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES))
            .await
            .expect("write ping");
        let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
        assert_eq!(ping_rid, 7);
        assert_eq!(ping_payload, PONG_BYTES);

        let data: Vec<u8> = (0..90u16).map(|i| (i % 251) as u8).collect();
        for idx in 0..3usize {
            client
                .write_all(&chunked_call_frame(
                    56,
                    idx as u16,
                    3,
                    &data[idx * 30..(idx + 1) * 30],
                    "grid",
                ))
                .await
                .expect("write chunk frame");
        }
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 56);
        let (control, consumed) =
            c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
        match control {
            ReplyControl::Success => {}
            other => panic!("expected successful reply after timeout cleanup, got {other:?}"),
        }
        assert_eq!(&reply_payload[consumed..], data.as_slice());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(client);
        handler.await.expect("connection handler completes");
    }

    // -- Reviewed server admission/ownership defects (recovery repair 2) --

    /// Decode a reply payload's structured error message.
    fn reply_error_message(payload: &[u8]) -> String {
        let (control, _) =
            c2_wire::control::decode_reply_control(payload, 0).expect("reply control");
        match control {
            ReplyControl::Error(err) => String::from_utf8_lossy(&err).to_string(),
            other => panic!("expected a structured error reply, got {other:?}"),
        }
    }

    /// A terminal chunk-processing capacity refusal while the first chunk is
    /// still parked in route admission must wake and fence the owner: after the
    /// stalled route gate is released there is no callback, no recreated
    /// assembly, no stored route admission, and no leaked permit.
    #[tokio::test]
    async fn capacity_refusal_fences_stalled_first_admission_against_post_abort_publication() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_terminal_capacity", 2);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        );
        route.scheduler = Arc::new(scheduler.clone());
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // Stall the first chunk's route admission and fill both chunk-processing
        // permits: the first chunk parks on the held dispatcher gate, one later
        // chunk parks on its admission.
        let dispatcher_guard = server.dispatcher.write().await;
        client
            .write_all(&chunked_call_frame(61, 0, 3, &[0x11u8; 32], "grid"))
            .await
            .expect("write first chunk");
        client
            .write_all(&chunked_call_frame(61, 1, 3, &[0x22u8; 32], "grid"))
            .await
            .expect("write later chunk");
        wait_until("chunk frames parked at capacity", || {
            server.chunk_admission_gate.len() == 1
                && server.chunk_processing_permits.available_permits() == 0
        })
        .await;

        // This frame cannot get a chunk-processing permit, so the request is
        // terminal: the refusal must fence the parked admission instead of
        // letting it publish when the route gate opens.
        client
            .write_all(&chunked_call_frame(61, 2, 3, &[0x33u8; 32], "grid"))
            .await
            .expect("write capacity-refused frame");
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 61);
        let message = reply_error_message(&reply_payload);
        assert!(
            message.contains("max_total_chunks=2"),
            "capacity refusal must name the chunk-processing bound: {message}"
        );

        // Release the stalled gate while the parked owner may still be inside
        // its route-admission await: even if the reservation completes, the
        // atomic admission commit must lose and the staged publication must
        // roll back.
        drop(dispatcher_guard);
        wait_until("terminal refusal released the parked admission", || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let conn_id = server.active_connection_ids()[0];
        assert!(!server.chunk_registry.contains(conn_id, 61));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no callback after refusal");
        assert!(
            scheduler.try_acquire(0).is_ok(),
            "the route pending permit must not stay charged"
        );
        assert_eq!(server.chunk_processing_permits.available_permits(), 2);

        // Control liveness on the same connection after the terminal refusal.
        client
            .write_all(&encode_frame(5, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES))
            .await
            .expect("write ping");
        let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
        assert_eq!(ping_rid, 5);
        assert_eq!(ping_payload, PONG_BYTES);

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// A malformed later chunk frame is a terminal request failure: it must
    /// publish the abort, wake the first chunk parked on its admission, release
    /// everything, and prevent publication when the route gate finally opens.
    #[tokio::test]
    async fn malformed_later_chunk_failure_fences_stalled_first_admission() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_terminal_malformed", 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        let dispatcher_guard = server.dispatcher.write().await;
        client
            .write_all(&chunked_call_frame(62, 0, 2, &[0x44u8; 32], "grid"))
            .await
            .expect("write first chunk");
        wait_until("first chunk parked on the held route gate", || {
            server.chunk_admission_gate.len() == 1
        })
        .await;

        // A chunked frame too short to hold a chunk header is terminal: its
        // dispatch reports the malformed frame and must tear the request down.
        client
            .write_all(&encode_frame(
                62,
                c2_wire::flags::FLAG_CALL_V2 | FLAG_CHUNKED,
                &[0x01, 0x02],
            ))
            .await
            .expect("write malformed later chunk");
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 62);
        let message = reply_error_message(&reply_payload);
        assert!(
            message.contains("chunk header decode failed"),
            "malformed chunk must be correlated: {message}"
        );

        drop(dispatcher_guard);
        wait_until("malformed-frame teardown released the parked admission", || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let conn_id = server.active_connection_ids()[0];
        assert!(!server.chunk_registry.contains(conn_id, 62));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// A duplicate first chunk over a pending admission refuses the duplicate
    /// frame and tears the request down: the parked owner publishes nothing,
    /// the caller gets exactly one correlated error, and every permit returns.
    #[tokio::test]
    async fn duplicate_first_chunk_refusal_aborts_pending_admission() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_duplicate_first", 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        let dispatcher_guard = server.dispatcher.write().await;
        client
            .write_all(&chunked_call_frame(81, 0, 2, &[0x66u8; 32], "grid"))
            .await
            .expect("write first chunk");
        wait_until("first admission pending", || {
            server.chunk_admission_gate.len() == 1
        })
        .await;

        client
            .write_all(&chunked_call_frame(81, 0, 2, &[0x77u8; 32], "grid"))
            .await
            .expect("write duplicate first chunk");
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 81);
        let message = reply_error_message(&reply_payload);
        assert!(
            message.contains("duplicate first chunk"),
            "duplicate first chunk must be refused: {message}"
        );

        drop(dispatcher_guard);
        wait_until("duplicate refusal released the request", || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let conn_id = server.active_connection_ids()[0];
        assert!(!server.chunk_registry.contains(conn_id, 81));
        assert!(server.chunk_route_pending.lock().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// A duplicate first chunk over an already-admitted request refuses the
    /// duplicate and tears the published generation down: the assembly is
    /// released immediately (not left until its later chunks or the assembler
    /// timeout) and its stored route admission returns.
    #[tokio::test]
    async fn duplicate_first_chunk_after_admission_aborts_the_published_assembly() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_duplicate_after_admission", 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        // The first chunk of a two-chunk request is admitted and its assembly
        // and stored route admission are published.
        client
            .write_all(&chunked_call_frame(82, 0, 2, &[0x21u8; 32], "grid"))
            .await
            .expect("write first chunk");
        wait_until("assembly published after admission", || {
            server.chunk_registry.active_count() == 1
                && server.chunk_route_pending.lock().len() == 1
        })
        .await;
        let conn_id = server.active_connection_ids()[0];
        assert!(server.chunk_registry.contains(conn_id, 82));

        // A duplicate first chunk for the same request id claims a fresh
        // admission entry (the previous one was released on admission), then
        // fails the registry duplicate check. The refusal must release the
        // published generation instead of stranding it.
        client
            .write_all(&chunked_call_frame(82, 0, 2, &[0x22u8; 32], "grid"))
            .await
            .expect("write duplicate first chunk");
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 82);
        let message = reply_error_message(&reply_payload);
        assert!(
            message.contains("duplicate assembly"),
            "duplicate admission must be correlated: {message}"
        );

        wait_until("duplicate refusal released the published generation", || {
            server.chunk_registry.active_count() == 0
                && server.chunk_route_pending.lock().is_empty()
                && server.chunk_processing_permits.available_permits() == 3
                && server.chunk_admission_gate.len() == 0
        })
        .await;
        assert!(!server.chunk_registry.contains(conn_id, 82));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        drop(client);
        handler.await.expect("connection handler completes");
    }

    /// Cancellation after the assembly/route publication but before the
    /// admission commit must roll the staged publication back exactly once.
    ///
    /// The owner below is already torn down when its route wait completes (the
    /// terminal abort won the atomic transition), which exercises the commit
    /// fence directly rather than the wake-up path.
    #[tokio::test]
    async fn lost_admission_commit_rolls_back_staged_publication_exactly_once() {
        use c2_mem::budget::BudgetKind;
        use c2_wire::chunk::encode_chunk_header;
        use c2_wire::control::encode_call_control;

        let budget = c2_mem::MemoryBudget::new(1 << 20, 1 << 20, 4096);
        let reassembly_pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    segment_size: 64 * 1024,
                    min_block_size: 4096,
                    max_segments: 2,
                    max_dedicated_segments: 2,
                    dedicated_crash_timeout_secs: 0.0,
                    buddy_idle_decay_secs: 0.0,
                    spill_threshold: 1.0,
                    spill_dir: std::env::temp_dir().join("c2_srv_commit_fence_test"),
                    ..PoolConfig::default()
                },
                "/c2srv_commit_fence".to_string(),
                budget.clone(),
            ),
        ));
        let mut config = ServerIpcConfig::default();
        config.base.max_total_chunks = 4;
        let server = Arc::new(
            Server::with_reassembly_pool(
                "ipc://chunk_commit_fence",
                config,
                ServerIdentity {
                    server_id: "chunk_commit_fence".into(),
                    server_instance_id: "chunk_commit_fence-instance".into(),
                },
                reassembly_pool,
            )
            .unwrap(),
        );
        let mut route = make_route("grid");
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        );
        route.scheduler = Arc::new(scheduler.clone());
        server.register_route(route).await.unwrap();

        let conn = Arc::new(Connection::new(321));
        let writer = closed_writer().await;
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
        payload.extend_from_slice(&[0x5au8; 512]);

        // Frame dispatch claimed this request's admission entry; a concurrent
        // capacity/feed/malformed refusal then tore the request down while the
        // owning task was still waiting for route admission.
        let owner = server.begin_chunk_admission(conn.conn_id(), 31).unwrap();
        server.abort_chunk_request(conn.conn_id(), 31);

        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            31,
            FLAG_CHUNKED,
            &payload,
            &writer,
            chunk_permit,
            ChunkFrameOrdering::First(owner),
        )
        .await;

        // Route admission completed and the assembly + stored admission were
        // published, but the atomic admission commit lost: the staged
        // publication was rolled back exactly once.
        let budget_used = budget.snapshot().cell(BudgetKind::Reassembly).used_bytes;
        assert_eq!(
            budget_used, 0,
            "the rolled-back assembly must refund its reassembly charge"
        );
        assert!(!server.chunk_registry.contains(conn.conn_id(), 31));
        assert_eq!(server.chunk_registry.active_count(), 0);
        assert_eq!(server.chunk_registry.total_bytes(), 0);
        assert!(server.chunk_route_pending.lock().is_empty());
        assert!(
            scheduler.try_acquire(0).is_ok(),
            "the route pending permit must be returned"
        );
        assert_eq!(server.chunk_admission_gate.len(), 0);
        assert_eq!(server.chunk_processing_permits.available_permits(), 4);
    }

    /// A later chunk whose admission ends in Refused/Aborted must still return
    /// its buddy-backed peer block instead of leaking the SHM allocation.
    #[tokio::test]
    async fn discarded_waiting_buddy_chunk_returns_its_peer_block() {
        use c2_mem::MemHandle;
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
        use c2_wire::chunk::encode_chunk_header;

        fn peer_segments(pool: &MemPool) -> Vec<(String, u32)> {
            (0..pool.segment_count())
                .filter_map(|i| {
                    let name = pool.segment_name(i)?.to_string();
                    let size = pool.segment(i)?.allocator().data_size() as u32;
                    Some((name, size))
                })
                .collect()
        }

        let mut peer_pool = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                ..PoolConfig::default()
            },
            unique_response_pool_prefix("discard"),
        );
        let allocate_buddy_frame = |peer_pool: &mut MemPool| -> Vec<u8> {
            let handle = peer_pool.try_alloc_shm(128).unwrap();
            let (seg_idx, generation, offset, len) = match handle {
                MemHandle::Buddy {
                    seg_idx,
                    generation,
                    offset,
                    len,
                    ..
                } => (seg_idx, generation, offset, len),
                other => panic!("expected buddy request block, got {other:?}"),
            };
            let mut payload = encode_buddy_payload(&BuddyPayload {
                seg_idx,
                generation,
                offset,
                data_size: len as u32,
                is_dedicated: false,
            })
            .to_vec();
            payload.extend_from_slice(&encode_chunk_header(1, 2));
            payload.extend_from_slice(&[0u8; 16]);
            payload
        };

        let conn = Arc::new(Connection::new(9));
        let writer = closed_writer().await;
        let server = ordering_test_server("ipc://chunked_discard_buddy", 4);

        // Refused admission path: the waiting frame is discarded before it
        // decodes its buddy payload, but the peer block must still be freed.
        let refused_payload = allocate_buddy_frame(&mut peer_pool);
        conn.init_peer_shm(peer_pool.prefix().to_string(), peer_segments(&peer_pool));
        let mut owner = server.begin_chunk_admission(conn.conn_id(), 41).unwrap();
        let waiter = server.chunk_admission_waiter(conn.conn_id(), 41).unwrap();
        owner.refuse();
        owner.release();
        assert_eq!(peer_pool.stats().alloc_count, 1);
        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            41,
            FLAG_CHUNKED | FLAG_BUDDY,
            &refused_payload,
            &writer,
            chunk_permit,
            ChunkFrameOrdering::Wait(waiter),
        )
        .await;
        assert_eq!(
            peer_pool.stats().alloc_count,
            0,
            "a refused waiting buddy chunk must return its peer block"
        );

        // Aborted admission path: same discard, same peer-block return.
        let aborted_payload = allocate_buddy_frame(&mut peer_pool);
        conn.init_peer_shm(peer_pool.prefix().to_string(), peer_segments(&peer_pool));
        let owner = server.begin_chunk_admission(conn.conn_id(), 42).unwrap();
        let waiter = server.chunk_admission_waiter(conn.conn_id(), 42).unwrap();
        server.abort_chunk_request(conn.conn_id(), 42);
        assert_eq!(peer_pool.stats().alloc_count, 1);
        let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
        dispatch_chunked_call(
            &server,
            &conn,
            42,
            FLAG_CHUNKED | FLAG_BUDDY,
            &aborted_payload,
            &writer,
            chunk_permit,
            ChunkFrameOrdering::Wait(waiter),
        )
        .await;
        assert_eq!(
            peer_pool.stats().alloc_count,
            0,
            "an aborted waiting buddy chunk must return its peer block"
        );
        drop(owner);
    }

    /// Control frames keep working while every chunk-processing permit is held
    /// by a stalled first chunk, and the stalled request still completes with
    /// byte-equal delivery once the route gate opens.
    #[tokio::test]
    async fn ping_is_served_while_chunk_processing_capacity_is_exhausted() {
        use std::sync::atomic::AtomicUsize;

        let server = ordering_test_server("ipc://chunked_ping_at_capacity", 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut route = make_route("grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();

        let (mut client, server_stream) = LocalStream::pair().await.expect("local pair");
        let handler = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { handle_connection(server, server_stream).await })
        };

        let dispatcher_guard = server.dispatcher.write().await;
        let data: Vec<u8> = (0..96u8).map(|i| i.wrapping_mul(7)).collect();
        client
            .write_all(&chunked_call_frame(71, 0, 1, &data, "grid"))
            .await
            .expect("write single chunk frame");
        wait_until("the only chunk-processing permit is held", || {
            server.chunk_processing_permits.available_permits() == 0
                && server.chunk_admission_gate.len() == 1
        })
        .await;
        assert_eq!(server.chunk_registry.active_count(), 0);

        // Control liveness at full chunk-processing capacity.
        client
            .write_all(&encode_frame(9, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES))
            .await
            .expect("write ping");
        let (ping_rid, ping_flags, ping_payload) = read_reply_frame(&mut client).await;
        assert_eq!(ping_rid, 9);
        assert!(ping_flags & FLAG_SIGNAL != 0);
        assert_eq!(ping_payload, PONG_BYTES);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Release the gate: the same request completes with exactly its bytes.
        drop(dispatcher_guard);
        let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
        assert_eq!(reply_rid, 71);
        let (control, consumed) =
            c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
        match control {
            ReplyControl::Success => {}
            other => panic!("expected successful reply after capacity stall, got {other:?}"),
        }
        assert_eq!(&reply_payload[consumed..], data.as_slice());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.chunk_processing_permits.available_permits(), 1);
        assert_eq!(server.chunk_admission_gate.len(), 0);
        assert_eq!(server.chunk_registry.active_count(), 0);

        drop(client);
        handler.await.expect("connection handler completes");
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
        let writer = closed_writer().await;
        let mut payload = Vec::new();
        payload.extend_from_slice(&encode_chunk_header(0, 2));
        payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
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
            chunk_frame_ordering(&server, conn.conn_id(), 11, FLAG_CHUNKED, &payload).unwrap(),
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
        let writer = closed_writer().await;
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
        let writer = closed_writer().await;

        let err = send_response_meta(
            &pool,
            &writer,
            7,
            ResponseMeta::ShmAlloc {
                seg_idx: alloc.seg_idx as u16,
                generation: alloc.generation,
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
        let writer = closed_writer().await;

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
        let writer = closed_writer().await;

        let err = send_response_meta(
            &pool,
            &writer,
            7,
            ResponseMeta::ShmAlloc {
                seg_idx: alloc.seg_idx as u16,
                generation: alloc.generation,
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
        let writer = closed_writer().await;
        let payload = b"x".repeat(8192);

        let err = smart_reply_with_data(&pool, &writer, 7, payload.as_slice(), 1024, 4096)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("buddy reply write failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    // ── Shared server memory budget ──────────────────────────────────────

    /// The response pool, the reassembly pool, and response prewarm all
    /// charge one server-direction budget; observing the snapshot never
    /// resets it.
    #[test]
    fn server_pools_and_prewarm_share_one_budget_context() {
        let base = c2_config::BaseIpcConfig {
            pool_segment_size: 64 * 1024,
            max_pool_segments: 2,
            max_pool_memory: 128 * 1024,
            pool_prewarm_segments: 1,
            ..c2_config::BaseIpcConfig::default()
        };
        let config = ServerIpcConfig {
            base,
            ..ServerIpcConfig::default()
        };
        let server = Server::new("ipc://server_budget_share", config).unwrap();

        let snapshot = server.memory_budget_snapshot();
        assert_eq!(ServerMemorySnapshot::SCOPE, "server");
        assert_eq!(
            snapshot.limits.shm_backing_budget_bytes,
            c2_config::MemoryBudgetLimits::default().shm_backing_budget_bytes
        );
        assert!(
            snapshot.budget.shm.used_bytes > 0,
            "explicit prewarm must charge the server budget"
        );

        // Both owner pools report the same accounting object.
        let response_pool = server.response_pool_arc();
        assert_eq!(
            response_pool.read().budget().unwrap().snapshot(),
            snapshot.budget
        );
        assert_eq!(
            server
                .chunk_registry
                .pool()
                .read()
                .budget()
                .unwrap()
                .snapshot(),
            snapshot.budget
        );

        // Observing twice is stable and never resets usage.
        assert_eq!(server.memory_budget_snapshot(), snapshot);
    }

    /// A server configured with zero shm backing admits no server-side
    /// mapping, while the snapshot still reports the resolved zero limits.
    #[test]
    fn server_zero_shm_budget_admits_no_backing_and_reports_zero_limits() {
        let base = c2_config::BaseIpcConfig {
            pool_segment_size: 64 * 1024,
            max_pool_segments: 2,
            max_pool_memory: 128 * 1024,
            shm_backing_budget_bytes: 0,
            file_backing_budget_bytes: 0,
            ..c2_config::BaseIpcConfig::default()
        };
        let server = Server::new(
            "ipc://server_budget_zero",
            ServerIpcConfig {
                base,
                ..ServerIpcConfig::default()
            },
        )
        .unwrap();
        let snapshot = server.memory_budget_snapshot();
        assert_eq!(snapshot.limits.shm_backing_budget_bytes, 0);
        assert_eq!(snapshot.limits.file_backing_budget_bytes, 0);
        assert_eq!(snapshot.budget.shm.used_bytes, 0);
        assert_eq!(server.response_pool_arc().read().segment_count(), 0);
    }

    /// A retained server budget observer must outlive the Server and report
    /// charges held by an outstanding response owner until that owner
    /// releases them. This is the read-only half of retired observability:
    /// the handle carries counters only, never the Server or its pools.
    #[test]
    fn server_budget_observer_outlives_the_server_and_tracks_retained_charges() {
        let base = c2_config::BaseIpcConfig {
            pool_segment_size: 64 * 1024,
            max_pool_segments: 2,
            max_pool_memory: 128 * 1024,
            ..c2_config::BaseIpcConfig::default()
        };
        let config = ServerIpcConfig {
            base,
            // Retire an idle segment at the next explicit GC so the release
            // is deterministic without sleeping.
            pool_decay_seconds: 0.0,
            ..ServerIpcConfig::default()
        };
        let server = Server::new("ipc://server_budget_observer", config).unwrap();
        let observer = server.memory_budget_observer();
        assert_eq!(
            *observer.limits(),
            server.memory_budget_snapshot().limits,
            "the observer must carry the server's resolved limits"
        );

        let response_pool = server.response_pool_arc();
        let alloc = response_pool
            .write()
            .alloc(4096)
            .expect("response allocation");
        let charged = observer.used_bytes();
        assert!(
            charged > 0,
            "a mapped response segment must charge the observed budget"
        );
        assert_eq!(
            server.memory_budget_snapshot().budget.shm.used_bytes,
            observer.snapshot().shm.used_bytes,
            "observer and live snapshot must share one accounting object"
        );

        // The Server can be dropped while the outstanding response owner
        // keeps the mapping (and its observed charge) alive.
        drop(server);
        assert_eq!(observer.used_bytes(), charged);

        response_pool.write().free(&alloc).expect("release");
        response_pool.write().gc_buddy();
        assert_eq!(
            observer.used_bytes(),
            0,
            "the retained observer must see the charge return after release"
        );
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
