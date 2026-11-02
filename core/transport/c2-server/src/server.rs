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
    ConnectionEndReason, ConnectionEventSource, ConnectionEvents, DEFAULT_CONNECT_TIMEOUT,
    EndpointCredential, EndpointReapResult, LocalEndpoint, LocalListener, LocalReadHalf,
    LocalStream,
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
use c2_wire::buddy::{BUDDY_PAYLOAD_SIZE, BuddyPayload, decode_buddy_payload};
use c2_wire::chunk::ChunkAdmissionError;
use c2_wire::chunk::header::{REPLY_CHUNK_META_SIZE, decode_chunk_header, encode_reply_chunk_meta};
use c2_wire::control::{
    ReplyControl, RouteCallIdentity, decode_call_control, encode_reply_control,
};
use c2_wire::flags::{
    FLAG_CHUNK_LAST, FLAG_CHUNKED, FLAG_CTRL, FLAG_HANDSHAKE, FLAG_REPLY_V2, FLAG_RESPONSE,
    FLAG_SIGNAL,
};
use c2_wire::frame::{self, decode_frame_body, encode_frame};
pub use c2_wire::handshake::ServerIdentity;
use c2_wire::handshake::{
    CAP_SHM_REQUEST, CAP_SHM_RESPONSE, HANDSHAKE_VERSION, MAX_ROUTES, MethodEntry, RouteInfo,
    VERSION_PREAMBLE_BODY_LEN, VersionPreambleKind, decode_handshake,
    decode_version_preamble_payload, encode_server_handshake, encode_version_preamble,
    validate_version_preamble_header,
};
use c2_wire::msg_type::{DISCONNECT_ACK_BYTES, MsgType, PONG_BYTES};
use c2_wire::registration_control::{
    PENDING_ROUTE_REJECT_INVALID, PENDING_ROUTE_REJECT_NOT_FOUND,
    PENDING_ROUTE_REJECT_TOKEN_MISMATCH, PendingRouteAttestation, PendingRouteAttestationResponse,
    decode_pending_route_attestation_request, encode_pending_route_attestation_response,
};
use c2_wire::route_catalog_control::{
    RouteContractWire, RouteListResponse, RouteLookupResponse, RouteMethodWire, RouteNack,
    RouteRecordWire, RouteSelector, RouteStateReasonWire, RouteStateWire,
    decode_route_list_request, decode_route_lookup_request, encode_route_list_response,
    encode_route_lookup_response, encode_route_nack,
};
use c2_wire::shutdown_control::{decode_shutdown_initiate, encode_shutdown_ack};

use crate::admission::AdmissionGate;
use crate::chunk_ordering::{
    ChunkAdmissionGate, ChunkAdmissionOutcome, ChunkAdmissionOwner, ChunkAdmissionWaiter,
    ChunkOrderingError,
};
use crate::config::ServerIpcConfig;
use crate::connection::Connection;
use crate::dispatcher::{
    BuiltRoute, CrmCallback, CrmError, CrmRoute, Dispatcher, PeerShmRequest, PreparedShmResponse,
    RequestData, ResponseMeta, RouteBuildSpec,
};
use crate::response::try_prepare_shm_response;
use crate::scheduler::{
    RouteAdmissionState, Scheduler, SchedulerAcquireError, SchedulerLimits, SchedulerPendingPermit,
    SchedulerSnapshot,
};
use crate::writer::ConnectionWriter;

const FRAME_FIXED_BODY_LEN: u32 = 12;
const SHUTDOWN_INITIATE_FRAME_BODY_LEN: u32 =
    FRAME_FIXED_BODY_LEN + c2_wire::msg_type::SHUTDOWN_CLIENT_BYTES.len() as u32;
const POST_SHUTDOWN_DUPLICATE_INITIATE_READ_TIMEOUT_MS: u64 = 100;
/// Default journal reason for an explicit direct IPC shutdown transaction.
pub const DIRECT_IPC_SHUTDOWN_REASON: &str = "direct_ipc_shutdown";
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
    Bind(std::io::Error),
    Failed(ServerFailure),
    StartupTimeout(ServerStartupTimeout),
    Config(String),
    Protocol(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) | Self::Bind(e) => write!(f, "IO error: {e}"),
            Self::Failed(failure) => failure.fmt(f),
            Self::StartupTimeout(timeout) => timeout.fmt(f),
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::Bind(error) => Some(error),
            Self::Failed(failure) => Some(failure),
            Self::StartupTimeout(timeout) => Some(timeout),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStartupStage {
    Readiness,
    Responsiveness,
}

impl ServerStartupStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readiness => "readiness",
            Self::Responsiveness => "responsiveness",
        }
    }
}

/// A local startup deadline, never a business-wire or OS I/O failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerStartupTimeout {
    pub stage: ServerStartupStage,
    pub timeout: Duration,
}

impl std::fmt::Display for ServerStartupTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "server startup timed out after {:.3}s during {}",
            self.timeout.as_secs_f64(),
            self.stage.as_str()
        )
    }
}

impl std::error::Error for ServerStartupTimeout {}

/// One clonable failure shared by lifecycle waiters; no independent error slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFailure {
    pub message: String,
    pub bind_cause: Option<c2_local::EndpointBindCause>,
    pub startup_timeout: Option<ServerStartupTimeout>,
}

impl ServerFailure {
    pub fn new(message: String) -> Self {
        Self {
            message,
            bind_cause: None,
            startup_timeout: None,
        }
    }
}

impl From<&ServerError> for ServerFailure {
    fn from(error: &ServerError) -> Self {
        if let ServerError::Failed(failure) = error {
            return failure.clone();
        }
        let bind_cause = match error {
            ServerError::Bind(source) => Some(source.into()),
            _ => None,
        };
        Self {
            message: error.to_string(),
            bind_cause,
            startup_timeout: match error {
                ServerError::StartupTimeout(timeout) => Some(*timeout),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for ServerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServerFailure {}

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
    Failed(ServerFailure),
}

impl ServerLifecycleState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Starting | Self::Ready | Self::Stopping)
    }
}

/// The listener credential is published with readiness on the existing watch.
#[derive(Debug, Clone)]
struct LifecycleSnapshot {
    state: ServerLifecycleState,
    credential: Option<EndpointCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRouteCloseOutcome {
    pub route_name: String,
    pub active_drained: bool,
    pub closed_reason: String,
}

/// Terminal server work and its once-consumed route-close journal.
/// A cleanup failure can coexist with successfully drained routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerShutdownCompletion {
    pub route_outcomes: Vec<ServerRouteCloseOutcome>,
    pub runtime_error: Option<String>,
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
/// dedicated read and write drivers.
pub struct Server {
    identity: ServerIdentity,
    config: ServerIpcConfig,
    endpoint: LocalEndpoint,
    /// **tokio async RwLock** — guards CRM dispatch table; requires `.read().await`
    /// / `.write().await`.  Do NOT confuse with `parking_lot::RwLock` below.
    dispatcher: RwLock<Dispatcher>,
    shutdown_tx: watch::Sender<bool>,
    lifecycle_tx: watch::Sender<LifecycleSnapshot>,
    connection_event_source: ConnectionEventSource,
    route_registration: Mutex<()>,
    /// Sharded chunk reassembly lifecycle manager.
    chunk_registry: Arc<c2_wire::chunk::ChunkRegistry<ChunkRouteAdmission>>,
    /// **parking_lot sync RwLock** — guards SHM memory pool; blocking `.read()`
    /// / `.write()` (no `.await`).  Safe to hold briefly inside tokio tasks.
    response_pool: Arc<crate::ResponsePool>,
    /// Shared transport memory context charged by the response pool, the
    /// reassembly pool inside `chunk_registry`, and response prewarm. One
    /// server direction, one budget; it owns only accounting counters and is
    /// never reset by shutdown.
    memory_budget: MemoryBudget,
    admission: Arc<AdmissionGate>,
    pending_requests: Arc<Semaphore>,
    control_reply_budget: Arc<Semaphore>,
    chunk_processing_permits: Arc<Semaphore>,
    /// Bounded per-request first-chunk admission ordering (see
    /// [`crate::chunk_ordering`]). Created at frame dispatch and resolved by the
    /// first chunk's task, its abort paths, or connection cleanup.
    chunk_admission_gate: ChunkAdmissionGate,
    active_connections: parking_lot::Mutex<HashMap<u64, Arc<Connection>>>,
    active_connections_notify: Notify,
    shutdown_route_outcomes: parking_lot::Mutex<Vec<ServerRouteCloseOutcome>>,
    /// Close reason stamped into the run loop's shutdown route journal.
    /// `direct_ipc_shutdown` is the default; callers may select their
    /// own reason so a control-EOF shutdown is distinguishable in outcomes.
    shutdown_close_reason: parking_lot::Mutex<&'static str>,
    run_entered: AtomicBool,
    execution_scheduler: Scheduler,
}

pub struct PendingRouteReservation {
    route_name: String,
    registration_token: String,
    route: Option<CrmRoute>,
    admission: Arc<AdmissionGate>,
    admission_generation: u64,
    resolved: bool,
}

pub struct RouteAdmissionToken {
    route_name: String,
    route_uid: String,
    admission: Arc<AdmissionGate>,
    admission_generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingRouteInfo {
    registration_token: String,
    route_uid: String,
    contract: c2_contract::ExpectedRouteContract,
    method_names: Vec<String>,
    max_payload_size: u64,
}

impl PendingRouteInfo {
    fn into_attestation(self) -> PendingRouteAttestation {
        PendingRouteAttestation {
            route_name: self.contract.route_name,
            route_uid: self.route_uid,
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
            self.admission.lock().pending.remove(&self.route_name);
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
            self.close_scheduler_for_abort();
        }
        self.resolve();
    }
}

impl Server {
    /// Create a new server for the given IPC address.
    ///
    /// Address format: `ipc://region_id`
    /// → platform native endpoint derived by `LocalEndpoint`
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
        Self::new_with_identity_and_endpoint(parse_local_endpoint(address)?, config, identity)
    }

    /// Construct in the caller's resolved namespace. The endpoint is retained
    /// for readiness probes, bind, restart, listener credentials and drop.
    /// Unix custom-root containers must already exist before running the server.
    pub fn new_with_endpoint(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
    ) -> Result<Self, ServerError> {
        let identity = ServerIdentity {
            server_id: server_id_from_ipc_address(endpoint.address())?,
            server_instance_id: uuid::Uuid::new_v4().simple().to_string(),
        };
        Self::new_with_identity_and_endpoint(endpoint, config, identity)
    }

    /// Construct with an explicit identity and an already-derived endpoint.
    /// This accepts both Unix-managed and Windows Named Pipe contexts without
    /// projecting either through another platform's path representation.
    pub fn new_with_identity_and_endpoint(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
        identity: ServerIdentity,
    ) -> Result<Self, ServerError> {
        config.validate().map_err(ServerError::Config)?;
        validate_server_identity(&identity)?;
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
            .map_err(ServerError::Config)?
        };
        Self::with_endpoint_and_reassembly_pool(
            endpoint,
            config,
            identity,
            Arc::new(parking_lot::RwLock::new(reassembly_pool)),
            Arc::new(AdmissionGate::new()),
        )
    }

