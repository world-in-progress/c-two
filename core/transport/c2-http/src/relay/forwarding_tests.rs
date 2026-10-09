//! Actual HTTP/1 disconnects against Axum and a shared live IPC connection.
//! No cancellation is injected into IPC: a disconnected HTTP waiter must leave
//! the native transaction, request owner and response disposal alive.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use c2_config::{CallExecutionLimits, ClientIpcConfig, LocalEndpointContext, RelayConfig};
use c2_ipc::{IpcClient, RouteBinding};
use c2_mem::MemPool;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
    RouteBuildSpec, SchedulerLimits, Server, ServerIdentity, ServerIpcConfig,
};
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::router::build_router;
use super::state::RelayState;
use super::test_support::{NoopDisseminator, TEST_ABI_HASH, TEST_SIGNATURE_HASH};
use super::types::RouteEntry;

const INPUT_LEN: usize = 8192;
const STEP: Duration = Duration::from_secs(10);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("real relay forwarding step exceeded 10s")
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    bounded(async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

fn unique_id() -> String {
    format!(
        "rf_{}_{}_{:x}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Carrier {
    Inline,
    Buddy,
    Dedicated,
    File,
    Chunked,
}

fn policy(carrier: Carrier) -> ClientIpcConfig {
    let mut config = ClientIpcConfig::default();
    config.base.pool_segment_size = 64 * 1024;
    config.base.max_pool_segments = 1;
    config.base.max_pool_memory = 64 * 1024;
    config.base.pool_enabled = carrier == Carrier::Buddy;
    config.base.pool_prewarm_segments = 0;
    config.base.chunk_size = if carrier == Carrier::Inline {
        16 * 1024
    } else {
        4096
    };
    config.base.shm_backing_budget_bytes = if matches!(carrier, Carrier::Buddy | Carrier::Dedicated)
    {
        1024 * 1024
    } else {
        0
    };
    config.base.file_backing_budget_bytes = if carrier == Carrier::File {
        4 * INPUT_LEN as u64
    } else {
        0
    };
    config.base.live_reassembly_budget_bytes = 4 * INPUT_LEN as u64;
    config.shm_threshold = if carrier == Carrier::Inline {
        64 * 1024
    } else {
        1
    };
    config.validate().unwrap();
    config
}

struct Probe {
    carrier: Carrier,
    input_len: usize,
    calls: [AtomicUsize; 3],
    entered: [Notify; 2],
    release: [parking_lot::Mutex<Option<std::sync::mpsc::Receiver<()>>>; 2],
    input_kind: AtomicUsize,
    response_kind: AtomicUsize,
    response_pool: parking_lot::Mutex<Option<Arc<parking_lot::RwLock<MemPool>>>>,
}

impl CrmCallback for Probe {
    fn invoke(
        &self,
        _: &str,
        method: u16,
        input: RequestData,
        response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        let kind = match &input {
            RequestData::Inline(_) => 1,
            RequestData::Shm {
                is_dedicated: false,
                ..
            } => 2,
            RequestData::Shm {
                is_dedicated: true, ..
            } => 3,
            RequestData::Handle(_) => 4,
        };
        let mut owner = RequestLease::new(input);
        let bytes = owner.copy_bytes().map_err(CrmError::InternalError)?;
        assert_eq!(bytes, vec![7; self.input_len]);
        self.calls[method as usize].fetch_add(1, Ordering::SeqCst);
        if method < 2 {
            if method == 1 {
                self.input_kind.store(kind, Ordering::SeqCst);
            }
            self.entered[method as usize].notify_one();
            if let Some(release) = self.release[method as usize].lock().take() {
                release
                    .recv_timeout(STEP)
                    .map_err(|e| CrmError::InternalError(e.to_string()))?;
            }
        }
        // Keep the actual server request owner until the controlled callback
        // exits. No observation or HTTP waiter departure refunds this owner.
        owner.release().map_err(CrmError::InternalError)?;
        if method != 1 || matches!(self.carrier, Carrier::Inline | Carrier::Chunked) {
            return Ok(ResponseMeta::Inline(vec![method as u8; 32]));
        }
        *self.response_pool.lock() = Some(response_pool.clone());
        if matches!(self.carrier, Carrier::Buddy | Carrier::Dedicated) {
            let prepared = c2_server::response::try_prepare_shm_response(
                &response_pool,
                1,
                INPUT_LEN,
                |output| {
                    output.fill(1);
                    Ok(())
                },
            )
            .map_err(CrmError::InternalError)?
            .expect("selected SHM response carrier must actually allocate");
            if let ResponseMeta::ShmAlloc { is_dedicated, .. } = &prepared {
                self.response_kind
                    .store(if *is_dedicated { 3 } else { 2 }, Ordering::SeqCst);
            } else {
                panic!("response helper did not produce SHM carrier");
            }
            Ok(prepared)
        } else {
            // The server cannot allocate SHM; production framing selects
            // chunked delivery, whose relay receive owner must use file spill.
            Ok(ResponseMeta::Inline(vec![1; INPUT_LEN]))
        }
    }
}

// Test client framing, independent of transport EOF: an early rejection may
// close with unread request bytes. Never accept a reset or EOF before a complete
// bounded response, and never consume EOF after its declared message boundary.
const MAX_REPLY: usize = 64 * 1024;

fn invalid_reply(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

async fn append_reply<R: tokio::io::AsyncRead + Unpin>(
    source: &mut R,
    reply: &mut Vec<u8>,
    count: usize,
) -> std::io::Result<()> {
    let end = reply
        .len()
        .checked_add(count)
        .filter(|end| *end <= MAX_REPLY)
        .ok_or_else(|| invalid_reply("response exceeds test client framing limit"))?;
    while reply.len() < end {
        let mut chunk = [0; 4096];
        let limit = (end - reply.len()).min(chunk.len());
        let size = source.read(&mut chunk[..limit]).await.map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "incomplete response after {} actual bytes: {error}; received={:?}",
                    reply.len(),
                    String::from_utf8_lossy(reply)
                ),
            )
        })?;
        if size == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete HTTP response",
            ));
        }
        reply.extend_from_slice(&chunk[..size]);
    }
    Ok(())
}

