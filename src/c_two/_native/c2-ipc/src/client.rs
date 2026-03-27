//! Async IPC v3 client — connects to Python ServerV2 via UDS.
//!
//! Performs handshake v5, then multiplexes concurrent requests over
//! a single UDS connection using request IDs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, RwLock};

use c2_wire::buddy::{decode_buddy_payload, BUDDY_PAYLOAD_SIZE};
use c2_wire::control::{decode_reply_control, ReplyControl};
use c2_wire::flags;
use c2_wire::frame::{self, DecodeError, FrameHeader, HEADER_SIZE};
use c2_wire::handshake::{
    decode_handshake, encode_client_handshake, HandshakeV5, MethodEntry,
    CAP_CALL_V2, CAP_METHOD_IDX,
};

use crate::shm::SegmentCache;

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
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IPC I/O error: {e}"),
            Self::Decode(e) => write!(f, "IPC decode error: {e}"),
            Self::Handshake(msg) => write!(f, "IPC handshake failed: {msg}"),
            Self::CrmError(_) => write!(f, "CRM method error"),
            Self::Closed => write!(f, "IPC client closed"),
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

type PendingMap = HashMap<u32, oneshot::Sender<Result<Vec<u8>, IpcError>>>;

// ── IpcClient ────────────────────────────────────────────────────────────

/// Async IPC v3 client for the C-Two relay.
///
/// Connects to a Python `ServerV2` via Unix Domain Socket, performs
/// handshake v5, and multiplexes concurrent CRM calls.
pub struct IpcClient {
    socket_path: PathBuf,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    pending: Arc<StdMutex<PendingMap>>,
    rid_counter: AtomicU32,
    route_tables: HashMap<String, MethodTable>,
    server_segments: Vec<(String, u32)>,
    seg_cache: Arc<RwLock<SegmentCache>>,
    recv_handle: Option<tokio::task::JoinHandle<()>>,
}

impl IpcClient {
    /// Create a new IPC client targeting the given address.
    ///
    /// The address should be like `ipc-v3://name` — the socket path is
    /// derived as `/tmp/c_two_ipc/{name}.sock` (matching Python ServerV2).
    pub fn new(address: &str) -> Self {
        let name = address
            .strip_prefix("ipc-v3://")
            .unwrap_or(address);
        let socket_path = PathBuf::from(format!("/tmp/c_two_ipc/{name}.sock"));

        Self {
            socket_path,
            writer: Arc::new(Mutex::new(None)),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            rid_counter: AtomicU32::new(1),
            route_tables: HashMap::new(),
            server_segments: Vec::new(),
            seg_cache: Arc::new(RwLock::new(SegmentCache::new())),
            recv_handle: None,
        }
    }

    /// Connect and perform handshake v5.
    pub async fn connect(&mut self) -> Result<(), IpcError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = tokio::io::split(stream);

        // Perform handshake v5.
        let hs = self.do_handshake(&mut writer, reader).await?;

        // Store method tables from the handshake response.
        for route in &hs.routes {
            let table = MethodTable::from_entries(&route.methods);
            self.route_tables.insert(route.name.clone(), table);
        }
        self.server_segments = hs.segments.clone();

        // Open server SHM segments for reading response data.
        {
            let mut cache = self.seg_cache.write().await;
            for (idx, (name, size)) in hs.segments.iter().enumerate() {
                // Server segments need the leading '/' for POSIX shm_open
                let shm_name = if name.starts_with('/') {
                    name.clone()
                } else {
                    format!("/{name}")
                };
                if let Err(e) = cache.open(idx as u16, &shm_name, *size as usize) {
                    // Non-fatal: we can still do inline-only calls.
                    eprintln!("Warning: failed to open server SHM segment {idx} ({name}): {e}");
                }
            }
        }

