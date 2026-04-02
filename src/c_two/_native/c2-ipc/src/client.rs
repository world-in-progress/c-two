//! Async IPC client — connects to Python ServerV2 via UDS.
//!
//! Performs handshake, then multiplexes concurrent requests over
//! a single UDS connection using request IDs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};

use c2_mem::FreeResult;
use c2_wire::assembler::ChunkAssembler;
use c2_wire::buddy::{decode_buddy_payload, encode_buddy_payload, BuddyPayload, BUDDY_PAYLOAD_SIZE};
use c2_wire::chunk::encode_chunk_header;
use c2_wire::control::{decode_reply_control, encode_call_control, ReplyControl};
use c2_wire::flags;
use c2_wire::frame::{self, DecodeError, FrameHeader, HEADER_SIZE};
use c2_wire::handshake::{
    decode_handshake, encode_client_handshake, Handshake, MethodEntry,
    CAP_CALL_V2, CAP_CHUNKED, CAP_METHOD_IDX,
};

use c2_mem::config::PoolConfig;
use c2_mem::MemPool;

use crate::response::ResponseData;

// ── Config ───────────────────────────────────────────────────────────────

/// Transport thresholds for IPC path selection.
#[derive(Debug, Clone)]
pub struct IpcConfig {
    /// Data sizes above this use buddy SHM path (default 4096 bytes).
    pub shm_threshold: usize,
    /// Chunk size for chunked transfer (default 128 KB).
    pub chunk_size: usize,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            shm_threshold: 4096,
            chunk_size: 131072,
        }
    }
}

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
    /// Called transparently by `PyResponseBuffer` before any SHM access
    /// (`__getbuffer__`, `release`, `Drop`).  Python never needs to know
    /// about segment management — this keeps it entirely inside Rust.
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

        let ptr = self.pool.data_ptr_at(seg_idx as u32, offset, is_dedicated)?;
        let data = unsafe {
            std::slice::from_raw_parts(ptr, data_size as usize)
        }.to_vec();

        let free_result = match self.pool.free_at(seg_idx as u32, offset, data_size, is_dedicated) {
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
    /// Wire protocol decoding error.
    Decode(DecodeError),
    /// Handshake failed or incompatible server.
    Handshake(String),
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
            Self::Decode(e) => write!(f, "IPC decode error: {e}"),
            Self::Handshake(msg) => write!(f, "IPC handshake failed: {msg}"),
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

// ── Method table ─────────────────────────────────────────────────────────

/// Per-route method table (name ↔ index).
#[derive(Debug, Clone)]
pub struct MethodTable {
    name_to_idx: HashMap<String, u16>,
}

impl MethodTable {
    fn from_entries(entries: &[MethodEntry]) -> Self {
        let mut name_to_idx = HashMap::with_capacity(entries.len());
        for e in entries {
            name_to_idx.insert(e.name.clone(), e.index);
        }
        Self { name_to_idx }
    }

    /// Look up method index by name.
    pub fn index_of(&self, name: &str) -> Option<u16> {
        self.name_to_idx.get(name).copied()
    }

    /// Get all method names.
    pub fn method_names(&self) -> Vec<&str> {
        self.name_to_idx.keys().map(|s| s.as_str()).collect()
    }
}

// ── Pending call ─────────────────────────────────────────────────────────

type PendingMap = HashMap<u32, oneshot::Sender<Result<ResponseData, IpcError>>>;

// ── IpcClient ────────────────────────────────────────────────────────────

/// Async IPC client for the C-Two relay.
///
/// Connects to a Python `ServerV2` via Unix Domain Socket, performs
/// handshake, and multiplexes concurrent CRM calls.
pub struct IpcClient {
    socket_path: PathBuf,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    pending: Arc<StdMutex<PendingMap>>,
    rid_counter: AtomicU32,
    route_tables: HashMap<String, MethodTable>,
    server_segments: Vec<(String, u32)>,
    /// Server SHM pool state for reading buddy reply responses.
    pub(crate) server_pool: Arc<StdMutex<Option<ServerPoolState>>>,
    recv_handle: Option<tokio::task::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    pool: Option<Arc<StdMutex<MemPool>>>,
    config: IpcConfig,
    /// Client-side pool for reassembling chunked responses.
    pub(crate) reassembly_pool: Arc<StdMutex<MemPool>>,
}

// Compile-time assertion: IpcClient is Send+Sync because all fields are
// Arc-wrapped (Send+Sync), atomic (Send+Sync), or standard collections
// of Send+Sync types. This is required for safe use from PyO3 frozen
// pyclass wrappers under Python 3.14t free-threading.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<IpcClient>();
        _assert_sync::<IpcClient>();
    }
};

