//! Async IPC client — connects to a C-Two IPC server through a local OS stream.
//!
//! Performs handshake, then multiplexes concurrent requests over
//! a single local connection using request IDs.

use parking_lot::{Mutex as StdMutex, RwLock};
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use c2_local::{
    ConnectionEndReason, ConnectionObservation, ConnectionPublisher, DEFAULT_CONNECT_TIMEOUT,
    LocalEndpoint, LocalReadHalf, LocalStream, LocalWriteHalf,
};
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, watch};

use c2_config::ConnectDeadline;

// The scope follows each future poll, rather than an OS thread. It projects
// Core's single deadline into native metadata operations reached by connect;
// it never creates a new budget or affects an ordinary business call.
tokio::task_local! { static CONNECT_OPERATION_DEADLINE: ConnectDeadline; }

fn operation_deadline() -> ConnectDeadline {
    CONNECT_OPERATION_DEADLINE
        .try_with(|deadline| *deadline)
        .unwrap_or_default()
}

fn connect_directory_read<T>(
    directory: &RwLock<T>,
) -> Result<parking_lot::RwLockReadGuard<'_, T>, IpcError> {
    let deadline = operation_deadline();
    connect_check(deadline, "route_directory_wait")?;
    let guard = match deadline.instant() {
        Some(instant) => directory
            .try_read_until(instant)
            .ok_or_else(|| connect_expired("route_directory_wait"))?,
        None => directory.read(),
    };
    connect_check(deadline, "route_directory_wait")?;
    Ok(guard)
}

fn connect_directory_write<T>(
    directory: &RwLock<T>,
) -> Result<parking_lot::RwLockWriteGuard<'_, T>, IpcError> {
    let deadline = operation_deadline();
    connect_check(deadline, "route_directory_wait")?;
    let guard = match deadline.instant() {
        Some(instant) => directory
            .try_write_until(instant)
            .ok_or_else(|| connect_expired("route_directory_wait"))?,
        None => directory.write(),
    };
    connect_check(deadline, "route_directory_wait")?;
    Ok(guard)
}
use c2_error::{C2Error, ErrorCode};

pub(crate) fn connect_expired(stage: &'static str) -> IpcError {
    IpcError::LocalCallRejected(
        C2Error::new(ErrorCode::CallDeadlineExceeded, "connect deadline exceeded").with_details(
            std::collections::BTreeMap::from([
                ("operation".into(), "connect".into()),
                ("transport_phase".into(), "pre_dispatch".into()),
                ("stage".into(), stage.into()),
                ("fallback_eligible".into(), "false".into()),
                ("route_withdrawal".into(), "false".into()),
            ]),
        ),
    )
}

pub(crate) fn connect_check(
    deadline: ConnectDeadline,
    stage: &'static str,
) -> Result<(), IpcError> {
    deadline.check(stage).map_err(|_| connect_expired(stage))
}

pub(crate) async fn with_connect_deadline<T>(
    deadline: ConnectDeadline,
    stage: &'static str,
    future: impl std::future::Future<Output = Result<T, IpcError>>,
) -> Result<T, IpcError> {
    connect_check(deadline, stage)?;
    let scoped = CONNECT_OPERATION_DEADLINE.scope(deadline, future);
    let result = match deadline.instant() {
        None => scoped.await,
        Some(instant) => tokio::time::timeout_at(instant.into(), scoped)
            .await
            .map_err(|_| connect_expired(stage))?,
    };
    // Preserve the precise native lock/phase that exhausted this same budget.
    if matches!(&result, Err(IpcError::LocalCallRejected(error))
        if error.code == ErrorCode::CallDeadlineExceeded
            && error.details.get("operation").is_some_and(|operation| operation == "connect"))
    {
        return result;
    }
    connect_check(deadline, stage)?;
    result
}

use crate::connection::{
    ClientConnection, EndState, PhysicalRetirement, ReadDriver, SendState, core_error,
};
use c2_wire::buddy::BuddyPayload;
#[cfg(test)]
use c2_wire::buddy::encode_buddy_payload;
use c2_wire::chunk::{ChunkAdmissionError, ChunkConfig, ChunkRegistry};
use c2_wire::connection::calls::ValueReceive;
use c2_wire::connection::{PreparedLoan, WriteRequest};
use c2_wire::flags;
use c2_wire::frame::{self, DecodeError, FrameHeader, HEADER_SIZE};
use c2_wire::handshake::{
    CAP_SHM_REQUEST, CAP_SHM_RESPONSE, HANDSHAKE_VERSION, Handshake, MethodEntry, RouteInfo,
    VersionPreambleKind, decode_handshake, decode_version_preamble_payload,
    encode_client_handshake, encode_version_preamble, validate_version_preamble_header,
};
use c2_wire::msg_type::MsgType;
use c2_wire::registration_control::{
    PENDING_ROUTE_REJECT_NOT_FOUND, PendingRouteAttestation, PendingRouteAttestationResponse,
    decode_pending_route_attestation_response, encode_pending_route_attestation_request,
};
use c2_wire::route_catalog_control::{
    RouteContractWire, RouteListRequest, RouteListResponse, RouteLookupRequest,
    RouteLookupResponse, RouteRecordWire, RouteSelector, RouteStateReasonWire, RouteStateWire,
    decode_route_list_response, decode_route_lookup_response, decode_route_nack,
    encode_route_list_request, encode_route_lookup_request,
};
use c2_wire::rpc::RouteCallIdentity;
use c2_wire::rpc::{
    self, CallMode, CallStatus, ChunkPosition, RpcRecord, RpcRecordKind, ValuePart, ValueStorage,
};

use c2_mem::config::PoolConfig;
use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget};

#[path = "forward.rs"]
pub mod forward;
#[path = "stream.rs"]
pub mod stream;
use c2_mem::{MemHandle, MemPool, PeerMappingCache, PeerMappingError, PeerRead, PoolAllocation};

use crate::response::{ResponseData, ServerShmResponse};

pub use c2_config::ClientIpcConfig;

const ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS: &[u64] = &[10, 25, 50, 100, 200];

// Peer mappings are a read-only cache for this physical connection.
pub struct ServerPoolState {
    pub(crate) cache: Option<PeerMappingCache>,
    #[cfg(test)]
    open_failure: Option<String>,
}
#[cfg(test)]
impl ServerPoolState {
    pub(crate) fn from_pool_for_test(pool: MemPool) -> Self {
        Self {
            cache: Some(
                PeerMappingCache::new(pool.config().clone(), pool.prefix().into()).unwrap(),
            ),
            open_failure: None,
        }
    }
}
impl ServerPoolState {
    pub(crate) fn admit(&mut self, payload: &BuddyPayload) -> Result<PeerRead, PeerMappingError> {
        #[cfg(test)]
        if let Some(error) = self.open_failure.take() {
            return Err(PeerMappingError::Open(error));
        }
        self.cache
            .as_ref()
            .expect("live connection peer cache")
            .admit(
                payload.seg_idx,
                payload.generation,
                payload.offset,
                payload.data_size,
                payload.is_dedicated,
            )
    }
}

/// Sole preparer of an unpublished request. Publication moves the real handle
/// into ConnectionCore; pending responses never own allocation cleanup.
pub(crate) struct RequestBlock {
    pool: Arc<RwLock<MemPool>>,
    alloc: PoolAllocation,
    handle: StdMutex<Option<MemHandle>>,
    len: usize,
}
impl RequestBlock {
    fn new(pool: Arc<RwLock<MemPool>>, handle: MemHandle) -> Self {
        let alloc = handle.shm_allocation().expect("request allocation is SHM");
        let len = handle.len();
        Self {
            pool,
            alloc,
            handle: StdMutex::new(Some(handle)),
            len,
        }
    }
    pub fn allocation(&self) -> &PoolAllocation {
        &self.alloc
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), IpcError> {
        let mut slot = self.handle.lock();
        let handle = slot.as_mut().ok_or(IpcError::Closed)?;
        let pool = self.pool.read();
        let end = offset
            .checked_add(data.len())
            .filter(|end| *end <= self.len)
            .ok_or_else(|| IpcError::Shm("request write exceeds prepared length".into()))?;
        pool.handle_slice_mut(handle)[offset..end].copy_from_slice(data);
        Ok(())
    }
    fn prepare(&self, conn: &ClientConnection) -> Result<PreparedLoan, IpcError> {
        let handle = self.handle.lock().take().ok_or(IpcError::Closed)?;
        conn.core
            .prepare_loan(self.pool.clone(), handle)
            .map_err(|error| {
                conn.end(ConnectionEndReason::ProtocolError, "request_loan_prepare");
                core_error(error)
            })
    }
}
impl Drop for RequestBlock {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.get_mut().take() {
            self.pool.write().release_handle(handle);
        }
    }
}

// ── Error type ───────────────────────────────────────────────────────────

/// Default deadline for one client close barrier (writer lock, receive join,
/// final writer clear).
const DEFAULT_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A chunk failure's stage, independent of allocator diagnostic text.
#[derive(Debug)]
pub enum ChunkError {
    Protocol(String),
    /// The peer already replied, but local reassembly admission failed.
    Capacity(String),
}

impl Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) | Self::Capacity(message) => f.write_str(message),
        }
    }
}

impl From<ChunkAdmissionError> for ChunkError {
    fn from(error: ChunkAdmissionError) -> Self {
        let message = format!("chunked reply reassembly admission failed: {error}");
        match error {
            ChunkAdmissionError::Capacity(_) => Self::Capacity(message),
            ChunkAdmissionError::Protocol(_) | ChunkAdmissionError::Duplicate { .. } => {
                Self::Protocol(message)
            }
        }
    }
}

/// Call-site authority for preparation and the first business frame byte.
///
/// Callbacks must be synchronous, nonblocking, and must not panic. The owner
/// (for example Core's call scope) supplies policy; IPC owns no deadline or
/// cancellation watcher. If a call reaches the first-byte boundary,
/// `try_begin_dispatch` runs once in the unique writer, before publication.
/// After permission is granted, IPC drives the entire send and response wait
/// without consulting these callbacks again. Dropping the caller removes its
/// waiter; an independently owned send completes every started request frame.
#[derive(Clone)]
pub struct IpcCallControl {
    before_prepare: Option<Arc<dyn Fn() -> Result<(), C2Error> + Send + Sync>>,
    try_begin_dispatch: Arc<dyn Fn() -> Result<(), C2Error> + Send + Sync>,
}

impl IpcCallControl {
    pub fn new(
        try_begin_dispatch: impl Fn() -> Result<(), C2Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            before_prepare: None,
            try_begin_dispatch: Arc::new(try_begin_dispatch),
        }
    }

    /// Optional active-scope check before and after preparing an unpublished
    /// request. It never grants dispatch permission and may run more than once
    /// if SHM preparation falls back to another transport.
    pub fn with_before_prepare(
        mut self,
        before_prepare: impl Fn() -> Result<(), C2Error> + Send + Sync + 'static,
    ) -> Self {
        self.before_prepare = Some(Arc::new(before_prepare));
        self
    }

    fn check_preparation(control: Option<&Self>) -> Result<(), IpcError> {
        if let Some(check) = control.and_then(|control| control.before_prepare.as_ref()) {
            check().map_err(IpcError::LocalCallRejected)?;
        }
        Ok(())
    }

    pub(crate) fn begin_dispatch(control: Option<&Self>) -> Result<(), IpcError> {
        if let Some(control) = control {
            (control.try_begin_dispatch)().map_err(IpcError::LocalCallRejected)?;
        }
        Ok(())
    }
}

/// IPC client error.
#[derive(Debug)]
pub enum IpcError {
    /// Call-site policy rejected this unpublished call. Retains the canonical
    /// code, message, and details; it is not a connection or route failure.
    LocalCallRejected(C2Error),
    /// The peer proved that resource callback execution never began.
    CallRejected(C2Error),
    /// I/O error on the local connection.
    Io(std::io::Error),
    /// The native local connection attempt failed before any handshake I/O.
    Connect(std::io::Error),
    /// Invalid client configuration or IPC address.
    Config(String),
    /// Wire protocol decoding error.
    Decode(DecodeError),
    /// Peer violated IPC route/control protocol after the transport connected.
    Protocol(String),
    /// The peer explicitly rejected this client's ordinary IPC protocol version.
    ProtocolVersionMismatch { local: u32, peer: u32 },
    /// Connected server identity does not match the expected owner.
    IdentityMismatch {
        expected_server_id: String,
        expected_server_instance_id: String,
        actual_server_id: String,
        actual_server_instance_id: String,
    },
    /// Connected route contract does not match the expected CRM contract.
    ContractMismatch(String),
    /// Requested route no longer exists on the connected IPC server.
    RouteNotFound(String),
    /// Requested route was explicitly removed from the connected IPC server.
    RouteRemoved { route_name: String },
    /// Requested route exists but no longer accepts new calls.
    RouteClosed { route_name: String, reason: String },
    /// Requested method does not exist on the connected route.
    MethodNotFound {
        route_name: String,
        method_name: String,
    },
    /// Shared-memory request/response setup failed.
    Shm(String),
    /// The request may have executed; response decoding/admission/materialization failed.
    ResponseFailure(Box<IpcError>),
    /// Chunked response assembly failed.
    Chunk(ChunkError),
    /// CRM method returned an error (serialized error bytes).
    CrmError(Vec<u8>),
    /// Client is closed or connection lost.
    Closed,
    /// Pool management error.
    Pool(String),
    /// The owning client pool permanently closed connection admission.
    PoolRetired,
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalCallRejected(error) => write!(f, "IPC local call rejected: {error}"),
            Self::CallRejected(error) => write!(f, "IPC call not executed: {error}"),
            Self::Io(e) | Self::Connect(e) => write!(f, "IPC I/O error: {e}"),
            Self::Config(msg) => write!(f, "IPC config error: {msg}"),
            Self::Decode(e) => write!(f, "IPC decode error: {e}"),
            Self::Protocol(msg) => write!(f, "IPC protocol violation: {msg}"),
            Self::ProtocolVersionMismatch { local, peer } => {
                write!(
                    f,
                    "IPC protocol version mismatch: local={local}, peer={peer}"
                )
            }
            Self::IdentityMismatch {
                expected_server_id,
                expected_server_instance_id,
                actual_server_id,
                actual_server_instance_id,
            } => write!(
                f,
                "IPC server identity mismatch: expected {expected_server_id}/{expected_server_instance_id}, got {actual_server_id}/{actual_server_instance_id}"
            ),
            Self::ContractMismatch(msg) => write!(f, "IPC contract mismatch: {msg}"),
            Self::RouteNotFound(route) => write!(f, "IPC route not found: {route}"),
            Self::RouteRemoved { route_name } => write!(f, "IPC route removed: {route_name}"),
            Self::RouteClosed { route_name, reason } => {
                write!(f, "IPC route closed: {route_name} reason={reason}")
            }
            Self::MethodNotFound {
                route_name,
                method_name,
            } => write!(
                f,
                "IPC method not found: route={route_name} method={method_name}"
            ),
            Self::Shm(msg) => write!(f, "IPC SHM error: {msg}"),
            Self::ResponseFailure(error) => write!(f, "IPC response failure: {error}"),
            Self::Chunk(msg) => write!(f, "IPC chunk error: {msg}"),
            Self::CrmError(_) => write!(f, "CRM method error"),
            Self::Closed => write!(f, "IPC client closed"),
            Self::Pool(msg) => write!(f, "Pool error: {msg}"),
            Self::PoolRetired => write!(f, "IPC client pool retired"),
        }
    }
}