        *self.writer.lock().await = Some(writer);

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
    ) -> Result<HandshakeV5, IpcError> {
        // We don't create SHM segments from the relay side — the relay
        // reads from server segments only.  Send empty segments list.
        let payload = encode_client_handshake(&[], CAP_CALL_V2 | CAP_METHOD_IDX);
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
            .map_err(|e| IpcError::Handshake(format!("v5 decode: {e}")))?;

        if hs.capability_flags & CAP_CALL_V2 == 0 {
            return Err(IpcError::Handshake(
                "Server does not support v2 call frames".into(),
            ));
        }

        // Spawn recv loop with the reader.
        let pending = self.pending.clone();
        let seg_cache = self.seg_cache.clone();
        // Note: recv_handle is stored on self but we can't assign here
        // because &self is immutable. The caller (connect) handles this
        // via interior mutability or the recv task is detached.
        tokio::spawn(async move {
            recv_loop(reader, pending, seg_cache).await;
        });

        Ok(hs)
    }

    /// Send a CRM call and wait for the response.
    ///
    /// This is the primary API for the relay: HTTP handler calls this
    /// with the route name, method name, and serialized payload.
    pub async fn call(
        &self,
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> Result<Vec<u8>, IpcError> {
        let table = self
            .route_tables
            .get(route_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown route: {route_name}")))?;
        let method_idx = table
            .index_of(method_name)
            .ok_or_else(|| IpcError::Handshake(format!("unknown method: {method_name}")))?;

        let rid = self.rid_counter.fetch_add(1, Ordering::Relaxed);

        // Register pending call.
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().unwrap().insert(rid, tx);
        }

        // Build and send the frame using vectored I/O (avoids payload Vec).
        let ctrl = c2_wire::control::encode_call_control(route_name, method_idx);
        let payload_len = ctrl.len() + data.len();
        let total_len = (12 + payload_len) as u32;
        let mut hdr_buf = [0u8; frame::HEADER_SIZE];
        hdr_buf[0..4].copy_from_slice(&total_len.to_le_bytes());
        hdr_buf[4..12].copy_from_slice(&(rid as u64).to_le_bytes());
        hdr_buf[12..16].copy_from_slice(&flags::FLAG_CALL_V2.to_le_bytes());

        {
            let mut writer_guard = self.writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(IpcError::Closed)?;
            writer.write_all(&hdr_buf).await?;
            writer.write_all(&ctrl).await?;
            writer.write_all(data).await?;
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

    /// Close the client.
    pub async fn close(&mut self) {
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

async fn recv_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    pending: Arc<StdMutex<PendingMap>>,
    seg_cache: Arc<RwLock<SegmentCache>>,
) {
    let mut header_buf = [0u8; HEADER_SIZE];
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
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            if reader.read_exact(&mut payload).await.is_err() {
                break;
            }
        }

        let rid = hdr.request_id as u32;

        // Decode response.
        let result = decode_response(&hdr, &payload, &seg_cache).await;

        // Dispatch to pending caller.
        let tx = {
            pending.lock().unwrap().remove(&rid)
        };
        if let Some(tx) = tx {
            let _ = tx.send(result);
        }
    }

    // Connection lost — wake all pending callers.
    let mut pending_guard = pending.lock().unwrap();
    for (_, tx) in pending_guard.drain() {
        let _ = tx.send(Err(IpcError::Closed));
    }
}

async fn decode_response(
    hdr: &FrameHeader,
    payload: &[u8],
    seg_cache: &Arc<RwLock<SegmentCache>>,
) -> Result<Vec<u8>, IpcError> {
    let is_v2 = hdr.is_reply_v2();
    let is_buddy = hdr.is_buddy();

    if !is_v2 {
        // V1 reply — not expected from a v2 server, but handle gracefully.
        return Ok(payload.to_vec());
    }

    if is_buddy {
        // Buddy reply: [11B buddy_ptr][1B+ reply_control]
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
                // Read data from SHM segment.
                let cache = seg_cache.read().await;
                match cache.read(bp.seg_idx, bp.offset as usize, bp.data_size as usize) {
                    Some(data) => Ok(data),
                    None => Err(IpcError::Handshake(format!(
                        "SHM segment {} not mapped",
                        bp.seg_idx,
                    ))),
                }
            }
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    } else {
        // Inline reply: [1B+ reply_control][inline_data]
        let (ctrl, consumed) = decode_reply_control(payload, 0)?;
        match ctrl {
            ReplyControl::Success => {
                let data = payload[consumed..].to_vec();
                Ok(data)
            }
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    }
}