async fn reply_line<R: tokio::io::AsyncRead + Unpin>(
    source: &mut R,
    reply: &mut Vec<u8>,
) -> std::io::Result<Vec<u8>> {
    let start = reply.len();
    loop {
        append_reply(source, reply, 1).await?;
        if reply[start..].ends_with(b"\r\n") {
            return Ok(reply[start..reply.len() - 2].to_vec());
        }
        if reply.len() - start > 16 * 1024 {
            return Err(invalid_reply("oversized HTTP line"));
        }
    }
}

fn reply_header(line: &[u8]) -> std::io::Result<(&str, &str)> {
    let line = std::str::from_utf8(line).map_err(|_| invalid_reply("non-ASCII HTTP header"))?;
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| invalid_reply("malformed HTTP header"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        || !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b))
    {
        return Err(invalid_reply("invalid HTTP header bytes"));
    }
    Ok((name, value.trim()))
}

async fn read_framed_reply<R: tokio::io::AsyncRead + Unpin>(
    source: &mut R,
) -> std::io::Result<Vec<u8>> {
    let mut reply = Vec::new();
    let status = reply_line(source, &mut reply).await?;
    let status = std::str::from_utf8(&status).map_err(|_| invalid_reply("invalid status line"))?;
    let fields: Vec<_> = status.splitn(3, ' ').collect();
    if fields.len() != 3
        || fields[0] != "HTTP/1.1"
        || fields[1].len() != 3
        || !fields[1].bytes().all(|b| b.is_ascii_digit())
        || !(200..600).contains(&fields[1].parse::<u16>().unwrap())
    {
        return Err(invalid_reply("unsupported HTTP response status"));
    }
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = reply_line(source, &mut reply).await?;
        if reply.len() > 16 * 1024 {
            return Err(invalid_reply("oversized response headers"));
        }
        if line.is_empty() {
            break;
        }
        let (name, value) = reply_header(&line)?;
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid_reply("invalid or duplicate Content-Length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| invalid_reply("Content-Length overflow"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(invalid_reply("unsupported or duplicate Transfer-Encoding"));
            }
            chunked = true;
        }
    }
    if chunked && length.is_some() {
        return Err(invalid_reply("ambiguous response framing"));
    }
    if let Some(length) = length {
        append_reply(source, &mut reply, length).await?;
    } else if chunked {
        loop {
            let line = reply_line(source, &mut reply).await?;
            if line.is_empty() || !line.iter().all(u8::is_ascii_hexdigit) {
                return Err(invalid_reply("invalid or unsupported chunk size"));
            }
            let size = usize::from_str_radix(std::str::from_utf8(&line).unwrap(), 16)
                .map_err(|_| invalid_reply("chunk size overflow"))?;
            if size == 0 {
                loop {
                    let trailer = reply_line(source, &mut reply).await?;
                    if trailer.is_empty() {
                        break;
                    }
                    let (name, _) = reply_header(&trailer)?;
                    if name.eq_ignore_ascii_case("content-length")
                        || name.eq_ignore_ascii_case("transfer-encoding")
                    {
                        return Err(invalid_reply("framing header in trailer"));
                    }
                }
                break;
            }
            append_reply(source, &mut reply, size).await?;
            let line = reply_line(source, &mut reply).await?;
            if !line.is_empty() {
                return Err(invalid_reply("missing chunk terminator"));
            }
        }
    } else {
        return Err(invalid_reply("response lacks explicit framing"));
    }
    Ok(reply)
}