impl IpcError {
    pub(crate) fn response_failure(self) -> Self {
        match self {
            Self::ResponseFailure(_) => self,
            error => Self::ResponseFailure(Box::new(error)),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::Connect(error) => Some(error),
            Self::ResponseFailure(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<DecodeError> for IpcError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

impl From<c2_wire::rpc::EncodeError> for IpcError {
    fn from(e: c2_wire::rpc::EncodeError) -> Self {
        Self::Protocol(e.to_string())
    }
}

// ── Method table ─────────────────────────────────────────────────────────

/// Per-route method table (name ↔ index).
#[derive(Debug, Clone)]
pub struct MethodTable {
    route_name: String,
    crm_ns: String,
    crm_name: String,
    crm_ver: String,
    abi_hash: String,
    signature_hash: String,
    max_payload_size: u64,
    name_to_idx: HashMap<String, u16>,
}

impl MethodTable {
    fn from_route(route: &RouteInfo) -> Self {
        Self::from_entries(
            &route.methods,
            RouteCallIdentity {
                route_name: route.name.clone(),
                crm_ns: route.crm_ns.clone(),
                crm_name: route.crm_name.clone(),
                crm_ver: route.crm_ver.clone(),
                abi_hash: route.abi_hash.clone(),
                signature_hash: route.signature_hash.clone(),
            },
            route.max_payload_size,
        )
    }

    pub(crate) fn from_entries(
        entries: &[MethodEntry],
        identity: RouteCallIdentity,
        max_payload_size: u64,
    ) -> Self {
        let mut name_to_idx = HashMap::with_capacity(entries.len());
        for e in entries {
            name_to_idx.insert(e.name.clone(), e.index);
        }
        Self {
            route_name: identity.route_name,
            crm_ns: identity.crm_ns,
            crm_name: identity.crm_name,
            crm_ver: identity.crm_ver,
            abi_hash: identity.abi_hash,
            signature_hash: identity.signature_hash,
            max_payload_size,
            name_to_idx,
        }
    }

    /// Look up method index by name.
    pub fn index_of(&self, name: &str) -> Option<u16> {
        self.name_to_idx.get(name).copied()
    }

    /// Get all method names.
    pub fn method_names(&self) -> Vec<&str> {
        self.name_to_idx.keys().map(|s| s.as_str()).collect()
    }

    /// CRM namespace advertised for this route in the IPC handshake.
    pub fn crm_ns(&self) -> &str {
        &self.crm_ns
    }

    /// CRM contract class/model name advertised for this route in the IPC handshake.
    pub fn crm_name(&self) -> &str {
        &self.crm_name
    }

    /// CRM version advertised for this route in the IPC handshake.
    pub fn crm_ver(&self) -> &str {
        &self.crm_ver
    }

    pub fn abi_hash(&self) -> &str {
        &self.abi_hash
    }

    pub fn signature_hash(&self) -> &str {
        &self.signature_hash
    }

    pub fn max_payload_size(&self) -> u64 {
        self.max_payload_size
    }

    pub(crate) fn call_identity(&self) -> RouteCallIdentity {
        RouteCallIdentity {
            route_name: self.route_name.clone(),
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
        }
    }
}

/// Route name and complete contract acquired by one route-bound client.
#[derive(Debug, Clone)]
pub struct RouteBinding {
    table: MethodTable,
}

impl RouteBinding {
    fn from_table(table: MethodTable) -> Self {
        Self { table }
    }

    pub fn route_name(&self) -> &str {
        &self.table.route_name
    }

    pub fn max_payload_size(&self) -> u64 {
        self.table.max_payload_size()
    }

    pub(crate) fn call_target_for(
        &self,
        method_name: &str,
    ) -> Result<(u16, RouteCallIdentity, u64), IpcError> {
        let method_idx =
            self.table
                .index_of(method_name)
                .ok_or_else(|| IpcError::MethodNotFound {
                    route_name: self.table.route_name.clone(),
                    method_name: method_name.to_string(),
                })?;
        Ok((
            method_idx,
            self.table.call_identity(),
            self.table.max_payload_size(),
        ))
    }
}

// ── Route directory ──────────────────────────────────────────────────────

/// Client-side projection of the connected server route catalog.
#[derive(Debug, Default)]
pub(crate) struct RouteDirectory {
    routes: HashMap<String, MethodTable>,
}

impl RouteDirectory {
    fn new() -> Self {
        Self::default()
    }

    fn seed_from_handshake(&mut self, routes: &[RouteInfo]) {
        self.routes.clear();
        for route in routes {
            self.routes
                .insert(route.name.clone(), MethodTable::from_route(route));
        }
    }

    fn rebuild_from_list(&mut self, response: RouteListResponse) {
        self.routes.clear();
        for record in response.routes {
            self.apply_record(record);
        }
    }

    fn apply_record(&mut self, record: RouteRecordWire) {
        match record.state {
            RouteStateWire::Ready => {
                self.routes
                    .insert(record.route_name.clone(), MethodTable::from_record(&record));
            }
            RouteStateWire::Closed => {
                self.routes.remove(&record.route_name);
            }
        }
    }

    fn remove_route(&mut self, route_name: &str) {
        self.routes.remove(route_name);
    }

    fn route_table(&self, name: &str) -> Option<MethodTable> {
        self.routes.get(name).cloned()
    }

    fn has_route(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    fn route_names(&self) -> Vec<String> {
        self.routes.keys().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn insert_table(&mut self, name: String, table: MethodTable) {
        self.routes.insert(name, table);
    }
}

impl MethodTable {
    fn from_record(record: &RouteRecordWire) -> Self {
        let methods = record
            .methods
            .iter()
            .map(|method| MethodEntry {
                name: method.name.clone(),
                index: method.index,
            })
            .collect::<Vec<_>>();
        Self::from_entries(
            &methods,
            RouteCallIdentity {
                route_name: record.route_name.clone(),
                crm_ns: record.contract.crm_ns.clone(),
                crm_name: record.contract.crm_name.clone(),
                crm_ver: record.contract.crm_ver.clone(),
                abi_hash: record.contract.abi_hash.clone(),
                signature_hash: record.contract.signature_hash.clone(),
            },
            record.max_payload_size,
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) enum PendingKind {
    Call(CallMode),
    Control,
}

// Active calls retain their original physical connection until terminal delivery.
pub(crate) struct PendingResponse {
    pub(crate) tx: Option<oneshot::Sender<Result<ResponseData, IpcError>>>,
    pub(crate) state: Arc<StdMutex<SendState>>,
    reply_chunks: Option<ValueReceive>,
    pub(crate) kind: PendingKind,
    upload_stop: Option<watch::Sender<bool>>,
    pub(crate) stream: Option<Arc<stream::StreamReceive>>,
    pub(crate) forward: Option<Arc<forward::ForwardReceive>>,
}
impl PendingResponse {
    pub(crate) fn complete(self, result: Result<ResponseData, IpcError>) {
        if let Some(forward) = self.forward {
            forward.transport_failed(result.err().unwrap_or_else(|| {
                IpcError::Protocol("unary response for forwarded stream".into())
            }));
            if let Some(tx) = self.tx {
                let _ = tx.send(Err(IpcError::Closed));
            }
        } else if let Some(stream) = self.stream {
            stream.transport_failed(
                result.err().unwrap_or_else(|| {
                    IpcError::Protocol("unary response for server stream".into())
                }),
            );
            // The upload waiter needs only a wakeup here; the stream retains
            // the concrete transport failure for completion().
            if let Some(tx) = self.tx {
                let _ = tx.send(Err(IpcError::Closed));
            }
        } else if let Some(tx) = self.tx {
            let _ = tx.send(result);
        }
        if let Some(stop) = self.upload_stop {
            stop.send_replace(true);
        }
    }

    pub(crate) fn finish_stream_upload(self, response: Result<ResponseData, IpcError>) {
        if let Some(tx) = self.tx {
            let _ = tx.send(response);
        }
        if let Some(stop) = self.upload_stop {
            stop.send_replace(true);
        }
    }
}
/// One physical connection's active calls and unpublished submissions.
/// Wire numbers are assigned only by its writer, never by a waiting caller.
#[derive(Default)]
pub(crate) struct PendingMap {
    pub(crate) active: HashMap<u64, PendingResponse>,
    pub(crate) queued: Vec<PendingResponse>,
    pub(crate) high_submitted: u64,
}
impl PendingMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty() && self.queued.is_empty()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn len(&self) -> usize {
        self.active.len() + self.queued.len()
    }
    pub(crate) fn remove(&mut self, rid: &u64) -> Option<PendingResponse> {
        self.active.remove(rid)
    }
    pub(crate) fn get(&self, rid: &u64) -> Option<&PendingResponse> {
        self.active.get(rid)
    }
    pub(crate) fn get_mut(&mut self, rid: &u64) -> Option<&mut PendingResponse> {
        self.active.get_mut(rid)
    }

    pub(crate) fn publish(
        &mut self,
        state: &Arc<StdMutex<SendState>>,
        rid: u64,
    ) -> Result<(), IpcError> {
        let index = self
            .queued
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.state, state))
            .ok_or(IpcError::Closed)?;
        let entry = self.queued.swap_remove(index);
        self.high_submitted = rid;
        self.active.insert(rid, entry);
        Ok(())
    }
    fn unpublished_mut(
        &mut self,
        state: &Arc<StdMutex<SendState>>,
    ) -> Option<&mut PendingResponse> {
        self.queued
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.state, state))
    }
}

struct SendGuard {
    pending: Arc<StdMutex<PendingMap>>,
    conn: Arc<ClientConnection>,
    state: Arc<StdMutex<SendState>>,
    producer: Option<tokio::task::AbortHandle>,
}
impl SendGuard {
    fn started(&self) -> bool {
        self.state.lock().started()
    }
}
impl Drop for SendGuard {
    fn drop(&mut self) {
        let (unpublished, rid) = {
            let mut state = self.state.lock();
            (state.cancel(), state.request_id())
        };
        let detached = {
            let mut calls = self.pending.lock();
            if unpublished {
                if let Some(rid) = rid {
                    calls.active.remove(&rid)
                } else {
                    calls
                        .queued
                        .iter()
                        .position(|entry| Arc::ptr_eq(&entry.state, &self.state))
                        .map(|index| calls.queued.swap_remove(index))
                }
            } else {
                // A deadline detaches this waiter; the sent call keeps draining.
                if let Some(entry) = rid.and_then(|rid| calls.active.get_mut(&rid)) {
                    entry.tx.take();
                }
                None
            }
        };
        if let Some(entry) = detached {
            entry.complete(Err(IpcError::Closed));
        }
        self.conn.wake.notify_one();
        if unpublished {
            if let Some(producer) = &self.producer {
                producer.abort();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestTransportKind {
    Inline,
    Buddy,
    Chunked,
}

/// Select the transport for one request body.
///
/// `shm_available` means the SHM request direction is genuinely usable for
/// this connection: a live request pool plus a negotiated `CAP_SHM_REQUEST`
/// ACK. It is never derived from `pool_enabled` alone (that flag does not
/// disable dedicated SHM requests).
pub(crate) fn choose_request_transport(
    config: &ClientIpcConfig,
    shm_available: bool,
    data_len: usize,
) -> RequestTransportKind {
    let data_len = u64::try_from(data_len).unwrap_or(u64::MAX);
    if shm_available && data_len > config.shm_threshold && data_len <= u64::from(u32::MAX) {
        RequestTransportKind::Buddy
    } else if data_len > config.chunk_size {
        RequestTransportKind::Chunked
    } else {
        RequestTransportKind::Inline
    }
}

fn checked_payload_len_usize(data_len: u64) -> Result<usize, IpcError> {
    usize::try_from(data_len).map_err(|_| {
        IpcError::Config(format!(
            "payload size {data_len} exceeds this platform's addressable memory"
        ))
    })
}

pub(crate) fn request_chunk_count(data_len: usize, chunk_size: usize) -> Result<usize, IpcError> {
    if chunk_size == 0 {
        return Err(IpcError::Config("chunk_size must be > 0".to_string()));
    }
    if data_len == 0 {
        return Ok(0);
    }
    let total_chunks = data_len.div_ceil(chunk_size);
    Ok(total_chunks)
}

fn stream_error<E: Display>(err: E) -> IpcError {
    IpcError::Io(std::io::Error::other(format!(
        "request body stream error: {err}"
    )))
}

fn pre_dispatch_call_error(source: IpcError) -> crate::sync_client::IpcCallError {
    crate::sync_client::IpcCallError::new(crate::sync_client::TransportPhase::PreDispatch, source)
}

fn dispatch_uncertain_call_error(source: IpcError) -> crate::sync_client::IpcCallError {
    crate::sync_client::IpcCallError::new(
        crate::sync_client::TransportPhase::DispatchUncertain,
        source,
    )
}

fn classified_call_error(source: IpcError) -> crate::sync_client::IpcCallError {
    crate::sync_client::IpcCallError::new(crate::sync_client::call_error_phase(&source), source)
}

fn dispatch_call_error(
    source: IpcError,
    sent_or_attempted: bool,
) -> crate::sync_client::IpcCallError {
    if matches!(source, IpcError::CallRejected(_)) {
        pre_dispatch_call_error(source)
    } else if sent_or_attempted || matches!(source, IpcError::ResponseFailure(_)) {
        dispatch_uncertain_call_error(source)
    } else {
        pre_dispatch_call_error(source)
    }
}

async fn collect_exact_stream<S, B, E>(data_size: usize, chunks: S) -> Result<Vec<u8>, IpcError>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: Display + Send + 'static,
{
    let mut data = Vec::with_capacity(data_size);
    futures_util::pin_mut!(chunks);
    while let Some(next) = chunks.next().await {
        let chunk = next.map_err(stream_error)?;
        let bytes = chunk.as_ref();
        if bytes.is_empty() {
            continue;
        }
        let next_len = data.len().checked_add(bytes.len()).ok_or_else(|| {
            IpcError::Config("request body size overflow while collecting stream".into())
        })?;
        if next_len > data_size {
            return Err(IpcError::Config(format!(
                "request body exceeded declared content length {data_size}"
            )));
        }
        data.extend_from_slice(bytes);
    }
    if data.len() != data_size {
        return Err(IpcError::Config(format!(
            "request body ended at {} bytes, expected {data_size}",
            data.len()
        )));
    }
    Ok(data)
}

/// Multiplexed client. Background I/O owns the physical connection after
/// deactivation; business references and copied responses do not own its loans.
pub struct IpcClient {
    endpoint: Result<LocalEndpoint, String>,
    driver: RwLock<Option<Arc<ClientConnection>>>,
    pending: Arc<StdMutex<PendingMap>>,
    pub(crate) route_directory: Arc<RwLock<RouteDirectory>>,
    pub(crate) server_identity: Option<c2_wire::handshake::ServerIdentity>,
    pub(crate) server_pool: StdMutex<Option<Arc<RwLock<ServerPoolState>>>>,
    connection: RwLock<Option<ConnectionPublisher>>,
    connection_event_source: Option<c2_local::ConnectionEventSource>,
    pub(crate) pool: StdMutex<Option<Arc<RwLock<MemPool>>>>,
    pub(crate) config: ClientIpcConfig,
    memory_budget: c2_mem::MemoryBudget,
    pub(crate) chunk_registry: StdMutex<Option<Arc<ChunkRegistry<()>>>>,
    conn_id: u64,
    negotiated_shm_request: AtomicBool,
}
static REASSEMBLY_POOL_GEN: AtomicU64 = AtomicU64::new(0);
static CLIENT_CONN_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLIENT_OWN_POOL_COUNTER: AtomicU64 = AtomicU64::new(0);

impl IpcClient {
    /// Config-owned request pool. Always a real `MemPool`: with buddy disabled
    /// it still serves dedicated SHM requests (and announces the wire prefix),
    /// only the buddy tiers are policy-disabled. The client's idle window
    /// (`pool_decay_seconds`) rides in through the role tuning. The pool
    /// charges `budget`, so a standalone client shares one memory context
    /// between its request and reassembly pools.
    fn own_pool_from_config(
        config: &ClientIpcConfig,
        budget: &c2_mem::MemoryBudget,
    ) -> Result<Arc<RwLock<MemPool>>, IpcError> {
        let pool_config = config.base.primary_pool_config(&config.pool_tuning());
        let counter = CLIENT_OWN_POOL_COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3d{:08x}{:08x}", std::process::id(), counter);
        Ok(Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(pool_config, prefix, budget.clone())
                .map_err(IpcError::Config)?,
        )))
    }

    fn from_parts(
        endpoint: Result<LocalEndpoint, String>,
        pool: Option<Arc<RwLock<MemPool>>>,
        config: ClientIpcConfig,
        memory_budget: c2_mem::MemoryBudget,
        chunk_registry: Option<Arc<ChunkRegistry<()>>>,
    ) -> Self {
        Self {
            endpoint,
            driver: RwLock::new(None),
            pending: Arc::new(StdMutex::new(PendingMap::default())),
            route_directory: Arc::new(RwLock::new(RouteDirectory::new())),
            server_identity: None,
            server_pool: StdMutex::new(None),
            connection: RwLock::new(None),
            connection_event_source: None,
            pool: StdMutex::new(pool),
            chunk_registry: StdMutex::new(chunk_registry),
            conn_id: CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed),
            negotiated_shm_request: AtomicBool::new(false),
            config,
            memory_budget,
        }
    }

    /// The transport-owned request pool, if this client currently holds it.
    ///
    /// Returns a clone so callers never hold the detach lock across I/O.
    pub(crate) fn request_pool(&self) -> Option<Arc<RwLock<MemPool>>> {
        self.pool.lock().clone()
    }

    pub(crate) fn select_request_pool(&self) -> Option<Arc<RwLock<MemPool>>> {
        self.request_pool()
    }

    /// Whether the client currently owns a request pool.
    pub(crate) fn has_request_pool(&self) -> bool {
        self.pool.lock().is_some()
    }

    /// Whether this connection may carry request bodies in client SHM blocks:
    /// a live request pool and a negotiated `CAP_SHM_REQUEST` direction.
    ///
    /// This is the single authority for request-side SHM transport selection
    /// and prealloc publication gating; `pool_enabled` alone is not consulted
    /// because it does not disable dedicated SHM requests.
    pub(crate) fn shm_request_available(&self) -> bool {
        self.has_request_pool() && self.negotiated_shm_request.load(Ordering::Acquire)
    }

    /// The complete resolved client configuration this connection was created
    /// from, frozen at construction.
    ///
    /// The cache compares this against every later requested policy so a
    /// same-address hit can never silently serve a connection built with
    /// different chunking, threshold, prewarm, or buddy policy. Budget-limit
    /// equality is tracked separately by the owning domain context.
    pub(crate) fn config(&self) -> &ClientIpcConfig {
        &self.config
    }

    /// Build a fresh reassembly registry whose pool charges `budget`.
    fn build_chunk_registry(
        config: &ClientIpcConfig,
        budget: &c2_mem::MemoryBudget,
    ) -> Result<Arc<ChunkRegistry<()>>, IpcError> {
        let counter = REASSEMBLY_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3a{:08x}{:08x}", std::process::id(), counter);
        // Reassembly follows the same buddy policy: with buddy disabled the
        // assembler stores into dedicated SHM (file spill as last resort)
        // instead of skipping chunked reception. The pool charges the same
        // shared context as the request pool.
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                config.base.reassembly_pool_config(&config.pool_tuning()),
                prefix,
                budget.clone(),
            )
            .map_err(IpcError::Config)?,
        ));
        let chunk_config = ChunkConfig::from_base(config);
        Ok(Arc::new(ChunkRegistry::new(pool, chunk_config)))
    }

    /// Create a new IPC client with a config-owned SHM pool.
    ///
    /// This keeps async callers such as the HTTP relay on the canonical
    /// `IpcClient` API while still allowing large request streams to be written
    /// directly into client SHM. The pool follows the config's buddy policy:
    /// dedicated SHM requests stay available when buddy is disabled. One
    /// finite budget derived from the config is shared by the request pool and
    /// the reassembly pool for this client's lifetime.
    pub fn with_config(address: &str, config: ClientIpcConfig) -> Result<Self, IpcError> {
        let budget = c2_mem::MemoryBudget::from_limits(&config.memory_budget_limits());
        Self::with_shared_budget(address, config, budget)
    }

    /// Publish future physical connections to the owning Runtime's event feed.
    pub(crate) fn with_connection_event_source(
        mut self,
        source: c2_local::ConnectionEventSource,
    ) -> Self {
        self.connection_event_source = Some(source);
        self
    }

    /// Create a new IPC client whose request and reassembly pools both charge
    /// `budget`.
    ///
    /// `budget` is shared by reference with every other pool or client handed
    /// the same [`c2_mem::MemoryBudget`] clone, so one owning domain (a
    /// Runtime's outgoing cache, a relay's upstream pool) can bound all of its
    /// connections with one finite context. The supplied budget is the
    /// authority for enforcement and observation; config is its input.
    pub fn with_shared_budget(
        address: &str,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
    ) -> Result<Self, IpcError> {
        Self::with_budget_endpoint(Self::default_endpoint(address), config, budget)
    }

    /// Create a client with a frozen endpoint and a config-owned memory budget.
    /// The caller resolves the context; this transport never reads environment roots.
    pub fn with_endpoint(
        endpoint: LocalEndpoint,
        config: ClientIpcConfig,
    ) -> Result<Self, IpcError> {
        let budget = c2_mem::MemoryBudget::from_limits(&config.memory_budget_limits());
        Self::with_endpoint_and_shared_budget(endpoint, config, budget)
    }

    /// Use the given endpoint and the owning domain's shared memory budget.
    pub fn with_endpoint_and_shared_budget(
        endpoint: LocalEndpoint,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
    ) -> Result<Self, IpcError> {
        Self::with_budget_endpoint(Ok(endpoint), config, budget)
    }

    fn with_budget_endpoint(
        endpoint: Result<LocalEndpoint, String>,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
    ) -> Result<Self, IpcError> {
        let pool = Self::own_pool_from_config(&config, &budget)?;
        let registry = Self::build_chunk_registry(&config, &budget)?;
        Ok(Self::from_parts(
            endpoint,
            Some(pool),
            config,
            budget,
            Some(registry),
        ))
    }

    /// Create a client around a transport-internal pool built from `config`
    /// (pooled-client acquire path). The pool is owned solely by the resulting
    /// client, so connect-time policy validation and explicit prewarm apply
    /// exactly as for [`with_config`](Self::with_config). `budget` is the
    /// owning cache's shared domain context; the pool was already created with
    /// it and the client's reassembly pool charges the same context.
    pub(crate) fn with_transport_pool(
        endpoint: LocalEndpoint,
        pool: Arc<RwLock<MemPool>>,
        config: ClientIpcConfig,
        budget: c2_mem::MemoryBudget,
    ) -> Result<Self, IpcError> {
        let registry = Self::build_chunk_registry(&config, &budget)?;
        Ok(Self::from_parts(
            Ok(endpoint),
            Some(pool),
            config,
            budget,
            Some(registry),
        ))
    }

    fn default_endpoint(address: &str) -> Result<LocalEndpoint, String> {
        crate::control::local_endpoint_from_ipc_address(address).map_err(|error| error.to_string())
    }

    /// The endpoint frozen at construction, retained across close and reconnect.
    pub fn local_endpoint(&self) -> Result<&LocalEndpoint, IpcError> {
        self.endpoint
            .as_ref()
            .map_err(|error| IpcError::Config(error.clone()))
    }

    fn begin_call(
        &self,
    ) -> Result<(SendGuard, oneshot::Receiver<Result<ResponseData, IpcError>>), IpcError> {
        self.begin_pending(PendingKind::Call(CallMode::Unary))
    }

    fn begin_control(
        &self,
    ) -> Result<(SendGuard, oneshot::Receiver<Result<ResponseData, IpcError>>), IpcError> {
        self.begin_pending(PendingKind::Control)
    }

    fn begin_pending(
        &self,
        kind: PendingKind,
    ) -> Result<(SendGuard, oneshot::Receiver<Result<ResponseData, IpcError>>), IpcError> {
        let conn = self.driver.read().clone().ok_or(IpcError::Closed)?;
        let mut pending = self.pending.lock();
        if conn.core.is_deactivated() {
            return Err(IpcError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(StdMutex::new(SendState::default()));
        pending.queued.push(PendingResponse {
            tx: Some(tx),
            state: state.clone(),
            reply_chunks: None,
            kind,
            upload_stop: None,
            stream: None,
            forward: None,
        });
        Ok((
            SendGuard {
                pending: self.pending.clone(),
                conn,
                state,
                producer: None,
            },
            rx,
        ))
    }

    /// Connect and perform handshake.
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        self.connect_with_deadline(ConnectDeadline::default()).await
    }

    pub async fn connect_with_deadline(
        &mut self,
        deadline: ConnectDeadline,
    ) -> Result<(), IpcError> {
        CONNECT_OPERATION_DEADLINE
            .scope(deadline, self.connect_attempt(deadline))
            .await
    }

    async fn connect_attempt(&mut self, deadline: ConnectDeadline) -> Result<(), IpcError> {
        connect_check(deadline, "ipc_connect")?;
        let reconnect = self.connection.get_mut().is_some();
        if self
            .connection
            .get_mut()
            .as_ref()
            .is_some_and(|publisher| !publisher.is_closed())
        {
            return Err(IpcError::Pool(
                "client must finish closing before reconnect".into(),
            ));
        }
        // Never reuse a prior physical connection's book, IDs or peer mappings.
        self.driver.get_mut().take();
        self.pending = Arc::new(StdMutex::new(PendingMap::default()));
        self.negotiated_shm_request.store(false, Ordering::Release);
        let endpoint = self.local_endpoint()?;
        let stream = with_connect_deadline(deadline, "os_connect", async {
            LocalStream::connect_until(endpoint, deadline.instant())
                .await
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::TimedOut
                        && error.raw_os_error().is_none()
                    {
                        IpcError::Io(error)
                    } else {
                        IpcError::Connect(error)
                    }
                })
        })
        .await?;
        let publisher = self.connection_event_source.as_ref().map_or_else(
            || ConnectionPublisher::new(c2_local::ConnectionSide::Client),
            |source| source.open_connection(c2_local::ConnectionSide::Client),
        );
        *self.connection.get_mut() = Some(publisher.clone());
        let end = EndState::new(publisher);
        let available = match stream.shared_memory_available() {
            Ok(available) => available,
            Err(error) => {
                end.record_io("shm_reachability", &error, false);
                drop(stream);
                end.publish();
                return Err(IpcError::Connect(error));
            }
        };
        let retirement = PhysicalRetirement::new(stream.retain_for_remote_read(), end.clone());
        let abort = stream.abort_handle();
        let (mut reader, mut writer) = stream.into_split();
        // A reconnect always creates fresh pool incarnations on the same budget.
        let pool = match (reconnect, self.pool.get_mut().take()) {
            (false, Some(pool)) => pool,
            _ => Self::own_pool_from_config(&self.config, &self.memory_budget)?,
        };
        if self.config.pool_prewarm_segments > 0 {
            pool.write()
                .ensure_buddy_segments(self.config.pool_prewarm_segments as usize)
                .map_err(IpcError::Shm)?;
        }
        *self.pool.get_mut() = Some(pool);
        let registry = match (reconnect, self.chunk_registry.get_mut().take()) {
            (false, Some(registry)) => registry,
            _ => Self::build_chunk_registry(&self.config, &self.memory_budget)?,
        };
        *self.chunk_registry.get_mut() = Some(registry.clone());
        let handshake = with_connect_deadline(deadline, "handshake", async {
            tokio::time::timeout(
                deadline
                    .remaining("handshake")
                    .map_err(|_| connect_expired("handshake"))?
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
                self.do_handshake(&mut writer, &mut reader, &end, available),
            )
            .await
            .map_err(|_| {
                IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "local handshake deadline expired",
                ))
            })?
        })
        .await;
        let (hs, server_pool) = match handshake {
            Ok(result) => result,
            Err(error) => {
                match &error {
                    IpcError::Io(error) | IpcError::Connect(error) => {
                        end.record_io("handshake", error, false)
                    }
                    IpcError::Protocol(_)
                    | IpcError::Decode(_)
                    | IpcError::ProtocolVersionMismatch { .. } => {
                        end.record(ConnectionEndReason::ProtocolError, "handshake")
                    }
                    _ => end.record(ConnectionEndReason::LocalClose, "connect_deadline"),
                }
                return Err(error);
            }
        };
        connect_directory_write(&self.route_directory)?.seed_from_handshake(&hs.routes);
        self.server_identity = hs.server_identity;
        *self.server_pool.get_mut() = Some(server_pool.clone());
        retirement.set_peer(server_pool.clone());
        let conn = ClientConnection::spawn(
            writer,
            self.pending.clone(),
            abort,
            retirement.clone(),
            self.config.write_timeout(),
        );
        conn.start_gc(
            self.request_pool().unwrap(),
            server_pool.clone(),
            registry.clone(),
            retirement.clone(),
            self.config.pool_decay_seconds,
        );
        *self.driver.get_mut() = Some(conn.clone());
        tokio::spawn(recv_loop(
            reader,
            self.pending.clone(),
            server_pool,
            conn,
            registry,
            self.conn_id,
            available && hs.capability_flags & CAP_SHM_RESPONSE != 0,
            retirement,
        ));
        let ready = connect_check(deadline, "ipc_ready");
        if ready.is_err() {
            self.deactivate();
        }
        ready
    }

    async fn do_handshake(
        &self,
        writer: &mut LocalWriteHalf,
        reader: &mut LocalReadHalf,
        end: &EndState,
        stream_shm_available: bool,
    ) -> Result<(Handshake, Arc<RwLock<ServerPoolState>>), IpcError> {
        let prefix = self
            .request_pool()
            .map(|pool| pool.read().prefix().to_string())
            .unwrap_or_default();
        // SHM directions are offered only when this exact stream can natively
        // reach shared memory with the peer, and — for the request direction —
        // only when a request pool exists to carry the blocks. The server's
        // ACK intersects the offer with its own native reachability.
        let offered_shm_request = stream_shm_available && self.has_request_pool();
        let offered_shm_response = stream_shm_available;
        let mut cap_flags = 0;
        if offered_shm_request {
            cap_flags |= CAP_SHM_REQUEST;
        }
        if offered_shm_response {
            cap_flags |= CAP_SHM_RESPONSE;
        }

        let payload = encode_client_handshake(cap_flags, &prefix)
            .map_err(|e| IpcError::Protocol(e.to_string()))?;
        // Preserve local metadata validation before sending any handshake bytes.
        // Version selection precedes publication of variable metadata. The caller's one
        // handshake guard covers both exchanges without restarting its budget.
        writer
            .write_all(&encode_version_preamble(VersionPreambleKind::Hello))
            .await
            .inspect_err(|error| {
                end.record_io("handshake_write", error, false);
            })?;
        let mut version_header = [0u8; 16];
        reader
            .read_exact(&mut version_header)
            .await
            .inspect_err(|error| {
                end.record_io("handshake_read", error, true);
            })?;
        validate_version_preamble_header(&version_header, true)
            .inspect_err(|_| {
                end.record(ConnectionEndReason::ProtocolError, "handshake_preamble");
            })
            .map_err(|error| IpcError::Protocol(error.to_string()))?;
        let mut version_payload = [0u8; 9];
        reader
            .read_exact(&mut version_payload)
            .await
            .inspect_err(|error| {
                end.record_io("handshake_read", error, true);
            })?;
        let (kind, peer) = decode_version_preamble_payload(&version_payload)
            .inspect_err(|_| {
                end.record(ConnectionEndReason::ProtocolError, "handshake_preamble");
            })
            .map_err(|error| IpcError::Protocol(error.to_string()))?;
        match (kind, peer == HANDSHAKE_VERSION) {
            (VersionPreambleKind::Accepted, true) => {}
            (VersionPreambleKind::VersionMismatch, false) => {
                end.record(ConnectionEndReason::ProtocolError, "handshake_version");
                return Err(IpcError::ProtocolVersionMismatch {
                    local: HANDSHAKE_VERSION,
                    peer,
                });
            }
            _ => {
                end.record(ConnectionEndReason::ProtocolError, "handshake_preamble");
                return Err(IpcError::Protocol(
                    "inconsistent server version preamble".into(),
                ));
            }
        }

        let frame_bytes = frame::encode_frame(0, flags::FLAG_HANDSHAKE, &payload);
        writer.write_all(&frame_bytes).await.inspect_err(|error| {
            end.record_io("handshake_write", error, false);
        })?;

        // Read handshake response.
        let mut header_buf = [0u8; HEADER_SIZE];
        reader
            .read_exact(&mut header_buf)
            .await
            .inspect_err(|error| {
                end.record_io("handshake_read", error, true);
            })?;
        let (total_len, body_rest) = frame::decode_total_len(&header_buf).inspect_err(|_| {
            end.record(ConnectionEndReason::ProtocolError, "handshake_frame");
        })?;
        let (hdr, _hdr_payload) =
            frame::decode_frame_body(body_rest, total_len).inspect_err(|_| {
                end.record(ConnectionEndReason::ProtocolError, "handshake_frame");
            })?;

        if total_len < 12
            || hdr.request_id != 0
            || hdr.flags != (flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE)
        {
            end.record(ConnectionEndReason::ProtocolError, "handshake_frame");
            return Err(IpcError::Protocol(
                "invalid server handshake metadata header".into(),
            ));
        }

        let payload_len = hdr.payload_len();
        let mut payload_buf = vec![0u8; payload_len];
        if payload_len > 0 {
            reader
                .read_exact(&mut payload_buf)
                .await
                .inspect_err(|error| {
                    end.record_io("handshake_read", error, true);
                })?;
        }

        let hs = decode_handshake(&payload_buf, true).inspect_err(|_| {
            end.record(ConnectionEndReason::ProtocolError, "handshake_decode");
        })?;

        if hs.server_identity.is_none() {
            end.record(ConnectionEndReason::ProtocolError, "handshake_identity");
            return Err(IpcError::Protocol(
                "server handshake missing server identity".into(),
            ));
        }

        // Direction negotiation: a direction is active only when this side
        // offered it and the server ACKed it. A server that ACKs an unoffered
        // bit has no effect — the offer is part of the intersection.
        let negotiated_shm_request =
            offered_shm_request && hs.capability_flags & CAP_SHM_REQUEST != 0;

        self.negotiated_shm_request
            .store(negotiated_shm_request, Ordering::Release);

        let cfg = PoolConfig {
            segment_size: 8192,
            max_segments: c2_config::MAX_IPC_POOL_SEGMENTS as usize,
            max_dedicated_segments: usize::from(u16::MAX) + 1,
            buddy_idle_decay_secs: self.config.pool_decay_seconds,
            ..PoolConfig::default()
        };
        let cache = PeerMappingCache::new(cfg, hs.prefix.clone()).map_err(IpcError::Shm)?;
        Ok((
            hs,
            Arc::new(RwLock::new(ServerPoolState {
                cache: Some(cache),
                #[cfg(test)]
                open_failure: None,
            })),
        ))
    }

    /// Identity announced by the connected IPC server handshake.
    pub fn server_identity(&self) -> Option<&c2_wire::handshake::ServerIdentity> {
        self.server_identity.as_ref()
    }

    /// Stable logical server ID announced by the connected IPC server.
    pub fn server_id(&self) -> Option<&str> {
        self.server_identity
            .as_ref()
            .map(|identity| identity.server_id.as_str())
    }

    /// Per-server-incarnation ID announced by the connected IPC server.
    pub fn server_instance_id(&self) -> Option<&str> {
        self.server_identity
            .as_ref()
            .map(|identity| identity.server_instance_id.as_str())
    }

    fn expected_contract_wire(expected: &c2_contract::ExpectedRouteContract) -> RouteContractWire {
        RouteContractWire {
            route_name: expected.route_name.clone(),
            crm_ns: expected.crm_ns.clone(),
            crm_name: expected.crm_name.clone(),
            crm_ver: expected.crm_ver.clone(),
            abi_hash: expected.abi_hash.clone(),
            signature_hash: expected.signature_hash.clone(),
        }
    }

    fn payload_msg_type(payload: &[u8]) -> Option<MsgType> {
        payload.first().and_then(|tag| MsgType::from_byte(*tag))
    }

    fn route_nack_error(payload: &[u8]) -> IpcError {
        match decode_route_nack(payload) {
            Ok(nack) => match ErrorCode::try_from(nack.error.code) {
                Ok(ErrorCode::ProtocolViolation) => IpcError::Protocol(nack.error.message),
                Ok(code) => IpcError::Protocol(format!(
                    "route catalog NACK {}: {}",
                    code.name(),
                    nack.error.message
                )),
                Err(()) => IpcError::Protocol(format!(
                    "route catalog NACK unknown code {}: {}",
                    nack.error.code, nack.error.message
                )),
            },
            Err(err) => IpcError::Protocol(format!("invalid route catalog NACK: {err}")),
        }
    }

    async fn list_routes_raw(
        &self,
        selector: RouteSelector,
    ) -> Result<RouteListResponse, IpcError> {
        let payload = encode_route_list_request(&RouteListRequest { selector })
            .map_err(IpcError::Protocol)?;
        let payload = self.send_control_inline(payload, "route list").await?;
        if Self::payload_msg_type(&payload) == Some(MsgType::RouteNack) {
            return Err(Self::route_nack_error(&payload));
        }
        decode_route_list_response(&payload).map_err(IpcError::Protocol)
    }

    fn bound_route_table(&self, route_name: &str) -> Result<MethodTable, IpcError> {
        connect_directory_read(&self.route_directory)?
            .route_table(route_name)
            .ok_or_else(|| IpcError::RouteNotFound(route_name.to_string()))
    }

    async fn call_resolved_target(
        &self,
        route_name: &str,
        method_idx: u16,
        identity: RouteCallIdentity,
        max_payload_size: u64,
        data: &[u8],
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        let data_len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if data_len > max_payload_size {
            return Err(pre_dispatch_call_error(IpcError::Config(format!(
                "request payload size {data_len} exceeds route '{route_name}' max_payload_size {max_payload_size}"
            ))));
        }

        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;

        match choose_request_transport(&self.config, self.shm_request_available(), data.len()) {
            RequestTransportKind::Buddy => {
                match self.call_buddy(&identity, method_idx, data, control).await {
                    Ok(result) => return Ok(result),
                    Err(error)
                        if error.is_retry_safe()
                            && matches!(error.source_error(), IpcError::Shm(_)) =>
                    {
                        // Pool allocation or SHM setup failed. Fall back through the
                        // non-SHM policy below rather than failing large relay calls
                        // that can still use chunked transfer.
                    }
                    Err(e) => return Err(e),
                }
            }
            RequestTransportKind::Chunked => {
                return self
                    .call_chunked(&identity, method_idx, data, control)
                    .await;
            }
            RequestTransportKind::Inline => {
                return self
                    .call_inline_controlled(&identity, method_idx, data, control)
                    .await;
            }
        }

        self.call_owned_bytes(&identity, method_idx, data, control)
            .await
    }

    /// Send fully owned request bytes through the non-SHM policy: chunked when
    /// the body exceeds the chunk size, otherwise one inline frame. Used for
    /// owned byte fallbacks selected before SHM preparation.
    async fn call_owned_bytes(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        match choose_request_transport(&self.config, false, data.len()) {
            RequestTransportKind::Chunked => {
                self.call_chunked(identity, method_idx, data, control).await
            }
            // Buddy is unreachable with `shm_available=false`; the inline
            // path is the only remaining owned transport.
            RequestTransportKind::Inline | RequestTransportKind::Buddy => {
                self.call_inline_controlled(identity, method_idx, data, control)
                    .await
            }
        }
    }

    /// Send a CRM call through a previously acquired immutable route binding.
    pub async fn call_bound(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        self.call_bound_with_control(binding, method_name, data, None)
            .await
            .map_err(crate::IpcCallError::into_source)
    }

    /// Route-bound call with first-byte dispatch authority supplied by its
    /// owner. Local rejection leaves this connection available to other calls.
    pub async fn call_bound_controlled(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        control: &IpcCallControl,
    ) -> Result<ResponseData, IpcError> {
        self.call_bound_with_control(binding, method_name, data, Some(control))
            .await
            .map_err(crate::IpcCallError::into_source)
    }

    async fn call_bound_with_control(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        let (method_idx, identity, max_payload_size) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        self.call_resolved_target(
            binding.route_name(),
            method_idx,
            identity,
            max_payload_size,
            data,
            control,
        )
        .await
    }

    /// Send a route-bound call with an explicit dispatch-safety phase.
    pub async fn call_bound_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, crate::sync_client::IpcCallError> {
        self.call_bound_with_control(binding, method_name, data, None)
            .await
    }

    /// Controlled route-bound call preserving transport dispatch phase.
    pub async fn call_bound_controlled_phased(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        control: &IpcCallControl,
    ) -> Result<ResponseData, crate::sync_client::IpcCallError> {
        self.call_bound_with_control(binding, method_name, data, Some(control))
            .await
    }

    /// Consume an input owner instead of borrowing it through the reply wait.
    /// SHM fallback is decided while this owner can still be read; received
    /// replies never cause a second request using the same input.
    pub async fn call_bound_owned_controlled_phased<B>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: B,
        control: &IpcCallControl,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        B: AsRef<[u8]> + Send + 'static,
    {
        let (method_idx, identity, max_payload_size) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        let data_len = data.as_ref().len();
        if u64::try_from(data_len).unwrap_or(u64::MAX) > max_payload_size {
            return Err(pre_dispatch_call_error(IpcError::Config(format!(
                "request payload size {data_len} exceeds route '{}' max_payload_size {max_payload_size}",
                binding.route_name()
            ))));
        }
        IpcCallControl::check_preparation(Some(control)).map_err(pre_dispatch_call_error)?;
        if choose_request_transport(&self.config, self.shm_request_available(), data_len)
            == RequestTransportKind::Buddy
        {
            let prepared = self.try_alloc_request_block(data_len).and_then(|block| {
                if let Some(block) = &block {
                    block.write_at(0, data.as_ref())?;
                }
                Ok(block)
            });
            match prepared {
                Ok(Some(block)) => {
                    drop(data);
                    return self
                        .call_with_prealloc_controlled(&identity, method_idx, &block, Some(control))
                        .await;
                }
                // Only unpublished SHM preparation can select another carrier.
                Ok(None) | Err(IpcError::Shm(_)) => {}
                Err(error) => return Err(pre_dispatch_call_error(error)),
            }
        }
        match choose_request_transport(&self.config, false, data_len) {
            RequestTransportKind::Chunked => {
                self.call_chunked_stream_controlled(
                    &identity,
                    method_idx,
                    data_len,
                    futures_util::stream::iter([Ok::<_, std::io::Error>(data)]),
                    Some(control.clone()),
                )
                .await
            }
            RequestTransportKind::Inline | RequestTransportKind::Buddy => {
                self.call_inline_controlled(&identity, method_idx, data, Some(control))
                    .await
            }
        }
    }

    /// Send a CRM call from an owned known-size stream. After first publication
    /// the send task retains the stream through completion, even if the caller
    /// cancels its response wait.
    pub async fn call_bound_sized_stream<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data_len: u64,
        chunks: S,
    ) -> Result<ResponseData, IpcError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        self.call_bound_sized_stream_phased(binding, method_name, data_len, chunks, None)
            .await
            .map_err(crate::sync_client::IpcCallError::into_source)
    }

    /// Send a route-bound streaming call with an explicit dispatch-safety
    /// phase.
    pub async fn call_bound_sized_stream_phased<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data_len: u64,
        chunks: S,
        payload_budget: Option<&PayloadBufferBudget>,
    ) -> Result<ResponseData, crate::sync_client::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let (method_idx, identity, max_payload_size) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        if data_len > max_payload_size {
            return Err(pre_dispatch_call_error(IpcError::Config(format!(
                "request payload size {data_len} exceeds route '{}' max_payload_size {max_payload_size}",
                binding.route_name()
            ))));
        }
        self.call_sized_stream_resolved_target_phased(
            method_idx,
            identity,
            data_len,
            chunks,
            payload_budget,
        )
        .await
    }

    async fn call_sized_stream_resolved_target_phased<S, B, E>(
        &self,
        method_idx: u16,
        identity: RouteCallIdentity,
        data_len: u64,
        chunks: S,
        payload_budget: Option<&PayloadBufferBudget>,
    ) -> Result<ResponseData, crate::sync_client::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let data_len = checked_payload_len_usize(data_len).map_err(pre_dispatch_call_error)?;
        if data_len == 0 {
            return self.call_inline(&identity, method_idx, &[]).await;
        }

        match choose_request_transport(&self.config, self.shm_request_available(), data_len) {
            RequestTransportKind::Buddy => {
                if let Some(block) = self
                    .try_alloc_request_block(data_len)
                    .map_err(pre_dispatch_call_error)?
                {
                    return self
                        .call_buddy_stream(&identity, method_idx, block, data_len, chunks)
                        .await;
                }
                match choose_request_transport(&self.config, false, data_len) {
                    RequestTransportKind::Chunked => {
                        self.call_chunked_stream_phased(&identity, method_idx, data_len, chunks)
                            .await
                    }
                    RequestTransportKind::Inline | RequestTransportKind::Buddy => {
                        self.call_collected_inline(
                            &identity,
                            method_idx,
                            data_len,
                            chunks,
                            payload_budget,
                        )
                        .await
                    }
                }
            }
            RequestTransportKind::Chunked => {
                self.call_chunked_stream_phased(&identity, method_idx, data_len, chunks)
                    .await
            }
            RequestTransportKind::Inline => {
                self.call_collected_inline(&identity, method_idx, data_len, chunks, payload_budget)
                    .await
            }
        }
    }

    async fn call_collected_inline<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data_len: usize,
        chunks: S,
        payload_budget: Option<&PayloadBufferBudget>,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        // The source already owns B. Reserve its sending copy before either
        // allocation or polling the source, without waiting on our own input.
        let reservation = payload_budget
            .map(|budget| budget.try_reserve(data_len as u64))
            .transpose()
            .map_err(|error| {
                pre_dispatch_call_error(IpcError::LocalCallRejected(
                    C2Error::new(ErrorCode::CallCapacityExceeded, error.to_string()).with_details(
                        std::collections::BTreeMap::from([(
                            "dispatch_phase".into(),
                            "pre_dispatch".into(),
                        )]),
                    ),
                ))
            })?;
        let data = collect_exact_stream(data_len, chunks)
            .await
            .map_err(pre_dispatch_call_error)?;
        match reservation {
            Some(reservation) => {
                self.call_inline_controlled(
                    identity,
                    method_idx,
                    BudgetedBuffer::new(data, reservation),
                    None,
                )
                .await
            }
            None => {
                self.call_inline_controlled(identity, method_idx, data, None)
                    .await
            }
        }
    }

    /// Inline call path: OPEN metadata and input share one frame.
    pub(crate) async fn call_inline(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, crate::IpcCallError> {
        self.call_inline_controlled(identity, method_idx, data, None)
            .await
    }

    async fn call_inline_controlled<B: AsRef<[u8]>>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: B,
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;
        let (send, mut rx) = self.begin_call().map_err(pre_dispatch_call_error)?;
        // The draft has no wire identity until the unique writer submits it.
        let bytes = rpc::encode_open_draft_frame(
            identity,
            method_idx,
            CallMode::Unary,
            ValuePart::whole(ValueStorage::Inline(data.as_ref())),
        )
        .map_err(|error| pre_dispatch_call_error(IpcError::Protocol(error.to_string())))?;
        let written = send
            .conn
            .submit(
                bytes,
                WriteRequest::Business(Vec::new()),
                send.state.clone(),
                control.cloned(),
            )
            .await;
        // Sending has finished. Keep only response ownership while awaiting rx.
        drop(data);
        let response = match written {
            Ok(()) => rx
                .await
                .map_err(|_| dispatch_call_error(IpcError::Closed, send.started()))?,
            Err(error) => prefer_ready_response(error, &mut rx),
        };
        owned_response(response).map_err(|error| dispatch_call_error(error, send.started()))
    }

    async fn call_buddy(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        let block = self
            .try_alloc_request_block(data.len())
            .map_err(pre_dispatch_call_error)?
            .ok_or_else(|| {
                pre_dispatch_call_error(IpcError::Shm("request SHM allocation unavailable".into()))
            })?;
        block.write_at(0, data).map_err(pre_dispatch_call_error)?;
        self.call_with_prealloc_controlled(identity, method_idx, &block, control)
            .await
    }

    pub(crate) fn try_alloc_request_block(
        &self,
        data_size: usize,
    ) -> Result<Option<RequestBlock>, IpcError> {
        if data_size > u32::MAX as usize {
            return Ok(None);
        }
        let Some(pool) = self.select_request_pool() else {
            return Ok(None);
        };
        let result = pool.write().alloc_shm_handle(data_size);
        Ok(result.ok().map(|handle| RequestBlock::new(pool, handle)))
    }

    async fn call_buddy_stream<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        block: RequestBlock,
        data_size: usize,
        chunks: S,
    ) -> Result<ResponseData, crate::sync_client::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let mut written = 0usize;
        futures_util::pin_mut!(chunks);
        // The block is still armed in this loop: no frame byte has been
        // written, so cancelling the future at any of these await points
        // drops the block and releases it through the owning pool.
        while let Some(next) = chunks.next().await {
            let chunk = match next {
                Ok(chunk) => chunk,
                Err(err) => {
                    return Err(pre_dispatch_call_error(stream_error(err)));
                }
            };
            let data = chunk.as_ref();
            if data.is_empty() {
                continue;
            }
            let Some(next_written) = written.checked_add(data.len()) else {
                return Err(pre_dispatch_call_error(IpcError::Config(
                    "request body size overflow while streaming to SHM".into(),
                )));
            };
            if next_written > data_size {
                return Err(pre_dispatch_call_error(IpcError::Config(format!(
                    "request body exceeded declared content length {data_size}"
                ))));
            }
            block
                .write_at(written, data)
                .map_err(pre_dispatch_call_error)?;
            written = next_written;
        }

        if written != data_size {
            return Err(pre_dispatch_call_error(IpcError::Config(format!(
                "request body ended at {written} bytes, expected {data_size}"
            ))));
        }

        // Preparation errors above are proven unpublished. Once the actual
        // send function takes over, preserve its existing error classification
        // and conservative ownership of any partially published buddy frame.
        self.call_with_prealloc(identity, method_idx, &block).await
    }

    /// Publish a filled block through its original pool and physical connection.
    /// The move-only preparation token releases it on cancellation before I/O;
    /// after publication the loan book waits for release or peer end.
    pub(crate) async fn call_with_prealloc(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        block: &RequestBlock,
    ) -> Result<ResponseData, crate::IpcCallError> {
        self.call_with_prealloc_controlled(identity, method_idx, block, None)
            .await
    }

    async fn call_with_prealloc_controlled(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        block: &RequestBlock,
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;
        let (send, mut rx) = self.begin_call().map_err(pre_dispatch_call_error)?;
        let loan = block.prepare(&send.conn).map_err(pre_dispatch_call_error)?;
        let alloc = block.allocation();
        let descriptor = BuddyPayload {
            seg_idx: alloc.seg_idx as u16,
            generation: alloc.generation,
            offset: alloc.offset,
            data_size: block.len() as u32,
            is_dedicated: alloc.is_dedicated,
            loan_id: loan.id(),
        };
        let bytes = rpc::encode_open_draft_frame(
            identity,
            method_idx,
            CallMode::Unary,
            ValuePart::whole(ValueStorage::Shm(descriptor)),
        )
        .map_err(|error| pre_dispatch_call_error(IpcError::Protocol(error.to_string())))?;
        let written = send
            .conn
            .submit(
                bytes,
                WriteRequest::Business(vec![loan]),
                send.state.clone(),
                control.cloned(),
            )
            .await;
        let response = match written {
            Ok(()) => rx
                .await
                .map_err(|_| dispatch_call_error(IpcError::Closed, send.started()))?,
            Err(error) => prefer_ready_response(error, &mut rx),
        };
        owned_response(response).map_err(|error| dispatch_call_error(error, send.started()))
    }

    async fn call_chunked(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
        control: Option<&IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError> {
        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;
        self.call_chunked_stream_controlled(
            identity,
            method_idx,
            data.len(),
            futures_util::stream::iter([Ok::<_, std::io::Error>(data.to_vec())]),
            control.cloned(),
        )
        .await
    }

    async fn call_chunked_stream_phased<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data_size: usize,
        chunks: S,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        self.call_chunked_stream_controlled(identity, method_idx, data_size, chunks, None)
            .await
    }

    async fn call_chunked_stream_controlled<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data_size: usize,
        chunks: S,
        policy: Option<IpcCallControl>,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let chunk_size = self.config.chunk_size as usize;
        let total_chunks =
            request_chunk_count(data_size, chunk_size).map_err(pre_dispatch_call_error)?;
        if total_chunks == 0 {
            return self
                .call_inline_controlled(identity, method_idx, &[], policy.as_ref())
                .await;
        }
        let (mut send, mut rx) = self.begin_call().map_err(pre_dispatch_call_error)?;
        let conn = send.conn.clone();
        let state = send.state.clone();
        let identity = identity.clone();
        let (upload_stop, stop_upload) = watch::channel(false);
        if let Some(entry) = self.pending.lock().unpublished_mut(&send.state) {
            entry.upload_stop = Some(upload_stop);
        }
        let producer = send.conn.spawn_send(async move {
            Self::send_chunked_stream_frames(
                conn,
                state,
                total_chunks,
                chunk_size,
                data_size,
                identity,
                method_idx,
                chunks,
                policy,
                stop_upload,
            )
            .await
        });
        send.producer = Some(producer.abort_handle());
        match producer.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let phase = error.phase();
                return owned_response(prefer_ready_response(error.into_source(), &mut rx))
                    .map_err(|source| {
                        dispatch_call_error(
                            source,
                            phase == crate::TransportPhase::DispatchUncertain,
                        )
                    });
            }
            Err(error) => {
                let source = IpcError::Io(std::io::Error::other(error.to_string()));
                if send.started() {
                    send.conn.end_io(
                        "request_send_task",
                        &std::io::Error::other(error.to_string()),
                        false,
                    );
                }
                return owned_response(prefer_ready_response(source, &mut rx))
                    .map_err(|source| dispatch_call_error(source, send.started()));
            }
        }
        match rx.await {
            Ok(result) => owned_response(result).map_err(|error| {
                if matches!(error, IpcError::Closed) && !send.started() {
                    pre_dispatch_call_error(error)
                } else {
                    classified_call_error(error)
                }
            }),
            Err(_) => Err(dispatch_uncertain_call_error(IpcError::Closed)),
        }
    }

    async fn send_chunked_stream_frames<S, B, E>(
        conn: Arc<ClientConnection>,
        state: Arc<StdMutex<SendState>>,
        total_chunks: usize,
        chunk_size: usize,
        data_size: usize,
        identity: RouteCallIdentity,
        method_idx: u16,
        chunks: S,
        policy: Option<IpcCallControl>,
        mut stop_upload: watch::Receiver<bool>,
    ) -> Result<(), crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let mut written = 0usize;
        let mut index = 0usize;
        let mut buffered = Vec::with_capacity(chunk_size.min(data_size));
        let mut response_open = true;
        futures_util::pin_mut!(chunks);
        loop {
            let next = loop {
                if *stop_upload.borrow() {
                    return Ok(());
                }
                tokio::select! {
                    biased;
                    changed = stop_upload.changed(), if response_open => {
                        // Dropping a dispatched caller does not cancel its
                        // upload. Only an actual response stops it early.
                        response_open = changed.is_ok();
                    }
                    next = chunks.next() => break next,
                }
            };
            let Some(next) = next else { break };
            let chunk = next
                .map_err(|error| Self::chunk_source_failure(&conn, &state, stream_error(error)))?;
            let mut data = chunk.as_ref();
            let next = written
                .checked_add(data.len())
                .filter(|next| *next <= data_size)
                .ok_or_else(|| {
                    Self::chunk_source_failure(
                        &conn,
                        &state,
                        IpcError::Config("request stream exceeded declared content length".into()),
                    )
                })?;
            while !data.is_empty() {
                if *stop_upload.borrow() {
                    return Ok(());
                }
                let take = (chunk_size - buffered.len()).min(data.len());
                buffered.extend_from_slice(&data[..take]);
                data = &data[take..];
                if buffered.len() == chunk_size {
                    Self::write_chunk_frame(
                        &conn,
                        &state,
                        index,
                        data_size,
                        (&identity, method_idx, CallMode::Unary),
                        &buffered,
                        if index == 0 { policy.clone() } else { None },
                    )
                    .await
                    .map_err(|error| dispatch_call_error(error, state.lock().started()))?;
                    buffered.clear();
                    index += 1;
                }
            }
            written = next;
        }
        if *stop_upload.borrow() {
            return Ok(());
        }
        if written != data_size {
            return Err(Self::chunk_source_failure(
                &conn,
                &state,
                IpcError::Config(format!(
                    "request stream ended at {written} bytes, expected {data_size}"
                )),
            ));
        }
        if !buffered.is_empty() {
            Self::write_chunk_frame(
                &conn,
                &state,
                index,
                data_size,
                (&identity, method_idx, CallMode::Unary),
                &buffered,
                if index == 0 { policy } else { None },
            )
            .await
            .map_err(|error| dispatch_call_error(error, state.lock().started()))?;
            index += 1;
        }
        if index != total_chunks {
            return Err(Self::chunk_source_failure(
                &conn,
                &state,
                IpcError::Config("request stream chunk count mismatch".into()),
            ));
        }
        Ok(())
    }

    fn chunk_source_failure(
        conn: &ClientConnection,
        state: &StdMutex<SendState>,
        error: IpcError,
    ) -> crate::IpcCallError {
        let started = {
            let mut state = state.lock();
            state.fail_upload();
            state.started()
        };
        if started {
            conn.end_io(
                "request_source",
                &std::io::Error::other(error.to_string()),
                false,
            );
        }
        dispatch_call_error(error, started)
    }

    async fn write_chunk_frame(
        conn: &Arc<ClientConnection>,
        state: &Arc<StdMutex<SendState>>,
        chunk_idx: usize,
        total_bytes: usize,
        open: (&RouteCallIdentity, u16, CallMode),
        chunk_data: &[u8],
        policy: Option<IpcCallControl>,
    ) -> Result<(), IpcError> {
        let bytes = if chunk_idx == 0 {
            rpc::encode_open_draft_frame(
                open.0,
                open.1,
                open.2,
                ValuePart::first(total_bytes as u64, ValueStorage::Inline(chunk_data)),
            )
        } else {
            rpc::encode_open_continuation_draft_frame(ValueStorage::Inline(chunk_data))
        }
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
        let request = if chunk_idx == 0 {
            WriteRequest::Business(Vec::new())
        } else {
            WriteRequest::CallContinuation(conn.core.prepare_call_record().map_err(core_error)?)
        };
        conn.submit(bytes, request, state.clone(), policy).await
    }

    fn validate_method_table_contract(
        route_name: &str,
        table: &MethodTable,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        if table.crm_ns() == expected.crm_ns
            && table.crm_name() == expected.crm_name
            && table.crm_ver() == expected.crm_ver
            && table.abi_hash() == expected.abi_hash
            && table.signature_hash() == expected.signature_hash
        {
            return Ok(());
        }
        Err(IpcError::ContractMismatch(format!(
            "CRM contract mismatch for route {}: expected {}/{}/{} abi_hash={} signature_hash={}, got {}/{}/{} abi_hash={} signature_hash={}",
            route_name,
            expected.crm_ns,
            expected.crm_name,
            expected.crm_ver,
            expected.abi_hash,
            expected.signature_hash,
            table.crm_ns(),
            table.crm_name(),
            table.crm_ver(),
            table.abi_hash(),
            table.signature_hash(),
        )))
    }

    async fn send_control_inline(
        &self,
        payload: Vec<u8>,
        description: &str,
    ) -> Result<Vec<u8>, IpcError> {
        let (send, mut rx) = self.begin_control()?;
        let control = send.conn.core.prepare_control_frame().map_err(core_error)?;
        let bytes = frame::encode_frame(0, flags::FLAG_CTRL, &payload);
        let written = send
            .conn
            .submit(
                bytes,
                WriteRequest::Control(control),
                send.state.clone(),
                None,
            )
            .await;
        let response = match written {
            Ok(()) => rx.await.map_err(|_| IpcError::Closed)?,
            Err(error) => prefer_ready_response(error, &mut rx),
        };
        match response? {
            ResponseData::Inline(payload) => Ok(payload),
            _ => Err(IpcError::Protocol(format!(
                "{description} returned non-inline response"
            ))),
        }
    }

    /// Get the method table for a route.
    pub fn route_table(&self, name: &str) -> Option<MethodTable> {
        self.route_directory.read().route_table(name)
    }

    /// Whether the cached route table contains a route.
    pub fn has_route(&self, name: &str) -> bool {
        self.route_directory.read().has_route(name)
    }

    /// Validate that the cached route matches the complete expected route contract.
    pub fn validate_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        let table = connect_directory_read(&self.route_directory)?
            .route_table(&expected.route_name)
            .ok_or_else(|| IpcError::RouteNotFound(expected.route_name.clone()))?;
        Self::validate_method_table_contract(&expected.route_name, &table, expected)
    }

    async fn rebuild_route_directory(&self) -> Result<(), IpcError> {
        let response = self.list_routes_raw(RouteSelector::All).await?;
        connect_directory_write(&self.route_directory)?.rebuild_from_list(response);
        Ok(())
    }

    async fn lookup_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        let request = RouteLookupRequest {
            expected: Self::expected_contract_wire(expected),
        };
        let payload = encode_route_lookup_request(&request).map_err(IpcError::Protocol)?;
        let payload = self.send_control_inline(payload, "route lookup").await?;
        if Self::payload_msg_type(&payload) == Some(MsgType::RouteNack) {
            return Err(Self::route_nack_error(&payload));
        }
        let response = decode_route_lookup_response(&payload).map_err(IpcError::Protocol)?;
        match response {
            RouteLookupResponse::Ready { current } => {
                connect_directory_write(&self.route_directory)?.apply_record(current);
                self.validate_route_contract(expected)
            }
            RouteLookupResponse::NotFound { route_name } => {
                connect_directory_write(&self.route_directory)?.remove_route(&route_name);
                Err(IpcError::RouteNotFound(route_name))
            }
            RouteLookupResponse::Removed { route_name } => {
                connect_directory_write(&self.route_directory)?.remove_route(&route_name);
                Err(IpcError::RouteRemoved { route_name })
            }
            RouteLookupResponse::Closed { route_name, reason } => {
                connect_directory_write(&self.route_directory)?.remove_route(&route_name);
                Err(IpcError::RouteClosed {
                    route_name,
                    reason: format!("{reason:?}"),
                })
            }
            RouteLookupResponse::ContractMismatch { current } => {
                let message = format!(
                    "CRM contract mismatch for route {}: expected {}/{}/{} abi_hash={} signature_hash={}, got {}/{}/{} abi_hash={} signature_hash={}",
                    expected.route_name,
                    expected.crm_ns,
                    expected.crm_name,
                    expected.crm_ver,
                    expected.abi_hash,
                    expected.signature_hash,
                    current.contract.crm_ns,
                    current.contract.crm_name,
                    current.contract.crm_ver,
                    current.contract.abi_hash,
                    current.contract.signature_hash,
                );
                connect_directory_write(&self.route_directory)?.apply_record(current);
                Err(IpcError::ContractMismatch(message))
            }
        }
    }

    async fn lookup_route_contract_for_acquire(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        let mut delays = ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS.iter();
        loop {
            let result = self.lookup_route_contract(expected).await;

            match result {
                Err(err @ IpcError::RouteNotFound(_)) => {
                    let Some(&delay_ms) = delays.next() else {
                        return Err(err);
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                other => return other,
            }
        }
    }

    /// Rebuild the route directory from the connected server catalog.
    pub async fn rebuild_route_catalog(&self) -> Result<(), IpcError> {
        self.rebuild_route_directory().await
    }

    /// Perform a contract-scoped route lookup against the connected server catalog.
    pub async fn lookup_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        self.lookup_route_contract(expected).await
    }

    /// Authoritatively acquire a route binding for the expected CRM contract.
    pub async fn acquire_route_with_deadline(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        deadline: ConnectDeadline,
    ) -> Result<RouteBinding, IpcError> {
        with_connect_deadline(deadline, "route_acquire", self.acquire_route(expected)).await
    }

    /// Authoritatively acquire a route binding without an extra total budget.
    pub async fn acquire_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteBinding, IpcError> {
        let mut delays = ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS.iter();
        loop {
            self.lookup_route_contract_for_acquire(expected).await?;
            match self.bind_cached_route(expected) {
                Err(err @ IpcError::RouteNotFound(_)) => {
                    let Some(&delay_ms) = delays.next() else {
                        return Err(err);
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                other => return other,
            }
        }
    }

    /// Attest a route for relay registration without requiring call admission.
    ///
    /// Relay registration has a publish-before-open phase where the server route
    /// exists in the catalog as `Closed(RegisterCommitted)`. Business clients
    /// must not call such a route, so [`IpcClient::acquire_route`] rejects it.
    /// Registration uses this narrower control-plane attestation instead.
    pub async fn attest_route_for_registration(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteRecordWire, IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        let response = self
            .list_routes_raw(RouteSelector::RouteName {
                route_name: expected.route_name.clone(),
            })
            .await?;
        let Some(record) = response
            .routes
            .into_iter()
            .find(|record| record.route_name == expected.route_name)
        else {
            return Err(IpcError::RouteNotFound(expected.route_name.clone()));
        };
        let table = MethodTable::from_record(&record);
        Self::validate_method_table_contract(&expected.route_name, &table, expected)?;
        match record.state {
            RouteStateWire::Ready => Ok(record),
            RouteStateWire::Closed
                if record.state_reason == RouteStateReasonWire::RegisterCommitted =>
            {
                Ok(record)
            }
            state => Err(IpcError::RouteClosed {
                route_name: record.route_name,
                reason: format!("{state:?}:{:?}", record.state_reason),
            }),
        }
    }

    /// Bind the cached route record for internal tests and post-lookup callers.
    pub(crate) fn bind_cached_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteBinding, IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        let table = self.bound_route_table(&expected.route_name)?;
        Self::validate_method_table_contract(&expected.route_name, &table, expected)?;
        Ok(RouteBinding::from_table(table))
    }

    /// CRM tag advertised by a route, if present.
    pub fn route_contract(&self, route_name: &str) -> Option<c2_contract::ExpectedRouteContract> {
        self.route_directory
            .read()
            .route_table(route_name)
            .map(|table| c2_contract::ExpectedRouteContract {
                route_name: route_name.to_string(),
                crm_ns: table.crm_ns().to_string(),
                crm_name: table.crm_name().to_string(),
                crm_ver: table.crm_ver().to_string(),
                abi_hash: table.abi_hash().to_string(),
                signature_hash: table.signature_hash().to_string(),
            })
    }

    /// Maximum logical payload size advertised by a route, if present.
    pub fn route_max_payload_size(&self, route_name: &str) -> Option<u64> {
        self.route_directory
            .read()
            .route_table(route_name)
            .map(|table| table.max_payload_size())
    }

    /// Attest and acquire a route that is still pending registration.
    ///
    /// This is a registration control-plane path: the route is not visible in
    /// the committed catalog yet, so ordinary authoritative route lookup cannot
    /// acquire it. The pending attestation response is
    /// returned directly as relay registration proof.
    pub async fn acquire_pending_route_attestation(
        &mut self,
        route_name: &str,
        registration_token: &str,
    ) -> Result<PendingRouteAttestation, IpcError> {
        let payload = encode_pending_route_attestation_request(route_name, registration_token)
            .map_err(IpcError::Protocol)?;
        let payload = self
            .send_control_inline(payload, "pending route attestation")
            .await?;
        match decode_pending_route_attestation_response(&payload).map_err(IpcError::Protocol)? {
            PendingRouteAttestationResponse::Attested { contract } => Ok(contract),
            PendingRouteAttestationResponse::Rejected { code, message } => {
                if code == PENDING_ROUTE_REJECT_NOT_FOUND {
                    Err(IpcError::RouteNotFound(route_name.to_string()))
                } else {
                    Err(IpcError::ContractMismatch(message))
                }
            }
        }
    }

    /// Get all route names.
    pub fn route_names(&self) -> Vec<String> {
        self.route_directory.read().route_names()
    }

    pub fn is_connected(&self) -> bool {
        self.driver
            .read()
            .as_ref()
            .is_some_and(|conn| !conn.core.is_deactivated() && !conn.publisher.is_closed())
    }

    /// The current physical connection, including its terminal record. This
    /// does not connect/reconnect; a client with no physical connection has None.
    pub fn connection_observation(&self) -> Option<ConnectionObservation> {
        self.connection
            .read()
            .as_ref()
            .map(ConnectionPublisher::observation)
    }

    /// Close the client.
    pub async fn close(&mut self) {
        self.close_shared().await;
    }

    /// Close the client through shared ownership with the default deadline.
    ///
    /// Deactivate business admission and observe the original connection.
    /// A timeout leaves its writer, remote reads and loans with their driver.
    pub async fn close_shared(&self) {
        let _ = self.close_shared_bounded(DEFAULT_CLOSE_TIMEOUT).await;
    }

    pub async fn close_shared_bounded(&self, timeout: std::time::Duration) -> bool {
        self.deactivate();
        let closed = match self.connection_observation() {
            Some(observation) => observation.wait_closed(Some(timeout)).await.is_some(),
            None => true,
        };
        if closed {
            self.server_pool.lock().take();
        }
        closed
    }

    /// Stop new calls and loans while sent calls finish on this connection.
    pub fn deactivate(&self) {
        if let Some(conn) = self.driver.read().as_ref() {
            conn.deactivate();
        }
        // The drivers and outstanding allocations own everything required by
        // dispatched work. A retained business reference must not cache empty
        // backing after those owners have drained.
        self.pool.lock().take();
        self.chunk_registry.lock().take();
    }

    /// Exact request-pool owner for bounded lock-contention tests. This is
    /// absent from default builds and never substitutes for release authority.
    #[cfg(any(test, feature = "test-support"))]
    ///
    /// # Safety
    /// Test code must not replace or destroy the pool or invalidate any live
    /// request allocation. The handle is solely for observation/lock contention.
    pub unsafe fn request_pool_for_test(&self) -> Option<Arc<RwLock<MemPool>>> {
        self.request_pool()
    }

    /// Number of live pending-response entries (test-only probe for
    /// per-call pending cleanup).
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_len_for_test(&self) -> usize {
        self.pending.lock().len()
    }

    /// Pause the next prepared write after `begin_io`, before its first byte.
    /// The existing one-shot pause is consumed by the writer; clearing it only
    /// removes a pause that has not yet been consumed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_write_pause_for_test(
        &self,
        pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) {
        let driver = self.driver.read();
        let conn = driver
            .as_ref()
            .expect("test write pause requires an established connection");
        *conn.pause.lock() = pause;
    }

    /// Pause the next business submission after its frame's first eight bytes
    /// have really been written, including when releases precede that frame.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_write_prefix_pause_for_test(
        &self,
        pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) {
        let driver = self.driver.read();
        let conn = driver
            .as_ref()
            .expect("test write pause requires a connection");
        *conn.prefix_pause.lock() = pause;
    }
}

