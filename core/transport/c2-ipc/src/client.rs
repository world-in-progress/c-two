//! Async IPC client — connects to a C-Two IPC server via UDS.
//!
//! Performs handshake, then multiplexes concurrent requests over
//! a single UDS connection using request IDs.

use parking_lot::{Mutex as StdMutex, RwLock};
use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, oneshot};

use c2_error::ErrorCode;
use c2_mem::FreeResult;
use c2_wire::buddy::{
    BUDDY_PAYLOAD_SIZE, BuddyPayload, decode_buddy_payload, encode_buddy_payload,
};
use c2_wire::chunk::encode_chunk_header;
use c2_wire::chunk::{ChunkConfig, ChunkRegistry};
use c2_wire::control::{
    ReplyControl, RouteCallIdentity, decode_reply_control, encode_call_control,
    encoded_call_control_len,
};
use c2_wire::flags;
use c2_wire::frame::{self, DecodeError, FrameHeader, HEADER_SIZE};
use c2_wire::handshake::{
    CAP_CALL_V2, CAP_CHUNKED, CAP_METHOD_IDX, Handshake, MethodEntry, RouteInfo, decode_handshake,
    encode_client_handshake,
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

use c2_mem::config::PoolConfig;
use c2_mem::{MemPool, PoolAllocation};

use crate::response::ResponseData;

pub use c2_config::ClientIpcConfig;

const ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS: &[u64] = &[10, 25, 50, 100, 200];

// ── Server pool state ────────────────────────────────────────────────────

/// State for reading server's SHM response data.
/// Mirrors `PeerShmState` in c2-server/connection.rs.
pub struct ServerPoolState {
    prefix: String,
    buddy_segment_size: usize,
    pub pool: MemPool,
}

impl ServerPoolState {
    fn buddy_segment_name(prefix: &str, idx: usize) -> String {
        format!("{}_{}{:04x}", prefix, "b", idx)
    }

    fn dedicated_segment_name(prefix: &str, idx: u32) -> String {
        format!("{}_{}{:04x}", prefix, "d", idx)
    }

    /// Ensure the pool has the buddy segment at `seg_idx` open.
    fn ensure_buddy_segment(&mut self, seg_idx: u16) -> Result<(), String> {
        let idx = seg_idx as usize;
        if idx < self.pool.segment_count() {
            return Ok(());
        }
        for i in self.pool.segment_count()..=idx {
            let name = Self::buddy_segment_name(&self.prefix, i);
            self.pool.open_segment(&name, self.buddy_segment_size)?;
        }
        Ok(())
    }

    /// Ensure a dedicated segment is open at the specific index.
    fn ensure_dedicated_segment(&mut self, seg_idx: u16, min_size: usize) -> Result<(), String> {
        let name = Self::dedicated_segment_name(&self.prefix, seg_idx as u32);
        self.pool.open_dedicated_at(seg_idx as u32, &name, min_size)
    }

    /// Lazy-open the segment for the given coordinates if not yet mapped.
    ///
    /// Called transparently by language binding response buffers before any
    /// SHM access. SDKs do not need to know about segment management; this
    /// keeps it entirely inside Rust.
    pub fn ensure_segment(
        &mut self,
        seg_idx: u16,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(), String> {
        if is_dedicated {
            self.ensure_dedicated_segment(seg_idx, data_size as usize)
        } else {
            self.ensure_buddy_segment(seg_idx)
        }
    }

    /// Read data from server SHM and free the allocation.
    pub fn read_and_free(
        &mut self,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(Vec<u8>, FreeResult), String> {
        if is_dedicated {
            self.ensure_dedicated_segment(seg_idx, data_size as usize)?;
        } else {
            self.ensure_buddy_segment(seg_idx)?;
        }

        let ptr = self
            .pool
            .data_ptr_at(seg_idx as u32, offset, is_dedicated)?;
        let data = unsafe { std::slice::from_raw_parts(ptr, data_size as usize) }.to_vec();

        let free_result = match self
            .pool
            .free_at(seg_idx as u32, offset, data_size, is_dedicated)
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Warning: server SHM free_at failed: {e}");
                FreeResult::Normal
            }
        };

        Ok((data, free_result))
    }
}

// ── Error type ───────────────────────────────────────────────────────────

/// IPC client error.
#[derive(Debug)]
pub enum IpcError {
    /// I/O error on the UDS connection.
    Io(std::io::Error),
    /// Invalid client configuration or IPC address.
    Config(String),
    /// Wire protocol decoding error.
    Decode(DecodeError),
    /// Handshake failed or incompatible server.
    Handshake(String),
    /// Peer violated IPC route/control protocol after the transport connected.
    Protocol(String),
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
    RouteRemoved {
        route_name: String,
        route_uid: Option<String>,
    },
    /// Requested route exists but no longer accepts new calls.
    RouteClosed {
        route_name: String,
        route_uid: String,
        reason: String,
    },
    /// Client observed an older route token than the connected server catalog.
    RouteStale {
        route_name: String,
        current_route_uid: String,
        current_route_revision: u64,
    },
    /// Route watch history was compacted and the directory must be rebuilt.
    CatalogCompacted {
        compacted_revision: u64,
        current_revision: u64,
    },
    /// Route watch state is unavailable, so cached route state cannot be trusted.
    WatchUnavailable(String),
    /// Requested method does not exist on the connected route.
    MethodNotFound {
        route_name: String,
        method_name: String,
    },
    /// Shared-memory request/response setup failed.
    Shm(String),
    /// Chunked response assembly failed.
    Chunk(String),
    /// CRM method returned an error (serialized error bytes).
    CrmError(Vec<u8>),
    /// Client is closed or connection lost.
    Closed,
    /// Pool management error.
    Pool(String),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IPC I/O error: {e}"),
            Self::Config(msg) => write!(f, "IPC config error: {msg}"),
            Self::Decode(e) => write!(f, "IPC decode error: {e}"),
            Self::Handshake(msg) => write!(f, "IPC handshake failed: {msg}"),
            Self::Protocol(msg) => write!(f, "IPC protocol violation: {msg}"),
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
            Self::RouteRemoved {
                route_name,
                route_uid,
            } => {
                if let Some(route_uid) = route_uid {
                    write!(f, "IPC route removed: {route_name} uid={route_uid}")
                } else {
                    write!(f, "IPC route removed: {route_name}")
                }
            }
            Self::RouteClosed {
                route_name,
                route_uid,
                reason,
            } => write!(
                f,
                "IPC route closed: {route_name} uid={route_uid} reason={reason}"
            ),
            Self::RouteStale {
                route_name,
                current_route_uid,
                current_route_revision,
            } => write!(
                f,
                "IPC route stale: {route_name} current_uid={current_route_uid} current_revision={current_route_revision}"
            ),
            Self::CatalogCompacted {
                compacted_revision,
                current_revision,
            } => write!(
                f,
                "IPC route catalog compacted: compacted_revision={compacted_revision} current_revision={current_revision}"
            ),
            Self::WatchUnavailable(msg) => write!(f, "IPC route watch unavailable: {msg}"),
            Self::MethodNotFound {
                route_name,
                method_name,
            } => write!(
                f,
                "IPC method not found: route={route_name} method={method_name}"
            ),
            Self::Shm(msg) => write!(f, "IPC SHM error: {msg}"),
            Self::Chunk(msg) => write!(f, "IPC chunk error: {msg}"),
            Self::CrmError(_) => write!(f, "CRM method error"),
            Self::Closed => write!(f, "IPC client closed"),
            Self::Pool(msg) => write!(f, "Pool error: {msg}"),
        }
    }
}

impl std::error::Error for IpcError {}

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

impl From<c2_wire::control::EncodeError> for IpcError {
    fn from(e: c2_wire::control::EncodeError) -> Self {
        Self::Protocol(e.to_string())
    }
}

// ── Method table ─────────────────────────────────────────────────────────

/// Per-route method table (name ↔ index).
#[derive(Debug, Clone)]
pub struct MethodTable {
    route_name: String,
    route_uid: String,
    route_revision: u64,
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
                route_uid: route.route_uid.clone(),
                observed_route_revision: route.route_revision,
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
            route_uid: identity.route_uid,
            route_revision: identity.observed_route_revision,
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

    pub fn route_uid(&self) -> &str {
        &self.route_uid
    }

    pub fn route_revision(&self) -> u64 {
        self.route_revision
    }

    pub(crate) fn call_identity(&self) -> RouteCallIdentity {
        RouteCallIdentity {
            route_name: self.route_name.clone(),
            route_uid: self.route_uid.clone(),
            observed_route_revision: self.route_revision,
            crm_ns: self.crm_ns.clone(),
            crm_name: self.crm_name.clone(),
            crm_ver: self.crm_ver.clone(),
            abi_hash: self.abi_hash.clone(),
            signature_hash: self.signature_hash.clone(),
        }
    }
}

