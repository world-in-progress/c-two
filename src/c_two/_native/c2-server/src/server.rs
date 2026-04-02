//! UDS server — accept loop, per-connection frame handler, CRM dispatch.
//!
//! Replaces the Python asyncio server (`transport/server/core.py`).
//! CRM method execution is delegated to [`CrmCallback`] (implemented
//! by PyO3 wrappers in c2-ffi).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex, RwLock};
use tracing::{debug, info, warn};

/// Monotonic counter ensuring each `Server` instance gets a unique SHM prefix,
/// even when multiple servers are created within the same PID (e.g. benchmarks
/// or tests that reset the registry).
static RESPONSE_POOL_GEN: AtomicU64 = AtomicU64::new(0);

use c2_mem::config::PoolConfig;
use c2_mem::MemPool;
use c2_wire::buddy::{decode_buddy_payload, encode_buddy_payload, BuddyPayload, BUDDY_PAYLOAD_SIZE};
use c2_wire::chunk::{decode_chunk_header, encode_reply_chunk_meta, REPLY_CHUNK_META_SIZE};
use c2_wire::control::{decode_call_control, encode_reply_control, ReplyControl};
use c2_wire::flags::{FLAG_BUDDY, FLAG_CHUNKED, FLAG_CHUNK_LAST, FLAG_HANDSHAKE, FLAG_REPLY_V2, FLAG_RESPONSE, FLAG_SIGNAL};
use c2_wire::frame::{self, decode_frame_body, encode_frame};
use c2_wire::handshake::{
    decode_handshake, encode_server_handshake, MethodEntry, RouteInfo, CAP_CALL_V2, CAP_CHUNKED,
    CAP_METHOD_IDX,
};
use c2_wire::msg_type::{MsgType, DISCONNECT_ACK_BYTES, PONG_BYTES, SHUTDOWN_ACK_BYTES};

use crate::config::IpcConfig;
use crate::connection::Connection;
use crate::dispatcher::{CrmError, CrmRoute, Dispatcher, RequestData, ResponseMeta};
use crate::heartbeat::run_heartbeat;

const IPC_SOCK_DIR: &str = "/tmp/c_two_ipc";

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

/// The main IPC server.
///
/// Binds a UDS socket, accepts connections, and dispatches CRM calls through
/// the [`Dispatcher`].  Each connection runs in its own tokio task with a
/// dedicated heartbeat probe.
pub struct Server {
    config: IpcConfig,
    socket_path: PathBuf,
    dispatcher: RwLock<Dispatcher>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    conn_counter: AtomicU64,
    /// Server-side MemPool for chunked reassembly buffers.
    reassembly_pool: Arc<std::sync::RwLock<MemPool>>,
    /// Server-side MemPool for writing buddy SHM responses.
    response_pool: Arc<std::sync::RwLock<MemPool>>,
}

impl Server {
    /// Create a new server for the given IPC address.
    ///
    /// Address format: `ipc://region_id` or `ipc-v3://region_id`
    /// → socket at `/tmp/c_two_ipc/region_id.sock`
    pub fn new(address: &str, config: IpcConfig) -> Result<Self, ServerError> {
        config.validate().map_err(ServerError::Config)?;
        let socket_path = parse_socket_path(address)?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let reassembly_cfg = PoolConfig {
            segment_size: 64 * 1024 * 1024,
            min_block_size: 4096,
            max_segments: 4,
            max_dedicated_segments: 4,
            dedicated_gc_delay_secs: 5.0,
            spill_threshold: 0.8,
            spill_dir: PathBuf::from("/tmp/c_two_reassembly"),
        };
        let reassembly_pool = MemPool::new(reassembly_cfg);
        let response_cfg = PoolConfig {
            segment_size: config.pool_segment_size as usize,
            min_block_size: 4096,
            max_segments: config.max_pool_segments as usize,
            max_dedicated_segments: 4,
            dedicated_gc_delay_secs: 5.0,
            spill_threshold: 0.8,
            spill_dir: PathBuf::from("/tmp/c_two_response_spill"),
        };
        let mut response_pool = {
            let pid = std::process::id();
            let generation = RESPONSE_POOL_GEN.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("/cc3r{:08x}{:02x}", pid, (generation & 0xFF) as u8);
            MemPool::new_with_prefix(response_cfg, prefix)
        };
        response_pool.ensure_ready()
            .map_err(|e| ServerError::Config(format!("response pool init: {e}")))?;
        Ok(Self {
            config,
            socket_path,
            dispatcher: RwLock::new(Dispatcher::new()),
            shutdown_tx,
            shutdown_rx,
            conn_counter: AtomicU64::new(0),
            reassembly_pool: Arc::new(std::sync::RwLock::new(reassembly_pool)),
            response_pool: Arc::new(std::sync::RwLock::new(response_pool)),
        })
    }