#[tokio::test]
async fn response_reader_requires_complete_bounded_http_framing_before_reset() {
    struct ResetAfter<'a>(&'a [u8]);
    impl tokio::io::AsyncRead for ResetAfter<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.0.is_empty() {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()));
            }
            let size = buffer.remaining().min(self.0.len());
            buffer.put_slice(&self.0[..size]);
            self.0 = &self.0[size..];
            std::task::Poll::Ready(Ok(()))
        }
    }
    for complete in [
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 3\r\n\r\nabc".as_slice(),
        b"HTTP/1.1 502 Bad Gateway\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
            .as_slice(),
    ] {
        assert_eq!(
            read_framed_reply(&mut ResetAfter(complete)).await.unwrap(),
            complete
        );
    }
    for incomplete in [
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 3\r\n".as_slice(),
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 3\r\n\r\nab".as_slice(),
        b"HTTP/1.1 502 Bad Gateway\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n"
            .as_slice(),
    ] {
        assert!(
            read_framed_reply(&mut ResetAfter(incomplete))
                .await
                .is_err()
        );
        assert!(read_framed_reply(&mut &incomplete[..]).await.is_err());
    }
    for invalid in [
        b"Content-Length: 999999999999999999999999999999".as_slice(),
        b"Content-Length: 65536",
        b"Content-Length: +3",
        b"Content-Length: 3\r\nContent-Length: 3",
        b"Content-Length: 3\r\nTransfer-Encoding: chunked",
        b"Transfer-Encoding: gzip",
        b"Transfer-Encoding: chunked\r\n\r\nX\r\n",
    ] {
        let mut wire = b"HTTP/1.1 502 Bad Gateway\r\n".to_vec();
        wire.extend_from_slice(invalid);
        wire.extend_from_slice(b"\r\n\r\nabc");
        assert!(read_framed_reply(&mut &wire[..]).await.is_err());
    }
}

struct Fixture {
    state: Arc<RelayState>,
    server: Arc<Server>,
    ipc_task: JoinHandle<Result<(), c2_server::server::ServerError>>,
    http_task: JoinHandle<std::io::Result<()>>,
    client_observer: JoinHandle<()>,
    http_stop: oneshot::Sender<()>,
    socket: std::net::SocketAddr,
    client: Arc<IpcClient>,
    binding: RouteBinding,
    entry: RouteEntry,
    probe: Arc<Probe>,
    release: [Option<std::sync::mpsc::Sender<()>>; 2],
}

impl Fixture {
    async fn start(carrier: Carrier, operations: u64, bytes: u64) -> Self {
        Self::start_with_input_len(carrier, operations, bytes, INPUT_LEN).await
    }