impl Drop for IpcClient {
    fn drop(&mut self) {
        self.deactivate();
    }
}

fn prefer_ready_response(
    error: IpcError,
    receiver: &mut oneshot::Receiver<Result<ResponseData, IpcError>>,
) -> Result<ResponseData, IpcError> {
    match receiver.try_recv() {
        // Local teardown can settle an unpublished entry without any reply.
        // It must not replace the writer's concrete pre-dispatch failure.
        Ok(Err(IpcError::Closed)) => Err(error),
        Ok(response) => response,
        Err(_) => Err(error),
    }
}

fn owned_response(response: Result<ResponseData, IpcError>) -> Result<ResponseData, IpcError> {
    response?
        .into_owned_bytes()
        .map(ResponseData::Inline)
        .map_err(|error| IpcError::Shm(error).response_failure())
}

struct ReceiveCleanup {
    conn: Arc<ClientConnection>,
    registry: Arc<ChunkRegistry<()>>,
    conn_id: u64,
    finished: bool,
}
impl Drop for ReceiveCleanup {
    fn drop(&mut self) {
        if !self.finished {
            self.registry.cleanup_connection(self.conn_id);
            self.conn
                .end(ConnectionEndReason::LocalClose, "receive_task_drop");
            self.conn.read_stopped(false);
        }
    }
}