    /// Register a CRM route with the dispatcher.
    pub async fn register_route(&self, route: CrmRoute) {
        self.dispatcher.write().await.register(route);
    }

    /// Remove a CRM route. Returns `true` if it existed.
    pub async fn unregister_route(&self, name: &str) -> bool {
        self.dispatcher.write().await.unregister(name)
    }

    /// Filesystem path of the bound UDS socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Get a shared reference to the response pool (for zero-copy dispatch).
    pub fn response_pool_arc(&self) -> Arc<std::sync::RwLock<MemPool>> {
        Arc::clone(&self.response_pool)
    }

    /// Get a shared reference to the reassembly pool.
    pub fn reassembly_pool_arc(&self) -> Arc<std::sync::RwLock<MemPool>> {
        Arc::clone(&self.reassembly_pool)
    }

    /// Run the accept loop.  Blocks until [`shutdown`](Self::shutdown) is called.
    pub async fn run(self: &Arc<Self>) -> Result<(), ServerError> {
        std::fs::create_dir_all(IPC_SOCK_DIR)?;
        let _ = std::fs::remove_file(&self.socket_path);

        let listener = UnixListener::bind(&self.socket_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &self.socket_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }

        info!(path = %self.socket_path.display(), "server listening");

        let mut shutdown_rx = self.shutdown_rx.clone();
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
                    if *shutdown_rx.borrow() {
                        info!("server shutting down");
                        break;
                    }
                }
            }
        }

        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Signal the server to stop and remove the socket file.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

// ---------------------------------------------------------------------------
// Address helpers
// ---------------------------------------------------------------------------