    async fn start_with_input_len(
        carrier: Carrier,
        operations: u64,
        bytes: u64,
        input_len: usize,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = listener.local_addr().unwrap();
        let url = format!("http://{socket}");
        let state = Arc::new(RelayState::new_with_execution_limits(
            Arc::new(RelayConfig {
                relay_id: unique_id(),
                idle_timeout_secs: 0,
                advertise_url: url.clone(),
                upstream_ipc: policy(carrier),
                ..RelayConfig::default()
            }),
            Arc::new(NoopDisseminator),
            LocalEndpointContext::default_for_platform().unwrap(),
            CallExecutionLimits {
                max_outstanding_calls: operations,
                retained_input_budget_bytes: bytes,
            },
        ));
        let id = unique_id();
        let address = format!("ipc://{id}");
        let mut config = ServerIpcConfig::default();
        config.base = policy(carrier).base;
        // Chunked requests still receive real backing on the server; only
        // relay request backing is denied to force the streaming wire path.
        if carrier == Carrier::Chunked {
            config.base.file_backing_budget_bytes = 4 * INPUT_LEN as u64;
        }
        config.max_execution_workers = 4;
        config.shm_threshold = if matches!(carrier, Carrier::Inline | Carrier::Chunked) {
            64 * 1024
        } else {
            1
        };
        config.validate().unwrap();
        let server = Arc::new(
            Server::new_with_identity(
                &address,
                config,
                ServerIdentity {
                    server_id: id.clone(),
                    server_instance_id: format!("{id}-instance"),
                },
            )
            .unwrap(),
        );
        let (other_tx, other_rx) = std::sync::mpsc::channel();
        let (target_tx, target_rx) = std::sync::mpsc::channel();
        let probe = Arc::new(Probe {
            carrier,
            input_len,
            calls: std::array::from_fn(|_| AtomicUsize::new(0)),
            entered: std::array::from_fn(|_| Notify::new()),
            release: [
                parking_lot::Mutex::new(Some(other_rx)),
                parking_lot::Mutex::new(Some(target_rx)),
            ],
            input_kind: AtomicUsize::new(0),
            response_kind: AtomicUsize::new(0),
            response_pool: parking_lot::Mutex::new(None),
        });
        let built = server
            .build_route(
                RouteBuildSpec {
                    name: "grid".into(),
                    crm_ns: "test.echo".into(),
                    crm_name: "Echo".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["other".into(), "target".into(), "ping".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                probe.clone(),
            )
            .unwrap();
        let reserved = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reserved).await.unwrap();
        let running = server.clone();
        let ipc_task = tokio::spawn(async move { running.run().await });
        let clients = state.clients.clone();
        let client_observer = tokio::spawn(async move { clients.run().await });
        let (http_stop, stop) = oneshot::channel();
        let app = build_router(state.clone());
        let http_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
        });
        let acquired = AssertUnwindSafe(async {
            bounded(server.wait_until_responsive(Duration::from_secs(2))).await.unwrap();
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let registered = bounded(http.post(format!("{url}/_register")).json(&serde_json::json!({"name":"grid", "server_id":id, "server_instance_id":format!("{id}-instance"), "address":address, "max_payload_size":server.config().max_payload_size})).send()).await.unwrap();
            assert_eq!(registered.status(), StatusCode::CREATED);
            bounded(registered.bytes()).await.unwrap();
            let entry = state.local_route("grid").unwrap();
            let (lease, _, binding) = bounded(state.acquire_upstream_for_route(&entry)).await.unwrap_or_else(|_| panic!("real attested pooled IPC acquire failed"));
            let client = lease.client();
            drop(lease);
            (entry, client, binding)
        }).catch_unwind().await;
        let (entry, client, binding) = match acquired {
            Ok(value) => value,
            Err(panic) => {
                let _ = other_tx.send(());
                let _ = target_tx.send(());
                let _ = http_stop.send(());
                state.forwarding.close();
                bounded(state.forwarding.drain()).await;
                bounded(http_task).await.unwrap().unwrap();
                bounded(state.stop_upstream_controls()).await;
                bounded(state.clients.shutdown()).await;
                bounded(client_observer).await.unwrap();
                assert_eq!(state.clients.outstanding(), 0);
                bounded(server.shutdown_and_wait(Duration::from_secs(2)))
                    .await
                    .unwrap();
                bounded(ipc_task).await.unwrap().unwrap();
                std::panic::resume_unwind(panic);
            }
        };
        Self {
            state,
            server,
            ipc_task,
            http_task,
            client_observer,
            http_stop,
            socket,
            client,
            binding,
            entry,
            probe,
            release: [Some(other_tx), Some(target_tx)],
        }
    }

    fn head(&self, method: &str, framing: &str) -> String {
        format!(
            "POST /grid/{method} HTTP/1.1\r\nHost: {}\r\n{framing}\r\nConnection: close\r\nx-c2-expected-crm-ns: test.echo\r\nx-c2-expected-crm-name: Echo\r\nx-c2-expected-crm-ver: 0.1.0\r\nx-c2-expected-abi-hash: {TEST_ABI_HASH}\r\nx-c2-expected-signature-hash: {TEST_SIGNATURE_HASH}\r\nx-c2-route-uid: {}\r\nx-c2-route-revision: {}\r\n\r\n",
            self.socket, self.entry.route_uid, self.entry.route_revision
        )
    }

    async fn request(&self, method: &str, bytes: &[u8]) -> Vec<u8> {
        let mut connection = bounded(tokio::net::TcpStream::connect(self.socket))
            .await
            .unwrap();
        bounded(
            connection.write_all(
                self.head(method, &format!("Content-Length: {}", bytes.len()))
                    .as_bytes(),
            ),
        )
        .await
        .unwrap();
        bounded(connection.write_all(bytes)).await.unwrap();
        let reply = bounded(read_framed_reply(&mut connection)).await.unwrap();
        eprintln!(
            "[relay-forwarding] actual capacity response: {}",
            String::from_utf8_lossy(&reply)
        );
        reply
    }

    fn release(&mut self, index: usize) {
        if let Some(tx) = self.release[index].take() {
            let _ = tx.send(());
        }
    }

    async fn stop(mut self) {
        self.release(0);
        self.release(1);
        self.state.forwarding.close();
        bounded(self.state.forwarding.drain()).await;
        let _ = self.http_stop.send(());
        bounded(self.http_task).await.unwrap().unwrap();
        bounded(self.state.stop_upstream_controls()).await;
        // Mirror the native Relay shutdown authority. Cached and failed
        // candidate clients stay with their bounded observer until close is
        // confirmed; a fixture's manual close must not conceal lost ownership.
        bounded(self.state.clients.shutdown()).await;
        bounded(self.client_observer).await.unwrap();
        assert_eq!(self.state.clients.outstanding(), 0);
        assert!(!self.client.is_connected());
        bounded(self.server.shutdown_and_wait(Duration::from_secs(2)))
            .await
            .unwrap();
        bounded(self.ipc_task).await.unwrap().unwrap();
        assert!(!self.server.is_running());
    }
}

