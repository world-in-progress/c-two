//! Local IPC server — accept loop, per-connection frame handler, CRM dispatch.
//!
//! CRM method execution is delegated to [`CrmCallback`] implementations
//! supplied by language bindings or native runtime adapters.
//!
//! Dispatcher publication uses a short synchronous lock. Contract preparation,
//! payload allocation/copy, callbacks and asynchronous I/O happen outside it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use c2_local::{
    ConnectionEndReason, ConnectionEventSource, ConnectionEvents, DEFAULT_CONNECT_TIMEOUT,
    EndpointCredential, EndpointReapResult, LocalEndpoint, LocalListener, LocalReadHalf,
    LocalStream,
};
use parking_lot::RwLock;
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tracing::{debug, info, warn};

/// Label counter for server pools. MemPool adds its incarnation and owns
/// platform segment-name derivation.
static RESPONSE_POOL_GEN: AtomicU64 = AtomicU64::new(0);

use c2_config::{DomainLimits, MAX_HANDSHAKE_FRAME_BYTES, MAX_RPC_FRAME_BYTES};
use c2_error::{C2Error, ErrorCode};
use c2_mem::{DomainBudget, MemPool, MemoryReservation, OperationPermit};
use c2_wire::buddy::BuddyPayload;
use c2_wire::chunk::{ChunkAdmissionError, ChunkRequest};
use c2_wire::connection::calls::{CallLookup, ValueReceive};
use c2_wire::flags::{FLAG_CTRL, FLAG_HANDSHAKE, FLAG_RESPONSE, FLAG_SIGNAL};
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
use c2_wire::rpc::RouteCallIdentity;
use c2_wire::rpc::{
    self, CallMode, CallStatus, ChunkPosition, RpcRecord, RpcRecordKind, ValuePart, ValueStorage,
};
use c2_wire::shutdown_control::{decode_shutdown_initiate, encode_shutdown_ack};
use parking_lot::Mutex as CallMutex;

use crate::admission::AdmissionGate;
use crate::config::ServerIpcConfig;
use crate::connection::Connection;
use crate::dispatcher::{
    BuiltRoute, CrmCallback, CrmError, CrmRoute, CrmStreamCallback, Dispatcher, PeerShmRequest,
    PreparedShmResponse, RequestData, ResponseMeta, RouteBuildSpec,
};
use crate::response::try_prepare_shm_response;
use crate::scheduler::{
    RouteAdmissionState, Scheduler, SchedulerAcquireError, SchedulerLimits, SchedulerPendingPermit,
    SchedulerSnapshot, StreamCancelCause,
};
use crate::writer::ConnectionWriter;

const FRAME_FIXED_BODY_LEN: u32 = 12;
const SHUTDOWN_INITIATE_FRAME_BODY_LEN: u32 =
    FRAME_FIXED_BODY_LEN + c2_wire::msg_type::SHUTDOWN_CLIENT_BYTES.len() as u32;