pub(crate) fn recv_loop(
    reader: LocalReadHalf,
    pending: Arc<StdMutex<PendingMap>>,
    server_pool: Arc<RwLock<ServerPoolState>>,
    conn: Arc<ClientConnection>,
    registry: Arc<ChunkRegistry<()>>,
    conn_id: u64,
    shm_allowed: bool,
    retirement: Arc<PhysicalRetirement>,
) -> impl std::future::Future<Output = ()> + Send {
    struct ReceiveTask {
        cleanup: ReceiveCleanup,
        server_pool: Arc<RwLock<ServerPoolState>>,
        pending: Arc<StdMutex<PendingMap>>,
        driver: ReadDriver,
    }
    let task = ReceiveTask {
        cleanup: ReceiveCleanup {
            conn,
            registry,
            conn_id,
            finished: false,
        },
        server_pool,
        pending,
        driver: ReadDriver {
            stream: Some(reader),
            retirement,
        },
    };
    async move {
        let mut task = task;
        let conn = &task.cleanup.conn;
        let registry = &task.cleanup.registry;
        let server_pool = &task.server_pool;
        let pending = &task.pending;
        let retirement = task.driver.retirement.clone();
        let reader = task.driver.stream.as_mut().unwrap();
        let mut header = [0u8; HEADER_SIZE];
        let mut payload = Vec::with_capacity(4096);
        let mut malformed = false;
        'frames: loop {
            if let Err(error) = reader.read_exact(&mut header).await {
                conn.end_io("read_header", &error, true);
                break;
            }
            let hdr = FrameHeader {
                total_len: u32::from_le_bytes(header[..4].try_into().unwrap()),
                request_id: u64::from_le_bytes(header[4..12].try_into().unwrap()),
                flags: u32::from_le_bytes(header[12..].try_into().unwrap()),
            };
            if hdr.total_len < 12 {
                conn.framing_failed("response_frame_size");
                malformed = true;
                break;
            }
            payload.clear();
            if let Err(error) = payload.try_reserve(hdr.payload_len()) {
                registry.abort(conn_id, hdr.request_id);
                let mut remaining = hdr.payload_len();
                let mut scratch = [0u8; 4096];
                let mut record_kind = None;
                // A readable frame still has a precise end. Settle an admitted
                // SHM descriptor even when allocating its whole frame failed.
                // Every valid unary SHM record fits this fixed metadata reserve.
                // It includes kind/status/first total before the descriptor.
                if hdr.is_buddy() && shm_allowed && remaining <= 64 {
                    let prefix = &mut scratch[..remaining];
                    if let Err(error) = reader.read_exact(prefix).await {
                        conn.end_io("discard_descriptor", &error, true);
                        break 'frames;
                    }
                    record_kind = prefix.first().copied();
                    remaining = 0;
                    let mode = match pending.lock().get(&hdr.request_id).map(|entry| entry.kind) {
                        Some(PendingKind::Call(mode)) => mode,
                        _ => CallMode::Unary,
                    };
                    let descriptor =
                        match rpc::decode_rpc_record_with_carrier(&hdr, prefix, Some(mode)) {
                            Ok(
                                RpcRecord::CallEnd {
                                    value:
                                        ValuePart {
                                            storage: ValueStorage::Shm(descriptor),
                                            ..
                                        },
                                    ..
                                }
                                | RpcRecord::CallEndContinuation {
                                    value:
                                        ValuePart {
                                            storage: ValueStorage::Shm(descriptor),
                                            ..
                                        },
                                }
                                | RpcRecord::ItemData {
                                    value:
                                        ValuePart {
                                            storage: ValueStorage::Shm(descriptor),
                                            ..
                                        },
                                },
                            ) => Some(descriptor),
                            Err(failure) => {
                                if failure.error == DecodeError::ZeroLoanId {
                                    conn.end(
                                        ConnectionEndReason::ProtocolError,
                                        "response_loan_admission",
                                    );
                                }
                                failure.carrier
                            }
                            _ => None,
                        };
                    if let Some(descriptor) = descriptor {
                        discard_response_storage(
                            ValueStorage::Shm(descriptor),
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                    }
                }
                while remaining > 0 {
                    let count = remaining.min(scratch.len());
                    if let Err(error) = reader.read_exact(&mut scratch[..count]).await {
                        conn.end_io("discard_payload", &error, true);
                        break 'frames;
                    }
                    if record_kind.is_none() {
                        record_kind = scratch.first().copied();
                    }
                    remaining -= count;
                }
                let error =
                    IpcError::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, error));
                if hdr.flags & flags::FLAG_RPC_RECORD != 0
                    && record_kind == Some(RpcRecordKind::CallEnd as u8)
                {
                    conn.terminal_response_failed(hdr.request_id, error);
                } else {
                    conn.response_failed(hdr.request_id, error);
                }
                continue;
            }
            payload.resize(hdr.payload_len(), 0);
            if let Err(error) = reader.read_exact(&mut payload).await {
                conn.end_io("read_payload", &error, true);
                break;
            }
            if hdr.flags & flags::FLAG_RPC_RECORD != 0
                && payload.first() == Some(&(RpcRecordKind::Release as u8))
            {
                let outcome = conn.core.receive_release(&hdr, &payload);
                drop(outcome.reclaims);
                if outcome.error.is_some() {
                    conn.end(ConnectionEndReason::ProtocolError, "release_decode");
                }
                continue;
            }
            if hdr.is_signal() && hdr.flags & flags::FLAG_RPC_RECORD == 0 {
                match payload.as_slice() {
                    [0x02] => {}
                    [0x09] => conn.end(ConnectionEndReason::LocalClose, "disconnect_ack"),
                    _ => conn.end(ConnectionEndReason::ProtocolError, "response_signal"),
                }
                continue;
            }
            let rid = hdr.request_id;
            if hdr.is_buddy() && !shm_allowed {
                conn.end(
                    ConnectionEndReason::ProtocolError,
                    "unnegotiated_shm_response",
                );
            }
            let (kind, historical) = {
                let calls = pending.lock();
                (
                    calls.get(&rid).map(|entry| entry.kind),
                    rid != 0 && rid <= calls.high_submitted,
                )
            };
            if hdr.is_ctrl() && hdr.flags & flags::FLAG_RPC_RECORD == 0 {
                if matches!(kind, Some(PendingKind::Control))
                    && hdr.flags == flags::FLAG_CTRL | flags::FLAG_RESPONSE
                {
                    complete_pending(
                        &pending,
                        rid,
                        Ok(ResponseData::Inline(std::mem::take(&mut payload))),
                    );
                } else if kind.is_some() {
                    conn.response_failed(
                        rid,
                        IpcError::Protocol("unexpected control reply for call".into()),
                    );
                } else if !historical {
                    conn.end(ConnectionEndReason::ProtocolError, "future_control_reply");
                }
                conn.wake.notify_one();
                continue;
            }
            let mode = match kind {
                Some(PendingKind::Call(mode)) => mode,
                _ => CallMode::Unary,
            };
            let record = match rpc::decode_rpc_record_with_carrier(&hdr, &payload, Some(mode)) {
                Ok(record) => record,
                Err(failure) => {
                    if let Some(descriptor) = failure.carrier {
                        discard_response_storage(
                            ValueStorage::Shm(descriptor),
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                    }
                    if failure.error == DecodeError::ZeroLoanId
                        || (hdr.is_buddy() && failure.carrier.is_none())
                    {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "response_loan_admission",
                        );
                    }
                    if kind.is_some() {
                        registry.abort(conn_id, rid);
                        if matches!(kind, Some(PendingKind::Call(CallMode::ServerStream)))
                            && payload.first() == Some(&(RpcRecordKind::CallEnd as u8))
                        {
                            conn.terminal_response_failed(rid, IpcError::Decode(failure.error));
                        } else {
                            conn.response_failed(rid, IpcError::Decode(failure.error));
                        }
                    } else if !historical {
                        conn.end(ConnectionEndReason::ProtocolError, "future_reply");
                    }
                    continue;
                }
            };
            if kind.is_none() && historical {
                match record {
                    RpcRecord::ItemData { value } => {
                        discard_response_storage(
                            value.storage,
                            server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        continue;
                    }
                    RpcRecord::ItemBegin { .. } => continue,
                    _ => {}
                }
            }
            let stream = pending
                .lock()
                .get(&rid)
                .and_then(|entry| entry.stream.clone());
            let forward = pending
                .lock()
                .get(&rid)
                .and_then(|entry| entry.forward.clone());
            if let Some(forward) = forward {
                let result = match record {
                    RpcRecord::ItemBegin { total_bytes } => forward.begin(total_bytes),
                    RpcRecord::ItemData { value } => {
                        forward.data(value.storage, server_pool, shm_allowed, retirement.clone())
                    }
                    RpcRecord::CallEnd {
                        status,
                        complete_items,
                        error,
                        value,
                    } => {
                        discard_response_storage(
                            value.storage,
                            server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        let terminal = stream::StreamTerminal {
                            status,
                            complete_items: complete_items.unwrap_or(0),
                            error,
                        };
                        let entry = pending.lock().remove(&rid).expect("active forwarded reply");
                        forward.finish(terminal, entry);
                        Ok(())
                    }
                    _ => Err(IpcError::Protocol(
                        "unexpected forwarded stream record".into(),
                    )),
                };
                if let Err(error) = result {
                    if matches!(&error, IpcError::Protocol(_)) {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "forward_record_admission",
                        );
                    }
                    forward.fail_response(error);
                }
                conn.wake.notify_one();
                continue;
            }
            if let Some(stream) = stream {
                let result = match record {
                    RpcRecord::ItemBegin { total_bytes } => stream.begin(total_bytes),
                    RpcRecord::ItemData { value } => {
                        stream.data(value.storage, server_pool, shm_allowed, retirement.clone())
                    }
                    RpcRecord::CallEnd {
                        status,
                        complete_items,
                        error,
                        value,
                    } => {
                        discard_response_storage(
                            value.storage,
                            server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        let terminal = stream::StreamTerminal {
                            status,
                            complete_items: complete_items.unwrap_or(0),
                            error,
                        };
                        // The call leaves the table at its decoded terminal.
                        // If its SHM item is still copying, that original
                        // entry moves with the copy and settles afterward.
                        let entry = pending.lock().remove(&rid).expect("active stream reply");
                        stream.finish(terminal, entry);
                        Ok(())
                    }
                    _ => Err(IpcError::Protocol("unexpected server-stream record".into())),
                };
                if let Err(error) = result {
                    if matches!(&error, IpcError::Protocol(_)) {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "stream_record_admission",
                        );
                    }
                    stream.fail_response(error);
                }
                conn.wake.notify_one();
                continue;
            }
            let value = match record {
                RpcRecord::CallEnd {
                    status,
                    value,
                    error,
                    ..
                } => {
                    if kind.is_none() {
                        discard_response_storage(
                            value.storage,
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        if !historical {
                            conn.end(ConnectionEndReason::ProtocolError, "future_reply");
                        }
                        continue;
                    }
                    if pending
                        .lock()
                        .get(&rid)
                        .is_some_and(|entry| entry.reply_chunks.is_some())
                    {
                        discard_response_storage(
                            value.storage,
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        registry.abort(conn_id, rid);
                        conn.response_failed(
                            rid,
                            IpcError::Protocol(
                                "duplicate CALL_END while its value is incomplete".into(),
                            ),
                        );
                        continue;
                    }
                    if !matches!(kind, Some(PendingKind::Call(_))) {
                        discard_response_storage(
                            value.storage,
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        conn.response_failed(
                            rid,
                            IpcError::Protocol("CALL_END for control".into()),
                        );
                        continue;
                    }
                    match status {
                        CallStatus::NotExecuted => {
                            complete_pending(
                                &pending,
                                rid,
                                Err(IpcError::CallRejected(error.expect("checked C2E1 refusal"))),
                            );
                            conn.wake.notify_one();
                            continue;
                        }
                        CallStatus::Failed => {
                            let ValueStorage::Inline(bytes) = value.storage else {
                                unreachable!("checked C2E1 storage")
                            };
                            let len = bytes.len();
                            let bytes = take_inline_response(&mut payload, len);
                            complete_pending(&pending, rid, Err(IpcError::CrmError(bytes)));
                            conn.wake.notify_one();
                            continue;
                        }
                        CallStatus::Success => value,
                    }
                }
                RpcRecord::CallEndContinuation { value } => {
                    if kind.is_none() {
                        discard_response_storage(
                            value.storage,
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                        if !historical {
                            conn.end(ConnectionEndReason::ProtocolError, "future_reply");
                        }
                        continue;
                    }
                    value
                }
                other => {
                    if let RpcRecord::ItemData { value } = other {
                        discard_response_storage(
                            value.storage,
                            &server_pool,
                            shm_allowed,
                            conn,
                            retirement.clone(),
                        );
                    }
                    registry.abort(conn_id, rid);
                    if kind.is_some() {
                        conn.response_failed(
                            rid,
                            IpcError::Protocol("unsupported response record for unary call".into()),
                        );
                    }
                    conn.end(
                        ConnectionEndReason::ProtocolError,
                        "unsupported_response_record",
                    );
                    continue;
                }
            };
            let result = match value.chunk {
                ChunkPosition::Whole => match value.storage {
                    ValueStorage::Inline(bytes) => {
                        let len = bytes.len();
                        Ok(Some(ResponseData::Inline(take_inline_response(
                            &mut payload,
                            len,
                        ))))
                    }
                    storage => admit_response_storage(
                        storage,
                        &server_pool,
                        shm_allowed,
                        conn,
                        retirement.clone(),
                    )
                    .map(Some),
                },
                _ => receive_rpc_chunk(
                    &hdr,
                    value,
                    &registry,
                    conn_id,
                    &pending,
                    &server_pool,
                    shm_allowed,
                    conn,
                    retirement.clone(),
                ),
            };
            match result {
                Ok(Some(response)) => complete_pending(&pending, rid, Ok(response)),
                Ok(None) => {}
                Err(error) => {
                    registry.abort(conn_id, rid);
                    conn.response_failed(rid, error);
                }
            }
            conn.wake.notify_one();
        }
        if malformed {
            // Framing is untrusted: fixed storage, no boundary guessing. Only a
            // read-side peer-end fact can settle remaining published allocations.
            let mut discard = [0u8; 4096];
            loop {
                match reader.read(&mut discard).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => {
                        conn.end_io("drain_read", &error, true);
                        break;
                    }
                }
            }
        }
        registry.cleanup_connection(conn_id);
        conn.read_stopped(reader.peer_ended());
        task.cleanup.finished = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn receive_rpc_chunk(
    hdr: &FrameHeader,
    value: ValuePart<'_>,
    registry: &ChunkRegistry<()>,
    conn_id: u64,
    pending: &StdMutex<PendingMap>,
    pool: &Arc<RwLock<ServerPoolState>>,
    shm_allowed: bool,
    conn: &ClientConnection,
    guard: Arc<PhysicalRetirement>,
) -> Result<Option<ResponseData>, IpcError> {
    let rid = hdr.request_id;
    let carrier = if let ValueStorage::Shm(descriptor) = value.storage {
        Some(admit_response_storage(
            ValueStorage::Shm(descriptor),
            pool,
            shm_allowed,
            conn,
            guard,
        )?)
    } else {
        None
    };
    let bytes = value.storage.len();
    let geometry = if let ChunkPosition::First { total_bytes } = value.chunk {
        let receive =
            ValueReceive::first(total_bytes, bytes).map_err(|e| IpcError::Protocol(e.into()))?;
        let geometry = receive
            .geometry()
            .map_err(|e| IpcError::Protocol(e.into()))?;
        let mut calls = pending.lock();
        let call = calls.get_mut(&rid).ok_or(IpcError::Closed)?;
        if call.reply_chunks.is_some() {
            return Err(IpcError::Protocol("duplicate CALL_END first chunk".into()));
        }
        call.reply_chunks = Some(receive);
        Some((total_bytes, geometry))
    } else {
        None
    };
    let index = {
        let mut calls = pending.lock();
        let receive = calls
            .get_mut(&rid)
            .and_then(|call| call.reply_chunks.as_mut())
            .ok_or_else(|| {
                IpcError::Protocol("CALL_END continuation has no started value".into())
            })?;
        receive
            .admit(bytes)
            .map_err(|e| IpcError::Protocol(e.into()))?
    };
    if let Some((total, (count, size))) = geometry {
        registry
            .insert_reply(conn_id, rid, total, count, size)
            .map_err(|error| IpcError::Chunk(error.into()))?;
    }
    let data = match (value.storage, carrier) {
        (ValueStorage::Inline(bytes), None) => std::borrow::Cow::Borrowed(bytes),
        (_, Some(carrier)) => {
            std::borrow::Cow::Owned(carrier.into_owned_bytes().map_err(IpcError::Shm)?)
        }
        _ => unreachable!("SHM carrier admitted before sequence validation"),
    };
    if !registry
        .feed(conn_id, rid, index, &data)
        .map_err(|e| IpcError::Protocol(e))?
    {
        return Ok(None);
    }
    let (assembler, ()) = registry.take(conn_id, rid).map_err(IpcError::Protocol)?;
    assembler
        .finish()
        .map(ResponseData::Handle)
        .map(Some)
        .map_err(IpcError::Protocol)
}

// The decoder already proved that the whole inline value ends at the frame's
// end. Reuse that admitted frame allocation instead of cloning the full value
// before the relay can reserve its bounded local copying window.
fn take_inline_response(frame: &mut Vec<u8>, len: usize) -> Vec<u8> {
    let start = frame.len() - len;
    let mut bytes = std::mem::take(frame);
    bytes.drain(..start);
    bytes
}

#[cfg(test)]
#[path = "response_frame_tests.rs"]
mod response_frame_tests;

fn complete_pending(
    pending: &StdMutex<PendingMap>,
    rid: u64,
    response: Result<ResponseData, IpcError>,
) {
    let entry = pending.lock().remove(&rid);
    if let Some(entry) = entry {
        entry.complete(response);
    }
    // An unclaimed response drops outside the metadata lock. Its remote-read
    // node queues release without waiting for a pool lock or any network I/O.
}

fn discard_response_storage(
    storage: ValueStorage<'_>,
    pool: &Arc<RwLock<ServerPoolState>>,
    shm_allowed: bool,
    conn: &ClientConnection,
    guard: Arc<PhysicalRetirement>,
) {
    if let ValueStorage::Shm(descriptor) = storage {
        drop(admit_response_storage(
            ValueStorage::Shm(descriptor),
            pool,
            shm_allowed,
            conn,
            guard,
        ));
    }
}

fn admit_response_storage(
    storage: ValueStorage<'_>,
    pool: &Arc<RwLock<ServerPoolState>>,
    shm_allowed: bool,
    conn: &ClientConnection,
    guard: Arc<PhysicalRetirement>,
) -> Result<ResponseData, IpcError> {
    match storage {
        ValueStorage::Inline(bytes) => Ok(ResponseData::Inline(bytes.to_vec())),
        ValueStorage::Shm(descriptor) => {
            if !shm_allowed {
                return Err(IpcError::Protocol(
                    "SHM response direction was not negotiated".into(),
                ));
            }
            let loan = conn
                .core
                .admit_remote_loan(descriptor.loan_id, Box::new((guard, pool.clone())))
                .map_err(|error| {
                    conn.end(
                        ConnectionEndReason::ProtocolError,
                        "response_loan_admission",
                    );
                    core_error(error)
                })?;
            ServerShmResponse::admit(pool.clone(), descriptor, loan)
                .map(ResponseData::Shm)
                .map_err(|error| match error {
                    PeerMappingError::InvalidDescriptor(message) => IpcError::Protocol(message),
                    PeerMappingError::Open(message) => IpcError::Shm(message),
                })
        }
    }
}

#[cfg(test)]
#[path = "p4_tests.rs"]
mod p4_tests;