/// Immutable route token acquired by one route-bound client/proxy.
///
/// A binding intentionally keeps the route UID and revision observed at acquire
/// time. Long-lived clients may keep their shared connection directory fresh,
/// but an existing proxy must not silently retarget itself to a newer resource
/// instance with the same route name.
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

    pub fn route_uid(&self) -> &str {
        self.table.route_uid()
    }

    pub fn route_revision(&self) -> u64 {
        self.table.route_revision()
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
    catalog_revision: u64,
    dirty: bool,
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
        self.catalog_revision = 0;
        self.dirty = false;
    }

    fn rebuild_from_list(&mut self, response: RouteListResponse) {
        self.routes.clear();
        for record in response.routes {
            self.apply_record(record);
        }
        self.catalog_revision = response.catalog_revision;
        self.dirty = false;
    }

    fn apply_record(&mut self, record: RouteRecordWire) {
        self.catalog_revision = self.catalog_revision.max(record.catalog_revision);
        match record.state {
            RouteStateWire::Ready => {
                self.routes
                    .insert(record.route_name.clone(), MethodTable::from_record(&record));
            }
            RouteStateWire::Pending
            | RouteStateWire::Draining
            | RouteStateWire::Closed
            | RouteStateWire::Removed => {
                self.routes.remove(&record.route_name);
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn remove_route(&mut self, route_name: &str) {
        self.routes.remove(route_name);
    }

    fn is_dirty(&self) -> bool {
        self.dirty
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
                route_uid: record.route_uid.clone(),
                observed_route_revision: record.route_revision,
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

// ── Pending call ─────────────────────────────────────────────────────────

enum PendingResponse {
    Unary(oneshot::Sender<Result<ResponseData, IpcError>>),
}

type PendingMap = HashMap<u32, PendingResponse>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestTransportKind {
    Inline,
    Buddy,
    Chunked,
}

pub(crate) fn choose_request_transport(
    config: &ClientIpcConfig,
    has_pool: bool,
    data_len: usize,
) -> RequestTransportKind {
    let data_len = u64::try_from(data_len).unwrap_or(u64::MAX);
    if has_pool && data_len > config.shm_threshold && data_len <= u64::from(u32::MAX) {
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
    if total_chunks > usize::from(u16::MAX) {
        return Err(IpcError::Config(format!(
            "request chunk count {total_chunks} exceeds wire limit {}",
            u16::MAX
        )));
    }
    Ok(total_chunks)
}

fn stream_error<E: Display>(err: E) -> IpcError {
    IpcError::Io(std::io::Error::other(format!(
        "request body stream error: {err}"
    )))
}

async fn collect_exact_stream<S, B, E>(data_size: usize, chunks: S) -> Result<Vec<u8>, IpcError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: Display,
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

// ── IpcClient ────────────────────────────────────────────────────────────

/// Async IPC client for the C-Two relay.
///
/// Connects to a C-Two IPC server via Unix Domain Socket, performs
/// handshake, and multiplexes concurrent CRM calls.
pub struct IpcClient {
    socket_path: PathBuf,
    address_error: Option<String>,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    pending: Arc<StdMutex<PendingMap>>,
    rid_counter: Arc<AtomicU32>,
    pub(crate) route_directory: Arc<RwLock<RouteDirectory>>,
    server_segments: Vec<(String, u32)>,
    pub(crate) server_identity: Option<c2_wire::handshake::ServerIdentity>,
    /// Server SHM pool state for reading buddy reply responses.
    pub(crate) server_pool: Arc<StdMutex<Option<ServerPoolState>>>,
    recv_handle: Arc<StdMutex<Option<tokio::task::JoinHandle<()>>>>,
    connected: Arc<AtomicBool>,
    pub(crate) pool: Option<Arc<StdMutex<MemPool>>>,
    pub(crate) config: ClientIpcConfig,
    /// Client-side chunk registry for reassembling chunked responses.
    pub(crate) chunk_registry: Arc<ChunkRegistry>,
    /// Unique connection identifier for the chunk registry.
    conn_id: u64,
}

// Compile-time assertion: IpcClient is Send+Sync because all fields are
// Arc-wrapped (Send+Sync), atomic (Send+Sync), or standard collections
// of Send+Sync types. This is required for safe use from language
// bindings that share clients across threads.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<IpcClient>();
        _assert_sync::<IpcClient>();
    }
};

/// Monotonic counter so each reassembly MemPool gets a unique SHM prefix.
/// Format: `/cc3a{pid:08x}{counter:08x}` — 32-bit range, 27 chars max.
static REASSEMBLY_POOL_GEN: AtomicU64 = AtomicU64::new(0);

/// Monotonic counter so each IpcClient gets a unique conn_id.
static CLIENT_CONN_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLIENT_OWN_POOL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn socket_path_from_address(address: &str) -> (PathBuf, Option<String>) {
    match crate::control::socket_path_from_ipc_address(address) {
        Ok(path) => (path, None),
        Err(error) => (
            PathBuf::from("/tmp/c_two_ipc").join("invalid.sock"),
            Some(error.to_string()),
        ),
    }
}

impl IpcClient {
    fn own_pool_from_config(config: &ClientIpcConfig) -> Option<Arc<StdMutex<MemPool>>> {
        if !config.pool_enabled {
            return None;
        }
        let pool_config = PoolConfig {
            segment_size: config.pool_segment_size as usize,
            max_segments: config.max_pool_segments as usize,
            ..PoolConfig::default()
        };
        let counter = CLIENT_OWN_POOL_COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3d{:08x}{:08x}", std::process::id(), counter);
        Some(Arc::new(StdMutex::new(MemPool::new_with_prefix(
            pool_config,
            prefix,
        ))))
    }

    fn from_parts(
        address: &str,
        pool: Option<Arc<StdMutex<MemPool>>>,
        config: ClientIpcConfig,
    ) -> Self {
        let (socket_path, address_error) = socket_path_from_address(address);

        Self {
            socket_path,
            address_error,
            writer: Arc::new(Mutex::new(None)),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            rid_counter: Arc::new(AtomicU32::new(1)),
            route_directory: Arc::new(RwLock::new(RouteDirectory::new())),
            server_segments: Vec::new(),
            server_identity: None,
            server_pool: Arc::new(StdMutex::new(None)),
            recv_handle: Arc::new(StdMutex::new(None)),
            connected: Arc::new(AtomicBool::new(false)),
            pool,
            chunk_registry: Self::make_chunk_registry(&config),
            conn_id: CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed),
            config,
        }
    }

    fn make_chunk_registry(config: &ClientIpcConfig) -> Arc<ChunkRegistry> {
        let seg_size = config.reassembly_segment_size as usize;
        let max_segs = config.reassembly_max_segments as usize;
        let counter = REASSEMBLY_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3a{:08x}{:08x}", std::process::id(), counter);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: seg_size,
                max_segments: max_segs,
                ..PoolConfig::default()
            },
            prefix,
        )));
        let chunk_config = ChunkConfig::from_base(config);
        Arc::new(ChunkRegistry::new(pool, chunk_config))
    }

    /// Create a new IPC client targeting the given address.
    ///
    /// The address should be like `ipc://name` — the socket path is
    /// derived as `/tmp/c_two_ipc/{name}.sock`.
    pub fn new(address: &str) -> Self {
        Self::from_parts(address, None, ClientIpcConfig::default())
    }

    /// Create a new IPC client with a config-owned SHM pool when enabled.
    ///
    /// This keeps async callers such as the HTTP relay on the canonical
    /// `IpcClient` API while still allowing large request streams to be written
    /// directly into client SHM.
    pub fn with_config(address: &str, config: ClientIpcConfig) -> Self {
        let pool = Self::own_pool_from_config(&config);
        Self::from_parts(address, pool, config)
    }

    /// Create a new IPC client with a buddy pool for SHM transfers.
    ///
    /// The pool is used for outgoing buddy allocations when data exceeds
    /// `config.shm_threshold`.
    pub fn with_pool(address: &str, pool: Arc<StdMutex<MemPool>>, config: ClientIpcConfig) -> Self {
        Self::from_parts(address, Some(pool), config)
    }

    /// Connect and perform handshake.
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        if let Some(error) = self.address_error.clone() {
            return Err(IpcError::Config(error));
        }
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = tokio::io::split(stream);

        // Pre-allocate first SHM segment so handshake announces it.
        if let Some(ref pool_arc) = self.pool {
            let mut pool = pool_arc.lock();
            pool.ensure_ready()
                .map_err(|e| IpcError::Io(std::io::Error::other(e)))?;
        }

        // Perform handshake.
        let hs = self.do_handshake(&mut writer, reader).await?;
        let server_identity = hs
            .server_identity
            .clone()
            .ok_or_else(|| IpcError::Protocol("server handshake missing server identity".into()))?;

        self.route_directory.write().seed_from_handshake(&hs.routes);
        self.server_segments = hs.segments.clone();
        self.server_identity = Some(server_identity);

        // Open server SHM segments into a ServerPoolState for buddy response reads.
        if !hs.segments.is_empty() {
            let buddy_seg_size = hs.segments[0].1 as usize;
            let cfg = c2_mem::config::PoolConfig {
                segment_size: buddy_seg_size,
                min_block_size: 4096,
                max_segments: 16,
                max_dedicated_segments: 8,
                dedicated_crash_timeout_secs: 60.0,
                buddy_idle_decay_secs: 60.0,
                spill_threshold: 1.0,
                spill_dir: std::path::PathBuf::from("/tmp"),
            };
            let mut pool = MemPool::new_with_prefix(cfg, hs.prefix.clone());
            for (name, size) in &hs.segments {
                if let Err(e) = pool.open_segment(name, *size as usize) {
                    eprintln!("Warning: failed to open server SHM segment ({name}): {e}");
                }
            }
            *self.server_pool.lock() = Some(ServerPoolState {
                prefix: hs.prefix.clone(),
                buddy_segment_size: buddy_seg_size,
                pool,
            });
        }

        *self.writer.lock().await = Some(writer);

        self.connected.store(true, Ordering::Release);

        // Spawn the receive loop — replaced below in `do_handshake`.
        // Actually, we need to spawn it with the reader after handshake.
        // The reader was consumed by do_handshake, so we get it back.
        // This is handled inside do_handshake which returns a new reader.

        Ok(())
    }

    async fn do_handshake(
        &self,
        writer: &mut tokio::io::WriteHalf<UnixStream>,
        mut reader: tokio::io::ReadHalf<UnixStream>,
    ) -> Result<Handshake, IpcError> {
        // Build segment list and prefix from pool (if available).
        let (segments, prefix, cap_flags) = if let Some(ref pool_arc) = self.pool {
            let pool = pool_arc.lock();
            let count = pool.segment_count();
            let mut segs = Vec::with_capacity(count);
            for i in 0..count {
                if let (Some(name), Some(seg)) = (pool.segment_name(i), pool.segment(i)) {
                    segs.push((name.to_string(), seg.allocator().data_size() as u32));
                }
            }
            let pfx = pool.prefix().to_string();
            (segs, pfx, CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED)
        } else {
            (vec![], String::new(), CAP_CALL_V2 | CAP_METHOD_IDX)
        };

        let payload = encode_client_handshake(&segments, cap_flags, &prefix)
            .map_err(|e| IpcError::Protocol(e.to_string()))?;
        let frame_bytes = frame::encode_frame(0, flags::FLAG_HANDSHAKE, &payload);
        writer.write_all(&frame_bytes).await?;

        // Read handshake response.
        let mut header_buf = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header_buf).await?;
        let (total_len, body_rest) = frame::decode_total_len(&header_buf)?;
        let (hdr, _hdr_payload) = frame::decode_frame_body(body_rest, total_len)?;

        if !hdr.is_handshake() {
            return Err(IpcError::Protocol(
                "Server response is not a handshake frame".into(),
            ));
        }

        let payload_len = hdr.payload_len();
        let mut payload_buf = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload_buf).await?;
        }

        let hs = decode_handshake(&payload_buf)?;

        if hs.capability_flags & CAP_CALL_V2 == 0 {
            return Err(IpcError::Protocol(
                "Server does not support v2 call frames".into(),
            ));
        }
        if hs.server_identity.is_none() {
            return Err(IpcError::Protocol(
                "server handshake missing server identity".into(),
            ));
        }

        // Spawn recv loop with the reader.
        let pending = self.pending.clone();
        let server_pool = self.server_pool.clone();
        let writer_clone = self.writer.clone();
        let connected = self.connected.clone();
        let chunk_registry = self.chunk_registry.clone();
        let conn_id = self.conn_id;
        let recv_handle = tokio::spawn(async move {
            recv_loop(
                reader,
                pending,
                server_pool,
                writer_clone,
                chunk_registry,
                conn_id,
            )
            .await;
            connected.store(false, Ordering::Release);
        });
        *self.recv_handle.lock() = Some(recv_handle);

        Ok(hs)
    }

    /// Get a reference to the server SHM pool (for materialising SHM responses).
    pub fn server_pool_arc(&self) -> &Arc<StdMutex<Option<ServerPoolState>>> {
        &self.server_pool
    }

    /// Get a reference to the reassembly pool (for materialising Handle responses).
    pub fn reassembly_pool_arc(&self) -> Arc<RwLock<MemPool>> {
        self.chunk_registry.pool().clone()
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

    fn observed_token_for(&self, route_name: &str) -> Option<(String, u64)> {
        self.route_directory
            .read()
            .route_table(route_name)
            .map(|table| (table.route_uid().to_string(), table.route_revision()))
    }

    fn payload_msg_type(payload: &[u8]) -> Option<MsgType> {
        payload.first().and_then(|tag| MsgType::from_byte(*tag))
    }

    fn route_nack_error(payload: &[u8]) -> IpcError {
        match decode_route_nack(payload) {
            Ok(nack) => match ErrorCode::try_from(nack.error.code) {
                Ok(ErrorCode::RouteCatalogCompacted) => IpcError::CatalogCompacted {
                    compacted_revision: nack.rejected_revision,
                    current_revision: nack.rejected_revision,
                },
                Ok(ErrorCode::RouteWatchUnavailable) => {
                    IpcError::WatchUnavailable(nack.error.message)
                }
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

    async fn send_control_unary_raw(
        writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
        pending: Arc<StdMutex<PendingMap>>,
        rid_counter: Arc<AtomicU32>,
        payload: Vec<u8>,
        description: &str,
    ) -> Result<Vec<u8>, IpcError> {
        let rid = rid_counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        let frame = frame::encode_frame(rid as u64, flags::FLAG_CTRL, &payload);
        let send_result: Result<(), IpcError> = async {
            let mut writer_guard = writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;
            writer.write_all(&frame).await?;
            Ok(())
        }
        .await;
        if let Err(err) = send_result {
            pending.lock().remove(&rid);
            return Err(err);
        }

        let response = match rx.await {
            Ok(result) => result?,
            Err(_) => return Err(IpcError::Closed),
        };
        match response {
            ResponseData::Inline(payload) => Ok(payload),
            _ => Err(IpcError::Protocol(format!(
                "{description} returned non-inline response"
            ))),
        }
    }

    async fn list_routes_raw(
        writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
        pending: Arc<StdMutex<PendingMap>>,
        rid_counter: Arc<AtomicU32>,
        selector: RouteSelector,
        min_revision: Option<u64>,
    ) -> Result<RouteListResponse, IpcError> {
        let payload = encode_route_list_request(&RouteListRequest {
            selector,
            min_revision,
        })
        .map_err(IpcError::Protocol)?;
        let payload =
            Self::send_control_unary_raw(writer, pending, rid_counter, payload, "route list")
                .await?;
        if Self::payload_msg_type(&payload) == Some(MsgType::RouteNack) {
            return Err(Self::route_nack_error(&payload));
        }
        decode_route_list_response(&payload).map_err(IpcError::Protocol)
    }

    async fn rebuild_directory_raw(
        writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
        pending: Arc<StdMutex<PendingMap>>,
        rid_counter: Arc<AtomicU32>,
        directory: Arc<RwLock<RouteDirectory>>,
    ) -> Result<(), IpcError> {
        let response =
            Self::list_routes_raw(writer, pending, rid_counter, RouteSelector::All, None).await?;
        directory.write().rebuild_from_list(response);
        Ok(())
    }

    fn bound_route_table(&self, route_name: &str) -> Result<MethodTable, IpcError> {
        self.route_directory
            .read()
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
    ) -> Result<ResponseData, IpcError> {
        let data_len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if data_len > max_payload_size {
            return Err(IpcError::Config(format!(
                "request payload size {data_len} exceeds route '{route_name}' max_payload_size {max_payload_size}"
            )));
        }

        match choose_request_transport(&self.config, self.pool.is_some(), data.len()) {
            RequestTransportKind::Buddy => {
                match self.call_buddy(&identity, method_idx, data).await {
                    Ok(result) => return Ok(result),
                    Err(IpcError::Shm(_)) => {
                        // Pool allocation or SHM setup failed. Fall back through the
                        // non-SHM policy below rather than failing large relay calls
                        // that can still use chunked transfer.
                    }
                    Err(e) => return Err(e),
                }
            }
            RequestTransportKind::Chunked => {
                return self.call_chunked(&identity, method_idx, data).await;
            }
            RequestTransportKind::Inline => {
                return self.call_inline(&identity, method_idx, data).await;
            }
        }

        match choose_request_transport(&self.config, false, data.len()) {
            RequestTransportKind::Chunked => self.call_chunked(&identity, method_idx, data).await,
            RequestTransportKind::Inline | RequestTransportKind::Buddy => {
                self.call_inline(&identity, method_idx, data).await
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
        let (method_idx, identity, max_payload_size) = binding.call_target_for(method_name)?;
        self.call_resolved_target(
            binding.route_name(),
            method_idx,
            identity,
            max_payload_size,
            data,
        )
        .await
    }

    /// Send a CRM call from a known-size body stream through a previously
    /// acquired immutable route binding.
    pub async fn call_bound_sized_stream<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data_len: u64,
        chunks: S,
    ) -> Result<ResponseData, IpcError>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: Display,
    {
        let (method_idx, identity, max_payload_size) = binding.call_target_for(method_name)?;
        if data_len > max_payload_size {
            return Err(IpcError::Config(format!(
                "request payload size {data_len} exceeds route '{}' max_payload_size {max_payload_size}",
                binding.route_name()
            )));
        }
        self.call_sized_stream_resolved_target(method_idx, identity, data_len, chunks)
            .await
    }

    async fn call_sized_stream_resolved_target<S, B, E>(
        &self,
        method_idx: u16,
        identity: RouteCallIdentity,
        data_len: u64,
        chunks: S,
    ) -> Result<ResponseData, IpcError>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: Display,
    {
        let data_len = checked_payload_len_usize(data_len)?;
        if data_len == 0 {
            return self.call_inline(&identity, method_idx, &[]).await;
        }

        match choose_request_transport(&self.config, self.pool.is_some(), data_len) {
            RequestTransportKind::Buddy => {
                if let Some(alloc) = self.try_alloc_request_block(data_len)? {
                    return self
                        .call_buddy_stream(&identity, method_idx, alloc, data_len, chunks)
                        .await;
                }
                match choose_request_transport(&self.config, false, data_len) {
                    RequestTransportKind::Chunked => {
                        self.call_chunked_stream(&identity, method_idx, data_len, chunks)
                            .await
                    }
                    RequestTransportKind::Inline | RequestTransportKind::Buddy => {
                        let data = collect_exact_stream(data_len, chunks).await?;
                        self.call_inline(&identity, method_idx, &data).await
                    }
                }
            }
            RequestTransportKind::Chunked => {
                self.call_chunked_stream(&identity, method_idx, data_len, chunks)
                    .await
            }
            RequestTransportKind::Inline => {
                let data = collect_exact_stream(data_len, chunks).await?;
                self.call_inline(&identity, method_idx, &data).await
            }
        }
    }

    /// Inline call path — sends call control + data in a single frame.
    async fn call_inline(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        // Build and send the frame.
        let ctrl_len = encoded_call_control_len(identity)?;
        let payload_len = ctrl_len + data.len();
        let total_len = (12 + payload_len) as u32;
        let frame_size = frame::HEADER_SIZE + payload_len;

        let send_result: Result<(), IpcError> = async {
            let mut writer_guard = self.writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;

            if frame_size <= 1024 {
                // Stack-allocate the entire frame (zero heap, single syscall).
                let mut buf = [0u8; 1024];
                buf[0..4].copy_from_slice(&total_len.to_le_bytes());
                buf[4..12].copy_from_slice(&(rid as u64).to_le_bytes());
                buf[12..16].copy_from_slice(&flags::FLAG_CALL_V2.to_le_bytes());
                let ctrl_written = c2_wire::control::encode_call_control_into(
                    &mut buf,
                    frame::HEADER_SIZE,
                    identity,
                    method_idx,
                )?;
                let data_off = frame::HEADER_SIZE + ctrl_written;
                buf[data_off..data_off + data.len()].copy_from_slice(data);
                writer.write_all(&buf[..frame_size]).await?;
            } else {
                // Large payload: header+ctrl on stack, data separate write.
                let mut hdr_buf = [0u8; frame::HEADER_SIZE];
                hdr_buf[0..4].copy_from_slice(&total_len.to_le_bytes());
                hdr_buf[4..12].copy_from_slice(&(rid as u64).to_le_bytes());
                hdr_buf[12..16].copy_from_slice(&flags::FLAG_CALL_V2.to_le_bytes());
                let ctrl = encode_call_control(identity, method_idx)?;
                writer.write_all(&hdr_buf).await?;
                writer.write_all(&ctrl).await?;
                writer.write_all(data).await?;
            }
            Ok(())
        }
        .await;

        if let Err(e) = send_result {
            self.pending.lock().remove(&rid);
            return Err(e);
        }

        // Await response.
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Closed),
        }
    }

    /// Buddy SHM call path — allocates from MemPool and sends buddy frame.
    ///
    /// The server reads data from SHM and frees the allocation.
    async fn call_buddy(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        if data.len() > u32::MAX as usize {
            return Err(IpcError::Config(format!(
                "buddy request payload size {} exceeds wire limit {}",
                data.len(),
                u32::MAX
            )));
        }
        let pool_arc = self.pool.as_ref().unwrap();

        // Allocate and write data to SHM.
        let alloc = {
            let mut pool = pool_arc.lock();
            pool.alloc(data.len())
                .map_err(|e| IpcError::Shm(format!("buddy alloc failed: {e}")))?
        };

        // Write data into the SHM region.
        {
            let pool = pool_arc.lock();
            let ptr = match pool.data_ptr(&alloc) {
                Ok(p) => p,
                Err(e) => {
                    drop(pool);
                    let _ = pool_arc.lock().free(&alloc);
                    return Err(IpcError::Shm(format!("buddy data_ptr failed: {e}")));
                }
            };
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            }
        }

        // Build buddy payload.
        let bp = BuddyPayload {
            seg_idx: alloc.seg_idx as u16,
            offset: alloc.offset,
            data_size: data.len() as u32,
            is_dedicated: alloc.is_dedicated,
        };
        let buddy_bytes = encode_buddy_payload(&bp);

        // Build call control.
        let ctrl = encode_call_control(identity, method_idx)?;

        // Assemble frame payload: [11B buddy][call_control]
        let payload_len = buddy_bytes.len() + ctrl.len();
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        // Send frame — free buddy allocation if send fails.
        let frame_flags = flags::FLAG_CALL_V2 | flags::FLAG_BUDDY;
        let frame_bytes = frame::encode_frame(rid as u64, frame_flags, &payload);
        {
            let mut writer_guard = self.writer.lock().await;
            let writer = match writer_guard.as_mut() {
                Some(w) => w,
                None => {
                    let _ = pool_arc.lock().free(&alloc);
                    self.pending.lock().remove(&rid);
                    return Err(IpcError::Closed);
                }
            };
            if let Err(e) = writer.write_all(&frame_bytes).await {
                let _ = pool_arc.lock().free(&alloc);
                self.pending.lock().remove(&rid);
                return Err(e.into());
            }
        }

        // Await response (server frees the buddy allocation after reading).
        match rx.await {
            Ok(result) => {
                // Free dedicated request allocation — server has read the data
                // and only its local peer pool was freed.  Buddy allocs are
                // freed by the server via cross-process SHM atomics; freeing
                // them again here would corrupt the allocator.
                if alloc.is_dedicated {
                    let mut pool = pool_arc.lock();
                    let _ = pool.free(&alloc);
                }
                result
            }
            Err(_) => Err(IpcError::Closed),
        }
    }

    fn try_alloc_request_block(
        &self,
        data_size: usize,
    ) -> Result<Option<PoolAllocation>, IpcError> {
        if data_size > u32::MAX as usize {
            return Ok(None);
        }
        let Some(pool_arc) = self.pool.as_ref() else {
            return Ok(None);
        };
        let alloc = pool_arc.lock().alloc(data_size).ok();
        Ok(alloc)
    }

    fn free_request_block(&self, alloc: &PoolAllocation) {
        if let Some(pool_arc) = self.pool.as_ref() {
            let mut pool = pool_arc.lock();
            let _ = pool.free(alloc);
        }
    }

    async fn call_buddy_stream<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        alloc: PoolAllocation,
        data_size: usize,
        chunks: S,
    ) -> Result<ResponseData, IpcError>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: Display,
    {
        let Some(pool_arc) = self.pool.as_ref() else {
            self.free_request_block(&alloc);
            return Err(IpcError::Pool("no client pool".into()));
        };

        let mut written = 0usize;
        futures_util::pin_mut!(chunks);
        while let Some(next) = chunks.next().await {
            let chunk = match next {
                Ok(chunk) => chunk,
                Err(err) => {
                    self.free_request_block(&alloc);
                    return Err(stream_error(err));
                }
            };
            let data = chunk.as_ref();
            if data.is_empty() {
                continue;
            }
            let Some(next_written) = written.checked_add(data.len()) else {
                self.free_request_block(&alloc);
                return Err(IpcError::Config(
                    "request body size overflow while streaming to SHM".into(),
                ));
            };
            if next_written > data_size {
                self.free_request_block(&alloc);
                return Err(IpcError::Config(format!(
                    "request body exceeded declared content length {data_size}"
                )));
            }
            {
                let pool = pool_arc.lock();
                let ptr = match pool.data_ptr(&alloc) {
                    Ok(ptr) => ptr,
                    Err(err) => {
                        drop(pool);
                        self.free_request_block(&alloc);
                        return Err(IpcError::Shm(format!("buddy data_ptr failed: {err}")));
                    }
                };
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.add(written), data.len());
                }
            }
            written = next_written;
        }

        if written != data_size {
            self.free_request_block(&alloc);
            return Err(IpcError::Config(format!(
                "request body ended at {written} bytes, expected {data_size}"
            )));
        }

        self.call_with_prealloc(identity, method_idx, &alloc, data_size)
            .await
    }

    /// Buddy SHM call path with pre-allocated data — sends buddy frame for
    /// data that was already written to the client's SHM pool.
    ///
    /// Unlike `call_buddy()`, this does NOT alloc or write — the caller
    /// already did that. On send failure, frees the allocation from the pool.
    pub(crate) async fn call_with_prealloc(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        alloc: &PoolAllocation,
        data_size: usize,
    ) -> Result<ResponseData, IpcError> {
        if data_size > u32::MAX as usize {
            self.free_prealloc(alloc);
            return Err(IpcError::Config(format!(
                "buddy request payload size {data_size} exceeds wire limit {}",
                u32::MAX
            )));
        }
        // Build buddy payload from pre-allocated coordinates.
        let bp = BuddyPayload {
            seg_idx: alloc.seg_idx as u16,
            offset: alloc.offset,
            data_size: data_size as u32,
            is_dedicated: alloc.is_dedicated,
        };
        let buddy_bytes = encode_buddy_payload(&bp);

        // Build call control.
        let ctrl = match encode_call_control(identity, method_idx) {
            Ok(ctrl) => ctrl,
            Err(err) => {
                self.free_prealloc(alloc);
                return Err(err.into());
            }
        };

        // Assemble frame payload: [11B buddy][call_control]
        let payload_len = buddy_bytes.len() + ctrl.len();
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        // Send buddy frame.
        let flags = flags::FLAG_CALL_V2 | flags::FLAG_BUDDY;
        let frame = frame::encode_frame(rid as u64, flags, &payload);

        let send_result: Result<(), IpcError> = async {
            let mut writer_guard = self.writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;
            writer.write_all(&frame).await?;
            Ok(())
        }
        .await;

        if let Err(e) = send_result {
            // Send failed — server never saw the allocation. Free it.
            // This matches call_buddy() behavior.
            self.free_prealloc(alloc);
            self.pending.lock().remove(&rid);
            return Err(e);
        }

        // Await response — server already consumed the SHM allocation.
        // For dedicated segments, the server only freed its local peer-pool
        // copy; the client must also free its own allocation so the dedicated
        // segment can be GC'd and the slot reused.
        match rx.await {
            Ok(result) => {
                if alloc.is_dedicated {
                    self.free_prealloc(alloc);
                }
                result
            }
            Err(_) => Err(IpcError::Closed),
        }
    }

    pub(crate) fn free_prealloc(&self, alloc: &PoolAllocation) {
        if let Some(ref pool_arc) = self.pool {
            let mut pool = pool_arc.lock();
            let _ = pool.free(alloc);
        }
    }

    /// Chunked call path — splits data into chunks and sends with FLAG_CHUNKED.
    async fn call_chunked(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let chunk_size = self.config.chunk_size as usize;
        let total_chunks = request_chunk_count(data.len(), chunk_size)?;

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call ONCE — reply comes after last chunk.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        // Build call control (included only in chunk 0).
        let ctrl = encode_call_control(identity, method_idx)?;

        let send_result: Result<(), IpcError> = async {
            let mut writer_guard = self.writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;

            for i in 0..total_chunks {
                let chunk_start = i * chunk_size;
                let chunk_end = std::cmp::min(chunk_start + chunk_size, data.len());
                let chunk_data = &data[chunk_start..chunk_end];

                let is_last = i == total_chunks - 1;
                let mut frame_flags = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED;
                if is_last {
                    frame_flags |= flags::FLAG_CHUNK_LAST;
                }

                let chunk_hdr = encode_chunk_header(i as u16, total_chunks as u16);

                // Chunk 0 includes call_control; subsequent chunks are data-only.
                let payload_len =
                    chunk_hdr.len() + if i == 0 { ctrl.len() } else { 0 } + chunk_data.len();
                let mut payload = Vec::with_capacity(payload_len);
                payload.extend_from_slice(&chunk_hdr);
                if i == 0 {
                    payload.extend_from_slice(&ctrl);
                }
                payload.extend_from_slice(chunk_data);

                let frame_bytes = frame::encode_frame(rid as u64, frame_flags, &payload);
                writer.write_all(&frame_bytes).await?;
            }
            Ok(())
        }
        .await;

        if let Err(e) = send_result {
            self.pending.lock().remove(&rid);
            return Err(e);
        }

        // Await response.
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Closed),
        }
    }

    async fn call_chunked_stream<S, B, E>(
        &self,
        identity: &RouteCallIdentity,
        method_idx: u16,
        data_size: usize,
        chunks: S,
    ) -> Result<ResponseData, IpcError>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: Display,
    {
        let chunk_size = self.config.chunk_size as usize;
        let total_chunks = request_chunk_count(data_size, chunk_size)?;
        if total_chunks == 0 {
            return self.call_inline(identity, method_idx, &[]).await;
        }

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().insert(rid, PendingResponse::Unary(tx));
        }

        let ctrl = encode_call_control(identity, method_idx)?;
        let send_result = self
            .send_chunked_stream_frames(rid, total_chunks, chunk_size, data_size, &ctrl, chunks)
            .await;
        if let Err(err) = send_result {
            self.pending.lock().remove(&rid);
            return Err(err);
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Closed),
        }
    }

    async fn send_chunked_stream_frames<S, B, E>(
        &self,
        rid: u32,
        total_chunks: usize,
        chunk_size: usize,
        data_size: usize,
        ctrl: &[u8],
        chunks: S,
    ) -> Result<(), IpcError>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: Display,
    {
        let mut written = 0usize;
        let mut chunk_idx = 0usize;
        let mut sent_or_attempted = false;
        let mut pending_chunk = Vec::with_capacity(chunk_size.min(data_size));
        futures_util::pin_mut!(chunks);

        while let Some(next) = chunks.next().await {
            let chunk = match next {
                Ok(chunk) => chunk,
                Err(err) => {
                    if sent_or_attempted {
                        self.close_shared().await;
                    }
                    return Err(stream_error(err));
                }
            };
            let mut data = chunk.as_ref();
            if data.is_empty() {
                continue;
            }
            let Some(next_written) = written.checked_add(data.len()) else {
                if sent_or_attempted {
                    self.close_shared().await;
                }
                return Err(IpcError::Config(
                    "request body size overflow while streaming chunks".into(),
                ));
            };
            if next_written > data_size {
                if sent_or_attempted {
                    self.close_shared().await;
                }
                return Err(IpcError::Config(format!(
                    "request body exceeded declared content length {data_size}"
                )));
            }

            while !data.is_empty() {
                let remaining = chunk_size - pending_chunk.len();
                let take = remaining.min(data.len());
                pending_chunk.extend_from_slice(&data[..take]);
                data = &data[take..];

                if pending_chunk.len() == chunk_size {
                    let is_last = chunk_idx + 1 == total_chunks;
                    sent_or_attempted = true;
                    if let Err(err) = self
                        .write_chunk_frame(
                            rid,
                            chunk_idx,
                            total_chunks,
                            if chunk_idx == 0 { Some(ctrl) } else { None },
                            &pending_chunk,
                            is_last,
                        )
                        .await
                    {
                        self.close_shared().await;
                        return Err(err);
                    }
                    pending_chunk.clear();
                    chunk_idx += 1;
                }
            }

            written = next_written;
        }

        if written != data_size {
            if sent_or_attempted {
                self.close_shared().await;
            }
            return Err(IpcError::Config(format!(
                "request body ended at {written} bytes, expected {data_size}"
            )));
        }

        if !pending_chunk.is_empty() {
            let is_last = chunk_idx + 1 == total_chunks;
            sent_or_attempted = true;
            if let Err(err) = self
                .write_chunk_frame(
                    rid,
                    chunk_idx,
                    total_chunks,
                    if chunk_idx == 0 { Some(ctrl) } else { None },
                    &pending_chunk,
                    is_last,
                )
                .await
            {
                self.close_shared().await;
                return Err(err);
            }
            chunk_idx += 1;
        }

        if chunk_idx != total_chunks {
            if sent_or_attempted {
                self.close_shared().await;
            }
            return Err(IpcError::Config(format!(
                "request stream emitted {chunk_idx} chunks, expected {total_chunks}"
            )));
        }
        Ok(())
    }

    async fn write_chunk_frame(
        &self,
        rid: u32,
        chunk_idx: usize,
        total_chunks: usize,
        ctrl: Option<&[u8]>,
        chunk_data: &[u8],
        is_last: bool,
    ) -> Result<(), IpcError> {
        let mut frame_flags = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED;
        if is_last {
            frame_flags |= flags::FLAG_CHUNK_LAST;
        }
        let chunk_hdr = encode_chunk_header(chunk_idx as u16, total_chunks as u16);
        let payload_len = chunk_hdr.len() + ctrl.map_or(0, <[u8]>::len) + chunk_data.len();
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&chunk_hdr);
        if let Some(ctrl) = ctrl {
            payload.extend_from_slice(ctrl);
        }
        payload.extend_from_slice(chunk_data);

        let frame_bytes = frame::encode_frame(rid as u64, frame_flags, &payload);
        let mut writer_guard = self.writer.lock().await;
        let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;
        writer.write_all(&frame_bytes).await?;
        Ok(())
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

    fn method_table_from_attestation(contract: &PendingRouteAttestation) -> MethodTable {
        let method_entries = contract
            .method_names
            .iter()
            .enumerate()
            .map(|(index, name)| MethodEntry {
                name: name.clone(),
                index: index as u16,
            })
            .collect::<Vec<_>>();
        MethodTable::from_entries(
            &method_entries,
            RouteCallIdentity {
                route_name: contract.route_name.clone(),
                route_uid: contract.route_uid.clone(),
                observed_route_revision: contract.route_revision,
                crm_ns: contract.crm_ns.clone(),
                crm_name: contract.crm_name.clone(),
                crm_ver: contract.crm_ver.clone(),
                abi_hash: contract.abi_hash.clone(),
                signature_hash: contract.signature_hash.clone(),
            },
            contract.max_payload_size,
        )
    }

    fn expected_contract_from_attestation(
        contract: PendingRouteAttestation,
    ) -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: contract.route_name,
            crm_ns: contract.crm_ns,
            crm_name: contract.crm_name,
            crm_ver: contract.crm_ver,
            abi_hash: contract.abi_hash,
            signature_hash: contract.signature_hash,
        }
    }

    fn cache_attested_contract(&self, contract: &PendingRouteAttestation) {
        self.route_directory.write().insert_table(
            contract.route_name.clone(),
            Self::method_table_from_attestation(contract),
        );
    }

    async fn send_control_inline(
        &self,
        payload: Vec<u8>,
        description: &str,
    ) -> Result<Vec<u8>, IpcError> {
        Self::send_control_unary_raw(
            Arc::clone(&self.writer),
            Arc::clone(&self.pending),
            Arc::clone(&self.rid_counter),
            payload,
            description,
        )
        .await
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
        let table = self
            .route_directory
            .read()
            .route_table(&expected.route_name)
            .ok_or_else(|| IpcError::RouteNotFound(expected.route_name.clone()))?;
        Self::validate_method_table_contract(&expected.route_name, &table, expected)
    }

    async fn rebuild_route_directory(&self) -> Result<(), IpcError> {
        Self::rebuild_directory_raw(
            Arc::clone(&self.writer),
            Arc::clone(&self.pending),
            Arc::clone(&self.rid_counter),
            Arc::clone(&self.route_directory),
        )
        .await
    }

    async fn lookup_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        let observed = self.observed_token_for(&expected.route_name);
        let (observed_route_uid, observed_route_revision) = match observed {
            Some((uid, revision)) => (Some(uid), Some(revision)),
            None => (None, None),
        };
        let request = RouteLookupRequest {
            expected: Self::expected_contract_wire(expected),
            observed_route_uid,
            observed_route_revision,
        };
        let payload = encode_route_lookup_request(&request).map_err(IpcError::Protocol)?;
        let payload = self.send_control_inline(payload, "route lookup").await?;
        if Self::payload_msg_type(&payload) == Some(MsgType::RouteNack) {
            self.route_directory.write().mark_dirty();
            return Err(Self::route_nack_error(&payload));
        }
        let response = decode_route_lookup_response(&payload).map_err(IpcError::Protocol)?;
        match response {
            RouteLookupResponse::Ready { current } | RouteLookupResponse::Stale { current } => {
                self.route_directory.write().apply_record(current);
                self.validate_route_contract(expected)
            }
            RouteLookupResponse::NotFound { route_name } => {
                self.route_directory.write().remove_route(&route_name);
                Err(IpcError::RouteNotFound(route_name))
            }
            RouteLookupResponse::Removed {
                route_name,
                route_uid,
            } => {
                self.route_directory.write().remove_route(&route_name);
                Err(IpcError::RouteRemoved {
                    route_name,
                    route_uid,
                })
            }
            RouteLookupResponse::Closed {
                route_name,
                route_uid,
                reason,
            } => {
                self.route_directory.write().remove_route(&route_name);
                Err(IpcError::RouteClosed {
                    route_name,
                    route_uid,
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
                self.route_directory.write().apply_record(current);
                Err(IpcError::ContractMismatch(message))
            }
        }
    }

    async fn lookup_route_contract_for_acquire(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        let attempts = ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS.len() + 1;
        let mut last_not_found = None;

        for attempt in 0..attempts {
            if attempt > 0 {
                let delay_ms = ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS[attempt - 1];
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            let result = match self.lookup_route_contract(expected).await {
                Err(IpcError::CatalogCompacted { .. } | IpcError::WatchUnavailable(_)) => {
                    self.rebuild_route_directory().await?;
                    self.lookup_route_contract(expected).await
                }
                other => other,
            };

            match result {
                Err(IpcError::RouteNotFound(route_name)) if attempt + 1 < attempts => {
                    last_not_found = Some(route_name);
                }
                Err(IpcError::RouteNotFound(route_name)) => {
                    return Err(IpcError::RouteNotFound(route_name));
                }
                other => return other,
            }
        }

        Err(IpcError::RouteNotFound(
            last_not_found.unwrap_or_else(|| expected.route_name.clone()),
        ))
    }

    /// Ensure the connected server currently exports a route matching the
    /// expected CRM contract, using the route catalog instead of the handshake
    /// snapshot as the authoritative source.
    pub async fn ensure_route_contract(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<(), IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        {
            let directory = self.route_directory.read();
            if !directory.is_dirty()
                && let Some(table) = directory.route_table(&expected.route_name)
                && Self::validate_method_table_contract(&expected.route_name, &table, expected)
                    .is_ok()
            {
                return Ok(());
            }
        }

        if self.route_directory.read().is_dirty() {
            self.rebuild_route_directory().await?;
            {
                let directory = self.route_directory.read();
                if let Some(table) = directory.route_table(&expected.route_name)
                    && Self::validate_method_table_contract(&expected.route_name, &table, expected)
                        .is_ok()
                {
                    return Ok(());
                }
            }
        }

        match self.lookup_route_contract(expected).await {
            Err(IpcError::CatalogCompacted { .. } | IpcError::WatchUnavailable(_)) => {
                self.rebuild_route_directory().await?;
                self.lookup_route_contract(expected).await
            }
            other => other,
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
    pub async fn acquire_route(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
    ) -> Result<RouteBinding, IpcError> {
        let mut last_unbound = None;

        for delay_ms in ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
        {
            self.lookup_route_contract_for_acquire(expected).await?;
            match self.bind_cached_route(expected) {
                Ok(binding) => return Ok(binding),
                Err(IpcError::RouteNotFound(route_name)) if delay_ms.is_some() => {
                    last_unbound = Some(IpcError::RouteNotFound(route_name));
                }
                Err(IpcError::WatchUnavailable(reason)) if delay_ms.is_some() => {
                    last_unbound = Some(IpcError::WatchUnavailable(reason));
                }
                Err(err) => return Err(err),
            }
            if let Some(delay_ms) = delay_ms {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        Err(last_unbound.unwrap_or_else(|| IpcError::RouteNotFound(expected.route_name.clone())))
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
    ) -> Result<RouteBinding, IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        let response = Self::list_routes_raw(
            Arc::clone(&self.writer),
            Arc::clone(&self.pending),
            Arc::clone(&self.rid_counter),
            RouteSelector::RouteName {
                route_name: expected.route_name.clone(),
            },
            None,
        )
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
            RouteStateWire::Ready => Ok(RouteBinding::from_table(table)),
            RouteStateWire::Closed
                if record.state_reason == Some(RouteStateReasonWire::RegisterCommitted) =>
            {
                Ok(RouteBinding::from_table(table))
            }
            RouteStateWire::Removed => Err(IpcError::RouteRemoved {
                route_name: record.route_name,
                route_uid: Some(record.route_uid),
            }),
            state => Err(IpcError::RouteClosed {
                route_name: record.route_name,
                route_uid: record.route_uid,
                reason: format!(
                    "{state:?}:{:?}",
                    record
                        .state_reason
                        .unwrap_or(RouteStateReasonWire::ProtocolViolation)
                ),
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

    /// Bind the cached route only if it still carries the exact route UID and
    /// revision observed by the caller.
    pub(crate) fn bind_cached_route_token(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        route_uid: &str,
        route_revision: u64,
    ) -> Result<RouteBinding, IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        let table = self.bound_route_table(&expected.route_name)?;
        Self::validate_method_table_contract(&expected.route_name, &table, expected)?;
        if table.route_uid() != route_uid || table.route_revision() != route_revision {
            return Err(IpcError::RouteStale {
                route_name: expected.route_name.clone(),
                current_route_uid: table.route_uid().to_string(),
                current_route_revision: table.route_revision(),
            });
        }
        Ok(RouteBinding::from_table(table))
    }

    /// Acquire a route token, refreshing the server route catalog before deciding
    /// that a cached missing or mismatched token is authoritative.
    pub async fn acquire_route_token(
        &self,
        expected: &c2_contract::ExpectedRouteContract,
        route_uid: &str,
        route_revision: u64,
    ) -> Result<RouteBinding, IpcError> {
        c2_contract::validate_expected_route_contract(expected)
            .map_err(|err| IpcError::ContractMismatch(err.to_string()))?;
        if let Ok(binding) = self.bind_cached_route_token(expected, route_uid, route_revision) {
            return Ok(binding);
        }

        if self.route_directory.read().is_dirty() {
            self.rebuild_route_directory().await?;
            if let Ok(binding) = self.bind_cached_route_token(expected, route_uid, route_revision) {
                return Ok(binding);
            }
        }

        let mut last_unbound = None;

        for delay_ms in ROUTE_PUBLICATION_LOOKUP_RETRY_DELAYS_MS
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
        {
            self.lookup_route_contract_for_acquire(expected).await?;
            match self.bind_cached_route_token(expected, route_uid, route_revision) {
                Ok(binding) => return Ok(binding),
                Err(IpcError::RouteNotFound(route_name)) if delay_ms.is_some() => {
                    last_unbound = Some(IpcError::RouteNotFound(route_name));
                }
                Err(IpcError::WatchUnavailable(reason)) if delay_ms.is_some() => {
                    last_unbound = Some(IpcError::WatchUnavailable(reason));
                }
                Err(err) => return Err(err),
            }
            if let Some(delay_ms) = delay_ms {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        Err(last_unbound.unwrap_or_else(|| IpcError::RouteNotFound(expected.route_name.clone())))
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
    /// acquire it. The pending attestation response is cached and immediately
    /// bound to a route token for relay registration proof.
    pub async fn acquire_pending_route_attestation(
        &mut self,
        route_name: &str,
        registration_token: &str,
    ) -> Result<(c2_contract::ExpectedRouteContract, RouteBinding), IpcError> {
        let payload = encode_pending_route_attestation_request(route_name, registration_token)
            .map_err(IpcError::Protocol)?;
        let payload = self
            .send_control_inline(payload, "pending route attestation")
            .await?;
        match decode_pending_route_attestation_response(&payload).map_err(IpcError::Protocol)? {
            PendingRouteAttestationResponse::Attested { contract } => {
                self.cache_attested_contract(&contract);
                let expected = Self::expected_contract_from_attestation(contract);
                let binding = self.bind_cached_route(&expected)?;
                Ok((expected, binding))
            }
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

    /// Whether the client has an active connection.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Manually override the connection flag.
    ///
    /// Intended for test scenarios where a real handshake is not
    /// performed.  Production code should rely on [`connect`] / [`close`].
    pub fn force_connected(&self, val: bool) {
        self.connected.store(val, Ordering::Release);
    }

    /// Close the client.
    pub async fn close(&mut self) {
        self.close_shared().await;
    }

    /// Close the client through shared ownership.
    ///
    /// This is intentionally best-effort: it marks the connection closed,
    /// sends a disconnect signal when possible, drops the writer, and wakes
    /// pending callers. It is used by owners that hold an `Arc<IpcClient>` and
    /// cannot prove unique ownership at shutdown time.
    pub async fn close_shared(&self) {
        self.connected.store(false, Ordering::Release);
        // Best-effort: send DISCONNECT signal so the server can clean up
        // immediately instead of waiting for heartbeat timeout.
        {
            let mut guard = self.writer.lock().await;
            if let Some(w) = guard.as_mut() {
                let disconnect_frame =
                    frame::encode_frame(0, flags::FLAG_SIGNAL, &[SIG_DISCONNECT]);
                let _ = w.write_all(&disconnect_frame).await;
            }
        }
        // Brief grace period for the server to reply DISCONNECT_ACK.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Drop writer to close the write half.
        *self.writer.lock().await = None;
        // Abort recv task in case the peer does not close promptly.
        if let Some(handle) = self.recv_handle.lock().take() {
            handle.abort();
        }
        // Wake pending callers.
        let mut pending = self.pending.lock();
        for (_, pending) in pending.drain() {
            let PendingResponse::Unary(tx) = pending;
            let _ = tx.send(Err(IpcError::Closed));
        }
    }
}

// ── Recv loop ────────────────────────────────────────────────────────────

/// Signal byte constants from the canonical wire protocol.
const SIG_PING: u8 = 0x01;
const SIG_PONG: u8 = 0x02;
const SIG_DISCONNECT: u8 = 0x08;
const SIG_DISCONNECT_ACK: u8 = 0x09;

fn complete_unary_pending(
    pending: Option<PendingResponse>,
    result: Result<ResponseData, IpcError>,
) {
    if let Some(PendingResponse::Unary(tx)) = pending {
        let _ = tx.send(result);
    }
}

async fn recv_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    pending: Arc<StdMutex<PendingMap>>,
    _server_pool: Arc<StdMutex<Option<ServerPoolState>>>,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    chunk_registry: Arc<ChunkRegistry>,
    conn_id: u64,
) {
    let mut header_buf = [0u8; HEADER_SIZE];
    let mut recv_buf = Vec::with_capacity(4096); // reusable buffer
    loop {
        // Read frame header.
        if reader.read_exact(&mut header_buf).await.is_err() {
            break; // Connection closed.
        }
        let (total_len, body_rest) = match frame::decode_total_len(&header_buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (hdr, _) = match frame::decode_frame_body(body_rest, total_len) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let payload_len = hdr.payload_len();
        // Reuse recv_buf: resize without shrinking allocation.
        recv_buf.clear();
        if payload_len > recv_buf.capacity() {
            recv_buf.reserve(payload_len - recv_buf.capacity());
        }
        recv_buf.resize(payload_len, 0);
        if payload_len > 0 && reader.read_exact(&mut recv_buf).await.is_err() {
            break;
        }

        // Handle signal frames.
        if hdr.is_signal() {
            if recv_buf.len() == 1 {
                match recv_buf[0] {
                    SIG_PING => {
                        let pong = frame::encode_frame(
                            hdr.request_id,
                            flags::FLAG_RESPONSE | flags::FLAG_SIGNAL,
                            &[SIG_PONG],
                        );
                        let mut guard = writer.lock().await;
                        if let Some(w) = guard.as_mut() {
                            let _ = w.write_all(&pong).await;
                        }
                    }
                    SIG_DISCONNECT_ACK => {
                        break; // Server acknowledged disconnect — exit cleanly.
                    }
                    _ => {} // Ignore unknown signals.
                }
            }
            continue; // Don't dispatch signals to pending callers.
        }

        let rid = hdr.request_id as u32;

        if hdr.is_response() && hdr.is_ctrl() {
            if let Some(PendingResponse::Unary(tx)) = pending.lock().remove(&rid) {
                let _ = tx.send(Ok(ResponseData::Inline(recv_buf.clone())));
            }
            continue;
        }

        // Handle chunked response frames.
        if hdr.is_response() && flags::is_chunked(hdr.flags) {
            use c2_wire::chunk::decode_reply_chunk_meta;

            let (total_size, total_chunks, chunk_idx, meta_consumed) =
                match decode_reply_chunk_meta(&recv_buf, 0) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Warning: reply chunk meta decode error: {e:?}");
                        continue;
                    }
                };
            let chunk_data = &recv_buf[meta_consumed..];

            // First chunk: create assembler in registry.
            if chunk_idx == 0 {
                let chunk_size = if total_chunks > 1 {
                    chunk_data.len()
                } else {
                    total_size as usize
                };
                if let Err(e) =
                    chunk_registry.insert(conn_id, rid as u64, total_chunks as usize, chunk_size)
                {
                    eprintln!("Warning: reply chunk assembler creation failed: {e}");
                    let tx = pending.lock().remove(&rid);
                    complete_unary_pending(
                        tx,
                        Err(IpcError::Chunk(format!(
                            "chunked reply assembler failed: {e}"
                        ))),
                    );
                    continue;
                }
            }

            // Feed chunk via registry.
            match chunk_registry.feed(conn_id, rid as u64, chunk_idx as usize, chunk_data) {
                Ok(complete) => {
                    if complete {
                        match chunk_registry.finish(conn_id, rid as u64) {
                            Ok(finished) => {
                                let tx = pending.lock().remove(&rid);
                                complete_unary_pending(
                                    tx,
                                    Ok(ResponseData::Handle(finished.handle)),
                                );
                            }
                            Err(e) => {
                                let tx = pending.lock().remove(&rid);
                                complete_unary_pending(
                                    tx,
                                    Err(IpcError::Chunk(format!(
                                        "chunked reply finish error: {e}"
                                    ))),
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Warning: reply chunk feed error: {e}");
                    let tx = pending.lock().remove(&rid);
                    complete_unary_pending(
                        tx,
                        Err(IpcError::Chunk(format!("chunked reply feed error: {e}"))),
                    );
                }
            }
            continue; // Don't fall through to decode_response.
        }

        // Decode non-chunked response.
        let result = decode_response(&hdr, &recv_buf);

        // Dispatch to pending caller.
        let tx = { pending.lock().remove(&rid) };
        complete_unary_pending(tx, result);
    }

    // Connection lost — cleanup all in-flight assemblies for this connection.
    chunk_registry.cleanup_connection(conn_id);

    // Connection lost — wake all pending callers.
    let mut pending_guard = pending.lock();
    for (_, pending) in pending_guard.drain() {
        let PendingResponse::Unary(tx) = pending;
        let _ = tx.send(Err(IpcError::Closed));
    }
}

fn decode_response(hdr: &FrameHeader, payload: &[u8]) -> Result<ResponseData, IpcError> {
    let is_v2 = hdr.is_reply_v2();
    let is_buddy = hdr.is_buddy();

    if !is_v2 {
        return Ok(ResponseData::Inline(payload.to_vec()));
    }

    if is_buddy {
        if payload.len() < BUDDY_PAYLOAD_SIZE + 1 {
            return Err(IpcError::Decode(DecodeError::BufferTooShort {
                need: BUDDY_PAYLOAD_SIZE + 1,
                have: payload.len(),
            }));
        }
        let (bp, _) = decode_buddy_payload(payload)?;
        let ctrl_start = BUDDY_PAYLOAD_SIZE;
        let (ctrl, _) = decode_reply_control(payload, ctrl_start)?;

        match ctrl {
            ReplyControl::Success => Ok(ResponseData::Shm {
                seg_idx: bp.seg_idx,
                offset: bp.offset,
                data_size: bp.data_size,
                is_dedicated: bp.is_dedicated,
            }),
            ReplyControl::RouteNotFound(route) => Err(IpcError::RouteNotFound(route)),
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    } else {
        let (ctrl, consumed) = decode_reply_control(payload, 0)?;
        match ctrl {
            ReplyControl::Success => Ok(ResponseData::Inline(payload[consumed..].to_vec())),
            ReplyControl::RouteNotFound(route) => Err(IpcError::RouteNotFound(route)),
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembly_pool_unique_prefixes() {
        let cfg = ClientIpcConfig::default();
        let r1 = IpcClient::make_chunk_registry(&cfg);
        let r2 = IpcClient::make_chunk_registry(&cfg);
        let r3 = IpcClient::make_chunk_registry(&cfg);
        let prefix1 = r1.pool().read().prefix().to_string();
        let prefix2 = r2.pool().read().prefix().to_string();
        let prefix3 = r3.pool().read().prefix().to_string();
        assert_ne!(prefix1, prefix2);
        assert_ne!(prefix2, prefix3);
        assert_ne!(prefix1, prefix3);
        assert!(prefix1.starts_with("/cc3a"), "unexpected prefix: {prefix1}");
        assert!(
            prefix1.len() <= 24,
            "prefix exceeds SHM name limit: {}",
            prefix1.len()
        );
    }

    #[test]
    fn client_projects_server_identity() {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "identity-server".to_string(),
            server_instance_id: "identity-instance".to_string(),
        };
        let mut client = IpcClient::new("ipc://identity_projection");
        client.server_identity = Some(identity.clone());

        assert_eq!(client.server_identity(), Some(&identity));
        assert_eq!(client.server_id(), Some("identity-server"));
        assert_eq!(client.server_instance_id(), Some("identity-instance"));
    }

    #[test]
    fn production_direct_ipc_does_not_start_route_watch_task() {
        let source = include_str!("client.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("client.rs must have a production section");
        assert!(
            !production.contains("RouteWatchRequest"),
            "ordinary direct IPC must not open a route-watch stream"
        );
        assert!(
            !production.contains("PendingResponse::Watch"),
            "ordinary direct IPC must not depend on watch response state"
        );
    }

    #[test]
    fn route_binding_keeps_acquired_token_after_directory_update() {
        const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let client = IpcClient::new("ipc://route_binding_projection");
        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable::from_entries(
                &[MethodEntry {
                    name: "ping".to_string(),
                    index: 0,
                }],
                RouteCallIdentity {
                    route_name: "grid".to_string(),
                    route_uid: "grid-route-uid-0001".to_string(),
                    observed_route_revision: 1,
                    crm_ns: "test.grid".to_string(),
                    crm_name: "Grid".to_string(),
                    crm_ver: "0.1.0".to_string(),
                    abi_hash: ABI_HASH.to_string(),
                    signature_hash: SIG_HASH.to_string(),
                },
                1024,
            ),
        );
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
        };
        let binding = client
            .bind_cached_route(&expected)
            .expect("initial route should bind");

        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable::from_entries(
                &[MethodEntry {
                    name: "ping".to_string(),
                    index: 0,
                }],
                RouteCallIdentity {
                    route_name: "grid".to_string(),
                    route_uid: "grid-route-uid-0002".to_string(),
                    observed_route_revision: 2,
                    crm_ns: "test.grid".to_string(),
                    crm_name: "Grid".to_string(),
                    crm_ver: "0.1.0".to_string(),
                    abi_hash: ABI_HASH.to_string(),
                    signature_hash: SIG_HASH.to_string(),
                },
                1024,
            ),
        );

        let (_, identity, _) = binding
            .call_target_for("ping")
            .expect("bound method should still exist");
        assert_eq!(identity.route_uid, "grid-route-uid-0001");
        assert_eq!(identity.observed_route_revision, 1);
        let current_binding = client
            .bind_cached_route(&expected)
            .expect("directory should expose current route");
        assert_eq!(current_binding.route_uid(), "grid-route-uid-0002");
        assert_eq!(current_binding.route_revision(), 2);
    }

    #[test]
    fn exact_token_binding_rejects_same_name_replacement() {
        const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let client = IpcClient::new("ipc://route_binding_stale_projection");
        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable::from_entries(
                &[MethodEntry {
                    name: "ping".to_string(),
                    index: 0,
                }],
                RouteCallIdentity {
                    route_name: "grid".to_string(),
                    route_uid: "grid-route-uid-0002".to_string(),
                    observed_route_revision: 2,
                    crm_ns: "test.grid".to_string(),
                    crm_name: "Grid".to_string(),
                    crm_ver: "0.1.0".to_string(),
                    abi_hash: ABI_HASH.to_string(),
                    signature_hash: SIG_HASH.to_string(),
                },
                1024,
            ),
        );
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
        };

        let err = client
            .bind_cached_route_token(&expected, "grid-route-uid-0001", 1)
            .expect_err("exact token binding must reject replacement route");

        assert!(
            matches!(
                &err,
                IpcError::RouteStale {
                    route_name,
                    current_route_uid,
                    current_route_revision,
                } if route_name == "grid"
                    && current_route_uid == "grid-route-uid-0002"
                    && *current_route_revision == 2
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn client_validates_route_crm_contract_from_handshake_metadata() {
        const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let client = IpcClient::new("ipc://contract_projection");
        let mut methods = HashMap::new();
        methods.insert("ping".to_string(), 0);
        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0001".to_string(),
                route_revision: 1,
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
                max_payload_size: 1024,
                name_to_idx: methods,
            },
        );
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
        };

        client
            .validate_route_contract(&expected)
            .expect("matching CRM contract should be accepted");

        let mut mismatched_name = expected.clone();
        mismatched_name.crm_name = "OtherGrid".to_string();
        let err = client
            .validate_route_contract(&mismatched_name)
            .expect_err("mismatched CRM name should be rejected");
        assert!(
            err.to_string().contains("CRM contract mismatch"),
            "unexpected error: {err}"
        );

        let mut mismatched_hash = expected;
        mismatched_hash.signature_hash =
            "1111111111111111111111111111111111111111111111111111111111111111".to_string();
        let err = client
            .validate_route_contract(&mismatched_hash)
            .expect_err("mismatched signature hash should be rejected");
        assert!(
            err.to_string().contains("signature_hash"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn client_rejects_request_over_route_payload_limit_before_transport() {
        const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let client = IpcClient::new("ipc://payload_limit");
        let mut methods = HashMap::new();
        methods.insert("ping".to_string(), 0);
        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0001".to_string(),
                route_revision: 1,
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
                max_payload_size: 4,
                name_to_idx: methods,
            },
        );

        let binding = client
            .bind_cached_route(&c2_contract::ExpectedRouteContract {
                route_name: "grid".to_string(),
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
            })
            .expect("cached test route should bind");

        let err = client
            .call_bound(&binding, "ping", b"12345")
            .await
            .expect_err("oversized bound call should be rejected before writer access");
        assert!(
            matches!(err, IpcError::Config(_)),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains("max_payload_size"));

        let stream = futures_util::stream::once(async {
            panic!("oversized sized stream should not be polled");
            #[allow(unreachable_code)]
            Ok::<&'static [u8], std::io::Error>(b"12345")
        });
        let err = client
            .call_bound_sized_stream(&binding, "ping", 5, stream)
            .await
            .expect_err("oversized streaming call should be rejected before body polling");
        assert!(
            matches!(err, IpcError::Config(_)),
            "unexpected error: {err:?}"
        );
        assert!(err.to_string().contains("max_payload_size"));
    }

    #[tokio::test]
    async fn sized_stream_releases_preallocated_pool_on_length_mismatch() {
        const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let cfg = ClientIpcConfig {
            shm_threshold: 10,
            base: c2_config::BaseIpcConfig {
                pool_segment_size: 65_536,
                max_pool_segments: 1,
                ..c2_config::BaseIpcConfig::default()
            },
        };
        let client = IpcClient::with_config("ipc://stream_length_mismatch", cfg);
        let mut methods = HashMap::new();
        methods.insert("ping".to_string(), 0);
        client.route_directory.write().insert_table(
            "grid".to_string(),
            MethodTable {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0001".to_string(),
                route_revision: 1,
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
                max_payload_size: 1024,
                name_to_idx: methods,
            },
        );

        let binding = client
            .bind_cached_route(&c2_contract::ExpectedRouteContract {
                route_name: "grid".to_string(),
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
            })
            .expect("cached test route should bind");

        let short_stream =
            futures_util::stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(vec![1; 100])]);
        let err = client
            .call_bound_sized_stream(&binding, "ping", 200, short_stream)
            .await
            .expect_err("short stream should fail before sending a frame");
        assert!(err.to_string().contains("expected 200"));

        let pool = client.pool.as_ref().expect("pool should be enabled");
        assert_eq!(pool.lock().stats().alloc_count, 0);

        let long_stream = futures_util::stream::iter(vec![
            Ok::<Vec<u8>, std::io::Error>(vec![1; 150]),
            Ok::<Vec<u8>, std::io::Error>(vec![2; 51]),
        ]);
        let err = client
            .call_bound_sized_stream(&binding, "ping", 200, long_stream)
            .await
            .expect_err("long stream should fail before sending a frame");
        assert!(err.to_string().contains("exceeded declared content length"));
        assert_eq!(pool.lock().stats().alloc_count, 0);
    }

    #[tokio::test]
    async fn client_rejects_path_like_ipc_region_before_connecting() {
        for address in [
            "ipc://../escape",
            "ipc://bad/name",
            "ipc://bad\\name",
            "ipc://.",
            "ipc://..",
            "ipc:// leading",
            "ipc://trailing ",
            "ipc://bad\nname",
            "tcp://not-ipc",
        ] {
            let mut client = IpcClient::new(address);
            let error = client
                .connect()
                .await
                .expect_err("invalid IPC address should not attempt UDS connect");
            assert!(
                matches!(error, IpcError::Config(_)),
                "expected config error for {address:?}, got {error:?}"
            );
        }
    }
}