const POST_SHUTDOWN_DUPLICATE_INITIATE_READ_TIMEOUT_MS: u64 = 100;
/// Default journal reason for an explicit direct IPC shutdown transaction.
pub const DIRECT_IPC_SHUTDOWN_REASON: &str = "direct_ipc_shutdown";
/// Bound an inline CALL_END error payload so the complete frame stays within
/// [`c2_config::MAX_RPC_FRAME_BYTES`]; UTF-8-safe truncation preserves the
/// error code and name.
fn error_wire(code: ErrorCode, message: impl Into<String>) -> Vec<u8> {
    rpc::bounded_call_end_error_bytes(&C2Error::new(code, message))
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

/// The main IPC server.
///
/// Binds a local OS endpoint, accepts connections, and dispatches CRM calls through
/// the [`Dispatcher`].  Each connection runs in its own tokio task with a
/// dedicated read and write drivers.
pub struct Server {
    identity: ServerIdentity,
    config: ServerIpcConfig,
    endpoint: LocalEndpoint,
    #[cfg(test)]
    test_write_timeout: Duration,
    /// The single route authority, published and reserved under a short lock.
    dispatcher: RwLock<Dispatcher>,
    shutdown_tx: watch::Sender<bool>,
    #[cfg(test)]
    shutdown_signal_pause: parking_lot::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    lifecycle_tx: watch::Sender<LifecycleSnapshot>,
    connection_event_source: ConnectionEventSource,
    route_registration: Mutex<()>,
    /// Sharded chunk reassembly lifecycle manager.
    chunk_registry: Arc<c2_wire::chunk::ChunkRegistry<()>>,
    /// **parking_lot sync RwLock** — guards SHM memory pool; blocking `.read()`
    /// / `.write()` (no `.await`).  Safe to hold briefly inside tokio tasks.
    response_pool: Arc<crate::ResponsePool>,
    /// Shared role-domain budget charged by the response pool, the reassembly
    /// pool inside `chunk_registry`, input copies and per-frame admission.
    /// One server direction, one budget; it owns only accounting counters and
    /// is never reset by shutdown.
    domain_budget: DomainBudget,
    admission: Arc<AdmissionGate>,
    /// Inbound chunk-copy concurrency gate (`C = clamp(P, 4, 64)`).
    copy_permits: Arc<Semaphore>,
    /// Stream item encoding concurrency gate (`E = clamp(P, 4, 64)`).
    encoding_permits: Arc<Semaphore>,
    /// Round-robin state for C-gated per-request chunk copies.
    copy_scheduler: Arc<CopyScheduler>,
    /// Shared weak handle so unit tests driving `handle_connection` directly
    /// without `run_inner` still share at most one coordinator while retaining
    /// zero strong `JoinHandle` ownership on `Server`.
    #[cfg(test)]
    direct_test_coordinator: parking_lot::Mutex<std::sync::Weak<AbortOnDrop>>,
    /// Per-connection control-reply slot budget `Q_ctrl = clamp(N, 1, 64)`.
    control_reply_capacity: usize,
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
    fn write_timeout(&self) -> Duration {
        #[cfg(test)]
        return self.test_write_timeout;
        #[cfg(not(test))]
        c2_config::WRITE_TIMEOUT
    }

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
        let budget = DomainBudget::from_limits(&DomainLimits::default());
        Self::new_with_identity_endpoint_and_budget(endpoint, config, identity, budget)
    }

    /// Construct with an explicit role-domain budget.
    ///
    /// A Host restart passes its retained [`DomainBudget`] here so in-flight
    /// accounting survives the new incarnation instead of starting at zero;
    /// plain constructors mint a fresh default-limits domain.
    pub fn new_with_identity_endpoint_and_budget(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
        identity: ServerIdentity,
        budget: DomainBudget,
    ) -> Result<Self, ServerError> {
        config
            .validate()
            .map_err(|error| ServerError::Config(error.to_string()))?;
        validate_server_identity(&identity)?;
        // One server direction, one finite budget: the response pool, the
        // chunk-reassembly pool, input copies and per-frame admission all
        // charge the same domain, so the server cannot double its configured
        // cap by owning two pools. The domain owns only accounting counters
        // and is never reset by shutdown; charges retained by outstanding
        // response data stay observable.
        let memory_limit = budget.snapshot().memory.limit_bytes;
        let reassembly_pool = {
            let pid = std::process::id();
            let ra_gen = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
            let prefix = format!("/cc3s{:08x}{:08x}", pid, ra_gen);
            // Centralized projection: reassembly follows the same buddy policy
            // as every other pool role (disabled buddy → dedicated/file storage).
            MemPool::new_with_prefix_and_budget(
                c2_mem::config::PoolConfig::reassembly_for_budget(memory_limit),
                prefix,
                budget.clone(),
            )
            .map_err(ServerError::Config)?
        };
        Self::with_endpoint_and_reassembly_pool(
            endpoint,
            config,
            identity,
            budget,
            Arc::new(parking_lot::RwLock::new(reassembly_pool)),
            Arc::new(AdmissionGate::new()),
        )
    }

    fn with_endpoint_and_reassembly_pool(
        endpoint: LocalEndpoint,
        config: ServerIpcConfig,
        identity: ServerIdentity,
        budget: DomainBudget,
        reassembly_pool: Arc<parking_lot::RwLock<MemPool>>,
        admission: Arc<AdmissionGate>,
    ) -> Result<Self, ServerError> {
        let (shutdown_tx, _) = watch::channel(false);
        // The injected test pool and the production reassembly pool both
        // charge the caller's domain budget, so both directions share one
        // accounting authority.
        let chunk_config =
            c2_wire::chunk::ChunkConfig::with_max_payload_size(config.max_payload_size);
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
            let memory_limit = budget.snapshot().memory.limit_bytes;
            MemPool::new_with_prefix_and_budget(
                c2_mem::config::PoolConfig::primary_for_budget(memory_limit),
                prefix,
                budget.clone(),
            )
            .map_err(ServerError::Config)?
        };
        let (lifecycle_tx, _lifecycle_rx) = watch::channel(LifecycleSnapshot {
            state: ServerLifecycleState::Initialized,
            credential: None,
        });
        // C/E gates and the writer's per-connection control budget all derive
        // from the same pool derivation the runtime builder uses.
        let derivation = crate::runtime::derive_blocking_pool(&config).map_err(ServerError::Io)?;
        let copy_permits = Arc::new(Semaphore::new(derivation.copy_permits));
        let encoding_permits = Arc::new(Semaphore::new(derivation.encoding_permits));
        // Q_ctrl = min(max(1, N_server), 64): bounded, connection-local
        // control/rejection replies. Even a closed domain (N = 0) keeps one
        // slot so refusals themselves can drain.
        let control_reply_capacity =
            budget.snapshot().operations.max_operations.clamp(1, 64) as usize;
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
            #[cfg(test)]
            test_write_timeout: c2_config::WRITE_TIMEOUT,
            dispatcher: RwLock::new(Dispatcher::new()),
            shutdown_tx,
            #[cfg(test)]
            shutdown_signal_pause: parking_lot::Mutex::new(None),
            lifecycle_tx,
            connection_event_source: ConnectionEventSource::new(),
            route_registration: Mutex::new(()),
            chunk_registry,
            response_pool: Arc::new(crate::ResponsePool::new(response_pool)),
            domain_budget: budget,
            admission,
            copy_permits,
            encoding_permits,
            copy_scheduler: Arc::new(CopyScheduler::new()),
            #[cfg(test)]
            direct_test_coordinator: parking_lot::Mutex::new(std::sync::Weak::new()),
            control_reply_capacity,
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

    fn cleanup_chunk_requests_for_connection(&self, conn_id: u64) {
        self.chunk_registry.cleanup_connection(conn_id);
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
            let dispatcher = self.dispatcher.read();
            dispatcher.routes_snapshot()
        };
        let outcomes = Self::wait_routes_drained(routes.clone(), closed_reason).await;
        for route in routes {
            route.scheduler.close();
        }
        self.dispatcher.write().take_all();
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

    pub fn build_stream_route(
        &self,
        mut spec: RouteBuildSpec,
        callback: Arc<dyn CrmStreamCallback>,
        descriptor: &c2_contract::ValidatedContractDescriptor,
    ) -> Result<BuiltRoute, ServerError> {
        if spec.crm_ns != descriptor.crm_namespace()
            || spec.crm_name != descriptor.crm_name()
            || spec.crm_ver != descriptor.crm_version()
            || spec.abi_hash != descriptor.abi_hash()
            || spec.signature_hash != descriptor.signature_hash()
            || spec.method_names.len() != descriptor.methods().len()
            || spec
                .method_names
                .iter()
                .zip(descriptor.methods())
                .any(|(name, method)| name != method.name())
        {
            return Err(ServerError::Protocol(
                "route spec differs from validated stream descriptor".into(),
            ));
        }
        let mut access_map = HashMap::with_capacity(descriptor.methods().len());
        let method_modes = descriptor
            .methods()
            .iter()
            .enumerate()
            .map(|(index, method)| {
                let index = u16::try_from(index).map_err(|_| {
                    ServerError::Protocol("stream method index exceeds wire range".into())
                })?;
                let access = match method.access() {
                    c2_contract::MethodAccess::Read => crate::scheduler::AccessLevel::Read,
                    c2_contract::MethodAccess::Write => crate::scheduler::AccessLevel::Write,
                };
                access_map.insert(index, access);
                Ok(match method.mode() {
                    c2_contract::MethodMode::Unary => CallMode::Unary,
                    c2_contract::MethodMode::ServerStream => CallMode::ServerStream,
                })
            })
            .collect::<Result<Vec<_>, ServerError>>()?;
        if spec
            .access_map
            .iter()
            .any(|(index, access)| access_map.get(index) != Some(access))
        {
            return Err(ServerError::Protocol(
                "route access map differs from validated stream descriptor".into(),
            ));
        }
        spec.access_map = access_map;
        let scheduler = Scheduler::with_admission(
            spec.concurrency_mode,
            spec.access_map.clone(),
            spec.limits,
            Arc::clone(&self.admission),
        );
        BuiltRoute::new(CrmRoute::new_stream(
            spec,
            scheduler,
            callback,
            method_modes,
        ))
    }

    /// Build a language-local stream route from modes already validated by
    /// its nonportable service definition. Portable routes use the descriptor
    /// checking entry point above.
    #[doc(hidden)]
    pub fn build_nonportable_stream_route(
        &self,
        spec: RouteBuildSpec,
        callback: Arc<dyn CrmStreamCallback>,
        method_modes: &[c2_contract::MethodMode],
    ) -> Result<BuiltRoute, ServerError> {
        if method_modes.len() != spec.method_names.len()
            || !method_modes
                .iter()
                .any(|mode| *mode == c2_contract::MethodMode::ServerStream)
        {
            return Err(ServerError::Protocol(
                "nonportable stream modes must cover the route and include a stream".into(),
            ));
        }
        let modes = method_modes
            .iter()
            .map(|mode| match mode {
                c2_contract::MethodMode::Unary => CallMode::Unary,
                c2_contract::MethodMode::ServerStream => CallMode::ServerStream,
            })
            .collect();
        let scheduler = Scheduler::with_admission(
            spec.concurrency_mode,
            spec.access_map.clone(),
            spec.limits,
            Arc::clone(&self.admission),
        );
        BuiltRoute::new(CrmRoute::new_stream(spec, scheduler, callback, modes))
    }

    pub async fn reserve_route(
        &self,
        route: BuiltRoute,
    ) -> Result<PendingRouteReservation, ServerError> {
        let route = route.into_route();
        let route_name = route.name.clone();
        let _guard = self.route_registration.lock().await;
        let dispatcher = self.dispatcher.read();
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
        let mut dispatcher = self.dispatcher.write();
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
        let dispatcher = self.dispatcher.read();
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
            let dispatcher = self.dispatcher.write();
            let route = dispatcher.resolve(name);
            if let Some(route) = &route {
                self.cancel_route_streams(Some(&route.route_uid), StreamCancelCause::Unregister);
                route
                    .scheduler
                    .close_and_cancel_local_streams(StreamCancelCause::Unregister);
            }
            route
        };
        if let Some(route) = route {
            route.scheduler.wait_drained_async().await;
            let mut dispatcher = self.dispatcher.write();
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
            let dispatcher = self.dispatcher.write();
            let mut seen = HashSet::new();
            names
                .iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|name| dispatcher.resolve(name))
                .inspect(|route| {
                    self.cancel_route_streams(Some(&route.route_uid), StreamCancelCause::Shutdown);
                    route
                        .scheduler
                        .close_and_cancel_local_streams(StreamCancelCause::Shutdown);
                })
                .collect::<Vec<_>>()
        };
        let outcomes = Self::wait_routes_drained(routes.clone(), closed_reason).await;
        let mut dispatcher = self.dispatcher.write();
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

    /// The server direction's role-domain budget.
    ///
    /// The handle shares only accounting counters; observing it never
    /// allocates, maps, connects, or resets anything.
    pub fn domain_budget(&self) -> &c2_mem::DomainBudget {
        &self.domain_budget
    }

    /// Read-only snapshot of the server direction's role-domain budget.
    ///
    /// Charges retained by outstanding response or reassembly data stay
    /// visible until their backing is released.
    pub fn domain_snapshot(&self) -> c2_mem::DomainSnapshot {
        self.domain_budget.snapshot()
    }

    /// Return the fixed response SHM threshold.
    pub fn response_shm_threshold(&self) -> u64 {
        c2_config::SHM_THRESHOLD
    }

    /// Return the configured maximum logical response payload size.
    pub fn response_max_payload_size(&self) -> u64 {
        self.config.max_payload_size
    }

    /// Capture a concrete route identity before starting its retirement transaction.
    pub async fn registered_route_identity(&self, name: &str) -> Option<String> {
        self.dispatcher
            .read()
            .resolve(name)
            .map(|route| route.route_uid.clone())
    }

    /// Return true if a route is currently registered.
    pub async fn contains_route(&self, name: &str) -> bool {
        self.dispatcher.read().resolve(name).is_some()
    }

    /// Return the current route scheduler state, if the route is registered.
    pub async fn route_scheduler_snapshot(&self, name: &str) -> Option<SchedulerSnapshot> {
        self.dispatcher
            .read()
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
                        if let Some(mut response_pool) = gc_server.response_pool.inner.try_write() {
                            response_pool.gc_buddy();
                        }
                        if let Some(mut reassembly_pool) = gc_server.chunk_registry.pool().try_write() {
                            reassembly_pool.gc_buddy();
                        }
                        let connections = gc_server.active_connections.lock()
                            .values().cloned().collect::<Vec<_>>();
                        for conn in connections {
                            if let Some(cache) = conn.peer_mappings() { cache.gc_idle(); }
                            let expired = conn.calls.lock().active().filter_map(|(id, call)| {
                                if !call.is_preparing_input() { return None; }
                                let retired = call.data.input.lock().as_ref().is_some_and(|input| input.request.storage_expired());
                                retired.then_some(id)
                            }).collect::<Vec<_>>();
                            let writer = conn.rpc_writer.lock().as_ref().and_then(std::sync::Weak::upgrade);
                            if let Some(writer) = writer {
                                for id in expired {
                                    queue_rejection(&writer, id, &error_wire(ErrorCode::ResourceUnavailable, "input assembly timed out; request was not executed")).await;
                                }
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
        // C-gated round-robin copies of inbound chunk frames. It outlives the
        // accept loop's business drain so accepted chunked calls still finish;
        // at terminal shutdown the coordinator itself is aborted (its spawned
        // frame workers finish on their own). The guard keeps the handle local
        // to `run_inner`: `Server` itself must not store a `JoinHandle`, which
        // would keep a cancelled executor's task state — and with it Windows
        // IOCP bookkeeping for accepted pipes — alive until the server drops.
        let mut copy_coordinator_guard = AbortOnDrop(tokio::spawn(copy_coordinator(
            Arc::downgrade(self),
            self.copy_scheduler.clone(),
            self.copy_permits.clone(),
        )));
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
                    copy_coordinator_guard.0.abort();
                    let _ = (&mut copy_coordinator_guard.0).await;
                    self.copy_scheduler.ready.lock().clear();
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
            let dispatcher = self.dispatcher.read();
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
        // The publication boundary and admission lock exclude a new local
        // stream between cancellation and the host drain transition.
        let publication = self.dispatcher.write();
        let mut registration = self.admission.lock();
        for route in publication.routes_snapshot() {
            route
                .scheduler
                .cancel_local_streams(StreamCancelCause::Shutdown);
        }
        registration.begin_drain();
        self.admission.notify.notify_waiters();
        drop(registration);
        // IPC stream publication holds the read side of this same boundary.
        self.cancel_route_streams(None, StreamCancelCause::Shutdown);
        drop(publication);
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

    fn cancel_route_streams(&self, route_uid: Option<&str>, cause: StreamCancelCause) {
        let connections = self
            .active_connections
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for conn in connections {
            let calls = conn.calls.lock().active().collect::<Vec<_>>();
            for (id, call) in calls {
                let matches = call
                    .data
                    .stream
                    .lock()
                    .as_ref()
                    .is_some_and(|stream| route_uid.is_none_or(|uid| stream.route_uid == uid));
                if matches {
                    submit_stream_cancel(&conn, id, &call, cause);
                }
            }
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
        let deadline = tokio::time::Instant::now() + server.write_timeout();
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
        // Direct unit-test callers drive connections without `run_inner`;
        // share one coordinator across active test connections via a weak slot
        // so `CopyScheduler` stays single-consumer while `Server` never holds
        // a strong `JoinHandle`. `run_inner`-driven connections skip this —
        // their coordinator is owned by the run loop's local guard.
        #[cfg(test)]
        let _copy_coordinator_guard = (!server.run_entered.load(Ordering::Acquire)).then(|| {
            let mut slot = server.direct_test_coordinator.lock();
            if let Some(existing) = slot.upgrade() {
                existing
            } else {
                let guard = Arc::new(AbortOnDrop(tokio::spawn(copy_coordinator(
                    Arc::downgrade(&server),
                    server.copy_scheduler.clone(),
                    server.copy_permits.clone(),
                ))));
                *slot = Arc::downgrade(&guard);
                guard
            }
        });
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
            server.write_timeout(),
            server.control_reply_capacity,
        );
        registration.writer_task = Some(writer_task);
        *conn.rpc_writer.lock() = Some(Arc::downgrade(&writer));

        debug!(conn_id, "connection accepted");

        receive_connection(
            &server,
            &conn,
            &mut reader,
            &writer,
            &abort,
            &mut shutdown_rx,
        )
        .await;
        drop(writer);
        let _ = registration.writer_task.as_mut().unwrap().await;
        registration.writer_task.take();
        drop(reader);
        conn.clear_remote_read_guard();
        conn.wait_idle().await;
        debug!(conn_id, "connection physically closed");
    }
}

/// The accepted connection's sole receive loop, including its handshake.
async fn receive_connection(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    reader: &mut LocalReadHalf,
    writer: &Arc<ConnectionWriter>,
    abort: &c2_local::AbortHandle,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let conn_id = conn.conn_id();
    let max_frame = MAX_RPC_FRAME_BYTES;
    let mut read_failed = false;

    'frames: loop {
        let mut len_buf = [0u8; 4];
        match read_connection_exact(reader, &mut len_buf, conn, shutdown_rx).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                record_read_error(conn, abort, "read_length", &error);
                read_failed = true;
                break;
            }
        }
        let total_len = u32::from_le_bytes(len_buf);

        if conn.write_peer_closed() && !conn.handshake_done() {
            // A failed handshake write leaves the peer at an unknown stage.
            // Drain its frames without metadata, admission or protocol checks;
            // only DISCONNECT or a real read end can supply the closing cause.
            let mut discard = [0u8; 8192];
            let mut remaining = total_len;
            while remaining != 0 {
                let take = remaining.min(discard.len() as u32) as usize;
                match read_connection_exact(reader, &mut discard[..take], conn, shutdown_rx).await {
                    Ok(true) => remaining -= take as u32,
                    Ok(false) => break 'frames,
                    Err(error) => {
                        record_read_error(conn, abort, "read_body", &error);
                        read_failed = true;
                        break 'frames;
                    }
                }
            }
            if total_len == FRAME_FIXED_BODY_LEN + c2_wire::msg_type::DISCONNECT_BYTES.len() as u32
                && discard[8..12] == FLAG_SIGNAL.to_le_bytes()
                && discard[12] == MsgType::Disconnect.as_byte()
            {
                conn.note_peer_close_message();
                conn.end(ConnectionEndReason::PeerCloseMessage, "disconnect");
            }
            continue;
        }

        let variable_frame_over_limit = u64::from(total_len) > max_frame;
        // Only the fixed-size preamble can reach header inspection outside
        // the variable-frame budget; its exact flags and bytes are checked below.
        if total_len < 12 || (variable_frame_over_limit && total_len != VERSION_PREAMBLE_BODY_LEN) {
            warn!(conn_id, total_len, "invalid frame length");
            conn.end(ConnectionEndReason::ProtocolError, "frame_length");
            break;
        }

        // Inspect the fixed header before reading any payload byte or
        // allocating. Even a header-only invalid preamble must be rejected
        // before reading the body.
        let mut prefix = [0u8; FRAME_FIXED_BODY_LEN as usize];
        let prefix_len = FRAME_FIXED_BODY_LEN as usize;
        match read_connection_exact(reader, &mut prefix[..prefix_len], conn, shutdown_rx).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                record_read_error(conn, abort, "read_frame_prefix", &error);
                read_failed = true;
                break;
            }
        }
        let (header, _) = decode_frame_body(&prefix[..prefix_len], total_len)
            .expect("validated fixed frame header");
        if header.flags == FLAG_HANDSHAKE {
            if conn.handshake_done() || conn.ordinary_protocol() {
                conn.end(ConnectionEndReason::ProtocolError, "duplicate_handshake");
                break;
            }
            let mut version_header = [0u8; 16];
            version_header[..4].copy_from_slice(&len_buf);
            version_header[4..].copy_from_slice(&prefix[..prefix_len]);
            let result = negotiate_handshake(server, conn, &version_header, reader, writer).await;
            if let Err(error) = result {
                warn!(conn_id, %error, "handshake failed");
                match &error {
                    ServerError::Io(error)
                        if conn.write_peer_closed() && c2_local::is_peer_closed_error(error) =>
                    {
                        // The next iteration drains the peer's remaining
                        // frames without restarting handshake interpretation.
                        continue;
                    }
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
        match read_connection_exact(reader, &mut body[prefix_len..], conn, shutdown_rx).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                record_read_error(conn, abort, "read_body", &error);
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

        if header.flags & c2_wire::flags::FLAG_RPC_RECORD != 0
            && payload.first() == Some(&(RpcRecordKind::Release as u8))
        {
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
        if flags & c2_wire::flags::FLAG_RPC_RECORD == 0
            && flags != FLAG_SIGNAL
            && flags != FLAG_CTRL
        {
            conn.end(ConnectionEndReason::ProtocolError, "request_flags");
            continue;
        }
        if header.is_signal() && header.flags & c2_wire::flags::FLAG_RPC_RECORD == 0 {
            // A standalone control exchange still commits the ordinary role.
            conn.select_ordinary_protocol();
            if matches!(
                payload.first().and_then(|&b| MsgType::from_byte(b)),
                Some(MsgType::ShutdownClient)
            ) {
                let deadline = tokio::time::Instant::now() + server.write_timeout();
                let initiated = handle_shutdown_signal(server, payload, request_id, writer).await;
                if initiated && !conn.handshake_done() {
                    // Adapter flush does not prove that a Named Pipe peer
                    // consumed the ACK. Its one-exchange client closes
                    // after reading it; retain the OS stream until then.
                    // Established RPC peers keep their ordinary receive
                    // loop so accepted calls and releases still progress.
                    wait_for_control_peer_close(reader, deadline).await;
                }
                if *server.shutdown_tx.borrow() {
                    conn.begin_shutdown();
                } else {
                    conn.end(ConnectionEndReason::LocalClose, "shutdown_control_close");
                }
                continue;
            }
            match handle_signal(payload, request_id, writer, conn).await {
                SignalAction::Continue => {}
                SignalAction::Disconnect => {
                    conn.end(ConnectionEndReason::PeerCloseMessage, "disconnect");
                }
            }
        } else if header.is_ctrl() && header.flags & c2_wire::flags::FLAG_RPC_RECORD == 0 {
            if let Ok(reply) = writer.reserve_control() {
                let srv = Arc::clone(server);
                let payload = payload.to_vec();
                tokio::spawn(async move {
                    handle_ctrl(&srv, &payload, request_id, reply).await;
                });
            }
        } else if header.flags & c2_wire::flags::FLAG_RPC_RECORD != 0 {
            handle_rpc_record(server, conn, writer, &header, payload).await;
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
                    record_read_error(conn, abort, "discard_read", &error);
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
        || u64::from(total_len) > MAX_HANDSHAKE_FRAME_BYTES
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
    conn.init_peer_shm(client_hs.prefix)
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
    let route_owners = server.dispatcher.read().routes_snapshot();
    let routes = route_owners
        .iter()
        .filter(|route| route.scheduler.route_admission_state() == RouteAdmissionState::Opened)
        .map(|route| RouteInfo {
            name: route.name.as_str(),
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
    // Reserve the ACK write before closing admission, but publish it only
    // after every connection refuses new calls. A reserved control frame
    // still writes after connection deactivation.
    let ack = writer.reserve_control();
    #[cfg(test)]
    let pause = server.shutdown_signal_pause.lock().take();
    #[cfg(test)]
    if let Some((entered, resume)) = pause {
        let _ = entered.send(());
        let _ = resume.await;
    }
    server.request_shutdown_signal();
    if let Ok(ack) = ack {
        ack.send(frame);
    }
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
    reply: crate::writer::QueuedReplyPermit,
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
    let routes = server.dispatcher.read().routes_snapshot();
    routes
        .iter()
        .map(|route| route_record(server, route))
        .collect()
}

fn route_record(server: &Server, route: &CrmRoute) -> RouteRecordWire {
    let admission = route.scheduler.route_admission_state();
    let host_admission_closed = server.business_admission_closed();
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
        state: if host_admission_closed {
            RouteStateWire::Closed
        } else {
            match admission {
                RouteAdmissionState::Opened => RouteStateWire::Ready,
                RouteAdmissionState::CommittedPending | RouteAdmissionState::Closed => {
                    RouteStateWire::Closed
                }
            }
        },
        state_reason: if host_admission_closed {
            RouteStateReasonWire::Shutdown
        } else {
            match admission {
                RouteAdmissionState::Opened | RouteAdmissionState::CommittedPending => {
                    RouteStateReasonWire::RegisterCommitted
                }
                RouteAdmissionState::Closed => RouteStateReasonWire::ExplicitUnregister,
            }
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
    let (route, was_removed) = {
        let dispatcher = server.dispatcher.read();
        (
            dispatcher.resolve(&request.expected.route_name),
            dispatcher.was_removed(&request.expected.route_name),
        )
    };
    let response = if let Some(route) = route {
        let current = route_record(server, &route);
        if current.contract != request.expected {
            RouteLookupResponse::ContractMismatch { current }
        } else if current.state != RouteStateWire::Ready {
            RouteLookupResponse::Closed {
                route_name: current.route_name,
                reason: current.state_reason,
            }
        } else {
            RouteLookupResponse::Ready { current }
        }
    } else if was_removed {
        RouteLookupResponse::Removed {
            route_name: request.expected.route_name,
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

#[derive(Default)]
pub(crate) struct ServerCall {
    input: CallMutex<Option<ServerInput>>,
    admission: CallMutex<Option<ChunkRouteAdmission>>,
    start_gate: CallMutex<()>,
    stream: CallMutex<Option<StreamCallControl>>,
    /// The call's role-domain operation permit (N admission). It drops with
    /// the record at the call's real terminal outcome — unary reply claimed or
    /// finished, stream cleanup plus terminal, or connection retirement.
    _operation: Option<OperationPermit>,
}

struct StreamCallControl {
    route_uid: String,
    cancellation: Option<StreamCancelCause>,
}

fn submit_stream_cancel(
    conn: &Connection,
    id: u64,
    call: &c2_wire::connection::calls::CallRecord<ServerCall>,
    cause: StreamCancelCause,
) {
    let _start = call.data.start_gate.lock();
    {
        let mut stream = call.data.stream.lock();
        let Some(stream) = stream.as_mut() else {
            return;
        };
        stream.cancellation.get_or_insert(cause);
    }
    // Only a consumer's CANCEL discards its pending item. Host lifecycle
    // cancellation keeps CREDIT live until the current item has been sent.
    if cause == StreamCancelCause::Consumer {
        if let Err(error) = conn.core.stream_cancel(id) {
            warn!(request_id=id, %error, "invalid stream cancel");
            conn.end(ConnectionEndReason::ProtocolError, "stream_cancel");
            return;
        }
    }
    let writer = conn
        .rpc_writer
        .lock()
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    if let Some(writer) = writer {
        if let Some(claim) = claim_terminal(&writer, id, CallStatus::NotExecuted) {
            let error = error_wire(ErrorCode::CallCancelled, cause.message());
            let frame = rpc::encode_call_end_frame(
                id,
                CallMode::ServerStream,
                claim.status,
                0,
                ValuePart::whole(ValueStorage::Inline(&error)),
            )
            .expect("canonical cancellation error");
            if let Err(error) = writer.queue_call_end(frame) {
                warn!(request_id=id, %error, "failed to queue cancelled CALL_END");
            }
        }
    }
    conn.writer_wake.notify_waiters();
}
struct ServerInput {
    receive: ValueReceive,
    queue_key: (u64, u64),
    scheduler: Arc<CopyScheduler>,
    request: ChunkRequest<()>,
    registry: Arc<c2_wire::chunk::ChunkRegistry<()>>,
    /// This request's pending-copy queue and its copy-scheduling state.
    copy: CallMutex<PendingCopyQueue>,
}

impl Drop for ServerInput {
    fn drop(&mut self) {
        self.scheduler.remove(self.queue_key);
        self.registry.settle(&self.request);
    }
}

/// One decoded chunk frame awaiting its C-gated blocking copy. `_memory`
/// covers the frame's inline payload bytes plus this queue node's storage
/// overhead and is released immediately after the copy.
pub(crate) struct PendingChunkFrame {
    chunk_idx: usize,
    first_geometry: Option<(u64, usize, usize)>,
    data: Result<RequestData, String>,
    _memory: MemoryReservation,
}

/// Queue-node storage overhead charged together with each frame's inline
/// payload bytes at frame admission.
pub(crate) const PENDING_FRAME_OVERHEAD: usize = std::mem::size_of::<PendingChunkFrame>();

/// Per-request pending-copy state. Continuations that arrive before the first
/// copy published the reassembly storage are only buffered here; they never
/// enter the ready queue, so they cannot exhaust blocking threads waiting on
/// `wait_ready`.
pub(crate) struct PendingCopyQueue {
    queue: VecDeque<PendingChunkFrame>,
    /// The first frame's copy completed `registry.allocate_request`.
    published: bool,
    /// Scheduled in the ready queue or being copied by a worker.
    readied: bool,
    /// Terminal: dispatched to the resource, refused, or discarded.
    finished: bool,
}

/// The request bytes plus the role-domain reservation that covers the local
/// copy. The reservation drops together with the owned `Vec<u8>` — after the
/// callback returns or a pre-execution refusal discards the data.
pub(crate) struct OwnedRequestInput {
    data: RequestData,
    memory: Option<MemoryReservation>,
}

impl OwnedRequestInput {
    #[cfg(test)]
    fn unreserved(data: RequestData) -> Self {
        Self { data, memory: None }
    }

    pub(crate) fn into_owned_bytes(self) -> Result<(Vec<u8>, Option<MemoryReservation>), String> {
        Ok((self.data.into_owned_bytes()?, self.memory))
    }
}

/// Server-wide round-robin queue of requests with copyable frames.
pub(crate) struct CopyScheduler {
    ready: parking_lot::Mutex<VecDeque<((u64, u64), std::sync::Weak<Connection>)>>,
    notify: Notify,
}

impl CopyScheduler {
    fn new() -> Self {
        Self {
            ready: parking_lot::Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    fn push_back(&self, conn: &Arc<Connection>, request_id: u64) {
        self.ready
            .lock()
            .push_back(((conn.conn_id(), request_id), Arc::downgrade(conn)));
        self.notify.notify_one();
    }

    fn pop_front(&self) -> Option<(Arc<Connection>, u64)> {
        let mut ready = self.ready.lock();
        while let Some(((_, id), conn)) = ready.pop_front() {
            if let Some(conn) = conn.upgrade() {
                return Some((conn, id));
            }
        }
        None
    }

    fn remove(&self, key: (u64, u64)) {
        self.ready.lock().retain(|(queued, _)| *queued != key);
    }

    fn is_empty(&self) -> bool {
        self.ready.lock().is_empty()
    }
}

/// Aborts the spawned copy-coordinator task when the guard drops.
///
/// The guard is held by `run_inner` (or, in direct `handle_connection` unit
/// tests, by the connection future), never by `Server`: a `JoinHandle` stored
/// on the server object would outlive a cancelled executor and keep the task,
/// its waker-owned scheduler slot and the Windows IOCP completion bookkeeping
/// for accepted named pipes referenced until the server itself drops.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct ChunkRouteAdmission {
    route: Arc<CrmRoute>,
    method_idx: u16,
    pending_permit: SchedulerPendingPermit,
    // A partially received request keeps the connection open between chunks.
    _flight: crate::connection::FlightGuard,
}

pub(crate) fn retire_connection_calls(conn: &Connection) {
    let calls = {
        let mut calls = conn.calls.lock();
        // Compete with the original worker's enter_resource CAS before removing
        // its table entry. An executor that has not entered may no longer start.
        for (_, call) in calls.active() {
            call.reject_before_execution();
        }
        std::mem::take(&mut *calls)
    };
    for (id, call) in calls.active() {
        conn.core.stream_retire(id);
        let input = call.data.input.lock().take();
        drop(input);
    }
    drop(calls);
}

fn request_storage(conn: &Arc<Connection>, value: ValueStorage<'_>) -> Result<RequestData, String> {
    match value {
        ValueStorage::Inline(bytes) => Ok(RequestData::Inline(bytes.to_vec())),
        ValueStorage::Shm(descriptor) => admit_peer_request(conn, descriptor),
    }
}

fn release_request_storage(conn: &Arc<Connection>, value: ValueStorage<'_>) {
    if let ValueStorage::Shm(descriptor) = value {
        drop(admit_peer_request(conn, descriptor));
    }
}

/// A refusal for an OPEN whose request-ID high-water mark already advanced
/// but whose active record is not inserted. No call record exists to claim,
/// and late continuations of this ID are dropped by the high-water mark.
async fn queue_open_refusal(writer: &Arc<ConnectionWriter>, request_id: u64, error: &C2Error) {
    write_error_record(
        writer,
        request_id,
        CallStatus::NotExecuted,
        &rpc::bounded_call_end_error_bytes(error),
        true,
    )
    .await;
}

/// Enqueue one frame into a request's pending-copy queue and re-ready the
/// request per the publication rules: a request joins the server-wide ready
/// queue only once its first frame published the reassembly storage, and a
/// request whose queue emptied waits for the next continuation to re-ready it.
fn enqueue_copy_frame(server: &Server, conn: &Arc<Connection>, id: u64, frame: PendingChunkFrame) {
    let call = match conn.calls.lock().get(id) {
        Some(call) => call,
        None => return,
    };
    {
        let input_guard = call.data.input.lock();
        let Some(input) = input_guard.as_ref() else {
            return;
        };
        let mut queue = input.copy.lock();
        if queue.finished {
            return;
        }
        queue.queue.push_back(frame);
        if queue.published && !queue.readied {
            queue.readied = true;
            server.copy_scheduler.push_back(conn, id);
        }
    }
}

async fn handle_rpc_record(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    writer: &Arc<ConnectionWriter>,
    header: &frame::FrameHeader,
    payload: &[u8],
) {
    let id = header.request_id;
    let new_open = payload.first() == Some(&(RpcRecordKind::Open as u8))
        && (header.flags & c2_wire::flags::FLAG_CHUNKED == 0
            || header.flags & c2_wire::flags::FLAG_FIRST_CHUNK != 0);
    // Identity history is advanced before any route, capacity or storage
    // refusal; the active record is inserted only after admission completes.
    let (mark_advanced, duplicate) = if new_open {
        match conn.calls.lock().advance_open(id) {
            Ok(()) => (true, false),
            Err(_) => (false, true),
        }
    } else {
        (false, false)
    };
    if header.flags & c2_wire::flags::FLAG_BUDDY != 0 && !conn.shm_request_capable() {
        // No descriptor may open a mapping in a direction never negotiated.
        if mark_advanced {
            // A new OPEN (mark advanced, no record yet) is refused without a
            // call record; the connection keeps serving other calls.
            queue_open_refusal(
                writer,
                id,
                &C2Error::new(
                    ErrorCode::ProtocolViolation,
                    "SHM request direction was not negotiated; request was not executed",
                ),
            )
            .await;
        } else if duplicate
            || !queue_rejection(
                writer,
                id,
                &error_wire(
                    ErrorCode::ProtocolViolation,
                    "SHM request direction was not negotiated; request was not executed",
                ),
            )
            .await
        {
            conn.end(
                ConnectionEndReason::ProtocolError,
                "unnegotiated_shm_request",
            );
        }
        return;
    }
    let record = match rpc::decode_rpc_record_with_carrier(header, payload, None) {
        Ok(record) => record,
        Err(failure) => {
            if let Some(descriptor) = failure.carrier {
                drop(request_storage(conn, ValueStorage::Shm(descriptor)));
            }
            if failure.error == frame::DecodeError::ZeroLoanId
                || matches!(
                    failure.error,
                    frame::DecodeError::InvalidRpcEnvelope { .. }
                        | frame::DecodeError::InvalidValue {
                            field: "call mode",
                            ..
                        }
                )
                || duplicate
                || (header.flags & c2_wire::flags::FLAG_BUDDY != 0 && failure.carrier.is_none())
            {
                conn.end(ConnectionEndReason::ProtocolError, "rpc_record_admission");
            }
            if mark_advanced {
                queue_open_refusal(
                    writer,
                    id,
                    &C2Error::new(
                        ErrorCode::ResourceInputDeserializing,
                        failure.error.to_string(),
                    ),
                )
                .await;
            } else if !duplicate {
                conn.end(ConnectionEndReason::ProtocolError, "rpc_record_decode");
            }
            return;
        }
    };
    if duplicate {
        if let RpcRecord::Open { value, .. } = record {
            release_request_storage(conn, value.storage);
        }
        conn.end(ConnectionEndReason::ProtocolError, "duplicate_open");
        return;
    }
    match record {
        RpcRecord::Credit {
            storage_bytes,
            item_demand,
        } => match conn.calls.lock().continuation(id) {
            CallLookup::Retired => return,
            CallLookup::Future => {
                conn.end(ConnectionEndReason::ProtocolError, "future_stream_credit")
            }
            CallLookup::Active(_) => {
                if let Err(error) = conn.core.stream_credit(id, storage_bytes, item_demand) {
                    warn!(request_id=id, %error, "invalid stream credit");
                    conn.end(ConnectionEndReason::ProtocolError, "stream_credit");
                }
            }
        },
        RpcRecord::Cancel { cause } => {
            let lookup = conn.calls.lock().continuation(id);
            match lookup {
                CallLookup::Retired => return,
                CallLookup::Future => {
                    conn.end(ConnectionEndReason::ProtocolError, "future_stream_cancel")
                }
                CallLookup::Active(call) => {
                    if call.data.stream.lock().is_none() {
                        conn.end(ConnectionEndReason::ProtocolError, "cancel_for_unary");
                    } else {
                        submit_stream_cancel(
                            conn,
                            id,
                            &call,
                            match cause {
                                rpc::CancelCause::Consumer => StreamCancelCause::Consumer,
                                rpc::CancelCause::Lifecycle => StreamCancelCause::RelayShutdown,
                            },
                        );
                    }
                }
            }
        }
        RpcRecord::Open {
            identity,
            method_idx,
            mode,
            value,
        } => {
            handle_open_record(server, conn, writer, id, identity, method_idx, mode, value).await;
        }
        RpcRecord::OpenContinuation { value } => {
            let lookup = conn.calls.lock().continuation(id);
            let call = match lookup {
                CallLookup::Retired => {
                    release_request_storage(conn, value.storage);
                    return;
                }
                CallLookup::Future => {
                    release_request_storage(conn, value.storage);
                    conn.end(
                        ConnectionEndReason::ProtocolError,
                        "future_open_continuation",
                    );
                    return;
                }
                CallLookup::Active(call) => call,
            };
            // Refresh assembly activity before validating this part.
            server.chunk_registry.touch(conn.conn_id(), id);
            let next = {
                let mut input = call.data.input.lock();
                input
                    .as_mut()
                    .ok_or("OPEN value is already complete")
                    .and_then(|input| {
                        input
                            .receive
                            .admit(value.storage.len())
                            .map(|index| (input.request.clone(), index))
                    })
            };
            let (_request, index) = match next {
                Ok(next) => next,
                Err(error) => {
                    release_request_storage(conn, value.storage);
                    if !queue_input_error(writer, id, error.into()).await {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "unexpected_input_continuation",
                        );
                    }
                    return;
                }
            };
            // Charge the frame's inline payload bytes plus the queue node
            // before copying any inline bytes out of the receive buffer.
            let inline_len = match value.storage {
                ValueStorage::Inline(bytes) => bytes.len() as u64,
                ValueStorage::Shm(_) => 0,
            };
            let frame_charge = inline_len + PENDING_FRAME_OVERHEAD as u64;
            let reservation = match server.domain_budget.try_reserve_memory(frame_charge) {
                Ok(reservation) => reservation,
                Err(error) => {
                    release_request_storage(conn, value.storage);
                    // Frame admission failed: one refusal settles the
                    // reassembly and retires the call; later frames of
                    // this ID take the Retired path above.
                    let error = input_admission_error("frame admission", frame_charge, &error);
                    queue_rejection(writer, id, &rpc::bounded_call_end_error_bytes(&error)).await;
                    return;
                }
            };
            let data = request_storage(conn, value.storage);
            enqueue_copy_frame(
                server,
                conn,
                id,
                PendingChunkFrame {
                    chunk_idx: index,
                    first_geometry: None,
                    data,
                    _memory: reservation,
                },
            );
        }
        _ => conn.end(
            ConnectionEndReason::ProtocolError,
            "unsupported_request_record",
        ),
    }
}

/// The OPEN admission path. The request-ID high-water mark is already
/// advanced by the caller; this function performs decode-independent
/// admission — direction/deactivation checks, route+contract+mode
/// validation, payload bound, N admission, then M admission per chunk
/// position — and inserts the active call record only once admission
/// completed. Every refusal before the insert releases any admitted peer
/// SHM, the route reservation and the operation permit.
#[allow(clippy::too_many_arguments)]
async fn handle_open_record(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    identity: RouteCallIdentity,
    method_idx: u16,
    mode: CallMode,
    value: rpc::ValuePart<'_>,
) {
    let flight = crate::connection::FlightGuard::new(conn);
    if conn.is_deactivated() || conn.shutdown_requested() {
        release_request_storage(conn, value.storage);
        queue_open_refusal(
            writer,
            id,
            &C2Error::new(
                ErrorCode::ResourceClosed,
                "connection admission is closed; request was not executed",
            ),
        )
        .await;
        return;
    }
    let route = match reserve_accepted_route_execution(server, &identity, method_idx, mode) {
        Ok(route) => route,
        Err(error) => {
            release_request_storage(conn, value.storage);
            queue_open_refusal(writer, id, &route_admission_error(error)).await;
            return;
        }
    };
    let input_size = match value.chunk {
        ChunkPosition::Whole => value.storage.len(),
        ChunkPosition::First { total_bytes } => total_bytes,
        ChunkPosition::Continuation => unreachable!("OPEN first record"),
    };
    if input_size > server.config.max_payload_size {
        release_request_storage(conn, value.storage);
        drop(route);
        queue_open_refusal(
            writer,
            id,
            &C2Error::new(
                ErrorCode::ResourceUnavailable,
                format!(
                    "request payload size {input_size} exceeds max_payload_size {}",
                    server.config.max_payload_size
                ),
            ),
        )
        .await;
        return;
    }
    // N admission: the permit follows the call to its real terminal outcome
    // through the inserted call record.
    let operation = match server.domain_budget.reserve_operation() {
        Ok(operation) => operation,
        Err(error) => {
            release_request_storage(conn, value.storage);
            drop(route);
            let reason = match error.reason {
                c2_mem::RetentionRejectReason::Closed => "closed",
                c2_mem::RetentionRejectReason::LimitReached => "limit_reached",
            };
            queue_open_refusal(
                writer,
                id,
                &C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    format!("server call admission rejected the request: {error}"),
                )
                .with_details(std::collections::BTreeMap::from([
                    ("stage".to_string(), "call_execution_admission".to_string()),
                    ("reason".to_string(), reason.to_string()),
                    ("used".to_string(), error.used_operations.to_string()),
                    ("max".to_string(), error.max_operations.to_string()),
                    ("request_id".to_string(), id.to_string()),
                ])),
            )
            .await;
            return;
        }
    };
    let admission = ChunkRouteAdmission {
        route: route.route,
        method_idx,
        pending_permit: route.pending_permit,
        _flight: flight,
    };
    let stream_mode = admission.route.method_modes[method_idx as usize] == CallMode::ServerStream;
    if stream_mode {
        let admitted = {
            let _publication = server.dispatcher.read();
            if admission.route.scheduler.route_admission_state() == RouteAdmissionState::Closed
                || conn.shutdown_requested()
                || server.admission.lock().is_closed()
            {
                Err("stream route closed before publication".to_owned())
            } else {
                conn.core.stream_open(id).map_err(|error| error.to_string())
            }
        };
        if let Err(error) = admitted {
            release_request_storage(conn, value.storage);
            drop(admission);
            drop(operation);
            queue_open_refusal(
                writer,
                id,
                &C2Error::new(ErrorCode::ResourceClosed, error.to_string()),
            )
            .await;
            return;
        }
    }
    match value.chunk {
        ChunkPosition::Whole => {
            // M admission for the whole request before any inline copy.
            let reservation = match server.domain_budget.try_reserve_memory(input_size) {
                Ok(reservation) => reservation,
                Err(error) => {
                    release_request_storage(conn, value.storage);
                    drop(admission);
                    drop(operation);
                    if stream_mode {
                        conn.core.stream_retire(id);
                    }
                    let error = input_admission_error("call input admission", input_size, &error);
                    queue_open_refusal(writer, id, &error).await;
                    return;
                }
            };
            let data = match request_storage(conn, value.storage) {
                Ok(data) => data,
                Err(error) => {
                    drop(reservation);
                    drop(admission);
                    drop(operation);
                    if stream_mode {
                        conn.core.stream_retire(id);
                    }
                    queue_open_refusal(
                        writer,
                        id,
                        &C2Error::new(ErrorCode::ResourceInputDeserializing, error),
                    )
                    .await;
                    return;
                }
            };
            let call = insert_admitted_call(conn, id, operation, stream_mode, &admission);
            call.queue();
            let input = OwnedRequestInput {
                data,
                memory: Some(reservation),
            };
            let server = server.clone();
            let conn = conn.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                dispatch_complete_call(&server, &conn, &writer, id, input, admission).await;
            });
        }
        ChunkPosition::First { total_bytes } => {
            let first_inline = match value.storage {
                ValueStorage::Inline(bytes) => bytes.len() as u64,
                ValueStorage::Shm(_) => 0,
            };
            let frame_charge = first_inline + PENDING_FRAME_OVERHEAD as u64;
            let reservation = match server.domain_budget.try_reserve_memory(frame_charge) {
                Ok(reservation) => reservation,
                Err(error) => {
                    release_request_storage(conn, value.storage);
                    drop(admission);
                    drop(operation);
                    if stream_mode {
                        conn.core.stream_retire(id);
                    }
                    let error = input_admission_error("frame admission", frame_charge, &error);
                    queue_open_refusal(writer, id, &error).await;
                    return;
                }
            };
            let mut receive = ValueReceive::first(total_bytes, value.storage.len())
                .expect("checked first value length");
            let geometry = match receive.geometry() {
                Ok(geometry) => geometry,
                Err(error) => {
                    drop(reservation);
                    release_request_storage(conn, value.storage);
                    drop(admission);
                    drop(operation);
                    if stream_mode {
                        conn.core.stream_retire(id);
                    }
                    queue_open_refusal(
                        writer,
                        id,
                        &C2Error::new(ErrorCode::ResourceInputDeserializing, error),
                    )
                    .await;
                    return;
                }
            };
            let index = receive
                .admit(value.storage.len())
                .expect("first part matches itself");
            let data = request_storage(conn, value.storage);
            let request = server.chunk_registry.begin_frame(conn.conn_id(), id);
            let call = insert_admitted_call(conn, id, operation, stream_mode, &admission);
            *call.data.admission.lock() = Some(admission);
            if let Err((error, _)) = server.chunk_registry.prepare_request(&request, ()) {
                drop(reservation);
                drop(data);
                queue_input_error(writer, id, error.to_string()).await;
                return;
            }
            let mut input = call.data.input.lock();
            *input = Some(ServerInput {
                receive,
                queue_key: (conn.conn_id(), id),
                scheduler: server.copy_scheduler.clone(),
                request: request.clone(),
                registry: server.chunk_registry.clone(),
                copy: CallMutex::new(PendingCopyQueue {
                    queue: [PendingChunkFrame {
                        chunk_idx: index,
                        first_geometry: Some((total_bytes, geometry.0, geometry.1)),
                        data,
                        _memory: reservation,
                    }]
                    .into_iter()
                    .collect(),
                    published: false,
                    readied: true,
                    finished: false,
                }),
            });
            // The first frame enters the ready queue immediately: its copy
            // publishes the reassembly storage via `registry.allocate_request`.
            server.copy_scheduler.push_back(conn, id);
        }
        ChunkPosition::Continuation => unreachable!("OPEN first record"),
    }
}

/// Insert the active call record once admission completed, carrying the
/// operation permit and, for stream calls, the publication control.
fn insert_admitted_call(
    conn: &Arc<Connection>,
    id: u64,
    operation: OperationPermit,
    stream_mode: bool,
    admission: &ChunkRouteAdmission,
) -> Arc<c2_wire::connection::calls::CallRecord<ServerCall>> {
    let call = ServerCall {
        _operation: Some(operation),
        ..ServerCall::default()
    };
    if stream_mode {
        *call.stream.lock() = Some(StreamCallControl {
            route_uid: admission.route.route_uid.clone(),
            cancellation: None,
        });
    }
    conn.calls
        .lock()
        .insert_open(id, call)
        .expect("admitted OPEN follows the advanced high-water mark")
}

/// A `ResourceUnavailable` call-input admission error with stage details.
fn input_admission_error(
    admission: &str,
    requested: u64,
    error: &c2_mem::PayloadBufferError,
) -> C2Error {
    let mut details = std::collections::BTreeMap::from([
        ("stage".to_string(), "call_input_admission".to_string()),
        ("requested".to_string(), requested.to_string()),
    ]);
    match error {
        c2_mem::PayloadBufferError::TooLarge { limit, .. } => {
            details.insert("limit".into(), limit.to_string());
        }
        c2_mem::PayloadBufferError::InsufficientCapacity { available, .. } => {
            details.insert("available".into(), available.to_string());
        }
        c2_mem::PayloadBufferError::DeadlineExceeded => {}
    }
    C2Error::new(
        ErrorCode::ResourceUnavailable,
        format!("server {admission} failed: {error}"),
    )
    .with_details(details)
}

enum RouteAdmissionError {
    RouteNotFound(String),
    RouteRemoved {
        route_name: String,
    },
    ContractMismatch {
        route_name: String,
        reason: String,
    },
    UnknownMethod {
        route_name: String,
        method_idx: u16,
    },
    CallModeMismatch {
        route_name: String,
        method_idx: u16,
        call_mode: CallMode,
        method_mode: CallMode,
    },
    Acquire(SchedulerAcquireError),
}

async fn execute_route_request<F>(
    execution_scheduler: Scheduler,
    route_pending_permit: SchedulerPendingPermit,
    conn: &Connection,
    request_id: u64,
    input: OwnedRequestInput,
    f: F,
) -> Result<ResponseMeta, RouteExecutionError>
where
    F: FnOnce(Vec<u8>) -> Result<ResponseMeta, CrmError> + Send + 'static,
{
    let route_guard = tokio::select! {
        result = route_pending_permit.async_activate() => match result {
            Ok(guard) => guard,
            Err(err) => {
                drop(input);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        _ = conn.wait_work_cancelled() => {
            drop(input);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    let execution_guard = tokio::select! {
        result = execution_scheduler.async_acquire_accepted(0, route_guard.admission_generation()) => match result {
            Ok(guard) => guard,
            Err(err) => {
                drop(input);
                return Err(RouteExecutionError::Acquire(err));
            }
        },
        err = route_guard.wait_rejected() => {
            drop(input);
            return Err(RouteExecutionError::Acquire(err));
        },
        _ = conn.wait_work_cancelled() => {
            drop(input);
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
    };
    if let Err(err) = route_guard.check_admission() {
        drop(input);
        return Err(RouteExecutionError::Acquire(err));
    }
    if conn.is_work_cancelled() {
        drop(input);
        return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
    }

    let record = conn
        .calls
        .lock()
        .get(request_id)
        .ok_or(RouteExecutionError::Acquire(SchedulerAcquireError::Closed))?;
    tokio::task::spawn_blocking(move || {
        let mut route_guard = route_guard;
        let _execution_guard = execution_guard;
        if let Err(err) = route_guard.commit() {
            drop(input);
            return Err(RouteExecutionError::Acquire(err));
        }
        // `input` owns the local-copy memory reservation; it drops after the
        // callback returns or a pre-execution refusal discards the data.
        let (bytes, _memory) = input
            .into_owned_bytes()
            .map_err(RouteExecutionError::Input)?;
        if !record.enter_resource() {
            return Err(RouteExecutionError::Acquire(SchedulerAcquireError::Closed));
        }
        f(bytes).map_err(RouteExecutionError::Crm)
    })
    .await
    .expect("route execution task panicked")
}

fn reserve_accepted_route_execution(
    server: &Server,
    identity: &RouteCallIdentity,
    method_idx: u16,
    call_mode: CallMode,
) -> Result<RouteExecutionAdmission, RouteAdmissionError> {
    // Lookup and route reservation share the dispatcher publication boundary.
    // Identity errors still precede host availability.
    let admission_generation = server.admission.lock().generation;
    let dispatcher = server.dispatcher.read();
    let route = match dispatcher.resolve(&identity.route_name) {
        Some(route) => route,
        None => {
            return Err(if dispatcher.was_removed(&identity.route_name) {
                RouteAdmissionError::RouteRemoved {
                    route_name: identity.route_name.clone(),
                }
            } else {
                RouteAdmissionError::RouteNotFound(identity.route_name.clone())
            });
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
    if !route.has_method_index(method_idx) {
        return Err(RouteAdmissionError::UnknownMethod {
            route_name: route.name.clone(),
            method_idx,
        });
    }
    let method_mode = route.method_modes[method_idx as usize];
    if call_mode != method_mode {
        return Err(RouteAdmissionError::CallModeMismatch {
            route_name: route.name.clone(),
            method_idx,
            call_mode,
            method_mode,
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
            // The unique terminal owner also owns any pre-write encoding error.
            let Some(_claim) = claim_terminal(writer, request_id, CallStatus::Success) else {
                return;
            };
            if let Err(error) = send_response_meta(
                &server.response_pool,
                writer,
                request_id,
                meta,
                c2_config::SHM_THRESHOLD,
                c2_config::CHUNK_SIZE as usize,
                server.config.max_payload_size,
                conn.shm_response_capable(),
            )
            .await
            {
                match error {
                    ResponseSendError::UserVisible(message)
                    | ResponseSendError::Capacity(message) => {
                        write_error_record(
                            writer,
                            request_id,
                            CallStatus::Failed,
                            &error_wire(ErrorCode::ResourceOutputSerializing, message),
                            false,
                        )
                        .await;
                    }
                    ResponseSendError::Transport(message) => {
                        warn!(request_id, %message, "response send failed")
                    }
                }
            }
        }
        Err(error) => {
            let status = if matches!(error, RouteExecutionError::Crm(_)) {
                CallStatus::Failed
            } else {
                CallStatus::NotExecuted
            };
            let bytes = match error {
                RouteExecutionError::Input(message) => {
                    error_wire(ErrorCode::ResourceInputDeserializing, message)
                }
                RouteExecutionError::Crm(CrmError::UserError(bytes)) => {
                    // Re-encode the resource's envelope through the bounded
                    // path so an oversized user error cannot produce an
                    // unbounded inline CALL_END frame.
                    match C2Error::from_wire_bytes(&bytes) {
                        Ok(Some(error)) => rpc::bounded_call_end_error_bytes(&error),
                        _ => error_wire(
                            ErrorCode::ResourceFunctionExecuting,
                            "invalid CRM error payload",
                        ),
                    }
                }
                RouteExecutionError::Crm(CrmError::InternalError(message)) => {
                    error_wire(ErrorCode::ResourceFunctionExecuting, message)
                }
                RouteExecutionError::Acquire(SchedulerAcquireError::Closed) => {
                    error_wire(ErrorCode::ResourceClosed, "route closed")
                }
                RouteExecutionError::Acquire(SchedulerAcquireError::Unavailable) => {
                    error_wire(ErrorCode::ResourceUnavailable, "host admission unavailable")
                }
                RouteExecutionError::Acquire(SchedulerAcquireError::Capacity { field, limit }) => {
                    error_wire(
                        ErrorCode::ResourceUnavailable,
                        format!("route concurrency capacity exceeded: {field}={limit}"),
                    )
                }
            };
            if let Some(claim) = claim_terminal(writer, request_id, status) {
                write_error_record(writer, request_id, claim.status, &bytes, false).await;
            }
        }
    }
}

fn route_admission_error(err: RouteAdmissionError) -> C2Error {
    let mut details = std::collections::BTreeMap::new();
    let (code, message) = match err {
        RouteAdmissionError::RouteNotFound(name) => {
            details.insert("route".into(), name.clone());
            (
                ErrorCode::ResourceNotFound,
                format!("route not found: {name}"),
            )
        }
        RouteAdmissionError::RouteRemoved { route_name } => {
            details.insert("route".into(), route_name.clone());
            (
                ErrorCode::ResourceRemoved,
                format!("route {route_name} was removed"),
            )
        }
        RouteAdmissionError::ContractMismatch { route_name, reason } => {
            details.insert("route".into(), route_name.clone());
            details.insert("reason".into(), reason.clone());
            (
                ErrorCode::ContractMismatch,
                format!("contract mismatch for route {route_name}: {reason}"),
            )
        }
        RouteAdmissionError::UnknownMethod {
            route_name,
            method_idx,
        } => {
            details.insert("route".into(), route_name.clone());
            details.insert("method_idx".into(), method_idx.to_string());
            (
                ErrorCode::ResourceFunctionExecuting,
                format!("unknown method index {method_idx} for route {route_name}"),
            )
        }
        RouteAdmissionError::CallModeMismatch {
            route_name,
            method_idx,
            call_mode,
            method_mode,
        } => {
            let label = |mode| match mode {
                CallMode::Unary => "unary",
                CallMode::ServerStream => "server_stream",
            };
            let call_mode = label(call_mode);
            let method_mode = label(method_mode);
            details.insert("route".into(), route_name.clone());
            details.insert("method_idx".into(), method_idx.to_string());
            details.insert("call_mode".into(), call_mode.into());
            details.insert("method_mode".into(), method_mode.into());
            (
                ErrorCode::UnsupportedCallMode,
                format!(
                    "call mode mismatch for route {route_name} method index {method_idx}: the call is {call_mode}, the method is {method_mode}"
                ),
            )
        }
        RouteAdmissionError::Acquire(SchedulerAcquireError::Closed) => {
            (ErrorCode::ResourceClosed, "route closed".into())
        }
        RouteAdmissionError::Acquire(SchedulerAcquireError::Unavailable) => (
            ErrorCode::ResourceUnavailable,
            "host admission unavailable".into(),
        ),
        RouteAdmissionError::Acquire(SchedulerAcquireError::Capacity { field, limit }) => {
            details.insert("field".into(), field.into());
            details.insert("limit".into(), limit.to_string());
            (
                ErrorCode::ResourceUnavailable,
                format!("route concurrency capacity exceeded: {field}={limit}"),
            )
        }
    };
    C2Error::new(code, message).with_details(details)
}

fn admit_peer_request(conn: &Arc<Connection>, bp: BuddyPayload) -> Result<RequestData, String> {
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

async fn dispatch_complete_call(
    server: &Server,
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    input: OwnedRequestInput,
    admission: ChunkRouteAdmission,
) {
    if admission.route.method_modes[admission.method_idx as usize] == CallMode::ServerStream {
        stream::dispatch_stream_call(server, conn, writer, request_id, input, admission).await;
        return;
    }
    let callback = admission.route.callback.clone();
    let name = admission.route.name.clone();
    let method_idx = admission.method_idx;
    let response_pool = server.response_pool.clone();
    let result = execute_route_request(
        server.execution_scheduler.clone(),
        admission.pending_permit,
        conn,
        request_id,
        input,
        move |request| callback.invoke(&name, method_idx, request, response_pool),
    )
    .await;
    send_route_execution_result(server, conn, writer, request_id, result).await;
}

// ---------------------------------------------------------------------------
// CRM call dispatch — chunked reassembly
// ---------------------------------------------------------------------------

/// Cancellation owns the same call record as refusal, GC and copying. Dropping
/// an unstarted task cannot leave an admitted call slot or revive another call.
struct ChunkTaskGuard {
    _call: Arc<c2_wire::connection::calls::CallRecord<ServerCall>>,
    registry: Arc<c2_wire::chunk::ChunkRegistry<()>>,
    request: ChunkRequest<()>,
    armed: bool,
}

impl Drop for ChunkTaskGuard {
    fn drop(&mut self) {
        if self.armed {
            self.registry.settle(&self.request);
        }
    }
}

/// Work returns the original call's execution/reply ownership. A refused
/// outcome already owns the error reply before its frame capacity is released.
enum ChunkCopyOutcome {
    Incomplete,
    Complete(OwnedRequestInput),
    Refused(Vec<u8>),
    Discarded,
}

// Field order settles the call and drops unread source data (releasing both
// the peer SHM loan and the frame's memory reservation) before the C permit
// is returned, including when the worker unwinds.
struct ChunkCopyWork {
    guard: ChunkTaskGuard,
    frame: PendingChunkFrame,
    _call: Arc<c2_wire::connection::calls::CallRecord<ServerCall>>,
    _copy_permit: OwnedSemaphorePermit,
}

fn refuse_chunk_copy(
    registry: &c2_wire::chunk::ChunkRegistry<()>,
    request: &ChunkRequest<()>,
    code: ErrorCode,
    message: String,
) -> ChunkCopyOutcome {
    if registry.settle(request) {
        ChunkCopyOutcome::Refused(error_wire(code, message))
    } else {
        ChunkCopyOutcome::Discarded
    }
}

fn copy_chunked_frame(
    registry: Arc<c2_wire::chunk::ChunkRegistry<()>>,
    request: ChunkRequest<()>,
    frame: PendingChunkFrame,
    copy_permit: OwnedSemaphorePermit,
    call: Arc<c2_wire::connection::calls::CallRecord<ServerCall>>,
    budget: DomainBudget,
) -> ChunkCopyOutcome {
    let work_call = call.clone();
    let mut work = ChunkCopyWork {
        _call: call,
        guard: ChunkTaskGuard {
            _call: work_call.clone(),
            registry: registry.clone(),
            request: request.clone(),
            armed: true,
        },
        frame,
        _copy_permit: copy_permit,
    };
    if let Some((total_size, total_chunks, chunk_size)) = work.frame.first_geometry {
        if let Err(error) =
            registry.allocate_request(&request, total_size, total_chunks, chunk_size)
        {
            let code = match error {
                ChunkAdmissionError::Capacity(_) => ErrorCode::ResourceUnavailable,
                _ => ErrorCode::ProtocolViolation,
            };
            return refuse_chunk_copy(
                &registry,
                &request,
                code,
                format!("chunked call reassembly admission failed: {error}"),
            );
        }
    }
    let copied = (|| {
        let copied = match work.frame.data? {
            RequestData::Inline(data) => registry
                .prepare_chunk(&request, work.frame.chunk_idx, data.len())?
                .copy_from(&data)?,
            RequestData::Shm(peer) => registry
                .prepare_chunk(&request, work.frame.chunk_idx, peer.len())?
                .copy_with(|destination| peer.copy_into(destination))?,
            RequestData::Handle(_) => unreachable!("reassembled data cannot be a chunk frame"),
        };
        drop(work.frame._memory);
        registry.commit_chunk(&request, copied)
    })();
    let complete = match copied {
        Ok(complete) => complete,
        Err(error) => {
            return refuse_chunk_copy(&registry, &request, ErrorCode::ResourceUnavailable, error);
        }
    };
    if complete {
        match registry.take_request(&request) {
            Ok((assembler, ())) => {
                match assembler
                    .finish()
                    .map_err(|message| C2Error::new(ErrorCode::ResourceUnavailable, message))
                    .and_then(|backing| owned_reassembly_input(&budget, backing))
                {
                    Ok(input) => ChunkCopyOutcome::Complete(input),
                    Err(error) => {
                        ChunkCopyOutcome::Refused(rpc::bounded_call_end_error_bytes(&error))
                    }
                }
            }
            Err(error) => {
                refuse_chunk_copy(&registry, &request, ErrorCode::ResourceUnavailable, error)
            }
        }
    } else {
        work.guard.armed = false;
        ChunkCopyOutcome::Incomplete
    }
}

/// Charge the completed reassembly's local copy to the server domain and pair
/// it with the backing. Buddy/file backings reserve before the `Vec` copy
/// happens inside dispatch; a dedicated segment may first release its own
/// charge (copy, `release`, retry) when the reservation raced its occupancy.
fn owned_reassembly_input(
    budget: &DomainBudget,
    mut backing: c2_wire::chunk::ReassemblyBacking,
) -> Result<OwnedRequestInput, C2Error> {
    let len = backing.len() as u64;
    let limit = budget.snapshot().memory.limit_bytes;
    match budget.try_reserve_memory(len) {
        Ok(reservation) => Ok(OwnedRequestInput {
            data: RequestData::Handle(backing),
            memory: Some(reservation),
        }),
        Err(error) => {
            if backing.is_dedicated() && len <= limit {
                let bytes = backing.copy_bytes().map_err(|message| {
                    C2Error::new(ErrorCode::ResourceUnavailable, message).with_details(
                        std::collections::BTreeMap::from([(
                            "stage".to_string(),
                            "call_input_admission".to_string(),
                        )]),
                    )
                })?;
                backing.release();
                match budget.try_reserve_memory(len) {
                    Ok(reservation) => Ok(OwnedRequestInput {
                        data: RequestData::Inline(bytes),
                        memory: Some(reservation),
                    }),
                    Err(error) => Err(input_admission_error("local copy admission", len, &error)),
                }
            } else {
                Err(input_admission_error("local copy admission", len, &error))
            }
        }
    }
}

/// The coordinator of the server-wide C-gated round-robin copy scheduling.
/// It pops ready requests from [`CopyScheduler`] and spawns one C-permit
/// worker per frame; a worker that leaves frames behind pushes its request to
/// the back of the ready queue (round robin). Continuations that arrived
/// before the first copy published the reassembly storage never enter the
/// ready queue, so they cannot occupy blocking threads waiting on storage.
async fn copy_coordinator(
    server: std::sync::Weak<Server>,
    scheduler: Arc<CopyScheduler>,
    permits: Arc<Semaphore>,
) {
    loop {
        let notified = scheduler.notify.notified();
        if scheduler.is_empty() {
            notified.await;
            continue;
        }
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return;
        };
        let Some(server) = server.upgrade() else {
            return;
        };
        if let Some((conn, id)) = scheduler.pop_front() {
            tokio::spawn(async move { run_copy_frame(&server, &conn, id, permit).await });
        }
    }
}

/// Copy exactly one queued frame of one request, then re-schedule per the
/// round-robin rules and, on completion, dispatch the assembled call.
async fn run_copy_frame(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    id: u64,
    permit: OwnedSemaphorePermit,
) {
    let writer = conn
        .rpc_writer
        .lock()
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    let Some(writer) = writer else {
        return;
    };
    let Some(call) = conn.calls.lock().get(id) else {
        return;
    };
    let work = {
        let input_guard = call.data.input.lock();
        let input = input_guard.as_ref();
        let mut queue = input.map(|input| input.copy.lock());
        match (input, queue.as_deref_mut()) {
            (Some(input), Some(queue)) if !queue.finished => queue
                .queue
                .pop_front()
                .map(|frame| (input.request.clone(), frame)),
            _ => None,
        }
    };
    let Some((request, frame)) = work else {
        return;
    };
    let was_first = frame.first_geometry.is_some();
    let _flight = crate::connection::FlightGuard::new(conn);
    let registry = server.chunk_registry.clone();
    let copying_call = call.clone();
    let copying_request = request.clone();
    let budget = server.domain_budget.clone();
    let copying = tokio::task::spawn_blocking(move || {
        copy_chunked_frame(
            registry,
            copying_request,
            frame,
            permit,
            copying_call,
            budget,
        )
    });
    let copied = tokio::select! {
        result = copying => result.expect("chunk copy task panicked"),
        _ = conn.wait_work_cancelled() => return,
    };
    match copied {
        ChunkCopyOutcome::Incomplete => {
            let input = call.data.input.lock();
            if let Some(input) = input.as_ref() {
                let mut queue = input.copy.lock();
                if was_first {
                    queue.published = true;
                }
                queue.readied = !queue.queue.is_empty() && !queue.finished;
                if queue.readied {
                    server.copy_scheduler.push_back(conn, id);
                } else if queue.queue.is_empty() {
                    queue.queue = VecDeque::new();
                }
            }
        }
        ChunkCopyOutcome::Discarded => {}
        ChunkCopyOutcome::Refused(reply) => {
            finish_copy_queue(&call);
            write_reply(&writer, id, &reply).await;
        }
        ChunkCopyOutcome::Complete(input) => {
            finish_copy_queue(&call);
            if !call.queue() {
                return;
            }
            let Some(admission) = call.data.admission.lock().take() else {
                return;
            };
            dispatch_complete_call(server, conn, &writer, id, input, admission).await;
        }
    }
}

/// Mark a request's pending-copy queue terminal: leftover frames drop with
/// their reservations, and late continuations stop entering the queue.
fn finish_copy_queue(call: &Arc<c2_wire::connection::calls::CallRecord<ServerCall>>) {
    let input_guard = call.data.input.lock();
    if let Some(input) = input_guard.as_ref() {
        input.copy.lock().finished = true;
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

const REPLY_FLAGS: u32 = FLAG_RESPONSE | c2_wire::flags::FLAG_RPC_RECORD;
const SUCCESS_REPLY: [u8; 2] = [RpcRecordKind::CallEnd as u8, CallStatus::Success as u8];

async fn queue_input_error(writer: &Arc<ConnectionWriter>, request_id: u64, error: String) -> bool {
    queue_rejection(
        writer,
        request_id,
        &error_wire(ErrorCode::ResourceInputDeserializing, error),
    )
    .await
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

struct TerminalClaim {
    status: CallStatus,
    _call: Arc<c2_wire::connection::calls::CallRecord<ServerCall>>,
}

fn claim_terminal(writer: &ConnectionWriter, id: u64, status: CallStatus) -> Option<TerminalClaim> {
    let (status, retired) = {
        let mut calls = writer.conn.calls.lock();
        let call = calls.get(id)?;
        let status = match status {
            CallStatus::NotExecuted => call.reject_before_execution()?,
            CallStatus::Success => call.finish_execution(true)?,
            CallStatus::Failed => call.finish_execution(false)?,
        };
        (status, calls.retire(id)?)
    };
    let call = &retired;
    // Retire storage outside the table lock. Copy tokens retain actual owners.
    let input = call.data.input.lock().take();
    drop(input);
    writer.conn.core.stream_retire(id);
    Some(TerminalClaim {
        status,
        _call: retired,
    })
}

async fn write_error_record(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    status: CallStatus,
    error: &[u8],
    queued: bool,
) {
    let frame = match rpc::encode_call_end_frame(
        request_id,
        CallMode::Unary,
        status,
        0,
        ValuePart::whole(ValueStorage::Inline(error)),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            warn!(request_id, %error, "invalid terminal error payload");
            return;
        }
    };
    let result = if queued {
        writer.queue_call_end(frame)
    } else {
        writer.write_call_end(&frame).await
    };
    if let Err(error) = result {
        warn!(request_id, %error, "failed to queue CALL_END");
    }
}

async fn write_reply(writer: &Arc<ConnectionWriter>, request_id: u64, error: &[u8]) {
    if let Some(claim) = claim_terminal(writer, request_id, CallStatus::NotExecuted) {
        write_error_record(writer, request_id, claim.status, error, false).await;
    }
}
async fn queue_rejection(writer: &Arc<ConnectionWriter>, request_id: u64, error: &[u8]) -> bool {
    if let Some(claim) = claim_terminal(writer, request_id, CallStatus::NotExecuted) {
        write_error_record(writer, request_id, claim.status, error, true).await;
        true
    } else {
        false
    }
}

/// Write a unary CALL_END with Success and its inline result.
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
            .write_call_end(&buf[..off])
            .await
            .map_err(|error| response_write_error("inline reply write failed", error))?;
    } else {
        // Large response: heap Vec (existing path)
        let frame = frame::encode_frame_parts(request_id, REPLY_FLAGS, &[&ctrl_bytes, data]);
        writer
            .write_call_end(&frame)
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

/// Continue one CALL_END value without another status or terminal outcome.
async fn write_chunked_reply(
    writer: &Arc<ConnectionWriter>,
    request_id: u64,
    data: &[u8],
    chunk_size: usize,
) -> Result<(), ResponseSendError> {
    for (index, part) in data.chunks(chunk_size).enumerate() {
        let bytes = if index == 0 {
            rpc::encode_call_end_frame(
                request_id,
                CallMode::Unary,
                CallStatus::Success,
                0,
                ValuePart::first(data.len() as u64, ValueStorage::Inline(part)),
            )
        } else {
            rpc::encode_value_continuation_frame(
                request_id,
                RpcRecordKind::CallEnd,
                ValueStorage::Inline(part),
            )
        }
        .map_err(|error| {
            incomplete_reply_error(
                writer,
                index > 0,
                ResponseSendError::UserVisible(error.to_string()),
            )
        })?;
        writer.write_call_end(&bytes).await.map_err(|error| {
            incomplete_reply_error(
                writer,
                index > 0,
                response_write_error("CALL_END chunk write failed", error),
            )
        })?;
    }
    Ok(())
}

fn incomplete_reply_error(
    writer: &ConnectionWriter,
    started: bool,
    error: ResponseSendError,
) -> ResponseSendError {
    if !started {
        return error;
    }
    let message = match error {
        ResponseSendError::Transport(message) => return ResponseSendError::Transport(message),
        ResponseSendError::Capacity(message) | ResponseSendError::UserVisible(message) => message,
    };
    // The peer has already seen this CALL_END's status. Keep reading trustworthy
    // frames and releases until accepted work drains, then EOF settles this value.
    writer
        .conn
        .end(ConnectionEndReason::ProtocolError, "incomplete_call_end");
    ResponseSendError::Transport(message)
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

#[cfg(test)]
#[path = "stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "disconnect_tests.rs"]
mod disconnect_tests;

#[path = "stream.rs"]
mod stream;