fn parse_socket_path(address: &str) -> Result<PathBuf, ServerError> {
    let region = address
        .strip_prefix("ipc-v3://")
        .or_else(|| address.strip_prefix("ipc://"))
        .ok_or_else(|| ServerError::Config(format!("invalid IPC address: {address}")))?;
    if region.is_empty() {
        return Err(ServerError::Config("empty region ID".into()));
    }
    Ok(PathBuf::from(IPC_SOCK_DIR).join(format!("{region}.sock")))
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

enum SignalAction {
    Continue,
    Disconnect,
    Shutdown,
}

async fn handle_connection(server: Arc<Server>, stream: UnixStream) {
    let conn_id = server.conn_counter.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(Connection::new(conn_id));

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
        if reader.read_exact(&mut len_buf).await.is_err() {
            break; // EOF or broken pipe
        }
        let total_len = u32::from_le_bytes(len_buf);

        if total_len < 12 || (total_len as u64) > max_frame {
            warn!(conn_id, total_len, "invalid frame length");
            break;
        }

        // 2. Read body (request_id + flags + payload).
        let mut body = vec![0u8; total_len as usize];
        if reader.read_exact(&mut body).await.is_err() {
            break;
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

        // 4. Dispatch by frame type.
        if header.is_handshake() {
            if let Err(e) = handle_handshake(&server, &conn, payload, request_id, &writer).await {
                warn!(conn_id, %e, "handshake failed");
                break;
            }
        } else if header.is_signal() {
            match handle_signal(payload, request_id, &writer, &server).await {
                SignalAction::Continue => {}
                SignalAction::Disconnect | SignalAction::Shutdown => break,
            }
        } else if header.is_ctrl() {
            debug!(conn_id, "ctrl frame ignored");
        } else if header.is_call_v2() {
            if c2_wire::flags::is_chunked(flags) {
                let srv = Arc::clone(&server);
                let cn = Arc::clone(&conn);
                let wr = Arc::clone(&writer);
                let pl = payload.to_vec();
                tokio::spawn(async move {
                    dispatch_chunked_call(&srv, &cn, request_id, flags, &pl, &wr).await;
                });
                continue;
            }
            if c2_wire::flags::is_buddy(flags) {
                let srv = Arc::clone(&server);
                let cn = Arc::clone(&conn);
                let wr = Arc::clone(&writer);
                let pl = payload.to_vec();
                tokio::spawn(async move {
                    dispatch_buddy_call(&srv, &cn, request_id, &pl, &wr).await;
                });
                continue;
            }

            let srv = Arc::clone(&server);
            let cn = Arc::clone(&conn);
            let wr = Arc::clone(&writer);
            let pl = payload.to_vec();
            tokio::spawn(async move {
                dispatch_call(&srv, &cn, request_id, &pl, &wr).await;
            });
        } else {
            warn!(conn_id, flags, "unknown frame type");
        }
    }

    debug!(conn_id, "connection closing, draining in-flight");
    hb_handle.abort();
    conn.wait_idle().await;
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
        let pool = server.response_pool.read().unwrap();
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
    let hs_bytes = encode_server_handshake(&server_segments, cap, &routes, &server_prefix);
    let frame = encode_frame(request_id, FLAG_HANDSHAKE | FLAG_RESPONSE, &hs_bytes);

    writer.lock().await.write_all(&frame).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

async fn handle_signal(
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    server: &Server,
) -> SignalAction {
    let sig = match payload.first().and_then(|&b| MsgType::from_byte(b)) {
        Some(s) => s,
        None => return SignalAction::Continue,
    };

    let (reply, action) = match sig {
        MsgType::Ping => (&PONG_BYTES[..], SignalAction::Continue),
        MsgType::ShutdownClient => (&SHUTDOWN_ACK_BYTES[..], SignalAction::Shutdown),
        MsgType::Disconnect => (&DISCONNECT_ACK_BYTES[..], SignalAction::Disconnect),
        _ => return SignalAction::Continue,
    };

    let frame = encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, reply);
    let _ = writer.lock().await.write_all(&frame).await;

    if matches!(action, SignalAction::Shutdown) {
        server.shutdown();
    }
    action
}

// ---------------------------------------------------------------------------
// CRM call dispatch (inline, non-buddy, non-chunked)
// ---------------------------------------------------------------------------

async fn dispatch_call(
    server: &Server,
    conn: &Connection,
    request_id: u64,
    payload: &[u8],
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) {
    conn.flight_inc();

    let (ctrl, consumed) = match decode_call_control(payload, 0) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "call control decode error");
            conn.flight_dec();
            return;
        }
    };

    let route = server.dispatcher.read().await.resolve(&ctrl.route_name);
    let route = match route {
        Some(r) => r,
        None => {
            let msg = format!("route not found: {}", ctrl.route_name);
            write_reply(writer, request_id, &ReplyControl::Error(msg.into_bytes())).await;
            conn.flight_dec();
            return;
        }
    };

    let callback = Arc::clone(&route.callback);
    let name = ctrl.route_name;
    let idx = ctrl.method_idx;
    let args = payload[consumed..].to_vec();
    let resp_pool = Arc::clone(&server.response_pool);

    let result = route
        .scheduler
        .execute(idx, move || callback.invoke(&name, idx, RequestData::Inline(args), resp_pool))
        .await;

    match result {
        Ok(meta) => send_response_meta(
            &server.response_pool, writer, request_id, meta,
            server.config.shm_threshold, server.config.reply_chunk_size,
        ).await,
        Err(CrmError::UserError(b)) => {
            write_reply(writer, request_id, &ReplyControl::Error(b)).await;
        }
        Err(CrmError::InternalError(s)) => {
            write_reply(writer, request_id, &ReplyControl::Error(s.into_bytes())).await;
        }
    }

    conn.flight_dec();
}

// ---------------------------------------------------------------------------
// CRM call dispatch — buddy SHM
// ---------------------------------------------------------------------------