impl IpcClient {
    fn default_reassembly_pool() -> Arc<StdMutex<MemPool>> {
        Arc::new(StdMutex::new(MemPool::new(PoolConfig {
            segment_size: 256 * 1024 * 1024,
            max_segments: 4,
            ..PoolConfig::default()
        })))
    }

    /// Create a new IPC client targeting the given address.
    ///
    /// The address should be like `ipc://name` — the socket path is
    /// derived as `/tmp/c_two_ipc/{name}.sock` (matching Python Server).
    pub fn new(address: &str) -> Self {
        let name = address
            .strip_prefix("ipc://")
            .unwrap_or(address);
        let socket_path = PathBuf::from(format!("/tmp/c_two_ipc/{name}.sock"));

        Self {
            socket_path,
            writer: Arc::new(Mutex::new(None)),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            rid_counter: AtomicU32::new(1),
            route_tables: HashMap::new(),
            server_segments: Vec::new(),
            server_pool: Arc::new(StdMutex::new(None)),
            recv_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            pool: None,
            config: IpcConfig::default(),
            reassembly_pool: Self::default_reassembly_pool(),
        }
    }

    /// Create a new IPC client with a buddy pool for SHM transfers.
    ///
    /// The pool is used for outgoing buddy allocations when data exceeds
    /// `config.shm_threshold`.
    pub fn with_pool(
        address: &str,
        pool: Arc<StdMutex<MemPool>>,
        config: IpcConfig,
    ) -> Self {
        let name = address
            .strip_prefix("ipc://")
            .unwrap_or(address);
        let socket_path = PathBuf::from(format!("/tmp/c_two_ipc/{name}.sock"));

        Self {
            socket_path,
            writer: Arc::new(Mutex::new(None)),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            rid_counter: AtomicU32::new(1),
            route_tables: HashMap::new(),
            server_segments: Vec::new(),
            server_pool: Arc::new(StdMutex::new(None)),
            recv_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            pool: Some(pool),
            config,
            reassembly_pool: Self::default_reassembly_pool(),
        }
    }

    /// Connect and perform handshake.
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = tokio::io::split(stream);

        // Pre-allocate first SHM segment so handshake announces it.
        if let Some(ref pool_arc) = self.pool {
            let mut pool = pool_arc.lock().unwrap();
            pool.ensure_ready()
                .map_err(|e| IpcError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        }

        // Perform handshake.
        let hs = self.do_handshake(&mut writer, reader).await?;

        // Store method tables from the handshake response.
        for route in &hs.routes {
            let table = MethodTable::from_entries(&route.methods);
            self.route_tables.insert(route.name.clone(), table);
        }
        self.server_segments = hs.segments.clone();

        // Open server SHM segments into a ServerPoolState for buddy response reads.
        if !hs.segments.is_empty() {
            let buddy_seg_size = hs.segments[0].1 as usize;
            let cfg = c2_mem::config::PoolConfig {
                segment_size: buddy_seg_size,
                min_block_size: 4096,
                max_segments: 16,
                max_dedicated_segments: 8,
                dedicated_crash_timeout_secs: 60.0,
                spill_threshold: 1.0,
                spill_dir: std::path::PathBuf::from("/tmp"),
            };
            let mut pool = MemPool::new_with_prefix(cfg, hs.prefix.clone());
            for (name, size) in &hs.segments {
                if let Err(e) = pool.open_segment(name, *size as usize) {
                    eprintln!("Warning: failed to open server SHM segment ({name}): {e}");
                }
            }
            *self.server_pool.lock().unwrap() = Some(ServerPoolState {
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
            let pool = pool_arc.lock().unwrap();
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

        let payload = encode_client_handshake(&segments, cap_flags, &prefix);
        let frame_bytes = frame::encode_frame(0, flags::FLAG_HANDSHAKE, &payload);
        writer.write_all(&frame_bytes).await?;

        // Read handshake response.
        let mut header_buf = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header_buf).await?;
        let (total_len, body_rest) = frame::decode_total_len(&header_buf)?;
        let (hdr, _hdr_payload) = frame::decode_frame_body(body_rest, total_len)?;

        if !hdr.is_handshake() {
            return Err(IpcError::Handshake(
                "Server response is not a handshake frame".into(),
            ));
        }

        let payload_len = hdr.payload_len();
        let mut payload_buf = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload_buf).await?;
        }

        let hs = decode_handshake(&payload_buf)
            .map_err(|e| IpcError::Handshake(format!("decode: {e}")))?;

        if hs.capability_flags & CAP_CALL_V2 == 0 {
            return Err(IpcError::Handshake(
                "Server does not support v2 call frames".into(),
            ));
        }

        // Spawn recv loop with the reader.
        let pending = self.pending.clone();
        let server_pool = self.server_pool.clone();
        let writer_clone = self.writer.clone();
        let connected = self.connected.clone();
        let reassembly_pool = self.reassembly_pool.clone();
        // Note: recv_handle is stored on self but we can't assign here
        // because &self is immutable. The caller (connect) handles this
        // via interior mutability or the recv task is detached.
        tokio::spawn(async move {
            recv_loop(reader, pending, server_pool, writer_clone, reassembly_pool).await;
            connected.store(false, Ordering::Release);
        });

        Ok(hs)
    }