    /// Build a server around an explicitly owned reassembly pool.
    ///
    /// The pool's `MemPool::budget()` reassembly cell is the server-side
    /// admission authority for chunked request reassembly. Production callers
    /// pass the config-derived pool through [`Server::new_with_identity`];
    /// tests use this seam to inject pools with tiny finite budgets.
    #[cfg(test)]
    fn with_reassembly_pool(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
        identity: ServerIdentity,
        reassembly_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<Self, ServerError> {
        Self::with_endpoint_and_reassembly_pool(
            endpoint,
            config,
            identity,
            reassembly_pool,
            Arc::new(AdmissionGate::new()),
        )
    }

    fn with_endpoint_and_reassembly_pool(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
        identity: ServerIdentity,
        reassembly_pool: Arc<parking_lot::RwLock<MemPool>>,
        admission: Arc<AdmissionGate>,
    ) -> Result<Self, ServerError> {
        config.validate().map_err(ServerError::Config)?;
        validate_server_identity(&identity)?;
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
            .map_err(ServerError::Config)?
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
        let (lifecycle_tx, _lifecycle_rx) = watch::channel(LifecycleSnapshot {
            state: ServerLifecycleState::Initialized,
            credential: None,
        });
        let pending_requests = Arc::new(Semaphore::new(config.max_pending_requests as usize));
        // Even a zero business budget must leave one control reply slot.
        let control_reply_budget =
            Arc::new(Semaphore::new(config.max_pending_requests.max(1) as usize));
        let chunk_processing_permits = Arc::new(Semaphore::new(config.max_total_chunks as usize));
        let execution_scheduler = Scheduler::with_admission(
            crate::scheduler::ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits::try_from_usize(None, Some(config.max_execution_workers as usize))
                .expect("validated server config must provide max_execution_workers >= 1"),
            Arc::clone(&admission),
        );
        Ok(Self {
            identity,
            config,
            endpoint,
            dispatcher: RwLock::new(Dispatcher::new()),
            shutdown_tx,
            lifecycle_tx,
            connection_event_source: ConnectionEventSource::new(),
            route_registration: Mutex::new(()),
            chunk_registry,
            response_pool: Arc::new(crate::ResponsePool::new(response_pool)),
            memory_budget,
            admission,
            pending_requests,
            control_reply_budget,
            chunk_processing_permits,
            chunk_admission_gate,
            active_connections: parking_lot::Mutex::new(HashMap::new()),
            active_connections_notify: Notify::new(),
            shutdown_route_outcomes: parking_lot::Mutex::new(Vec::new()),
            shutdown_close_reason: parking_lot::Mutex::new(DIRECT_IPC_SHUTDOWN_REASON),
            run_entered: AtomicBool::new(false),
            execution_scheduler,
        })
    }

    /// Install a source prepared/subscribed by Core before server construction.
    /// Consuming the server prevents replacing the feed of an active host.
    pub fn with_connection_event_source(mut self, source: ConnectionEventSource) -> Self {
        self.connection_event_source = source;
        self
    }

    /// Subscribe from now on without starting the server or resolving config.
    pub fn connection_events(&self) -> ConnectionEvents {
        self.connection_event_source.subscribe()
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

    /// Tear down a failed/aborted chunk request.
    ///
    /// Order is the correctness argument:
    /// 1. Publish the terminal ordering outcome and take a gate fence. A
    ///    first-chunk task still parked in route admission wakes here and can
    ///    no longer win the admission commit; the fence keeps the gate entry in
    ///    place so a same-key successor generation cannot begin mid-teardown.
    /// 2. Remove the assembly and admission together, releasing the backing
    ///    before the pending permit outside the registry lock.
    /// 3. Drop the fence, allowing a successor generation to begin.
    fn abort_chunk_request(&self, conn_id: u64, request_id: u64) {
        let reclaim = self.chunk_admission_gate.abort_key((conn_id, request_id));
        self.chunk_registry.abort(conn_id, request_id);
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
        if aborted_ordering > 0 {
            debug!(
                conn_id,
                aborted_ordering, "aborted pending chunk admission ordering on disconnect"
            );
        }
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
        // A complete frame (or an admitted first chunk) may still be waiting
        // for route lookup. Keep those routes available until its original
        // response has drained; shutdown already refuses new frame admission.
        let connections = self
            .active_connections
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for conn in connections {
            conn.wait_idle().await;
        }
        let routes = {
            let _guard = self.route_registration.lock().await;
            let dispatcher = self.dispatcher.read().await;
            dispatcher.routes_snapshot()
        };
        let outcomes = Self::wait_routes_drained(routes.clone(), closed_reason).await;
        for route in routes {
            route.scheduler.close();
        }
        self.dispatcher.write().await.take_all();
        outcomes
    }

    async fn wait_routes_drained(
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

    pub async fn wait_connections_settled(&self) {
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

    pub fn build_route(
        &self,
        spec: RouteBuildSpec,
        callback: Arc<dyn CrmCallback>,
    ) -> Result<BuiltRoute, ServerError> {
        let scheduler = Scheduler::with_admission(
            spec.concurrency_mode,
            spec.access_map.clone(),
            spec.limits,
            Arc::clone(&self.admission),
        );
        let route = CrmRoute::new(spec, scheduler, callback);
        BuiltRoute::new(route)
    }

    pub async fn reserve_route(
        &self,
        route: BuiltRoute,
    ) -> Result<PendingRouteReservation, ServerError> {
        let route = route.into_route();
        let route_name = route.name.clone();
        let _guard = self.route_registration.lock().await;
        let dispatcher = self.dispatcher.read().await;
        if dispatcher.resolve(&route_name).is_some() {
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                route_name
            )));
        }
        let mut state = self.admission.lock();
        if state.pending.contains_key(&route_name) {
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                route_name
            )));
        }
        if dispatcher.len() + state.pending.len() >= MAX_ROUTES {
            return Err(ServerError::Protocol(format!(
                "route count exceeds wire limit: {} > {}",
                dispatcher.len() + state.pending.len() + 1,
                MAX_ROUTES
            )));
        }
        if matches!(self.lifecycle_state(), ServerLifecycleState::Stopping) {
            return Err(ServerError::Protocol(format!(
                "server is shutting down; cannot reserve route {}",
                route_name
            )));
        }
        if state.is_closed() {
            return Err(ServerError::Protocol(format!(
                "server business admission is closed; cannot reserve route {}",
                route_name
            )));
        }
        if !route.scheduler.belongs_to(&self.admission) {
            return Err(ServerError::Protocol(
                "route belongs to another server admission gate".into(),
            ));
        }
        drop(dispatcher);
        let registration_token = uuid::Uuid::new_v4().simple().to_string();
        state.pending.insert(
            route_name.clone(),
            PendingRouteInfo {
                registration_token: registration_token.clone(),
                route_uid: route.route_uid.clone(),
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
            admission: Arc::clone(&self.admission),
            admission_generation: state.generation,
            resolved: false,
        })
    }

    #[cfg(test)]
    async fn register_route(&self, route: CrmRoute) -> Result<(), ServerError> {
        let reservation = self.reserve_route(BuiltRoute::new(route)?).await?;
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
        let mut dispatcher = self.dispatcher.write().await;
        let mut state = self.admission.lock();
        if reservation.admission_generation != state.generation
            || matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
            || state.is_closed()
        {
            let refusal = if state.is_closed()
                && !*self.shutdown_tx.borrow()
                && !matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
            {
                "server business admission is closed"
            } else {
                "server is shutting down"
            };
            reservation.close_scheduler_for_abort();
            return Err(ServerError::Protocol(format!(
                "{refusal}; cannot commit route {}",
                reservation.route_name()
            )));
        }
        if !Arc::ptr_eq(&reservation.admission, &self.admission) {
            reservation.close_scheduler_for_abort();
            return Err(ServerError::Protocol(
                "route reservation belongs to another server".into(),
            ));
        }
        if dispatcher.resolve(reservation.route_name()).is_some() {
            reservation.close_scheduler_for_abort();
            return Err(ServerError::Protocol(format!(
                "route already registered: {}",
                reservation.route_name()
            )));
        }
        if !admission_open {
            let pending = reservation
                .route
                .as_ref()
                .expect("pending route reservation must still own the route")
                .scheduler
                .commit_awaiting_admission();
            if !pending {
                return Err(ServerError::Protocol(format!(
                    "route admission already closed; cannot commit route {}",
                    reservation.route_name()
                )));
            }
        }
        state.pending.remove(reservation.route_name());
        reservation.resolved = true;
        let route = reservation.into_route();
        let token = RouteAdmissionToken {
            route_name: route.name.clone(),
            route_uid: route.route_uid.clone(),
            admission: Arc::clone(&self.admission),
            admission_generation: state.generation,
        };
        dispatcher.register(route);
        Ok(token)
    }

    pub async fn open_route_admission(
        &self,
        token: RouteAdmissionToken,
    ) -> Result<(), ServerError> {
        let _guard = self.route_registration.lock().await;
        let dispatcher = self.dispatcher.read().await;
        let state = self.admission.lock();
        let name = token.route_name;
        if token.admission_generation != state.generation
            || matches!(self.lifecycle_state(), ServerLifecycleState::Stopping)
        {
            return Err(ServerError::Protocol(format!(
                "server is shutting down; cannot open route {name}"
            )));
        }
        if state.is_closed() {
            return Err(ServerError::Protocol(format!(
                "server business admission is closed; cannot open route {name}"
            )));
        }
        if !Arc::ptr_eq(&token.admission, &self.admission) {
            return Err(ServerError::Protocol(format!(
                "route admission token belongs to another server: {name}"
            )));
        }
        let Some(route) = dispatcher.resolve(&name) else {
            return Err(ServerError::Protocol(format!(
                "route not registered: {name}"
            )));
        };
        if route.route_uid != token.route_uid {
            return Err(ServerError::Protocol(format!(
                "route admission token does not match current route: {name}"
            )));
        }
        if !route.scheduler.open_admission_for_registration() {
            return Err(ServerError::Protocol(format!(
                "route is no longer awaiting admission: {name}"
            )));
        }
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
        let state = self.admission.lock();
        let Some(info) = state.pending.get(route_name) else {
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
        let route = {
            let _guard = self.route_registration.lock().await;
            let dispatcher = self.dispatcher.read().await;
            let route = dispatcher.resolve(name);
            if let Some(route) = &route {
                route.scheduler.close();
            }
            route
        };
        if let Some(route) = route {
            route.scheduler.wait_drained_async().await;
            let mut dispatcher = self.dispatcher.write().await;
            if dispatcher
                .resolve(name)
                .is_some_and(|current| Arc::ptr_eq(&current, &route))
            {
                dispatcher.unregister(name);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Close all target admissions before waiting for any route to drain.
    pub async fn unregister_routes_for_shutdown(
        &self,
        names: &[String],
        closed_reason: &str,
    ) -> Vec<ServerRouteCloseOutcome> {
        let routes = {
            let _guard = self.route_registration.lock().await;
            let dispatcher = self.dispatcher.read().await;
            let mut seen = HashSet::new();
            names
                .iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|name| dispatcher.resolve(name))
                .inspect(|route| route.scheduler.close())
                .collect::<Vec<_>>()
        };
        let outcomes = Self::wait_routes_drained(routes.clone(), closed_reason).await;
        let mut dispatcher = self.dispatcher.write().await;
        for route in routes {
            if dispatcher
                .resolve(&route.name)
                .is_some_and(|current| Arc::ptr_eq(&current, &route))
            {
                dispatcher.unregister(&route.name);
            }
        }
        outcomes
    }

    /// The validated local endpoint, independent of the OS transport.
    pub fn local_endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
    }

    fn set_lifecycle_state(&self, state: ServerLifecycleState) {
        if matches!(
            state,
            ServerLifecycleState::Stopped | ServerLifecycleState::Failed(_)
        ) {
            // Terminal metadata may outlive this run and its executor. Keep
            // only backing still owned by real allocations/connection loans;
            // their original pool refunds it on the final release.
            self.set_idle_backing_retention(false);
        }
        self.lifecycle_tx.send_modify(|snapshot| {
            if matches!(
                &state,
                ServerLifecycleState::Starting
                    | ServerLifecycleState::Stopped
                    | ServerLifecycleState::Failed(_)
            ) {
                snapshot.credential = None;
            }
            snapshot.state = state;
        });
    }

    fn set_idle_backing_retention(&self, retain: bool) {
        self.response_pool
            .inner
            .write()
            .set_idle_backing_retention(retain);
        self.chunk_registry
            .pool()
            .write()
            .set_idle_backing_retention(retain);
    }

    fn publish_ready(&self, credential: EndpointCredential) {
        self.lifecycle_tx.send_replace(LifecycleSnapshot {
            state: ServerLifecycleState::Ready,
            credential: Some(credential),
        });
    }

    pub fn lifecycle_state(&self) -> ServerLifecycleState {
        self.lifecycle_tx.borrow().state.clone()
    }

    /// Credential captured from the bound listener; it does not prove liveness.
    pub fn endpoint_credential(&self) -> Option<EndpointCredential> {
        self.lifecycle_tx.borrow().credential.clone()
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
        let mut registration = self.admission.lock();
        match self.lifecycle_state() {
            ServerLifecycleState::Initialized
            | ServerLifecycleState::Stopped
            | ServerLifecycleState::Failed(_) => {
                self.set_idle_backing_retention(true);
                if self.config.pool_prewarm_segments > 0 {
                    let prewarm = self
                        .response_pool
                        .inner
                        .write()
                        .ensure_buddy_segments(self.config.pool_prewarm_segments as usize);
                    if let Err(error) = prewarm {
                        self.set_idle_backing_retention(false);
                        return Err(ServerError::Config(format!(
                            "response pool prewarm: {error}"
                        )));
                    }
                }
                if !matches!(self.lifecycle_state(), ServerLifecycleState::Initialized)
                    || registration.is_closed()
                {
                    registration.begin_start();
                    self.admission.notify.notify_waiters();
                }
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
                let state = rx.borrow().state.clone();
                match state {
                    ServerLifecycleState::Ready => return Ok(()),
                    ServerLifecycleState::Failed(mut failure) => {
                        failure.message = format!("server failed to start: {}", failure.message);
                        return Err(ServerError::Failed(failure));
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
            ServerError::StartupTimeout(ServerStartupTimeout {
                stage: ServerStartupStage::Readiness,
                timeout,
            })
        })?
    }

    pub async fn wait_until_responsive(&self, timeout: Duration) -> Result<(), ServerError> {
        let started = tokio::time::Instant::now();
        self.wait_until_ready(timeout).await?;
        loop {
            if started.elapsed() >= timeout {
                return Err(ServerError::StartupTimeout(ServerStartupTimeout {
                    stage: ServerStartupStage::Responsiveness,
                    timeout,
                }));
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            let probe_timeout = remaining.min(Duration::from_millis(100));
            if tokio::time::timeout(probe_timeout, ping_server_endpoint(self.local_endpoint()))
                .await
                == Ok(true)
            {
                return Ok(());
            }
            tokio::time::sleep(
                timeout
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_millis(10)),
            )
            .await;
        }
    }

    pub async fn wait_until_stopped(&self, timeout: Duration) -> Result<(), ServerError> {
        let mut rx = self.lifecycle_tx.subscribe();
        let wait = async {
            loop {
                let state = rx.borrow().state.clone();
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

    /// Observe initiation of the shutdown transaction.
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
                &rx.borrow().state,
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
            ServerLifecycleState::Stopping
                | ServerLifecycleState::Stopped
                | ServerLifecycleState::Failed(_)
        ) {
            self.wait_until_terminal().await?;
            self.completed_shutdown_outcomes()
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn shutdown_and_wait_unbounded(
        &self,
    ) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        self.request_shutdown_signal();
        self.wait_until_terminal().await?;
        self.completed_shutdown_outcomes()
    }

    // Terminal observation is a lifecycle fence even for Failed. Shutdown
    // completion must also report that failure instead of claiming success.
    fn completed_shutdown_outcomes(&self) -> Result<Vec<ServerRouteCloseOutcome>, ServerError> {
        if let ServerLifecycleState::Failed(failure) = self.lifecycle_state() {
            return Err(ServerError::Failed(failure));
        }
        Ok(self.take_shutdown_route_outcomes())
    }

    /// Observe terminal work before consuming the native close journal.
    /// Runtime owners use this to retain actual drain outcomes alongside a
    /// listener-cleanup error. It never exposes an unfenced journal drain.
    pub async fn observe_shutdown_completion_unbounded(
        &self,
    ) -> Result<ServerShutdownCompletion, ServerError> {
        self.wait_until_terminal().await?;
        let runtime_error = match self.lifecycle_state() {
            ServerLifecycleState::Initialized | ServerLifecycleState::Stopped => None,
            ServerLifecycleState::Failed(failure) => Some(failure.message),
            state => {
                return Err(ServerError::Config(format!(
                    "server restarted during shutdown completion observation: {state:?}"
                )));
            }
        };
        Ok(ServerShutdownCompletion {
            route_outcomes: self.take_shutdown_route_outcomes(),
            runtime_error,
        })
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
    pub fn response_pool_arc(&self) -> Arc<crate::ResponsePool> {
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
        c2_mem::BudgetObserver::new(self.memory_budget.clone())
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
    pub async fn registered_route_identity(&self, name: &str) -> Option<String> {
        self.dispatcher
            .read()
            .await
            .resolve(name)
            .map(|route| route.route_uid.clone())
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
        self.run_inner(None).await
    }

    /// Run with the Host's original startup deadline, including the native
    /// listener ownership wait. It does not bound the serving lifetime.
    pub async fn run_with_startup_deadline(
        self: &Arc<Self>,
        startup_deadline: std::time::Instant,
        startup_timeout: Duration,
    ) -> Result<(), ServerError> {
        self.run_inner(Some((startup_deadline, startup_timeout)))
            .await
    }

    async fn run_inner(
        self: &Arc<Self>,
        startup_budget: Option<(std::time::Instant, Duration)>,
    ) -> Result<(), ServerError> {
        if !matches!(self.lifecycle_state(), ServerLifecycleState::Starting) {
            self.begin_start_attempt()?;
        }

        self.run_entered.store(true, Ordering::Release);
        let endpoint = self.endpoint.clone();
        let startup = tokio::task::spawn_blocking(move || {
            LocalListener::bind_until(&endpoint, startup_budget.map(|(deadline, _)| deadline))
        })
        .await
        .map_err(|error| ServerError::Config(format!("listener startup task failed: {error}")))
        .and_then(|result| {
            result.map_err(|error| {
                if let Some((deadline, timeout)) = startup_budget
                    && std::time::Instant::now() >= deadline
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
                {
                    ServerError::StartupTimeout(ServerStartupTimeout {
                        stage: ServerStartupStage::Readiness,
                        timeout,
                    })
                } else {
                    ServerError::Bind(error)
                }
            })
        });

        let listener = match startup {
            Ok(listener) => listener,
            Err(err) => {
                self.set_lifecycle_state(ServerLifecycleState::Failed((&err).into()));
                return Err(err);
            }
        };

        self.publish_ready(listener.credential());
        info!(endpoint = ?self.endpoint.os_name(), "server listening");

        // Spawn periodic GC sweep for expired chunk assemblies. The same
        // bounded task reclaims both storage tiers of the server's owner
        // pools: idle buddy segments retire down to the configured minimum.
        // Dedicated mappings end when their last local allocation is released.
        let gc_server = Arc::clone(self);
        let gc_interval = self.chunk_registry.config().gc_interval;
        let mut gc_shutdown_rx = self.shutdown_tx.subscribe();
        let gc_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(gc_interval);
            interval.tick().await; // skip first immediate tick
            let mut stopping = false;
            loop {
                let connections_changed = gc_server.active_connections_notify.notified();
                tokio::pin!(connections_changed);
                connections_changed.as_mut().enable();
                if stopping && gc_server.active_connections.lock().is_empty() {
                    break;
                }
                tokio::select! {
                    _ = interval.tick() => {
                        let stats = gc_server.chunk_registry.gc_sweep();
                        {
                            let mut response_pool = gc_server.response_pool.inner.write();
                            response_pool.gc_buddy();
                        }
                        {
                            let mut reassembly_pool = gc_server.chunk_registry.pool().write();
                            reassembly_pool.gc_buddy();
                        }
                        let connections = gc_server.active_connections.lock()
                            .values().cloned().collect::<Vec<_>>();
                        for conn in connections {
                            if let Some(cache) = conn.peer_mappings() {
                                cache.gc_idle();
                            }
                        }
                        if stats.expired > 0 {
                            info!(
                                expired = stats.expired,
                                freed = stats.freed_bytes,
                                released_route_pending = stats.expired,
                                "chunk GC sweep"
                            );
                        }
                    }
                    _ = wait_for_shutdown(&mut gc_shutdown_rx), if !stopping => stopping = true,
                    _ = &mut connections_changed, if stopping => {},
                }
            }
        });

        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let (shutdown_done_tx, mut shutdown_done_rx) = mpsc::unbounded_channel();
        let mut shutdown_close_started = false;
        let mut listener = Some(listener);
        loop {
            tokio::select! {
                result = async {
                    match listener.as_mut() {
                        Some(listener) => listener.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
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
                    // Business drain does not wait for response loans retained
                    // by the peer. Their drivers stay on this same executor.
                    let connections = self.active_connections.lock().values().cloned().collect::<Vec<_>>();
                    for conn in connections { conn.wait_idle().await; }
                    let listener_result = self.close_listener_for_shutdown(listener.take().expect("open listener")).await;
                    if !shutdown_route_outcomes.is_empty() {
                        self.shutdown_route_outcomes
                            .lock()
                            .extend(shutdown_route_outcomes);
                    }
                    if self.active_connections.lock().is_empty() {
                        let _ = gc_handle.await;
                    } else {
                        // Retained connections still need idle mapping GC.
                        // The existing maintenance task ends at their drain;
                        // business shutdown does not wait for their loans.
                        drop(gc_handle);
                    }
                    match listener_result {
                        Ok(()) => self.set_lifecycle_state(ServerLifecycleState::Stopped),
                        Err(error) => {
                            self.set_lifecycle_state(ServerLifecycleState::Failed((&error).into()));
                            return Err(error);
                        }
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    async fn close_listener_for_shutdown(
        &self,
        listener: LocalListener,
    ) -> Result<(), ServerError> {
        // Drop is deliberately opportunistic. Normal shutdown must wait for
        // bounded owner retirement and observe its result. Keep that synchronous
        // gate wait off Tokio workers: a task on this same runtime may release it.
        let result = tokio::task::spawn_blocking(move || listener.close())
            .await
            .map_err(|error| ServerError::Config(format!("listener cleanup task failed: {error}")))
            .and_then(|outcome| match outcome {
                EndpointReapResult::Reaped
                | EndpointReapResult::AlreadyAbsent
                | EndpointReapResult::NotApplicable => Ok(()),
                EndpointReapResult::IoError(error) => Err(ServerError::Io(error)),
                other => Err(ServerError::Config(format!(
                    "listener cleanup failed: {other:?}"
                ))),
            });
        result
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
            ServerLifecycleState::Stopping
                | ServerLifecycleState::Stopped
                | ServerLifecycleState::Failed(_)
        ) {
            self.wait_until_stopped(timeout).await?;
            self.completed_shutdown_outcomes()
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
        self.completed_shutdown_outcomes()
    }

    /// Initiate the shutdown transaction with an explicit journal reason.
    ///
    /// The reason must be a static string because it is stored for the lifetime of
    /// the shutdown transaction.
    pub fn request_shutdown_signal_with_reason(&self, close_reason: &'static str) {
        *self.shutdown_close_reason.lock() = close_reason;
        self.request_shutdown_signal();
    }

    /// Terminally close business admission without starting the shutdown
    /// transaction. Every registered route scheduler closes so new requests
    /// are rejected, later route
    /// registrations fail, and existing work continues to drain under the
    /// original rules. The listener stays open and the lifecycle state is
    /// unchanged; the full shutdown transaction still has to run separately.
    /// Returns the number of routes whose admission was closed by this call
    /// (already-closed routes are not counted again).
    pub async fn close_business_admission(&self, _reason: &str) -> usize {
        {
            let mut state = self.admission.lock();
            state.terminate();
            self.admission.notify.notify_waiters();
        }
        let routes = {
            let _guard = self.route_registration.lock().await;
            let dispatcher = self.dispatcher.read().await;
            dispatcher.routes_snapshot()
        };
        let mut closed = 0usize;
        for route in &routes {
            let newly_closed =
                route.scheduler.route_admission_state() != RouteAdmissionState::Closed;
            if newly_closed {
                route.scheduler.close();
            }
            if newly_closed {
                closed += 1;
            }
        }
        closed
    }

    /// Whether the shared host gate currently rejects new business admission.
    pub fn business_admission_closed(&self) -> bool {
        self.admission.lock().is_closed()
    }

    /// Reject a start attempt that has not published readiness yet.
    ///
    /// This moves an unpolled `Starting` or cancelled `Stopping` attempt to `Failed`, which makes every
    /// readiness waiter fail fast with `message`. It never touches a
    /// server that is running or already terminal.
    pub fn reject_start_attempt(&self, failure: ServerFailure) -> Result<(), ServerError> {
        match self.lifecycle_state() {
            ServerLifecycleState::Starting | ServerLifecycleState::Stopping
                if !self.run_entered.load(Ordering::Acquire) =>
            {
                self.set_lifecycle_state(ServerLifecycleState::Failed(failure));
                Ok(())
            }
            state => Err(ServerError::Config(format!(
                "cannot reject a start attempt while lifecycle state is {state:?}",
            ))),
        }
    }

    fn request_shutdown_signal(&self) {
        let mut registration = self.admission.lock();
        registration.begin_drain();
        self.admission.notify.notify_waiters();
        match self.lifecycle_state() {
            ServerLifecycleState::Starting | ServerLifecycleState::Ready => {
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
            // Retain the first close reason while accepted calls flush their
            // responses. Closed route admission rejects any new business.
            for conn in self.active_connections.lock().values() {
                conn.begin_shutdown();
            }
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

fn parse_local_endpoint(address: &str) -> Result<LocalEndpoint, ServerError> {
    // c2-config owns platform endpoint derivation and address validation.
    LocalEndpoint::from_address(address).map_err(|error| {
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
    c2_config::validate_server_id(region)
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

async fn wait_for_control_peer_close(reader: &mut LocalReadHalf, deadline: tokio::time::Instant) {
    // The control client consumes the complete acknowledgement and closes its
    // one-exchange connection. Retain the pipe until that EOF where possible.
    // A timeout or unexpected byte only ends the wait; neither proves that the
    // peer consumed the acknowledgement.
    let mut byte = [0_u8; 1];
    let _ = tokio::time::timeout_at(deadline, reader.read(&mut byte)).await;
}

async fn handle_post_shutdown_connection(
    server: Arc<Server>,
    conn: Arc<Connection>,
    stream: LocalStream,
) {
    let conn_id = conn.conn_id();
    let (mut reader, mut write_half) = stream.into_split();
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
        if decode_shutdown_initiate(payload).is_err() {
            return;
        }
        server.request_shutdown_signal();
        let ack = encode_frame(
            header.request_id,
            FLAG_RESPONSE | FLAG_SIGNAL,
            &encode_shutdown_ack(),
        );
        let deadline = tokio::time::Instant::now() + server.config.write_timeout();
        let _ = tokio::time::timeout_at(deadline, async {
            tokio::io::AsyncWriteExt::write_all(&mut write_half, &ack).await?;
            tokio::io::AsyncWriteExt::flush(&mut write_half).await
        })
        .await;
        wait_for_control_peer_close(&mut reader, deadline).await;
    }
}

/// Registration exists before the task is spawned, so shutdown also waits
/// for accepted connections whose handler has not yet been polled.
struct RegisteredConnection {
    server: Arc<Server>,
    conn: Arc<Connection>,
    stream: Option<LocalStream>,
    writer_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RegisteredConnection {
    fn drop(&mut self) {
        self.conn
            .end(ConnectionEndReason::LocalClose, "connection_task_drop");
        self.conn.cancel_queued_work();
        self.conn.stop_reading();
        if let Some(task) = self.writer_task.take() {
            task.abort();
        }
        // Also close an accepted stream if its future was never polled.
        drop(self.stream.take());
        let had_guard = self.conn.has_ever_installed_remote_read_guard();
        self.conn.clear_remote_read_guard();
        if !had_guard {
            self.conn.publish_physical_close();
        }
        self.server
            .unregister_active_connection(self.conn.conn_id());
    }
}

/// Preserve progress through a partially read frame when admission closes.
/// Only a close decision from ConnectionCore can cancel the physical read.
async fn read_connection_exact(
    reader: &mut LocalReadHalf,
    bytes: &mut [u8],
    conn: &Connection,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<bool> {
    let read = reader.read_exact(bytes);
    tokio::pin!(read);
    loop {
        let changed = conn.writer_wake.notified();
        if conn.try_close() {
            return Ok(false);
        }
        tokio::select! {
            result = &mut read => return result.map(|_| true),
            _ = changed => {},
            _ = wait_for_shutdown(shutdown), if !conn.shutdown_requested() => {
                conn.begin_shutdown();
            }
        }
    }
}

fn record_read_error(
    conn: &Connection,
    abort: &c2_local::AbortHandle,
    operation: &'static str,
    error: &std::io::Error,
) {
    if error.kind() == std::io::ErrorKind::ConnectionAborted && abort.is_aborted() {
        conn.end(ConnectionEndReason::LocalClose, "local_abort");
    } else {
        conn.end_read(operation, error);
    }
}

fn handle_connection(
    server: Arc<Server>,
    stream: LocalStream,
) -> impl std::future::Future<Output = ()> + Send {
    let publisher = server
        .connection_event_source
        .open_connection(c2_local::ConnectionSide::Server);
    let conn = Arc::new(Connection::observed(publisher));
    let conn_id = conn.conn_id();
    server.register_active_connection(Arc::clone(&conn));
    let registration = RegisteredConnection {
        server,
        conn,
        stream: Some(stream),
        writer_task: None,
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
            conn.end(ConnectionEndReason::LocalClose, "server_shutdown");
            handle_post_shutdown_connection(server, conn, stream).await;
            return;
        }

        // Native shared-memory reachability for this exact accepted stream
        // (same TokenSessionId as the pipe peer on Windows; always reachable
        // on Unix). Captured before the halves split so the handshake ACK can
        // intersect the client's SHM offers with actual reachability. The
        // query is part of connection establishment: an indeterminate answer
        // terminates the connection with its original OS cause through the
        // existing IO-error observation chain — the same terminal path a
        // ServerError::Io from the handshake feeds — before any handshake
        // byte is read. An unknown reachability must never become a live
        // connection negotiating without SHM.
        let native_shm_reachable = match stream.shared_memory_available() {
            Ok(reachable) => reachable,
            Err(error) => {
                warn!(
                    conn_id,
                    %error,
                    "native shared-memory reachability indeterminate; terminating connection"
                );
                conn.end_io("shm_reachability", &error);
                return;
            }
        };
        conn.set_native_shm_reachable(native_shm_reachable);

        conn.install_remote_read_guard(stream.retain_for_remote_read());
        let abort = stream.abort_handle();
        let (mut reader, write_half) = stream.into_split();
        let (writer, writer_task) = ConnectionWriter::spawn(
            write_half,
            Arc::clone(&conn),
            server.config.write_timeout(),
            server.control_reply_budget.clone(),
        );
        registration.writer_task = Some(writer_task);

        debug!(conn_id, "connection accepted");

        let max_frame = server.config.max_frame_size;
        let mut read_failed = false;

        loop {
            let mut len_buf = [0u8; 4];
            match read_connection_exact(&mut reader, &mut len_buf, &conn, &mut shutdown_rx).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    record_read_error(&conn, &abort, "read_length", &error);
                    read_failed = true;
                    break;
                }
            }
            let total_len = u32::from_le_bytes(len_buf);

            let variable_frame_over_limit = u64::from(total_len) > max_frame;
            // Only the fixed-size preamble can reach header inspection outside
            // the variable-frame budget; its exact flags and bytes are checked below.
            if total_len < 12
                || (variable_frame_over_limit && total_len != VERSION_PREAMBLE_BODY_LEN)
            {
                warn!(conn_id, total_len, "invalid frame length");
                conn.end(ConnectionEndReason::ProtocolError, "frame_length");
                break;
            }

            // Inspect the fixed header before reading any payload byte or
            // allocating. Even a header-only invalid preamble must be rejected
            // before reading the body.
            let mut prefix = [0u8; FRAME_FIXED_BODY_LEN as usize];
            let prefix_len = FRAME_FIXED_BODY_LEN as usize;
            match read_connection_exact(
                &mut reader,
                &mut prefix[..prefix_len],
                &conn,
                &mut shutdown_rx,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    record_read_error(&conn, &abort, "read_frame_prefix", &error);
                    read_failed = true;
                    break;
                }
            }
            let (header, _) = decode_frame_body(&prefix[..prefix_len], total_len)
                .expect("validated fixed frame header");
            if header.is_handshake() {
                if conn.handshake_done() || conn.ordinary_protocol() {
                    conn.end(ConnectionEndReason::ProtocolError, "duplicate_handshake");
                    break;
                }
                let mut version_header = [0u8; 16];
                version_header[..4].copy_from_slice(&len_buf);
                version_header[4..].copy_from_slice(&prefix[..prefix_len]);
                let result =
                    negotiate_handshake(&server, &conn, &version_header, &mut reader, &writer)
                        .await;
                if let Err(error) = result {
                    warn!(conn_id, %error, "handshake failed");
                    match &error {
                        ServerError::Io(error) => conn.end_io("handshake", error),
                        _ => conn.end(ConnectionEndReason::ProtocolError, "handshake"),
                    }
                    break;
                }
                continue;
            }
            if variable_frame_over_limit {
                conn.end(ConnectionEndReason::ProtocolError, "frame_length");
                break;
            }
            if !conn.handshake_done() && !header.is_signal() {
                conn.end(ConnectionEndReason::ProtocolError, "frame_before_handshake");
                break;
            }

            // 2. Read the remaining body (request_id + flags + payload).
            let mut body = vec![0u8; total_len as usize];
            body[..prefix_len].copy_from_slice(&prefix[..prefix_len]);
            match read_connection_exact(
                &mut reader,
                &mut body[prefix_len..],
                &conn,
                &mut shutdown_rx,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    record_read_error(&conn, &abort, "read_body", &error);
                    read_failed = true;
                    break;
                }
            }

            // 3. Decode header + payload.
            let (header, payload) = match decode_frame_body(&body, total_len) {
                Ok(v) => v,
                Err(e) => {
                    warn!(conn_id, ?e, "frame decode error");
                    conn.end(ConnectionEndReason::ProtocolError, "frame_decode");
                    break;
                }
            };

            let flags = header.flags;
            let request_id = header.request_id;

            if header.is_ctrl() && payload.first() == Some(&MsgType::Release.as_byte()) {
                let outcome = {
                    let _publication = conn.publication_gate.lock();
                    conn.core.receive_release(&header, payload)
                };
                drop(outcome.reclaims);
                if let Some(error) = outcome.error {
                    warn!(conn_id, %error, "invalid release record");
                    conn.end(ConnectionEndReason::ProtocolError, "release");
                }
                continue;
            }

            // 4. Dispatch by frame type.
            if header.is_signal() {
                // A standalone control exchange still commits the ordinary role.
                conn.select_ordinary_protocol();
                if matches!(
                    payload.first().and_then(|&b| MsgType::from_byte(b)),
                    Some(MsgType::ShutdownClient)
                ) {
                    let deadline = tokio::time::Instant::now() + server.config.write_timeout();
                    let initiated =
                        handle_shutdown_signal(&server, payload, request_id, &writer).await;
                    if initiated && !conn.handshake_done() {
                        // Adapter flush does not prove that a Named Pipe peer
                        // consumed the ACK. Its one-exchange client closes
                        // after reading it; retain the OS stream until then.
                        // Established RPC peers keep their ordinary receive
                        // loop so accepted calls and releases still progress.
                        wait_for_control_peer_close(&mut reader, deadline).await;
                    }
                    if *server.shutdown_tx.borrow() {
                        conn.begin_shutdown();
                    } else {
                        conn.end(ConnectionEndReason::LocalClose, "shutdown_control_close");
                    }
                    continue;
                }
                match handle_signal(payload, request_id, &writer, &conn).await {
                    SignalAction::Continue => {}
                    SignalAction::Disconnect => {
                        conn.end(ConnectionEndReason::PeerCloseMessage, "disconnect");
                    }
                }
            } else if header.is_ctrl() {
                if let Ok(reply) = writer.reserve_control() {
                    let srv = Arc::clone(&server);
                    let payload = payload.to_vec();
                    tokio::spawn(async move {
                        handle_ctrl(&srv, &payload, request_id, reply).await;
                    });
                }
            } else if header.is_call_v2() {
                // Reserve before any admission await or task submission so a
                // concurrent shutdown cannot miss an already accepted call.
                let flight = crate::connection::FlightGuard::new(&conn);
                if (conn.is_deactivated() || conn.shutdown_requested())
                    && !accepted_chunk_continuation(&server, conn_id, request_id, flags, payload)
                {
                    if c2_wire::flags::is_buddy(flags) && conn.shm_request_capable() {
                        cleanup_buddy_request_block(&conn, payload, &server.memory_budget);
                    }
                    queue_reply(
                        &writer,
                        request_id,
                        &ReplyControl::Error(error_wire(
                            ErrorCode::ResourceClosed,
                            "connection admission is closed; request was not executed",
                        )),
                    )
                    .await;
                    continue;
                }
                // Un-negotiated buddy coordinates must be refused before any
                // admission, lazy-open, or late-discard cleanup could touch a
                // mapping this connection was never promised. The rejection
                // deliberately does not decode the buddy payload or run the
                // buddy cleanup: cleanup itself would lazy-open the very
                // mapping the direction gate exists to protect.
                if c2_wire::flags::is_buddy(flags) && !conn.shm_request_capable() {
                    warn!(
                        conn_id,
                        request_id, "buddy request frame without negotiated SHM request direction"
                    );
                    if c2_wire::flags::is_chunked(flags) {
                        // The frame may belong to a request whose earlier
                        // inline chunks were admitted; tear that admission
                        // down terminal-first exactly like a capacity refusal
                        // (no mappings involved), then correlate the caller.
                        server.abort_chunk_request(conn.conn_id(), request_id);
                    }
                    queue_reply(
                        &writer,
                        request_id,
                        &ReplyControl::Error(error_wire(
                            ErrorCode::ProtocolViolation,
                            "buddy SHM request frames were not negotiated for this connection"
                                .to_string(),
                        )),
                    )
                    .await;
                    continue;
                }
                if c2_wire::flags::is_chunked(flags) {
                    let chunk_processing_permit = match server.try_acquire_chunk_processing_permit()
                    {
                        Ok(permit) => permit,
                        Err(limit) => {
                            if c2_wire::flags::is_buddy(flags) {
                                cleanup_buddy_request_block(&conn, payload, &server.memory_budget);
                            }
                            // The frame cannot be processed, so the request is
                            // terminal: publish the abort and release (or fence)
                            // any pending admission, assembly, and route work
                            // for this request before correlating the refusal.
                            // A first-chunk task still parked in route
                            // admission wakes and rolls back; it can never
                            // publish after this terminal outcome.
                            server.abort_chunk_request(conn.conn_id(), request_id);
                            queue_chunk_processing_capacity_error(&writer, request_id, limit).await;
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
                                cleanup_buddy_request_block(&conn, payload, &server.memory_budget);
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
                            queue_reply(
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
                    let buddy = match decode_request_buddy(&conn, payload) {
                        Ok(value) => value,
                        Err(error) => {
                            warn!(conn_id, request_id, %error, "buddy payload decode error");
                            queue_input_error(&writer, request_id, error).await;
                            continue;
                        }
                    };
                    let (ctrl, ctrl_consumed) =
                        match decode_call_control(payload, BUDDY_PAYLOAD_SIZE) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(
                                    conn_id = conn.conn_id(),
                                    ?e,
                                    "buddy call control decode error"
                                );
                                drop(admit_peer_request(&conn, buddy, &server.memory_budget));
                                queue_input_error(&writer, request_id, e.to_string()).await;
                                continue;
                            }
                        };
                    let request = match admit_peer_request(&conn, buddy, &server.memory_budget) {
                        Ok(request) => request,
                        Err(error) => {
                            queue_reply(
                                &writer,
                                request_id,
                                &ReplyControl::Error(error_wire(
                                    ErrorCode::ResourceInputDeserializing,
                                    error,
                                )),
                            )
                            .await;
                            continue;
                        }
                    };
                    let route_admission = match reserve_accepted_route_execution(
                        &server,
                        &ctrl.identity,
                        ctrl.method_idx,
                    )
                    .await
                    {
                        Ok(admission) => admission,
                        Err(err) => {
                            drop(request);
                            queue_route_admission_error(&writer, request_id, err).await;
                            continue;
                        }
                    };
                    let pending_permit = match server.try_acquire_pending_request() {
                        Ok(permit) => permit,
                        Err(limit) => {
                            drop(route_admission.pending_permit);
                            drop(request);
                            queue_server_pending_capacity_error(&writer, request_id, limit).await;
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
                        let _flight = flight;
                        dispatch_admitted_buddy_call(AdmittedBuddyCall {
                            server: &srv,
                            conn: &cn,
                            request_id,
                            request,
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
                        queue_input_error(&writer, request_id, e.to_string()).await;
                        continue;
                    }
                };
                let route_admission = match reserve_accepted_route_execution(
                    &server,
                    &ctrl.identity,
                    ctrl.method_idx,
                )
                .await
                {
                    Ok(admission) => admission,
                    Err(err) => {
                        queue_route_admission_error(&writer, request_id, err).await;
                        continue;
                    }
                };
                let pending_permit = match server.try_acquire_pending_request() {
                    Ok(permit) => permit,
                    Err(limit) => {
                        drop(route_admission.pending_permit);
                        queue_server_pending_capacity_error(&writer, request_id, limit).await;
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
                    let _flight = flight;
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

        conn.cancel_queued_work();
        server.cleanup_chunk_requests_for_connection(conn_id);
        if reader.peer_ended() {
            conn.peer_end();
        }
        // Invalid framing has no recoverable next boundary. Read only into a
        // fixed discard buffer until peer end; a write error is no end proof.
        let mut discard = [0u8; 8192];
        if read_failed && !reader.peer_ended() {
            conn.stop_reading();
        }
        if reader.peer_ended() {
            loop {
                let changed = conn.writer_wake.notified();
                if conn.try_close() {
                    break;
                }
                changed.await;
            }
        }
        while !conn.reading_stopped() && !conn.try_close() {
            let changed = conn.writer_wake.notified();
            if reader.peer_ended() {
                changed.await;
                continue;
            }
            tokio::select! {
                result = reader.read(&mut discard) => match result {
                    Ok(0) => { conn.end(ConnectionEndReason::PeerEof, "discard_eof"); conn.peer_end(); },
                    Ok(_) => {},
                    Err(error) => {
                        record_read_error(&conn, &abort, "discard_read", &error);
                        if reader.peer_ended() { conn.peer_end(); }
                        else {
                            // An unrecoverable read without end proof cannot
                            // grant permission to reuse published storage.
                            conn.stop_reading();
                        }
                    }
                },
                _ = changed => {},
            }
        }
        if conn.reading_stopped() {
            // No more release facts can be read. Wake any blocked writer I/O;
            // retirement guards still retain the native stream until B=0,
            // and the original loan book keeps every unproved published loan.
            abort.abort();
        }
        drop(writer);
        let _ = registration.writer_task.as_mut().unwrap().await;
        registration.writer_task.take();
        drop(reader);
        conn.clear_remote_read_guard();
        conn.wait_idle().await;
        debug!(conn_id, "connection physically closed");
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

async fn read_handshake_exact(
    server: &Server,
    conn: &Connection,
    reader: &mut LocalReadHalf,
    bytes: &mut [u8],
) -> Result<(), ServerError> {
    let mut shutdown = server.shutdown_tx.subscribe();
    if read_connection_exact(reader, bytes, conn, &mut shutdown).await? {
        Ok(())
    } else {
        Err(ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "handshake deactivated",
        )))
    }
}

async fn negotiate_handshake(
    server: &Server,
    conn: &Connection,
    header: &[u8; 16],
    reader: &mut LocalReadHalf,
    writer: &Arc<ConnectionWriter>,
) -> Result<(), ServerError> {
    validate_version_preamble_header(header, false)
        .map_err(|error| ServerError::Protocol(format!("version preamble header: {error}")))?;
    let mut payload = [0u8; 9];
    read_handshake_exact(server, conn, reader, &mut payload).await?;
    let (kind, peer) = decode_version_preamble_payload(&payload)
        .map_err(|error| ServerError::Protocol(format!("version preamble payload: {error}")))?;
    if kind != VersionPreambleKind::Hello {
        return Err(ServerError::Protocol("expected version Hello".into()));
    }
    if peer != HANDSHAKE_VERSION {
        // One absolute budget covers writer acquisition, write, asynchronous
        // flush and waiting for peer EOF; shutdown can end the peer-close wait.
        let deadline = tokio::time::Instant::now() + DEFAULT_CONNECT_TIMEOUT;
        tokio::time::timeout_at(deadline, async {
            writer
                .write_all(&encode_version_preamble(
                    VersionPreambleKind::VersionMismatch,
                ))
                .await
        })
        .await
        .map_err(|_| {
            ServerError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "version rejection deadline expired",
            ))
        })??;
        conn.end(ConnectionEndReason::ProtocolError, "handshake_version");
        let mut shutdown = server.shutdown_tx.subscribe();
        tokio::select! {
            _ = wait_for_control_peer_close(reader, deadline) => {},
            _ = wait_for_shutdown(&mut shutdown) => {},
        }
        return Err(ServerError::Protocol(format!(
            "IPC protocol version mismatch: local={HANDSHAKE_VERSION}, peer={peer}"
        )));
    }
    writer
        .write_all(&encode_version_preamble(VersionPreambleKind::Accepted))
        .await?;

    // This same stack owns both reads. Metadata cannot be interleaved with a
    // second Hello or normal route dispatch.
    let mut metadata_header = [0u8; 16];
    read_handshake_exact(server, conn, reader, &mut metadata_header).await?;
    let (total_len, body) = frame::decode_total_len(&metadata_header)
        .map_err(|error| ServerError::Protocol(format!("handshake metadata header: {error}")))?;
    let (header, _) = decode_frame_body(body, total_len)
        .map_err(|error| ServerError::Protocol(format!("handshake metadata header: {error}")))?;
    if total_len < FRAME_FIXED_BODY_LEN
        || u64::from(total_len) > server.config.max_frame_size
        || header.request_id != 0
        || header.flags != FLAG_HANDSHAKE
    {
        return Err(ServerError::Protocol(
            "invalid handshake metadata header".into(),
        ));
    }
    let mut metadata = vec![0u8; header.payload_len()];
    read_handshake_exact(server, conn, reader, &mut metadata).await?;
    handle_handshake(server, conn, &metadata, 0, writer).await
}

async fn handle_handshake(
    server: &Server,
    conn: &Connection,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<ConnectionWriter>,
) -> Result<(), ServerError> {
    let client_hs = decode_handshake(payload, false)
        .map_err(|e| ServerError::Protocol(format!("handshake decode: {e:?}")))?;

    // Frames identify peer backings by pool incarnation, index and generation.
    conn.init_peer_shm(client_hs.prefix, server.config.pool_decay_seconds)
        .map_err(ServerError::Protocol)?;

    conn.set_chunked_capable(true);

    // SHM direction negotiation: the ACK is the intersection of the client's
    // offer with this connection's actual native shared-memory reachability.
    // A client that did not offer a direction, or a stream that cannot
    // natively reach shared memory with its peer, keeps that direction off —
    // the peer then never receives coordinates it could not map, and the
    // frame-admission gates below reject any that still arrive.
    let native_shm = conn.native_shm_reachable();
    let negotiated_shm_request = native_shm && client_hs.capability_flags & CAP_SHM_REQUEST != 0;
    let negotiated_shm_response = native_shm && client_hs.capability_flags & CAP_SHM_RESPONSE != 0;
    conn.set_shm_request_capable(negotiated_shm_request);
    conn.set_shm_response_capable(negotiated_shm_response);

    let server_prefix = server.response_pool.read().prefix().to_string();
    let route_owners = server.dispatcher.read().await.routes_snapshot();
    let routes = route_owners
        .iter()
        .filter(|route| route.scheduler.route_admission_state() == RouteAdmissionState::Opened)
        .map(|route| RouteInfo {
            name: route.name.as_str(),
            route_uid: route.route_uid.as_str(),
            crm_ns: route.crm_ns.as_str(),
            crm_name: route.crm_name.as_str(),
            crm_ver: route.crm_ver.as_str(),
            abi_hash: route.abi_hash.as_str(),
            signature_hash: route.signature_hash.as_str(),
            max_payload_size: server.config.max_payload_size,
            methods: route
                .method_names
                .iter()
                .enumerate()
                .map(|(index, name)| MethodEntry {
                    name: name.as_str(),
                    index: index as u16,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let mut cap = 0;
    if negotiated_shm_request {
        cap |= CAP_SHM_REQUEST;
    }
    if negotiated_shm_response {
        cap |= CAP_SHM_RESPONSE;
    }
    let hs_bytes = encode_server_handshake(cap, &routes, &server_prefix, server.identity())
        .map_err(|error| ServerError::Protocol(error.to_string()))?;
    let frame = encode_frame(request_id, FLAG_HANDSHAKE | FLAG_RESPONSE, &hs_bytes);

    writer.write_all(&frame).await?;
    // Business admission opens only after the complete final ACK is written.
    conn.set_handshake_done(true)
        .map_err(|error| ServerError::Protocol(error.to_string()))?;
    conn.select_ordinary_protocol();
    Ok(())
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

async fn handle_shutdown_signal(
    server: &Server,
    payload: &[u8],
    request_id: u64,
    writer: &Arc<ConnectionWriter>,
) -> bool {
    if decode_shutdown_initiate(payload).is_err() {
        return false;
    }
    let frame = encode_frame(
        request_id,
        FLAG_RESPONSE | FLAG_SIGNAL,
        &encode_shutdown_ack(),
    );
    // Enqueue the courtesy ACK before closing admission. Completion is observed
    // through the server shutdown transaction, independently of this write.
    let _ = writer.queue_control(frame);
    server.request_shutdown_signal();
    true
}

async fn handle_signal(
    payload: &[u8],
    request_id: u64,
    writer: &Arc<ConnectionWriter>,
    conn: &Connection,
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

    if matches!(action, SignalAction::Disconnect) {
        // Parsing DISCONNECT precedes any ACK write failure on another task.
        // This records the first cause only; the caller still deactivates after
        // enqueueing the courtesy reply; native read-side end settles loans.
        conn.note_peer_close_message();
    }
    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, reply);
    #[cfg(test)]
    let pause = conn.signal_reply_pause.lock().take();
    #[cfg(test)]
    let drain_reply = if let Some((entered, resume)) = pause {
        let _ = entered.send(());
        let _ = resume.await;
        true
    } else {
        false
    };
    let _ = writer.queue_control(frame);
    #[cfg(test)]
    if drain_reply {
        // Allow the writer's terminal result to win the scheduling race.
        conn.wait_idle().await;
    }
    action
}

async fn handle_ctrl(
    server: &Server,
    payload: &[u8],
    request_id: u64,
    reply: crate::writer::ControlReplyPermit,
) {
    let Some(msg_type) = payload.first().and_then(|&b| MsgType::from_byte(b)) else {
        return;
    };
    let response = match msg_type {
        MsgType::PendingRouteAttest => pending_route_attestation_payload(server, payload),
        MsgType::RouteList => route_list_payload(server, payload).await,
        MsgType::RouteLookup => route_lookup_payload(server, payload).await,
        _ => return,
    };
    reply.send(encode_frame(
        request_id,
        FLAG_RESPONSE | FLAG_CTRL,
        &response,
    ));
}

async fn route_records(server: &Server) -> Vec<RouteRecordWire> {
    server
        .dispatcher
        .read()
        .await
        .routes_snapshot()
        .iter()
        .map(|route| route_record(server, route))
        .collect()
}

fn route_record(server: &Server, route: &CrmRoute) -> RouteRecordWire {
    let admission = route.scheduler.route_admission_state();
    RouteRecordWire {
        route_name: route.name.clone(),
        route_uid: route.route_uid.clone(),
        owner_server_id: server.server_id().to_string(),
        owner_server_instance_id: server.server_instance_id().to_string(),
        contract: RouteContractWire {
            route_name: route.name.clone(),
            crm_ns: route.crm_ns.clone(),
            crm_name: route.crm_name.clone(),
            crm_ver: route.crm_ver.clone(),
            abi_hash: route.abi_hash.clone(),
            signature_hash: route.signature_hash.clone(),
        },
        methods: route
            .method_names
            .iter()
            .enumerate()
            .map(|(index, name)| RouteMethodWire {
                name: name.clone(),
                index: index as u16,
            })
            .collect(),
        max_payload_size: server.config.max_payload_size,
        state: match admission {
            RouteAdmissionState::Opened => RouteStateWire::Ready,
            RouteAdmissionState::CommittedPending | RouteAdmissionState::Closed => {
                RouteStateWire::Closed
            }
        },
        state_reason: match admission {
            RouteAdmissionState::Opened | RouteAdmissionState::CommittedPending => {
                RouteStateReasonWire::RegisterCommitted
            }
            RouteAdmissionState::Closed if server.business_admission_closed() => {
                RouteStateReasonWire::Shutdown
            }
            RouteAdmissionState::Closed => RouteStateReasonWire::ExplicitUnregister,
        },
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

fn route_catalog_nack_payload(message: String) -> Vec<u8> {
    encode_route_nack(&RouteNack {
        error: C2Error::new(ErrorCode::ProtocolViolation, message).envelope(),
    })
    .unwrap_or_default()
}

async fn route_list_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let request = match decode_route_list_request(payload) {
        Ok(request) => request,
        Err(error) => return route_catalog_nack_payload(error),
    };
    let routes = route_records(server)
        .await
        .into_iter()
        .filter(|record| match &request.selector {
            RouteSelector::All => true,
            RouteSelector::RouteName { route_name } => route_name == &record.route_name,
            RouteSelector::Contract { expected } => expected == &record.contract,
        })
        .collect();
    encode_route_list_response(&RouteListResponse { routes })
        .unwrap_or_else(route_catalog_nack_payload)
}

async fn route_lookup_payload(server: &Server, payload: &[u8]) -> Vec<u8> {
    let request = match decode_route_lookup_request(payload) {
        Ok(request) => request,
        Err(error) => return route_catalog_nack_payload(error),
    };
    let dispatcher = server.dispatcher.read().await;
    let response = if let Some(route) = dispatcher.resolve(&request.expected.route_name) {
        let current = route_record(server, &route);
        if current.contract != request.expected {
            RouteLookupResponse::ContractMismatch { current }
        } else if request
            .observed_route_uid
            .is_some_and(|uid| uid != current.route_uid)
        {
            RouteLookupResponse::Stale { current }
        } else if current.state != RouteStateWire::Ready {
            RouteLookupResponse::Closed {
                route_name: current.route_name,
                route_uid: current.route_uid,
                reason: current.state_reason,
            }
        } else {
            RouteLookupResponse::Ready { current }
        }
    } else if let Some(uid) = dispatcher.removed_uid(&request.expected.route_name) {
        RouteLookupResponse::Removed {
            route_name: request.expected.route_name,
            route_uid: Some(uid),
        }
    } else {
        RouteLookupResponse::NotFound {
            route_name: request.expected.route_name,
        }
    };
    encode_route_lookup_response(&response).unwrap_or_else(route_catalog_nack_payload)
}

#[derive(Debug)]
enum RouteExecutionError {
    Input(String),
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
    // A partially received request keeps the connection open between chunks.
    _flight: crate::connection::FlightGuard,
}

enum RouteAdmissionError {
    RouteNotFound(String),
    RouteRemoved {
        route_name: String,
        route_uid: Option<String>,
    },
    RouteStale {
        route_name: String,
        expected_uid: String,
        actual_uid: String,
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
    F: FnOnce(Vec<u8>) -> Result<ResponseMeta, CrmError> + Send + 'static,
{
    let route_guard = tokio::select! {
        result = route_pending_permit.async_activate() => match result {
            Ok(guard) => guard,
            Err(err) => {
                drop(request);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        _ = conn.wait_work_cancelled() => {
            drop(request);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    let execution_guard = tokio::select! {
        result = execution_scheduler.async_acquire_accepted(0, route_guard.admission_generation()) => match result {
            Ok(guard) => guard,
            Err(err) => {
                drop(request);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        err = route_guard.wait_rejected() => {
            drop(request);
            return Err(RouteExecutionError::Acquire(err));
        },
        _ = conn.wait_work_cancelled() => {
            drop(request);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    if let Err(err) = route_guard.check_admission() {
        drop(request);
        return Err(RouteExecutionError::Acquire(err));
    }
    if conn.is_work_cancelled() {
        drop(request);
        return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
    }

    tokio::task::spawn_blocking(move || {
        let mut route_guard = route_guard;
        let _execution_guard = execution_guard;
        if let Err(err) = route_guard.commit() {
            drop(request);
            return Err(RouteExecutionError::Acquire(err));
        }
        let bytes = request
            .into_owned_bytes()
            .map_err(RouteExecutionError::Input)?;
        f(bytes).map_err(RouteExecutionError::Crm)
    })
    .await
    .expect("route execution task panicked")
}

async fn reserve_accepted_route_execution(
    server: &Server,
    identity: &RouteCallIdentity,
    method_idx: u16,
) -> Result<RouteExecutionAdmission, RouteAdmissionError> {
    // Capture before any dispatcher wait, but preserve identity/contract/method
    // errors ahead of host availability. The final reservation checks this
    // same generation after those validations and all lookup waits.
    let admission_generation = server.admission.lock().generation;
    let route = {
        let dispatcher = server.dispatcher.read().await;
        match dispatcher.resolve(&identity.route_name) {
            Some(route) => route,
            None => {
                return Err(match dispatcher.removed_uid(&identity.route_name) {
                    Some(route_uid) => RouteAdmissionError::RouteRemoved {
                        route_name: identity.route_name.clone(),
                        route_uid: Some(route_uid),
                    },
                    None => RouteAdmissionError::RouteNotFound(identity.route_name.clone()),
                });
            }
        }
    };
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
    if route.route_uid != identity.route_uid {
        return Err(RouteAdmissionError::RouteStale {
            route_name: identity.route_name.clone(),
            expected_uid: identity.route_uid.clone(),
            actual_uid: route.route_uid.clone(),
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
        .reserve_pending_in_generation(method_idx, admission_generation)
        .map_err(RouteAdmissionError::Acquire)?;
    Ok(RouteExecutionAdmission {
        route,
        pending_permit,
    })
}

async fn send_route_execution_result(
    server: &Server,
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
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
                conn.shm_response_capable(),
            )
            .await
            {
                match err {
                    ResponseSendError::UserVisible(message)
                    | ResponseSendError::Capacity(message) => {
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
        Err(RouteExecutionError::Input(error)) => {
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(ErrorCode::ResourceInputDeserializing, error)),
            )
            .await;
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
        Err(RouteExecutionError::Acquire(SchedulerAcquireError::Unavailable)) => {
            write_reply(
                writer,
                request_id,
                &ReplyControl::Error(error_wire(
                    ErrorCode::ResourceUnavailable,
                    "host admission unavailable",
                )),
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

fn route_admission_reply(err: RouteAdmissionError) -> ReplyControl {
    let (code, message) = match err {
        RouteAdmissionError::RouteNotFound(name) => return ReplyControl::RouteNotFound(name),
        RouteAdmissionError::RouteRemoved {
            route_name,
            route_uid,
        } => (
            ErrorCode::ResourceRemoved,
            match route_uid {
                Some(uid) => format!("route {route_name} was removed: route_uid {uid}"),
                None => format!("route {route_name} was removed"),
            },
        ),
        RouteAdmissionError::RouteStale {
            route_name,
            expected_uid,
            actual_uid,
        } => (
            ErrorCode::RouteStale,
            format!(
                "stale route token for {route_name}: expected route_uid {expected_uid}, current route_uid {actual_uid}"
            ),
        ),
        RouteAdmissionError::ContractMismatch { route_name, reason } => (
            ErrorCode::ContractMismatch,
            format!("contract mismatch for route {route_name}: {reason}"),
        ),
        RouteAdmissionError::UnknownMethod {
            route_name,
            method_idx,
        } => (
            ErrorCode::ResourceFunctionExecuting,
            format!("unknown method index {method_idx} for route {route_name}"),
        ),
        RouteAdmissionError::Acquire(SchedulerAcquireError::Closed) => {
            (ErrorCode::ResourceClosed, "route closed".into())
        }
        RouteAdmissionError::Acquire(SchedulerAcquireError::Unavailable) => (
            ErrorCode::ResourceUnavailable,
            "host admission unavailable".into(),
        ),
        RouteAdmissionError::Acquire(SchedulerAcquireError::Capacity { field, limit }) => (
            ErrorCode::ResourceUnavailable,
            format!("route concurrency capacity exceeded: {field}={limit}"),
        ),
    };
    ReplyControl::Error(error_wire(code, message))
}

async fn write_route_admission_error(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    err: RouteAdmissionError,
) {
    write_reply(writer, request_id, &route_admission_reply(err)).await;
}

async fn queue_route_admission_error(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    err: RouteAdmissionError,
) {
    queue_reply(writer, request_id, &route_admission_reply(err)).await;
}

struct AdmittedCall<'a> {
    server: &'a Server,
    conn: &'a Connection,
    request_id: u64,
    payload: &'a [u8],
    control_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &'a Arc<ConnectionWriter>,
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

    send_route_execution_result(server, conn, writer, request_id, result).await;
    drop(_pending_permit);
}

// CRM call dispatch — buddy SHM

fn admit_peer_request(
    conn: &Arc<Connection>,
    bp: BuddyPayload,
    _budget: &MemoryBudget,
) -> Result<RequestData, String> {
    let cache = conn
        .peer_mappings()
        .ok_or_else(|| "peer pool not initialised".to_string())?;
    // Bind the original OS stream and mapping before the first fallible map.
    // Even failed copies/admission end this node and queue a release record.
    let loan = {
        let _publication = conn.publication_gate.lock();
        conn.core
            .admit_remote_loan(
                bp.loan_id,
                Box::new((conn.remote_read_guard(), Arc::clone(&cache))),
            )
            .map_err(|error| error.to_string())?
    };
    PeerShmRequest::admit(cache, bp, loan).map(RequestData::Shm)
}

fn cleanup_buddy_request_block(conn: &Arc<Connection>, payload: &[u8], budget: &MemoryBudget) {
    let result =
        decode_request_buddy(conn, payload).and_then(|bp| admit_peer_request(conn, bp, budget));
    if let Err(error) = result {
        warn!(conn_id = conn.conn_id(), %error, "request discard failed");
    }
}

fn decode_request_buddy(conn: &Connection, payload: &[u8]) -> Result<BuddyPayload, String> {
    decode_buddy_payload(payload)
        .map(|(buddy, _)| buddy)
        .map_err(|error| {
            // No decoded descriptor means there is no trustworthy loan ID to
            // release. Seal admission, but keep the next frame boundary.
            conn.end(ConnectionEndReason::ProtocolError, "buddy_payload");
            error.to_string()
        })
}

struct AdmittedBuddyCall<'a> {
    server: &'a Server,
    conn: &'a Arc<Connection>,
    request_id: u64,
    request: RequestData,
    payload: &'a [u8],
    ctrl_consumed: usize,
    route: Arc<CrmRoute>,
    method_idx: u16,
    writer: &'a Arc<ConnectionWriter>,
    _pending_permit: OwnedSemaphorePermit,
    route_pending_permit: SchedulerPendingPermit,
}

async fn dispatch_admitted_buddy_call(call: AdmittedBuddyCall<'_>) {
    let AdmittedBuddyCall {
        server,
        conn,
        request_id,
        request,
        payload,
        ctrl_consumed,
        route,
        method_idx,
        writer,
        _pending_permit,
        route_pending_permit,
    } = call;

    // 3. Check for extra inline args appended after control header.
    let inline_start = BUDDY_PAYLOAD_SIZE + ctrl_consumed;
    let extra_args = if inline_start < payload.len() {
        &payload[inline_start..]
    } else {
        &[]
    };

    let extra_args = extra_args.to_vec();

    let callback = Arc::clone(&route.callback);
    let name = route.name.clone();
    let resp_pool = Arc::clone(&server.response_pool);

    let execution_scheduler = server.execution_scheduler.clone();
    let result = execute_route_request(
        execution_scheduler,
        route_pending_permit,
        conn,
        request,
        move |mut request| {
            request.extend_from_slice(&extra_args);
            callback.invoke(&name, method_idx, request, resp_pool)
        },
    )
    .await;

    send_route_execution_result(server, conn, writer, request_id, result).await;
    drop(_pending_permit);
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
    writer: &Arc<ConnectionWriter>,
    chunk_processing_permit: OwnedSemaphorePermit,
    ordering: ChunkFrameOrdering,
) {
    let peer_request = if c2_wire::flags::is_buddy(flags) {
        Some(
            decode_request_buddy(conn, payload)
                .and_then(|bp| admit_peer_request(conn, bp, &server.memory_budget)),
        )
    } else {
        None
    };
    let srv = Arc::clone(server);
    let cn = Arc::clone(conn);
    let wr = Arc::clone(writer);
    let pl = payload.to_vec();
    let flight = crate::connection::FlightGuard::new(conn);
    tokio::spawn(async move {
        let _flight = flight;
        dispatch_chunked_call(
            &srv,
            &cn,
            request_id,
            flags,
            &pl,
            &wr,
            chunk_processing_permit,
            ordering,
            peer_request,
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

/// Only a continuation of a request accepted before deactivation may join it.
fn accepted_chunk_continuation(
    server: &Server,
    conn_id: u64,
    request_id: u64,
    flags: u32,
    payload: &[u8],
) -> bool {
    if !c2_wire::flags::is_chunked(flags) {
        return false;
    }
    let offset = if c2_wire::flags::is_buddy(flags) {
        BUDDY_PAYLOAD_SIZE
    } else {
        0
    };
    matches!(decode_chunk_header(payload, offset), Ok((index, _, _)) if index > 0)
        && (server.chunk_registry.contains(conn_id, request_id)
            || server.chunk_admission_waiter(conn_id, request_id).is_some())
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
    writer: &Arc<ConnectionWriter>,
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

/// Roll back an uncommitted first chunk by its exact assembly generation.
/// The admission gate still fences publication; rollback releases the pending
/// permit before the backing, including cancellation and a losing commit.
struct ChunkPublicationGuard<'a> {
    server: &'a Server,
    assembly: Option<c2_wire::chunk::ChunkAssemblyId>,
}

impl Drop for ChunkPublicationGuard<'_> {
    fn drop(&mut self) {
        if let Some(assembly) = self.assembly.take()
            && let Some((assembler, admission)) = self.server.chunk_registry.take_id(assembly)
        {
            drop(admission);
            drop(assembler);
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
    writer: &Arc<ConnectionWriter>,
    chunk_processing_permit: OwnedSemaphorePermit,
    ordering: ChunkFrameOrdering,
    peer_request: Option<Result<RequestData, String>>,
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
                ChunkAdmissionOutcome::Refused | ChunkAdmissionOutcome::Aborted => {
                    Some("chunk admission ended before this later chunk could feed")
                }
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
                    drop(peer_request);
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
        offset = BUDDY_PAYLOAD_SIZE;
        match peer_request
            .expect("buddy frame admitted before spawn")
            .and_then(RequestData::into_owned_bytes)
        {
            Ok(data) => {
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
            // the dispatcher write lock). A peer end must not leave this
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
                    reserve_accepted_route_execution(server, &ctrl.identity, ctrl.method_idx) => {
                    AdmissionWait::Ready(admission)
                }
                _ = terminal => AdmissionWait::Terminal,
                _ = conn.wait_work_cancelled() => AdmissionWait::Cancelled,
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

        // Assembly and route admission publish together. A rejected insert
        // returns the permit, which stays live through the error reply await.
        let assembly = match server.chunk_registry.insert(
            conn.conn_id(),
            request_id,
            total_chunks as usize,
            chunk_size,
            ChunkRouteAdmission {
                route: route_admission.route,
                method_idx: ctrl.method_idx,
                pending_permit: route_admission.pending_permit,
                _flight: crate::connection::FlightGuard::new(conn),
            },
        ) {
            Ok(assembly) => assembly,
            Err((e, _admission)) => {
                // Correlate the admission failure back to the pending caller
                // instead of logging and leaving it waiting. The message
                // carries the budget cell and size detail from the admission
                // error.
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
        let mut publication = ChunkPublicationGuard {
            server,
            assembly: Some(assembly),
        };
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
        publication.assembly = None;
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
        let (assembler, chunk_admission) =
            match server.chunk_registry.take(conn.conn_id(), request_id) {
                Ok(owned) => owned,
                Err(_) => {
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
        let payload_size = assembler.written_end();
        if payload_size as u64 > server.config.max_payload_size {
            drop(assembler);
            drop(chunk_admission);
            fail_chunked_request(
                server,
                conn,
                writer,
                request_id,
                admission_owner.take(),
                ErrorCode::ResourceUnavailable,
                format!(
                    "request payload size {payload_size} exceeds max_payload_size {}",
                    server.config.max_payload_size
                ),
            )
            .await;
            return;
        }
        let backing = match assembler.finish() {
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
        // The carrier owns the reassembly pool, handle, and budget charge;
        // its release ordering (storage through the pool authority first,
        // charge refund after) applies on every path below.
        let request = RequestData::Handle(backing);
        let _pending_permit = match server.try_acquire_pending_request() {
            Ok(permit) => permit,
            Err(limit) => {
                drop(request);
                write_server_pending_capacity_error(writer, request_id, limit).await;
                return;
            }
        };
        drop(chunk_processing_permit);
        let method_idx = chunk_admission.method_idx;

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

        send_route_execution_result(server, conn, writer, request_id, result).await;
        drop(_pending_permit);
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

const REPLY_FLAGS: u32 = FLAG_RESPONSE | FLAG_REPLY_V2;
const SUCCESS_REPLY: [u8; 1] = [c2_wire::control::STATUS_SUCCESS];

async fn queue_input_error(writer: &Arc<ConnectionWriter>, request_id: u64, error: String) {
    queue_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceInputDeserializing, error)),
    )
    .await;
}

fn checked_frame_total_len(payload_len: usize, context: &str) -> Result<u32, String> {
    let total_len = 12usize
        .checked_add(payload_len)
        .ok_or_else(|| format!("{context} length overflow"))?;
    u32::try_from(total_len)
        .map_err(|_| format!("{context} length {total_len} exceeds u32 frame limit"))
}

fn inline_reply_total_len(data_len: usize) -> Result<u32, String> {
    let payload_len = SUCCESS_REPLY
        .len()
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
enum ResponseSendError {
    UserVisible(String),
    Capacity(String),
    Transport(String),
}

impl std::fmt::Display for ResponseSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserVisible(message) | Self::Capacity(message) | Self::Transport(message) => {
                f.write_str(message)
            }
        }
    }
}

fn response_write_error(context: &str, error: std::io::Error) -> ResponseSendError {
    let message = format!("{context}: {error}");
    if error.kind() == std::io::ErrorKind::OutOfMemory {
        ResponseSendError::Capacity(message)
    } else {
        ResponseSendError::Transport(message)
    }
}

async fn write_server_pending_capacity_error(
    writer: &Arc<ConnectionWriter>,
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
async fn queue_server_pending_capacity_error(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    limit: u32,
) {
    let msg = format!("server execution capacity exceeded: max_pending_requests={limit}");
    queue_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
    )
    .await;
}

async fn queue_chunk_processing_capacity_error(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    limit: u32,
) {
    let msg = format!("server chunk processing capacity exceeded: max_total_chunks={limit}");
    queue_reply(
        writer,
        request_id,
        &ReplyControl::Error(error_wire(ErrorCode::ResourceUnavailable, msg)),
    )
    .await;
}

async fn write_reply(writer: &Arc<ConnectionWriter>, request_id: u64, ctrl: &ReplyControl) {
    let payload = encode_reply_control(ctrl);
    let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
    if let Err(err) = writer.write_all(&frame).await {
        warn!(request_id, error = %err, "failed to queue reply frame");
    }
}
async fn queue_reply(writer: &Arc<ConnectionWriter>, request_id: u64, ctrl: &ReplyControl) {
    let payload = encode_reply_control(ctrl);
    let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
    if let Err(err) = writer.queue_control(frame) {
        warn!(request_id, error = %err, "failed to queue reply frame");
    }
}

/// Write a success reply: control header (STATUS_SUCCESS) + result data.
/// Uses stack buffer for small responses (≤1024B total frame) to avoid heap allocation.
async fn write_reply_with_data(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    data: &[u8],
) -> Result<(), ResponseSendError> {
    let ctrl_bytes = SUCCESS_REPLY;
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
            .write_all(&buf[..off])
            .await
            .map_err(|error| response_write_error("inline reply write failed", error))?;
    } else {
        // Large response: heap Vec (existing path)
        let frame = frame::encode_frame_parts(request_id, REPLY_FLAGS, &[&ctrl_bytes, data]);
        writer
            .write_all(&frame)
            .await
            .map_err(|error| response_write_error("inline reply write failed", error))?;
    }
    Ok(())
}

/// Send a filled SHM owner once; publication never falls back to another transport.
async fn send_prepared_shm_response(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    prepared: PreparedShmResponse,
    chunk_size: usize,
) -> Result<(), ResponseSendError> {
    if let Some(prepared) = writer
        .write_prepared(request_id, prepared)
        .await
        .map_err(|error| response_write_error("prepared SHM reply write failed", error))?
    {
        let bytes = prepared
            .copy_logical_bytes()
            .map_err(ResponseSendError::UserVisible)?;
        drop(prepared);
        if bytes.len() > chunk_size || inline_reply_total_len(bytes.len()).is_err() {
            write_chunked_reply(writer, request_id, &bytes, chunk_size).await?;
        } else {
            write_reply_with_data(writer, request_id, &bytes).await?;
        }
    }
    Ok(())
}

/// Write a success reply via chunked transfer: split data into chunks,
/// each with reply chunk meta header. Uses FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_CHUNKED.
/// Last chunk also sets FLAG_CHUNK_LAST.
async fn write_chunked_reply(
    writer: &Arc<ConnectionWriter>,
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

        writer
            .write_all(&frame)
            .await
            .map_err(|error| response_write_error("chunked reply write failed", error))?;
    }
    Ok(())
}

/// Dispatch a `ResponseMeta` to the appropriate reply path.
///
/// `shm_response_allowed` is this connection's negotiated server→client SHM
/// response direction: while false, no reply on this connection ever carries
/// response coordinates — owned bytes go out inline or chunked instead.
async fn send_response_meta(
    response_pool: &crate::ResponsePool,
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    meta: ResponseMeta,
    shm_threshold: u64,
    chunk_size: usize,
    max_payload_size: u64,
    shm_response_allowed: bool,
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
                shm_response_allowed,
            )
            .await?;
        }
        ResponseMeta::Empty => {
            write_reply_with_data(writer, request_id, &[]).await?;
        }
        ResponseMeta::ShmAlloc(prepared) => {
            let data_size = prepared.data_size();
            ensure_response_meta_len(data_size as usize, max_payload_size)?;
            if !shm_response_allowed {
                let bytes = prepared
                    .copy_logical_bytes()
                    .map_err(ResponseSendError::UserVisible)?;
                drop(prepared);
                return smart_reply_with_data(
                    response_pool,
                    writer,
                    request_id,
                    &bytes,
                    shm_threshold,
                    chunk_size,
                    false,
                )
                .await;
            }
            send_prepared_shm_response(writer, request_id, prepared, chunk_size).await?;
        }
    }
    Ok(())
}

/// Choose buddy SHM or inline reply based on data size and threshold.
async fn smart_reply_with_data(
    response_pool: &crate::ResponsePool,
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    data: &[u8],
    shm_threshold: u64,
    chunk_size: usize,
    shm_response_allowed: bool,
) -> Result<(), ResponseSendError> {
    if data.len() as u64 > shm_threshold {
        if shm_response_allowed {
            match try_prepare_shm_response(response_pool, shm_threshold, data.len(), |dst| {
                dst.copy_from_slice(data);
                Ok(())
            }) {
                Ok(Some(prepared)) => {
                    return send_prepared_shm_response(writer, request_id, prepared, chunk_size)
                        .await;
                }
                // Preserve pre-write fallback. Once prepared, a write error returns above.
                Ok(None) => {}
                Err(error) => return Err(ResponseSendError::UserVisible(error)),
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
#[path = "server_tests.rs"]
mod tests;