async fn dispatch_buddy_call(
    server: &Server,
    conn: &Arc<Connection>,
    request_id: u64,
    payload: &[u8],
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) {
    conn.flight_inc();

    // 1. Decode buddy pointer (11 bytes).
    let (bp, _bp_consumed) = match decode_buddy_payload(payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "buddy payload decode error");
            conn.flight_dec();
            return;
        }
    };

    // 2. Decode call control (route_name + method_idx) after buddy header.
    let (ctrl, ctrl_consumed) = match decode_call_control(payload, BUDDY_PAYLOAD_SIZE) {
        Ok(v) => v,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), ?e, "buddy call control decode error");
            conn.flight_dec();
            return;
        }
    };

    // 3. Read data from peer SHM.
    let args = match conn.read_peer_data(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated) {
        Ok(data) => data,
        Err(e) => {
            warn!(conn_id = conn.conn_id(), %e, "buddy SHM read failed");
            let msg = format!("buddy SHM read: {e}");
            write_reply(writer, request_id, &ReplyControl::Error(msg.into_bytes())).await;
            conn.flight_dec();
            return;
        }
    };

    // 4. Free the client's allocation.
    let free_result = conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
    if let c2_mem::FreeResult::SegmentIdle { .. } = free_result {
        let conn2 = Arc::clone(&conn);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            conn2.gc_peer_buddy();
        });
    }

    // 5. Check for inline args appended after control header.
    let inline_start = BUDDY_PAYLOAD_SIZE + ctrl_consumed;
    let extra_args = if inline_start < payload.len() {
        &payload[inline_start..]
    } else {
        &[]
    };

    // Combine SHM data with any trailing inline data.
    let full_args = if extra_args.is_empty() {
        args
    } else {
        let mut combined = args;
        combined.extend_from_slice(extra_args);
        combined
    };

    // 6. Route + execute.
    let route = server.dispatcher.read().await.resolve(&ctrl.route_name);
    let route = match route {
        Some(r) => r,
        None => {
            let msg = format!("route not found: {}", ctrl.route_name);
            write_reply(writer, request_id, &ReplyControl::Error(msg.into_bytes())).await;
            conn.flight_dec();
            return;
        }
    };

    let callback = Arc::clone(&route.callback);
    let name = ctrl.route_name;
    let idx = ctrl.method_idx;
    let resp_pool = Arc::clone(&server.response_pool);

    let result = route
        .scheduler
        .execute(idx, move || callback.invoke(&name, idx, RequestData::Inline(full_args), resp_pool))
        .await;

    match result {
        Ok(meta) => send_response_meta(
            &server.response_pool, writer, request_id, meta,
            server.config.shm_threshold, server.config.reply_chunk_size,
        ).await,
        Err(CrmError::UserError(b)) => {
            write_reply(writer, request_id, &ReplyControl::Error(b)).await;
        }
        Err(CrmError::InternalError(s)) => {
            write_reply(writer, request_id, &ReplyControl::Error(s.into_bytes())).await;
        }
    }

    conn.flight_dec();
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
) {
    let is_buddy = c2_wire::flags::is_buddy(flags);
    let mut offset: usize = 0;

    // 1. If buddy-backed chunk, read data from SHM first.
    let shm_data: Option<Vec<u8>>;
    if is_buddy {
        let (bp, bp_consumed) = match decode_buddy_payload(payload) {
            Ok(v) => v,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), ?e, "chunked buddy decode error");
                return;
            }
        };
        offset = bp_consumed;
        match conn.read_peer_data(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated) {
            Ok(data) => {
                let free_result = conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
                if let c2_mem::FreeResult::SegmentIdle { .. } = free_result {
                    let conn2 = Arc::clone(&conn);
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        conn2.gc_peer_buddy();
                    });
                }
                shm_data = Some(data);
            }
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunked SHM read failed");
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
            return;
        }
    };
    offset += ch_consumed;

    // 3. On first chunk, decode call control and create assembler.
    if chunk_idx == 0 {
        let (ctrl, ctrl_consumed) = match decode_call_control(payload, offset) {
            Ok(v) => v,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), ?e, "chunked call control decode error");
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
            warn!(conn_id = conn.conn_id(), "chunked: first chunk has zero data");
            return;
        }

        let asm = {
            let mut pool = server.reassembly_pool.write().unwrap();
            match c2_wire::assembler::ChunkAssembler::new(
                &mut pool,
                total_chunks as usize,
                chunk_size,
            ) {
                Ok(mut a) => {
                    a.route_name = Some(ctrl.route_name);
                    a.method_idx = Some(ctrl.method_idx);
                    a
                }
                Err(e) => {
                    warn!(conn_id = conn.conn_id(), %e, "chunk assembler creation failed");
                    return;
                }
            }
        };
        conn.insert_assembler(request_id, asm);
    }

    // 4. Get chunk data.
    let chunk_data: &[u8] = if let Some(ref sd) = shm_data {
        sd.as_slice()
    } else {
        &payload[offset..]
    };

    // 5. Feed chunk to assembler.
    let complete = {
        let pool = server.reassembly_pool.read().unwrap();
        match conn.feed_chunk(request_id, &pool, chunk_idx as usize, chunk_data) {
            Ok(complete) => complete,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunk feed error");
                return;
            }
        }
    };

    // 6. If complete, extract data and dispatch.
    if complete {
        let asm = match conn.take_assembler(request_id) {
            Some(a) => a,
            None => {
                warn!(conn_id = conn.conn_id(), "assembler missing after completion");
                return;
            }
        };
        let route_name = asm.route_name.clone().unwrap_or_default();
        let method_idx = asm.method_idx.unwrap_or(0);

        let handle = match asm.finish() {
            Ok(h) => h,
            Err(e) => {
                warn!(conn_id = conn.conn_id(), %e, "chunk finish failed");
                return;
            }
        };

        let args = {
            let pool = server.reassembly_pool.read().unwrap();
            pool.handle_slice(&handle).to_vec()
        };
        {
            let mut pool = server.reassembly_pool.write().unwrap();
            pool.release_handle(handle);
        }

        // Now dispatch like a normal call.
        conn.flight_inc();

        let route = server.dispatcher.read().await.resolve(&route_name);
        let route = match route {
            Some(r) => r,
            None => {
                let msg = format!("route not found: {route_name}");
                write_reply(writer, request_id, &ReplyControl::Error(msg.into_bytes())).await;
                conn.flight_dec();
                return;
            }
        };

        let callback = Arc::clone(&route.callback);
        let name = route_name;
        let idx = method_idx;
        let resp_pool = Arc::clone(&server.response_pool);

        let result = route
            .scheduler
            .execute(idx, move || callback.invoke(&name, idx, RequestData::Inline(args), resp_pool))
            .await;

        match result {
            Ok(meta) => send_response_meta(
                &server.response_pool, writer, request_id, meta,
                server.config.shm_threshold, server.config.reply_chunk_size,
            ).await,
            Err(CrmError::UserError(b)) => {
                write_reply(writer, request_id, &ReplyControl::Error(b)).await;
            }
            Err(CrmError::InternalError(s)) => {
                write_reply(writer, request_id, &ReplyControl::Error(s.into_bytes())).await;
            }
        }

        conn.flight_dec();
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

const REPLY_FLAGS: u32 = FLAG_RESPONSE | FLAG_REPLY_V2;

async fn write_reply(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, ctrl: &ReplyControl) {
    let payload = encode_reply_control(ctrl);
    let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
}

/// Write a success reply: control header (STATUS_SUCCESS) + result data.
/// Uses stack buffer for small responses (≤1024B total frame) to avoid heap allocation.
async fn write_reply_with_data(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, data: &[u8]) {
    let ctrl_bytes = encode_reply_control(&ReplyControl::Success);
    let payload_len = ctrl_bytes.len() + data.len();
    let total_len = (12 + payload_len) as u32;
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
        let _ = writer.lock().await.write_all(&buf[..off]).await;
    } else {
        // Large response: heap Vec (existing path)
        let mut payload = Vec::with_capacity(ctrl_bytes.len() + data.len());
        payload.extend_from_slice(&ctrl_bytes);
        payload.extend_from_slice(data);
        let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
        let _ = writer.lock().await.write_all(&frame).await;
    }
}