    /// Get a reference to the server SHM pool (for materialising SHM responses).
    pub fn server_pool_arc(&self) -> &Arc<StdMutex<Option<ServerPoolState>>> {
        &self.server_pool
    }

    /// Get a reference to the reassembly pool (for materialising Handle responses).
    pub fn reassembly_pool_arc(&self) -> &Arc<StdMutex<MemPool>> {
        &self.reassembly_pool
    }

    /// Send a CRM call and wait for the response (inline path only).
    ///
    /// This is the primary API for the relay: HTTP handler calls this
    /// with the route name, method name, and serialized payload.
    pub async fn call(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let table = self
            .route_tables
            .get(route_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown route: {route_name}")))?;
        let method_idx = table
            .index_of(method_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown method: {method_name}")))?;

        self.call_inline(route_name, method_idx, data).await
    }

    /// Send a CRM call with automatic transport path selection.
    ///
    /// Selects the optimal transport based on data size and pool availability:
    /// - Buddy SHM for data above `config.shm_threshold` (when pool available)
    /// - Chunked transfer for data above `config.chunk_size`
    /// - Inline for small payloads
    pub async fn call_full(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let table = self
            .route_tables
            .get(route_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown route: {route_name}")))?;
        let method_idx = table
            .index_of(method_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown method: {method_name}")))?;

        // Try buddy SHM for large payloads when pool is available.
        if self.pool.is_some() && data.len() > self.config.shm_threshold {
            match self.call_buddy(route_name, method_idx, data).await {
                Ok(result) => return Ok(result),
                Err(IpcError::Handshake(_)) => {
                    // Pool alloc failed — fall through to chunked/inline.
                }
                Err(e) => return Err(e),
            }
        }

        // Use chunked transfer for data exceeding chunk size.
        if data.len() > self.config.chunk_size {
            return self.call_chunked(route_name, method_idx, data).await;
        }

        // Default: inline.
        self.call_inline(route_name, method_idx, data).await
    }

    /// Inline call path — sends call control + data in a single frame.
    async fn call_inline(
        &self,
        route_name: &str,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().unwrap().insert(rid, tx);
        }

        // Build and send the frame.
        // Compute ctrl size: 1 byte name_len + name + 2 bytes method_idx
        let ctrl_len = 1 + route_name.len() + 2;
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
                    &mut buf, frame::HEADER_SIZE, route_name, method_idx,
                );
                let data_off = frame::HEADER_SIZE + ctrl_written;
                buf[data_off..data_off + data.len()].copy_from_slice(data);
                writer.write_all(&buf[..frame_size]).await?;
            } else {
                // Large payload: header+ctrl on stack, data separate write.
                let mut hdr_buf = [0u8; frame::HEADER_SIZE];
                hdr_buf[0..4].copy_from_slice(&total_len.to_le_bytes());
                hdr_buf[4..12].copy_from_slice(&(rid as u64).to_le_bytes());
                hdr_buf[12..16].copy_from_slice(&flags::FLAG_CALL_V2.to_le_bytes());
                let ctrl = encode_call_control(route_name, method_idx);
                writer.write_all(&hdr_buf).await?;
                writer.write_all(&ctrl).await?;
                writer.write_all(data).await?;
            }
            Ok(())
        }.await;

        if let Err(e) = send_result {
            self.pending.lock().unwrap().remove(&rid);
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
        route_name: &str,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let pool_arc = self.pool.as_ref().unwrap();

        // Allocate and write data to SHM.
        let alloc = {
            let mut pool = pool_arc.lock().unwrap();
            pool.alloc(data.len()).map_err(|e| {
                IpcError::Handshake(format!("buddy alloc failed: {e}"))
            })?
        };

        // Write data into the SHM region.
        {
            let pool = pool_arc.lock().unwrap();
            let ptr = match pool.data_ptr(&alloc) {
                Ok(p) => p,
                Err(e) => {
                    drop(pool);
                    let _ = pool_arc.lock().unwrap().free(&alloc);
                    return Err(IpcError::Handshake(format!("buddy data_ptr failed: {e}")));
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
        let ctrl = encode_call_control(route_name, method_idx);

        // Assemble frame payload: [11B buddy][call_control]
        let payload_len = buddy_bytes.len() + ctrl.len();
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().unwrap().insert(rid, tx);
        }

        // Send frame — free buddy allocation if send fails.
        let frame_flags = flags::FLAG_CALL_V2 | flags::FLAG_BUDDY;
        let frame_bytes = frame::encode_frame(rid as u64, frame_flags, &payload);
        {
            let mut writer_guard = self.writer.lock().await;
            let writer = match writer_guard.as_mut() {
                Some(w) => w,
                None => {
                    let _ = pool_arc.lock().unwrap().free(&alloc);
                    self.pending.lock().unwrap().remove(&rid);
                    return Err(IpcError::Closed);
                }
            };
            if let Err(e) = writer.write_all(&frame_bytes).await {
                let _ = pool_arc.lock().unwrap().free(&alloc);
                self.pending.lock().unwrap().remove(&rid);
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
                    let mut pool = pool_arc.lock().unwrap();
                    let _ = pool.free(&alloc);
                }
                result
            }
            Err(_) => Err(IpcError::Closed),
        }
    }

    /// Chunked call path — splits data into chunks and sends with FLAG_CHUNKED.
    async fn call_chunked(
        &self,
        route_name: &str,
        method_idx: u16,
        data: &[u8],
    ) -> Result<ResponseData, IpcError> {
        let chunk_size = self.config.chunk_size;
        let total_chunks = (data.len() + chunk_size - 1) / chunk_size;

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call ONCE — reply comes after last chunk.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().unwrap().insert(rid, tx);
        }

        // Build call control (included only in chunk 0).
        let ctrl = encode_call_control(route_name, method_idx);

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
                let payload_len = chunk_hdr.len()
                    + if i == 0 { ctrl.len() } else { 0 }
                    + chunk_data.len();
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
        }.await;

        if let Err(e) = send_result {
            self.pending.lock().unwrap().remove(&rid);
            return Err(e);
        }

        // Await response.
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Closed),
        }
    }

    /// Get the method table for a route.
    pub fn route_table(&self, name: &str) -> Option<&MethodTable> {
        self.route_tables.get(name)
    }

    /// Get all route names.
    pub fn route_names(&self) -> Vec<&str> {
        self.route_tables.keys().map(|s| s.as_str()).collect()
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
        // Abort recv task.
        if let Some(handle) = self.recv_handle.take() {
            handle.abort();
        }
        // Wake pending callers.
        let mut pending = self.pending.lock().unwrap();
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(IpcError::Closed));
        }
    }
}

