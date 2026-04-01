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

use c2_wire::control::{decode_call_control, encode_reply_control, ReplyControl};
use c2_wire::flags::{FLAG_HANDSHAKE, FLAG_RESPONSE, FLAG_SIGNAL};
use c2_wire::frame::{decode_frame_body, encode_frame};
use c2_wire::handshake::{
    decode_handshake, encode_server_handshake, MethodEntry, RouteInfo, CAP_CALL_V2, CAP_METHOD_IDX,
};
use c2_wire::msg_type::{MsgType, DISCONNECT_ACK_BYTES, PONG_BYTES, SHUTDOWN_ACK_BYTES};

use crate::config::IpcConfig;
use crate::connection::Connection;
use crate::dispatcher::{CrmError, CrmRoute, Dispatcher};
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
        Ok(Self {
            config,
            socket_path,
            dispatcher: RwLock::new(Dispatcher::new()),
            shutdown_tx,
            shutdown_rx,
            conn_counter: AtomicU64::new(0),
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
            if let Err(e) = handle_handshake(&server, payload, request_id, &writer).await {
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
                // TODO: chunked reassembly (Phase 3)
                warn!(conn_id, "chunked call not yet supported, skipping");
                continue;
            }
            if c2_wire::flags::is_buddy(flags) {
                // TODO: buddy SHM resolution (Phase 3)
                warn!(conn_id, "buddy call not yet supported, skipping");
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
    payload: &[u8],
    request_id: u64,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> Result<(), ServerError> {
    let _client_hs = decode_handshake(payload)
        .map_err(|e| ServerError::Protocol(format!("handshake decode: {e:?}")))?;

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

    let cap = CAP_CALL_V2 | CAP_METHOD_IDX;
    let hs_bytes = encode_server_handshake(&[], cap, &routes, "server");
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

    let result = route
        .scheduler
        .execute(idx, move || callback.invoke(&name, idx, &args))
        .await;

    match result {
        Ok(data) => write_reply_with_data(writer, request_id, &data).await,
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
// Reply helpers
// ---------------------------------------------------------------------------

async fn write_reply(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, ctrl: &ReplyControl) {
    let payload = encode_reply_control(ctrl);
    let frame = encode_frame(request_id, FLAG_RESPONSE, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
}

/// Write a success reply: control header (STATUS_SUCCESS) + result data.
async fn write_reply_with_data(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, data: &[u8]) {
    let ctrl_bytes = encode_reply_control(&ReplyControl::Success);
    let mut payload = Vec::with_capacity(ctrl_bytes.len() + data.len());
    payload.extend_from_slice(&ctrl_bytes);
    payload.extend_from_slice(data);
    let frame = encode_frame(request_id, FLAG_RESPONSE, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
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
        fn invoke(&self, _: &str, _: u16, p: &[u8]) -> Result<Vec<u8>, CrmError> {
            Ok(p.to_vec())
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
}