/// Write a success reply via buddy SHM: allocate from response pool, write
/// data, send 11-byte pointer frame. Falls back to inline on alloc failure.
async fn write_buddy_reply_with_data(
    response_pool: &std::sync::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
) -> Result<(), String> {
    // 1. Allocate from response pool.
    let alloc = {
        let mut pool = response_pool.write().unwrap();
        pool.alloc(data.len())
    };
    let alloc = match alloc {
        Ok(a) => a,
        Err(e) => {
            return Err(format!("alloc failed: {e}"));
        }
    };

    // 2. Write data to SHM (single lock scope).
    let write_ok = {
        let pool = response_pool.read().unwrap();
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
            let mut pool = response_pool.write().unwrap();
            let _ = pool.free(&alloc);
        }
        return Err("data_ptr failed".into());
    }

    // 3. Encode buddy payload + reply control.
    let bp = BuddyPayload {
        seg_idx: alloc.seg_idx as u16,
        offset: alloc.offset,
        data_size: data.len() as u32,
        is_dedicated: alloc.is_dedicated,
    };
    let buddy_bytes = encode_buddy_payload(&bp);
    let ctrl_bytes = encode_reply_control(&ReplyControl::Success);

    let mut payload = Vec::with_capacity(BUDDY_PAYLOAD_SIZE + ctrl_bytes.len());
    payload.extend_from_slice(&buddy_bytes);
    payload.extend_from_slice(&ctrl_bytes);

    // 4. Send frame with FLAG_BUDDY.
    let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY;
    let frame = encode_frame(request_id, flags, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
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
) {
    let total_chunks = ((data.len() + chunk_size - 1) / chunk_size) as u32;
    let total_size = data.len() as u64;

    for (idx, chunk) in data.chunks(chunk_size).enumerate() {
        let meta = encode_reply_chunk_meta(total_size, total_chunks, idx as u32);
        let mut flags = REPLY_FLAGS | FLAG_CHUNKED;
        if idx as u32 == total_chunks - 1 {
            flags |= FLAG_CHUNK_LAST;
        }

        // Build frame: header + meta + chunk data
        let payload_len = REPLY_CHUNK_META_SIZE + chunk.len();
        let total_len = (12 + payload_len) as u32;

        let mut frame = Vec::with_capacity(frame::HEADER_SIZE + payload_len);
        frame.extend_from_slice(&total_len.to_le_bytes());
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&flags.to_le_bytes());
        frame.extend_from_slice(&meta);
        frame.extend_from_slice(chunk);

        let _ = writer.lock().await.write_all(&frame).await;
    }
}