async fn disconnected_response_case(carrier: Carrier, operations: u64) {
    let mut fixture = Fixture::start(carrier, operations, INPUT_LEN as u64).await;
    let client = fixture.client.clone();
    let binding = fixture.binding.clone();
    let mut other = tokio::spawn(async move {
        client
            .call_bound_phased(&binding, "other", &vec![7; INPUT_LEN])
            .await
    });
    let mut other_joined = false;
    let result = AssertUnwindSafe(async {
        bounded(fixture.probe.entered[0].notified()).await;
        let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket)).await.unwrap();
        bounded(upload.write_all(fixture.head("target", &format!("Content-Length: {INPUT_LEN}")).as_bytes())).await.unwrap();
        bounded(upload.write_all(&vec![7; INPUT_LEN])).await.unwrap();
        bounded(fixture.probe.entered[1].notified()).await;
        let admitted = fixture.state.forwarding.snapshot();
        assert_eq!((admitted.used_operations, admitted.used_retained_bytes), (1, INPUT_LEN as u64));
        if carrier == Carrier::File {
            assert_eq!(fixture.state.upstream_memory_snapshot().reassembly.peak_bytes, 0, "only the later response may charge relay receive reassembly");
        }
        let detached = fixture.state.forwarding.detached_waiters();
        drop(upload); // Real HTTP/1 socket disconnection, after complete upload.
        eventually(|| fixture.state.forwarding.detached_waiters() > detached).await;
        assert_eq!((fixture.state.forwarding.snapshot().used_operations, fixture.state.forwarding.snapshot().used_retained_bytes), (1, INPUT_LEN as u64));
        assert!(fixture.client.is_connected(), "HTTP waiter drop must preserve original shared IPC incarnation");
        let rejected = fixture.request("ping", &[7; INPUT_LEN]).await;
        assert!(rejected.starts_with(b"HTTP/1.1 502 "), "capacity rejects before upstream dispatch: {}", String::from_utf8_lossy(&rejected));
        assert!(String::from_utf8_lossy(&rejected).contains("pre_dispatch"));
        assert_eq!(fixture.probe.calls[2].load(Ordering::SeqCst), 0);
        assert!(fixture.state.local_route("grid").is_some(), "capacity or disconnect must not withdraw route");
        assert!(fixture.state.evict_idle(0).iter().all(|(_, client)| client.is_none()), "detached native task must keep upstream lease active");
        fixture.state.forwarding.close();
        assert!(tokio::time::timeout(Duration::from_millis(30), fixture.state.forwarding.drain()).await.is_err(), "closed domain must observe pending native work");
        assert_eq!(fixture.state.forwarding.snapshot().used_operations, 1);
        fixture.release(1);
        bounded(fixture.state.forwarding.drain()).await;
        fixture.release(0);
        let other_result = bounded(&mut other).await;
        other_joined = true;
        let other_reply = other_result.unwrap().unwrap();
        assert_eq!(other_reply.into_bytes_with_pool(fixture.client.server_pool_arc()).unwrap(), vec![0; 32], "already-in-flight other RID must succeed on same IPC client");
        assert!(fixture.client.is_connected());
        let ping = bounded(fixture.client.call_bound_phased(&fixture.binding, "ping", &[7; INPUT_LEN])).await.unwrap();
        assert_eq!(ping.into_bytes_with_pool(fixture.client.server_pool_arc()).unwrap(), vec![2; 32], "same client ping must succeed without reconnect");
        let (lease, _, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry)).await.unwrap_or_else(|_| panic!("same pooled client acquire failed"));
        assert!(Arc::ptr_eq(&lease.client(), &fixture.client), "liveness cannot be masked by reconnecting");
        drop(lease);
        assert_eq!((fixture.state.forwarding.snapshot().used_operations, fixture.state.forwarding.snapshot().used_retained_bytes), (0, 0));
        assert_eq!(fixture.probe.calls.each_ref().map(|c| c.load(Ordering::SeqCst)), [1, 1, 1]);
        let expected_input = match carrier { Carrier::Inline => 1, Carrier::Buddy => 2, Carrier::Dedicated => 3, Carrier::File | Carrier::Chunked => 4 };
        assert_eq!(fixture.probe.input_kind.load(Ordering::SeqCst), expected_input, "real request carrier for {carrier:?}");
        if matches!(carrier, Carrier::Buddy | Carrier::Dedicated) {
            assert_eq!(fixture.probe.response_kind.load(Ordering::SeqCst), expected_input);
            let pool = fixture.probe.response_pool.lock().as_ref().unwrap().clone();
            eventually(|| pool.read().stats().alloc_count == 0).await;
            if carrier == Carrier::Dedicated {
                eventually(|| { pool.write().gc_dedicated(); fixture.server.memory_budget_snapshot().budget.shm.used_bytes == 0 }).await;
            }
        }
        let memory = fixture.state.upstream_memory_snapshot();
        assert_eq!((memory.file.used_bytes, memory.reassembly.used_bytes), (0, 0), "late reply carrier must really release before shutdown");
        if carrier == Carrier::File {
            assert!(memory.file.peak_bytes >= INPUT_LEN as u64, "actual late reply must exercise file/reassembly carrier");
            assert!(memory.reassembly.peak_bytes >= INPUT_LEN as u64);
        }
        eprintln!("[relay-forwarding] carrier={carrier:?} limit_operations={operations} detached={} retention={:?} memory={memory:?} callbacks={:?}", fixture.state.forwarding.detached_waiters(), fixture.state.forwarding.snapshot(), fixture.probe.calls.each_ref().map(|c| c.load(Ordering::SeqCst)));
    }).catch_unwind().await;
    // Release all callback barriers and join both actual listeners even when
    // an assertion fails; no unlimited background worker or process survives.
    fixture.release(0);
    fixture.release(1);
    if !other_joined {
        let _ = bounded(&mut other).await;
    }
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_keeps_shared_ipc_other_rid_and_late_inline_response() {
    disconnected_response_case(Carrier::Inline, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_keeps_byte_budget_and_late_buddy_response() {
    disconnected_response_case(Carrier::Buddy, 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_reclaims_late_dedicated_response() {
    disconnected_response_case(Carrier::Dedicated, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_reclaims_late_file_reassembly_response() {
    disconnected_response_case(Carrier::File, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_finishes_chunked_upstream_and_keeps_same_client() {
    disconnected_response_case(Carrier::Chunked, 1).await;
}

// Incomplete real network sources remain transport/body terminal errors. They
// are not deadline expiry and the test never supplies missing bytes to IPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_partial_upload_source_fault_refunds_without_dispatch_or_withdrawal() {
    let fixture = Fixture::start(Carrier::Inline, 2, INPUT_LEN as u64).await;
    let result = AssertUnwindSafe(async {
        for unknown in [false, true] {
            let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket))
                .await
                .unwrap();
            let framing = if unknown {
                "Transfer-Encoding: chunked".into()
            } else {
                format!("Content-Length: {INPUT_LEN}")
            };
            bounded(upload.write_all(fixture.head("target", &framing).as_bytes()))
                .await
                .unwrap();
            if unknown {
                bounded(upload.write_all(b"8\r\n\x07\x07\x07\x07\x07\x07\x07\x07\r\n"))
                    .await
                    .unwrap();
            } else {
                bounded(upload.write_all(&[7; 8])).await.unwrap();
            }
            let expected_charge = if unknown { 8 } else { INPUT_LEN as u64 };
            eventually(|| {
                fixture.state.forwarding.snapshot().used_retained_bytes == expected_charge
            })
            .await;
            assert_eq!(fixture.state.forwarding.snapshot().used_operations, 1);
            assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0);
            let before_faults = fixture.state.forwarding.source_faults(unknown);
            bounded(upload.shutdown()).await.unwrap(); // Real TCP write EOF.
            let mut reply = Vec::new();
            if let Err(error) = bounded(upload.read_to_end(&mut reply)).await {
                assert!(
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::UnexpectedEof
                    ),
                    "unexpected HTTP failure while observing actual source EOF: {error}"
                );
            }
            // Hyper may terminate this failed HTTP connection before an
            // error response can be written. Observe the actual native Body
            // failure/classification, independently of reply delivery.
            eventually(|| fixture.state.forwarding.source_faults(unknown) == before_faults + 1)
                .await;
            // Error delivery across the failed H1 source is optional and
            // may be truncated. A complete JSON response corroborates the
            // observed native fault; classification does not depend on it.
            if let Some(header_end) = reply.windows(4).position(|part| part == b"\r\n\r\n")
                && serde_json::from_slice::<serde_json::Value>(&reply[header_end + 4..]).is_ok()
            {
                let status = if unknown {
                    b"HTTP/1.1 400 ".as_slice()
                } else {
                    b"HTTP/1.1 502 ".as_slice()
                };
                assert!(
                    reply.starts_with(status),
                    "natural input error: {}",
                    String::from_utf8_lossy(&reply)
                );
                let message = String::from_utf8_lossy(&reply);
                if unknown {
                    assert!(message.contains("RequestBodyReadError"));
                } else {
                    assert!(message.contains("pre_dispatch"));
                    assert!(message.contains("request body stream error"));
                }
                assert!(!message.contains("Deadline"));
            }
            eventually(|| fixture.state.forwarding.snapshot().used_operations == 0).await;
            assert_eq!(fixture.state.forwarding.snapshot().used_retained_bytes, 0);
            assert!(fixture.state.local_route("grid").is_some());
            assert!(fixture.client.is_connected());
        }
        assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0);
        let ping = bounded(fixture.client.call_bound_phased(
            &fixture.binding,
            "ping",
            &[7; INPUT_LEN],
        ))
        .await
        .unwrap();
        assert_eq!(
            ping.into_bytes_with_pool(fixture.client.server_pool_arc())
                .unwrap(),
            vec![2; 32]
        );
        eprintln!(
            "[relay-forwarding] known and unknown HTTP source faults: retention={:?}",
            fixture.state.forwarding.snapshot()
        );
    })
    .catch_unwind()
    .await;
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_unknown_chunk_growth_rejects_before_takeover_and_dispatch() {
    let fixture = Fixture::start(Carrier::Inline, 2, 8).await;
    let result = AssertUnwindSafe(async {
        let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket))
            .await
            .unwrap();
        bounded(
            upload.write_all(
                fixture
                    .head("target", "Transfer-Encoding: chunked")
                    .as_bytes(),
            ),
        )
        .await
        .unwrap();
        bounded(upload.write_all(b"8\r\n\x07\x07\x07\x07\x07\x07\x07\x07\r\n"))
            .await
            .unwrap();
        eventually(|| fixture.state.forwarding.snapshot().used_retained_bytes == 8).await;
        bounded(upload.write_all(b"1\r\n\x07\r\n0\r\n\r\n"))
            .await
            .unwrap();
        let mut reply = Vec::new();
        bounded(upload.read_to_end(&mut reply)).await.unwrap();
        assert!(
            reply.starts_with(b"HTTP/1.1 502 "),
            "actual incremental capacity rejection: {}",
            String::from_utf8_lossy(&reply)
        );
        assert!(String::from_utf8_lossy(&reply).contains("pre_dispatch"));
        eventually(|| fixture.state.forwarding.snapshot().used_operations == 0).await;
        let retention = fixture.state.forwarding.snapshot();
        assert_eq!(
            (retention.used_retained_bytes, retention.peak_retained_bytes),
            (0, 8)
        );
        assert!(retention.rejected_reservations > 0);
        assert_eq!(
            fixture.probe.calls[1].load(Ordering::SeqCst),
            0,
            "rejected body must not be dispatched"
        );
        assert!(fixture.state.local_route("grid").is_some());
        assert!(fixture.client.is_connected());
        eprintln!("[relay-forwarding] actual unknown chunk growth rejection: {retention:?}");
    })
    .catch_unwind()
    .await;
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// Genuine source failure after a complete first IPC chunk is distinct from a
// detached waiter with complete input. The native transport may legitimately
// abort this incomplete shared stream; no test fills in the missing chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_chunked_upstream_source_fault_is_dispatch_uncertain_and_reclaims_actual_owner() {
    let fixture = Fixture::start(Carrier::Chunked, 1, INPUT_LEN as u64).await;
    let result = AssertUnwindSafe(async {
        assert_eq!(fixture.server.memory_budget_snapshot().budget.reassembly.used_bytes, 0);
        let before_faults = fixture.state.forwarding.published_source_faults();
        let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket)).await.unwrap();
        bounded(upload.write_all(fixture.head("target", &format!("Content-Length: {INPUT_LEN}")).as_bytes())).await.unwrap();
        bounded(upload.write_all(&[7; INPUT_LEN / 2])).await.unwrap();
        // This charge belongs to the real server ChunkRegistry. It can only
        // exist after the first actual IPC chunk reaches the upstream reader.
        eventually(|| fixture.server.memory_budget_snapshot().budget.reassembly.used_bytes == INPUT_LEN as u64).await;
        assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0);
        assert_eq!((fixture.state.forwarding.snapshot().used_operations, fixture.state.forwarding.snapshot().used_retained_bytes), (1, INPUT_LEN as u64));
        bounded(upload.shutdown()).await.unwrap();
        let mut wire_reply = Vec::new();
        if let Err(error) = bounded(upload.read_to_end(&mut wire_reply)).await {
            assert!(matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::UnexpectedEof), "unexpected H1 fault observation: {error}");
        }
        eventually(|| fixture.state.forwarding.published_source_faults() == before_faults + 1).await;
        eventually(|| fixture.state.forwarding.snapshot().used_operations == 0).await;
        assert_eq!(fixture.state.forwarding.snapshot().used_retained_bytes, 0);
        assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0, "incomplete input must not invoke resource");
        assert!(fixture.state.local_route("grid").is_some(), "real transport fault must not withdraw route");
        assert!(!fixture.client.is_connected(), "incomplete published IPC transaction uses real connection failure, not deadline cancellation");
        eventually(|| fixture.server.memory_budget_snapshot().budget.reassembly.used_bytes == 0).await;
        assert_eq!(fixture.server.memory_budget_snapshot().budget.file.used_bytes, 0);
        eprintln!("[relay-forwarding] actual first-chunk source failure dispatch_uncertain={} retention={:?} server={:?} HTTP={}", fixture.state.forwarding.published_source_faults(), fixture.state.forwarding.snapshot(), fixture.server.memory_budget_snapshot(), String::from_utf8_lossy(&wire_reply));
    }).catch_unwind().await;
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// The partial writer is an explicit per-client test feature. It uses the
// shared LocalStream writer on every supported platform, including Windows.
// The HTTP input is complete before its socket leaves; missing source bytes
// cannot be mistaken for a cancelled waiter in this regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_disconnect_after_real_eight_byte_ipc_prefix_preserves_same_client() {
    const LEN: usize = 32;
    let mut fixture = Fixture::start_with_input_len(Carrier::Inline, 1, LEN as u64, LEN).await;
    let client = fixture.client.clone();
    let binding = fixture.binding.clone();
    let mut other =
        tokio::spawn(async move { client.call_bound_phased(&binding, "other", &[7; LEN]).await });
    let mut other_joined = false;
    let (prefix_tx, prefix_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mut write_release = Some(release_tx);
    let result = AssertUnwindSafe(async {
        // This other RID has already been dispatched and entered its actual
        // callback on the same connection before the target writer parks.
        bounded(fixture.probe.entered[0].notified()).await;
        fixture.client.set_frame_write_seam_for_test(Some(c2_ipc::client::FrameWriteSeam {
            prefix_bytes: 8,
            prefix_written: prefix_tx,
            release: release_rx,
        }));
        let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket)).await.unwrap();
        bounded(upload.write_all(fixture.head("target", "Content-Length: 32").as_bytes())).await.unwrap();
        bounded(upload.write_all(&[7; LEN])).await.unwrap();
        bounded(prefix_rx).await.expect("the real IPC writer must write eight bytes before parking");
        assert_eq!(fixture.client.pending_len_for_test(), 2, "target and previously dispatched other RID share one client");
        assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0, "eight real frame bytes cannot invoke the target callback");
        let before = fixture.state.forwarding.detached_waiters();
        drop(upload);
        eventually(|| fixture.state.forwarding.detached_waiters() > before).await;
        let held = fixture.state.forwarding.snapshot();
        assert_eq!((held.used_operations, held.used_retained_bytes), (1, LEN as u64), "native task still owns full input while real writer is parked");
        assert_eq!(fixture.client.pending_len_for_test(), 2);
        assert!(fixture.client.is_connected(), "waiter departure must not trigger SendGuard's partial-frame abort");
        let rejected = fixture.request("ping", &[7; LEN]).await;
        assert!(rejected.starts_with(b"HTTP/1.1 502 "));
        assert!(String::from_utf8_lossy(&rejected).contains("pre_dispatch"));
        assert_eq!(fixture.probe.calls[2].load(Ordering::SeqCst), 0, "exhausted slot rejects dispatch while target owner remains");
        fixture.release(0);
        let other_result = bounded(&mut other).await;
        other_joined = true;
        assert_eq!(other_result.unwrap().unwrap().into_bytes_with_pool(fixture.client.server_pool_arc()).unwrap(), vec![0; 32], "already-dispatched RID must finish before the target writer resumes");
        assert!(fixture.client.is_connected());
        assert_eq!((fixture.state.forwarding.snapshot().used_operations, fixture.state.forwarding.snapshot().used_retained_bytes), (1, LEN as u64));
        write_release.take().unwrap().send(()).unwrap();
        bounded(fixture.probe.entered[1].notified()).await;
        assert_eq!(fixture.probe.input_kind.load(Ordering::SeqCst), 1, "complete input reaches real upstream callback only after remaining frame bytes are written");
        fixture.release(1);
        eventually(|| fixture.state.forwarding.snapshot().used_operations == 0).await;
        eventually(|| fixture.client.pending_len_for_test() == 0).await;
        assert_eq!(fixture.state.forwarding.snapshot().used_retained_bytes, 0);
        let ping = bounded(fixture.client.call_bound_phased(&fixture.binding, "ping", &[7; LEN])).await.unwrap();
        assert_eq!(ping.into_bytes_with_pool(fixture.client.server_pool_arc()).unwrap(), vec![2; 32]);
        let current = fixture.state.local_route("grid").expect("HTTP disconnect must not withdraw route");
        assert_eq!(current.route_uid, fixture.entry.route_uid);
        assert_eq!(current.route_revision, fixture.entry.route_revision);
        let (lease, _, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry)).await.unwrap_or_else(|_| panic!("same pooled IPC acquire failed"));
        assert!(Arc::ptr_eq(&lease.client(), &fixture.client), "same-client ping cannot be concealed by reconnecting");
        drop(lease);
        assert_eq!(fixture.probe.calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)), [1, 1, 1]);
        assert_eq!(fixture.state.upstream_memory_snapshot().reassembly.used_bytes, 0);
        eprintln!("[relay-forwarding] real H1 full_input=32 partial_ipc_prefix=8 same_client=true pending={} retention={:?}", fixture.client.pending_len_for_test(), fixture.state.forwarding.snapshot());
    }).catch_unwind().await;
    // Release the one-shot writer even after assertion failure, then release
    // bounded callback waits and confirm both actual listener tasks exit.
    if let Some(release) = write_release.take() {
        let _ = release.send(());
    }
    fixture.client.set_frame_write_seam_for_test(None);
    fixture.release(0);
    fixture.release(1);
    if !other_joined {
        let _ = bounded(&mut other).await;
    }
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h1_admitted_unknown_upload_growth_after_close_is_pre_dispatch_capacity_terminal() {
    let fixture = Fixture::start(Carrier::Inline, 1, INPUT_LEN as u64).await;
    let result = AssertUnwindSafe(async {
        let mut upload = bounded(tokio::net::TcpStream::connect(fixture.socket))
            .await
            .unwrap();
        bounded(
            upload.write_all(
                fixture
                    .head("target", "Transfer-Encoding: chunked")
                    .as_bytes(),
            ),
        )
        .await
        .unwrap();
        bounded(upload.write_all(b"8\r\n\x07\x07\x07\x07\x07\x07\x07\x07\r\n"))
            .await
            .unwrap();
        eventually(|| fixture.state.forwarding.snapshot().used_retained_bytes == 8).await;
        assert_eq!(fixture.state.forwarding.snapshot().used_operations, 1);
        fixture.state.forwarding.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), fixture.state.forwarding.drain())
                .await
                .is_err(),
            "closing admission cannot manufacture completed upload ownership"
        );
        // A complete valid source continues after close. Its next growth is
        // rejected by the frozen budget's Closed state before taking bytes or
        // dispatching. This is capacity termination, never a body fault or
        // deadline, and an already-admitted upload has no grandfathered growth.
        bounded(upload.write_all(b"1\r\n\x07\r\n0\r\n\r\n"))
            .await
            .unwrap();
        let mut reply = Vec::new();
        bounded(upload.read_to_end(&mut reply)).await.unwrap();
        assert!(
            reply.starts_with(b"HTTP/1.1 502 "),
            "actual close rejection: {}",
            String::from_utf8_lossy(&reply)
        );
        let message = String::from_utf8_lossy(&reply);
        assert!(message.contains("pre_dispatch"));
        assert!(message.contains("retention budget is closed"));
        assert!(!message.contains("Deadline"));
        bounded(fixture.state.forwarding.drain()).await;
        let refunded = fixture.state.forwarding.snapshot();
        assert_eq!(
            (
                refunded.used_operations,
                refunded.used_retained_bytes,
                refunded.peak_retained_bytes
            ),
            (0, 0, 8)
        );
        assert_eq!(
            fixture.state.forwarding.source_faults(true),
            0,
            "complete HTTP source is not a sourcefault"
        );
        assert_eq!(fixture.probe.calls[1].load(Ordering::SeqCst), 0);
        assert!(fixture.state.local_route("grid").is_some());
        assert!(fixture.client.is_connected());
        let ping = bounded(fixture.client.call_bound_phased(
            &fixture.binding,
            "ping",
            &[7; INPUT_LEN],
        ))
        .await
        .unwrap();
        assert_eq!(
            ping.into_bytes_with_pool(fixture.client.server_pool_arc())
                .unwrap(),
            vec![2; 32]
        );
    })
    .catch_unwind()
    .await;
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