// ── Recv loop ────────────────────────────────────────────────────────────

/// Signal byte constants (match Python `MsgType` enum).
const SIG_PING: u8 = 0x01;
const SIG_PONG: u8 = 0x02;
const SIG_DISCONNECT: u8 = 0x08;
const SIG_DISCONNECT_ACK: u8 = 0x09;

async fn recv_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    pending: Arc<StdMutex<PendingMap>>,
    _server_pool: Arc<StdMutex<Option<ServerPoolState>>>,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    reassembly_pool: Arc<StdMutex<MemPool>>,
) {
    let mut header_buf = [0u8; HEADER_SIZE];
    let mut recv_buf = Vec::with_capacity(4096); // reusable buffer
    let mut assemblers: HashMap<u32, ChunkAssembler> = HashMap::new();
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
        if payload_len > 0 {
            if reader.read_exact(&mut recv_buf).await.is_err() {
                break;
            }
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

            // First chunk: create assembler.
            if chunk_idx == 0 {
                let chunk_size = if total_chunks > 1 {
                    chunk_data.len()
                } else {
                    total_size as usize
                };
                let mut pool = reassembly_pool.lock().unwrap();
                match ChunkAssembler::new(&mut pool, total_chunks as usize, chunk_size) {
                    Ok(asm) => { assemblers.insert(rid, asm); }
                    Err(e) => {
                        eprintln!("Warning: reply chunk assembler creation failed: {e}");
                        let tx = pending.lock().unwrap().remove(&rid);
                        if let Some(tx) = tx {
                            let _ = tx.send(Err(IpcError::Handshake(
                                format!("chunked reply assembler failed: {e}"),
                            )));
                        }
                        continue;
                    }
                }
            }

            // Feed chunk.
            if let Some(asm) = assemblers.get_mut(&rid) {
                let pool = reassembly_pool.lock().unwrap();
                if let Err(e) = asm.feed_chunk(&pool, chunk_idx as usize, chunk_data) {
                    drop(pool);
                    eprintln!("Warning: reply chunk feed error: {e}");
                    if let Some(failed) = assemblers.remove(&rid) {
                        let mut pool = reassembly_pool.lock().unwrap();
                        failed.abort(&mut pool);
                    }
                    let tx = pending.lock().unwrap().remove(&rid);
                    if let Some(tx) = tx {
                        let _ = tx.send(Err(IpcError::Handshake(
                            format!("chunked reply feed error: {e}"),
                        )));
                    }
                    continue;
                }

                // Complete: finish and send to caller.
                if asm.is_complete() {
                    drop(pool);
                    let asm = assemblers.remove(&rid).unwrap();
                    match asm.finish() {
                        Ok(handle) => {
                            let tx = pending.lock().unwrap().remove(&rid);
                            if let Some(tx) = tx {
                                let _ = tx.send(Ok(ResponseData::Handle(handle)));
                            }
                        }
                        Err(e) => {
                            let tx = pending.lock().unwrap().remove(&rid);
                            if let Some(tx) = tx {
                                let _ = tx.send(Err(IpcError::Handshake(
                                    format!("chunked reply finish error: {e}"),
                                )));
                            }
                        }
                    }
                }
            }
            continue; // Don't fall through to decode_response.
        }

        // Decode non-chunked response.
        let result = decode_response(&hdr, &recv_buf);

        // Dispatch to pending caller.
        let tx = {
            pending.lock().unwrap().remove(&rid)
        };
        if let Some(tx) = tx {
            let _ = tx.send(result);
        }
    }

    // Connection lost — abort in-flight assemblers.
    if !assemblers.is_empty() {
        let mut pool = reassembly_pool.lock().unwrap();
        for (_, asm) in assemblers.drain() {
            asm.abort(&mut pool);
        }
    }

    // Connection lost — wake all pending callers.
    let mut pending_guard = pending.lock().unwrap();
    for (_, tx) in pending_guard.drain() {
        let _ = tx.send(Err(IpcError::Closed));
    }
}

fn decode_response(
    hdr: &FrameHeader,
    payload: &[u8],
) -> Result<ResponseData, IpcError> {
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
            ReplyControl::Success => {
                Ok(ResponseData::Shm {
                    seg_idx: bp.seg_idx,
                    offset: bp.offset,
                    data_size: bp.data_size,
                    is_dedicated: bp.is_dedicated,
                })
            }
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    } else {
        let (ctrl, consumed) = decode_reply_control(payload, 0)?;
        match ctrl {
            ReplyControl::Success => Ok(ResponseData::Inline(payload[consumed..].to_vec())),
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    }
}