/// Dispatch a `ResponseMeta` to the appropriate reply path.
async fn send_response_meta(
    response_pool: &std::sync::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    meta: ResponseMeta,
    shm_threshold: u64,
    chunk_size: usize,
) {
    match meta {
        ResponseMeta::Inline(data) => {
            smart_reply_with_data(
                response_pool, writer, request_id, &data, shm_threshold, chunk_size,
            ).await;
        }
        ResponseMeta::Empty => {
            write_reply_with_data(writer, request_id, &[]).await;
        }
        ResponseMeta::ShmAlloc { seg_idx, offset, data_size, is_dedicated } => {
            // CRM already wrote into our response pool — send buddy pointer.
            let bp = BuddyPayload { seg_idx, offset, data_size, is_dedicated };
            let buddy_bytes = encode_buddy_payload(&bp);
            let ctrl_bytes = encode_reply_control(&ReplyControl::Success);
            let mut payload = Vec::with_capacity(BUDDY_PAYLOAD_SIZE + ctrl_bytes.len());
            payload.extend_from_slice(&buddy_bytes);
            payload.extend_from_slice(&ctrl_bytes);
            let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY;
            let frame = encode_frame(request_id, flags, &payload);
            let _ = writer.lock().await.write_all(&frame).await;
        }
    }
}

/// Choose buddy SHM or inline reply based on data size and threshold.
async fn smart_reply_with_data(
    response_pool: &std::sync::RwLock<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    shm_threshold: u64,
    chunk_size: usize,
) {
    if data.len() as u64 > shm_threshold {
        // Try buddy SHM first
        if write_buddy_reply_with_data(response_pool, writer, request_id, data).await.is_err() {
            // SHM failed — use chunked for large data, inline for small
            if data.len() > chunk_size {
                write_chunked_reply(writer, request_id, data, chunk_size).await;
            } else {
                write_reply_with_data(writer, request_id, data).await;
            }
        }
    } else {
        write_reply_with_data(writer, request_id, data).await;
    }
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
    fn parse_ipc_v3_address() {
        let p = parse_socket_path("ipc-v3://region42").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/c_two_ipc/region42.sock"));
    }

    #[test]
    fn parse_invalid_scheme() {
        assert!(parse_socket_path("tcp://host").is_err());
    }

    #[test]
    fn parse_empty_region() {
        assert!(parse_socket_path("ipc://").is_err());
    }

    // -- server construction --

    #[test]
    fn server_new_default_config() {
        let s = Server::new("ipc://test_srv", IpcConfig::default()).unwrap();
        assert_eq!(s.socket_path(), Path::new("/tmp/c_two_ipc/test_srv.sock"));
    }

    #[test]
    fn server_new_bad_address() {
        assert!(Server::new("http://bad", IpcConfig::default()).is_err());
    }

    #[test]
    fn server_new_bad_config() {
        let cfg = IpcConfig { max_payload_size: 0, ..IpcConfig::default() };
        assert!(Server::new("ipc://x", cfg).is_err());
    }

    // -- route registration --

    struct Echo;
    impl CrmCallback for Echo {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: RequestData,
            _response_pool: Arc<std::sync::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"echo".to_vec()))
        }
    }

    fn make_route(name: &str) -> CrmRoute {
        CrmRoute {
            name: name.into(),
            scheduler: Arc::new(Scheduler::new(ConcurrencyMode::ReadParallel, HashMap::new())),
            callback: Arc::new(Echo),
            method_names: vec!["step".into(), "query".into()],
        }
    }

    #[tokio::test]
    async fn register_unregister_route() {
        let s = Arc::new(Server::new("ipc://reg_test", IpcConfig::default()).unwrap());

        s.register_route(make_route("grid")).await;
        assert!(s.dispatcher.read().await.resolve("grid").is_some());

        assert!(s.unregister_route("grid").await);
        assert!(s.dispatcher.read().await.resolve("grid").is_none());
    }

    // -- shutdown --

    #[tokio::test]
    async fn shutdown_sets_signal() {
        let s = Server::new("ipc://shut_test", IpcConfig::default()).unwrap();
        let mut rx = s.shutdown_rx.clone();
        s.shutdown();
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
        use c2_wire::buddy::{encode_buddy_payload, BuddyPayload, BUDDY_PAYLOAD_SIZE};
        use c2_wire::control::encode_call_control;

        let bp = BuddyPayload {
            seg_idx: 0,
            offset: 4096,
            data_size: 256,
            is_dedicated: false,
        };
        let bp_bytes = encode_buddy_payload(&bp);
        let ctrl_bytes = encode_call_control("grid", 1);

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

    // -- chunked reassembly via connection --

    #[test]
    fn chunked_reassembly_via_connection() {
        let conn = Connection::new(99);

        let reassembly_cfg = c2_mem::config::PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_gc_delay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_srv_chunk_test"),
        };
        let mut pool = c2_mem::MemPool::new(reassembly_cfg);

        let request_id = 42u64;
        let total_chunks = 3usize;
        let chunk_size = 8usize;

        // Create assembler.
        let mut asm =
            c2_wire::assembler::ChunkAssembler::new(&mut pool, total_chunks, chunk_size).unwrap();
        asm.route_name = Some("grid".into());
        asm.method_idx = Some(0);
        conn.insert_assembler(request_id, asm);

        // Feed chunks.
        assert!(!conn.feed_chunk(request_id, &pool, 0, b"aaaaaaaa").unwrap());
        assert!(!conn.feed_chunk(request_id, &pool, 1, b"bbbbbbbb").unwrap());
        assert!(conn.feed_chunk(request_id, &pool, 2, b"cc").unwrap());

        // Take and finish.
        let asm = conn.take_assembler(request_id).unwrap();
        assert_eq!(asm.route_name.as_deref(), Some("grid"));
        assert_eq!(asm.method_idx, Some(0));
        let handle = asm.finish().unwrap();
        assert_eq!(handle.len(), 18); // 8+8+2
        let slice = pool.handle_slice(&handle);
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        pool.release_handle(handle);
    }

    // -- handshake extraction --

    #[test]
    fn handshake_extracts_client_info() {
        use c2_wire::handshake::{encode_client_handshake, decode_handshake, CAP_CALL_V2, CAP_CHUNKED};

        let segments = vec![("seg0".into(), 4096u32), ("seg1".into(), 8192u32)];
        let cap = CAP_CALL_V2 | CAP_CHUNKED;
        let hs_bytes = encode_client_handshake(&segments, cap, "/cc3b_test");

        let decoded = decode_handshake(&hs_bytes).unwrap();
        assert_eq!(decoded.prefix, "/cc3b_test");
        assert_eq!(decoded.segments.len(), 2);
        assert_eq!(decoded.segments[0].0, "seg0");
        assert_eq!(decoded.segments[0].1, 4096);
        assert_eq!(decoded.capability_flags & CAP_CHUNKED, CAP_CHUNKED);
    }
}
