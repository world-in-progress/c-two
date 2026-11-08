//! One-hop HTTP/1 → live IPC 16 server-stream and slow-reader evidence.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use c2_config::{CallExecutionLimits, ClientIpcConfig, LocalEndpointContext, RelayConfig};
use c2_contract::ValidatedContractDescriptor;
use c2_ipc::IpcClient;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, CrmStream, CrmStreamCallback, CrmStreamItem,
    ResponseMeta, ResponsePool, RouteBuildSpec, SchedulerLimits, Server, ServerIdentity,
    ServerIpcConfig,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

use super::router::build_router;
use super::state::{RegisterCommitResult, RelayState};
use super::test_support::NoopDisseminator;
use crate::stream_wire::{self, HttpStreamRecordKind, HttpStreamStatus};

const WAIT: Duration = Duration::from_secs(15);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");
static NEXT: AtomicU64 = AtomicU64::new(0);

async fn bounded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, work)
        .await
        .expect("live relay step timed out")
}

#[derive(Default)]
struct Observed {
    produced: AtomicUsize,
    consumed: AtomicUsize,
    peak_ahead: AtomicUsize,
    progress_gate: parking_lot::Mutex<()>,
    unary_calls: AtomicUsize,
    opened: AtomicUsize,
    finished: AtomicUsize,
    fail_second_item: AtomicBool,
    finish_gate: parking_lot::Mutex<Option<Arc<FinishGate>>>,
}

struct FinishGate {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

struct ReleaseFinish(Arc<FinishGate>);
impl Drop for ReleaseFinish {
    fn drop(&mut self) {
        self.0.release.wait();
    }
}

struct Resource(Arc<Observed>);
struct Stream {
    observed: Arc<Observed>,
    remaining: usize,
    size: usize,
    index: usize,
}

struct RangeFailureItem(Vec<u8>);

impl CrmStreamItem for RangeFailureItem {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), CrmError> {
        if offset + destination.len() > 4096 {
            return Err(CrmError::InternalError("relay range write failed".into()));
        }
        destination.copy_from_slice(&self.0[offset..offset + destination.len()]);
        Ok(())
    }
}

impl CrmCallback for Resource {
    fn invoke(
        &self,
        _: &str,
        method: u16,
        input: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.0.unary_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(method, 0);
        if input.is_empty() {
            return Ok(ResponseMeta::Inline(b"pong".to_vec()));
        }
        let size = u32::from_le_bytes(input.try_into().expect("unary size input")) as usize;
        Ok(ResponseMeta::Inline(vec![7; size]))
    }
}

impl CrmStreamCallback for Resource {
    fn open_stream(
        &self,
        _: &str,
        method: u16,
        input: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        assert_eq!(method, 1);
        assert_eq!(input.len(), 8);
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        let count = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(input[4..].try_into().unwrap()) as usize;
        Ok(Box::new(Stream {
            observed: self.0.clone(),
            remaining: count,
            size,
            index: 0,
        }))
    }
}

impl CrmStream for Stream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let serial = self.index as u8;
        self.index += 1;
        let item = vec![serial; self.size];
        {
            // Serialize the two test counters so the peak cannot be missed
            // when a yield and a final-consumer completion happen together.
            let _progress = self.observed.progress_gate.lock();
            let produced = self.observed.produced.fetch_add(1, Ordering::SeqCst) + 1;
            let consumed = self.observed.consumed.load(Ordering::SeqCst);
            self.observed
                .peak_ahead
                .fetch_max(produced - consumed, Ordering::SeqCst);
        }
        if self.index == 2 && self.observed.fail_second_item.load(Ordering::SeqCst) {
            Ok(Some(Box::new(RangeFailureItem(item))))
        } else {
            Ok(Some(Box::new(item)))
        }
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        if let Some(gate) = self.observed.finish_gate.lock().clone() {
            gate.entered.wait();
            gate.release.wait();
        }
        self.observed.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Observe only an actual Pending returned by the TCP socket's poll_write.
/// The wrapper delegates all bytes and wakeups to the real socket.
struct MeteredIo {
    socket: tokio::net::TcpStream,
    armed: Arc<AtomicBool>,
    pending: mpsc::Sender<()>,
}
impl AsyncRead for MeteredIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_read(cx, buf)
    }
}
impl AsyncWrite for MeteredIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.socket).poll_write(cx, buf);
        if result.is_pending() && self.armed.load(Ordering::SeqCst) {
            let _ = self.pending.try_send(());
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.socket.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.socket).poll_write_vectored(cx, bufs);
        if result.is_pending() && self.armed.load(Ordering::SeqCst) {
            let _ = self.pending.try_send(());
        }
        result
    }
}
struct MeteredListener {
    inner: tokio::net::TcpListener,
    armed: Arc<AtomicBool>,
    pending: mpsc::Sender<()>,
}
impl Listener for MeteredListener {
    type Io = MeteredIo;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (socket, addr) = self.inner.accept().await.unwrap();
        (
            MeteredIo {
                socket,
                armed: self.armed.clone(),
                pending: self.pending.clone(),
            },
            addr,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

struct Fixture {
    _root: Option<tempfile::TempDir>,
    state: Arc<RelayState>,
    server: Arc<Server>,
    server_task: tokio::task::JoinHandle<Result<(), c2_server::ServerError>>,
    observer_task: tokio::task::JoinHandle<()>,
    http_task: tokio::task::JoinHandle<io::Result<()>>,
    http_stop: oneshot::Sender<()>,
    socket: SocketAddr,
    entry: super::types::RouteEntry,
    observed: Arc<Observed>,
    armed: Arc<AtomicBool>,
    pending: mpsc::Receiver<()>,
    budget_bytes: u64,
    frame_tap: Option<tokio::task::JoinHandle<()>>,
}

type RpcFrames = Arc<parking_lot::Mutex<Vec<(bool, u64, c2_wire::rpc::RpcRecordKind)>>>;

// A transparent test peer observes complete frames on both native OS backends.
// It forwards handshake/control frames too, and adds no production observer.
async fn forward_frames(
    mut reader: c2_local::LocalReadHalf,
    mut writer: c2_local::LocalWriteHalf,
    response: bool,
    frames: RpcFrames,
) {
    use c2_wire::{flags, frame};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let mut raw = [0; frame::HEADER_SIZE];
        if reader.read(&mut raw[..1]).await.unwrap() == 0 {
            break;
        }
        reader.read_exact(&mut raw[1..]).await.unwrap();
        let (length, body) = frame::decode_total_len(&raw).unwrap();
        let (header, _) = frame::decode_frame_body(body, length).unwrap();
        let mut payload = vec![0; header.payload_len()];
        reader.read_exact(&mut payload).await.unwrap();
        if header.flags & flags::FLAG_RPC_RECORD != 0 {
            frames
                .lock()
                .push((response, header.request_id, payload[0].try_into().unwrap()));
        }
        let mut frame = raw.to_vec();
        frame.extend_from_slice(&payload);
        // Keep the tap's reader alive when an idle client has already closed:
        // the inherent write_all aborts both halves on a failed courtesy ACK.
        let written = async {
            AsyncWriteExt::write_all(&mut writer, &frame).await?;
            writer.flush().await
        }
        .await;
        if let Err(error) = written {
            // An idle client closes without waiting for the courtesy ACK.
            // Still read the upstream through EOF to count its entire output.
            assert!(
                response && c2_local::is_peer_closed_error(&error),
                "{error}"
            );
            assert_eq!(header.request_id, 0);
            assert_eq!(header.flags, flags::FLAG_SIGNAL | flags::FLAG_RESPONSE);
            assert_eq!(payload, c2_wire::msg_type::DISCONNECT_ACK_BYTES);
        }
    }
}

impl Fixture {
    async fn start(shm: bool, budget_bytes: u64) -> Self {
        Self::start_with_frame_tap(shm, budget_bytes, None).await
    }

    async fn start_with_frame_tap(shm: bool, budget_bytes: u64, frames: Option<RpcFrames>) -> Self {
        let ordinal = NEXT.fetch_add(1, Ordering::SeqCst);
        let id = format!("b8-{}-{ordinal}", std::process::id());
        let address = format!("ipc://{id}");
        #[cfg(unix)]
        let root = tempfile::Builder::new().prefix("c2b8-").tempdir().unwrap();
        #[cfg(unix)]
        let context = LocalEndpointContext::with_unix_root(root.path()).unwrap();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
        let mut server_config = ServerIpcConfig::default();
        server_config.base.pool_enabled = false;
        server_config.base.pool_prewarm_segments = 0;
        server_config.base.chunk_size = 4096;
        server_config.base.shm_backing_budget_bytes = if shm { 16 * 1024 * 1024 } else { 0 };
        server_config.shm_threshold = if shm { 1 } else { 1 << 30 };
        server_config.max_execution_workers = 4;
        let endpoint = context.endpoint(&address).unwrap();
        let server = Arc::new(
            Server::new_with_identity_and_endpoint(
                endpoint.clone(),
                server_config,
                ServerIdentity {
                    server_id: id.clone(),
                    server_instance_id: format!("{id}-instance"),
                },
            )
            .unwrap(),
        );
        let observed = Arc::new(Observed::default());
        let built = server
            .build_stream_route(
                RouteBuildSpec {
                    name: "portable".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: vec!["ping".into(), "echo".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Resource(observed.clone())),
                &descriptor,
            )
            .unwrap();
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        server.begin_start_attempt().unwrap();
        let server_task = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });
        bounded(server.wait_until_responsive(Duration::from_secs(2)))
            .await
            .unwrap();

        let (address, frame_tap) = if let Some(frames) = frames {
            let address = format!("ipc://{id}-tap");
            let mut listener =
                c2_local::LocalListener::bind(&context.endpoint(&address).unwrap()).unwrap();
            let endpoint = endpoint.clone();
            let task = tokio::spawn(async move {
                let incoming = bounded(listener.accept()).await.unwrap();
                let outgoing = bounded(c2_local::LocalStream::connect(&endpoint, WAIT))
                    .await
                    .unwrap();
                let (incoming_read, incoming_write) = incoming.into_split();
                let (outgoing_read, outgoing_write) = outgoing.into_split();
                tokio::join!(
                    forward_frames(incoming_read, outgoing_write, false, frames.clone()),
                    forward_frames(outgoing_read, incoming_write, true, frames),
                );
                let result = tokio::task::spawn_blocking(move || listener.close())
                    .await
                    .unwrap();
                assert!(matches!(
                    result,
                    c2_local::EndpointReapResult::Reaped
                        | c2_local::EndpointReapResult::NotApplicable
                ));
            });
            (address, Some(task))
        } else {
            (address, None)
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = listener.local_addr().unwrap();
        let mut client_config = ClientIpcConfig::default();
        client_config.base.pool_enabled = false;
        client_config.base.pool_prewarm_segments = 0;
        client_config.base.chunk_size = 4096;
        client_config.base.shm_backing_budget_bytes = if shm { 16 * 1024 * 1024 } else { 0 };
        client_config.shm_threshold = if shm { 1 } else { 1 << 30 };
        let state = Arc::new(RelayState::new_with_execution_limits(
            Arc::new(RelayConfig {
                relay_id: format!("relay-{id}"),
                advertise_url: format!("http://{socket}"),
                idle_timeout_secs: 0,
                upstream_ipc: client_config,
                remote_payload_chunk_size: 4096,
                ..RelayConfig::default()
            }),
            Arc::new(NoopDisseminator),
            context,
            CallExecutionLimits {
                max_outstanding_calls: 4,
                payload_buffer_budget_bytes: budget_bytes,
            },
        ));
        let mut attestor = IpcClient::with_endpoint(endpoint, ClientIpcConfig::default()).unwrap();
        bounded(attestor.connect()).await.unwrap();
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "portable".into(),
            crm_ns: descriptor.crm_namespace().into(),
            crm_name: descriptor.crm_name().into(),
            crm_ver: descriptor.crm_version().into(),
            abi_hash: descriptor.abi_hash().into(),
            signature_hash: descriptor.signature_hash().into(),
        };
        let binding = bounded(attestor.attest_route_for_registration(&expected))
            .await
            .unwrap();
        let entry = match test_commit_registration!(
            state,
            "portable".into(),
            id.clone(),
            format!("{id}-instance"),
            address,
            descriptor.crm_namespace().into(),
            descriptor.crm_name().into(),
            descriptor.crm_version().into(),
            descriptor.abi_hash().into(),
            descriptor.signature_hash().into(),
            server.config().max_payload_size,
            binding.route_uid,
            None
        ) {
            RegisterCommitResult::Registered { entry } => entry,
            _ => panic!("route registration failed"),
        };
        bounded(attestor.close()).await;
        let clients = state.clients.clone();
        let observer_task = tokio::spawn(async move { clients.run().await });
        let armed = Arc::new(AtomicBool::new(false));
        let (pending_tx, pending) = mpsc::channel(4);
        let (http_stop, stop) = oneshot::channel();
        let app = build_router(state.clone());
        let listener = MeteredListener {
            inner: listener,
            armed: armed.clone(),
            pending: pending_tx,
        };
        let http_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
        });
        Self {
            _root: {
                #[cfg(unix)]
                {
                    Some(root)
                }
                #[cfg(windows)]
                {
                    None
                }
            },
            state,
            server,
            server_task,
            observer_task,
            http_task,
            http_stop,
            socket,
            entry,
            observed,
            armed,
            pending,
            budget_bytes,
            frame_tap,
        }
    }

    fn request(&self, count: u32, size: u32) -> reqwest::RequestBuilder {
        self.method_request("echo", true)
            .body([count.to_le_bytes(), size.to_le_bytes()].concat())
    }

    fn method_request(&self, method: &str, stream: bool) -> reqwest::RequestBuilder {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = client
            .post(format!("http://{}/portable/{method}", self.socket))
            .header("x-c2-expected-crm-ns", &self.entry.crm_ns)
            .header("x-c2-expected-crm-name", &self.entry.crm_name)
            .header("x-c2-expected-crm-ver", &self.entry.crm_ver)
            .header("x-c2-expected-abi-hash", &self.entry.abi_hash)
            .header("x-c2-expected-signature-hash", &self.entry.signature_hash)
            .header("x-c2-route-uid", &self.entry.route_uid);
        if stream {
            request.header("accept", stream_wire::MEDIA_TYPE)
        } else {
            request
        }
    }

    async fn stop(self) {
        self.state.forwarding.close();
        bounded(self.state.forwarding.drain()).await;
        let _ = self.http_stop.send(());
        bounded(self.http_task).await.unwrap().unwrap();
        bounded(self.state.stop_upstream_controls()).await;
        bounded(self.state.clients.shutdown()).await;
        bounded(self.observer_task).await.unwrap();
        if let Some(task) = self.frame_tap {
            bounded(task).await.unwrap();
        }
        bounded(self.server.shutdown_and_wait(Duration::from_secs(2)))
            .await
            .unwrap();
        bounded(self.server_task).await.unwrap().unwrap();
        assert_eq!(
            self.state.forwarding.payload_budget().snapshot().used_bytes,
            0
        );
    }
}

fn parse_records(body: &[u8], expected_count: usize, item_size: usize) {
    let mut cursor = body;
    for index in 0..expected_count {
        let (kind, len) = stream_wire::decode_header(cursor[..9].try_into().unwrap()).unwrap();
        assert_eq!(kind, HttpStreamRecordKind::ItemBegin);
        assert_eq!(len, 8);
        assert_eq!(
            u64::from_le_bytes(cursor[9..17].try_into().unwrap()),
            item_size as u64
        );
        cursor = &cursor[17..];
        let mut received = 0;
        while received < item_size {
            let (kind, len) = stream_wire::decode_header(cursor[..9].try_into().unwrap()).unwrap();
            assert_eq!(kind, HttpStreamRecordKind::ItemData);
            let len = len as usize;
            assert!(len <= item_size - received);
            assert!(cursor[9..9 + len].iter().all(|&byte| byte == index as u8));
            cursor = &cursor[9 + len..];
            received += len;
        }
    }
    let (kind, len) = stream_wire::decode_header(cursor[..9].try_into().unwrap()).unwrap();
    assert_eq!(kind, HttpStreamRecordKind::CallEnd);
    let terminal = stream_wire::decode_terminal(&cursor[9..]).unwrap();
    assert_eq!(len as usize, cursor.len() - 9);
    assert_eq!(terminal.status, HttpStreamStatus::Success);
    assert_eq!(terminal.complete_items, Some(expected_count as u64));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn item_write_range_failure_keeps_real_terminal_and_http_ends_normally() {
    for shm in [false, true] {
        let fixture = Fixture::start(shm, 64 * 1024).await;
        fixture
            .observed
            .fail_second_item
            .store(true, Ordering::SeqCst);
        let response = bounded(fixture.request(2, 8192).send()).await.unwrap();
        assert_eq!(response.status(), 200);
        let result = bounded(response.bytes()).await;
        assert!(
            result.is_ok(),
            "the actual Failed CALL_END must end HTTP normally: {result:?}"
        );
        let bytes = result.unwrap();
        let mut offset = 0usize;
        let mut total = 0usize;
        let mut received = 0usize;
        let mut complete = 0u64;
        let mut terminal = None;
        while offset < bytes.len() {
            let (kind, length) =
                stream_wire::decode_header(bytes[offset..offset + 9].try_into().unwrap()).unwrap();
            offset += 9;
            let payload = &bytes[offset..offset + length as usize];
            offset += length as usize;
            match kind as u8 {
                1 => {
                    assert_eq!(payload.len(), 8);
                    total = u64::from_le_bytes(payload.try_into().unwrap()) as usize;
                    received = 0;
                }
                4 => {
                    assert!(payload.iter().all(|&b| b == complete as u8));
                    received += payload.len();
                    if received == total {
                        complete += 1;
                    }
                }
                2 => terminal = Some(stream_wire::decode_terminal(payload).unwrap()),
                _ => panic!("unexpected record in the resource-error response"),
            }
        }
        assert_eq!(complete, 1);
        assert!(received < total, "second item remains incomplete");
        let terminal = terminal.expect("actual terminal record");
        assert_eq!(terminal.status, HttpStreamStatus::Failed);
        assert_eq!(terminal.complete_items, Some(1));
        let error = terminal.error.unwrap();
        assert_eq!(error.code, c2_error::ErrorCode::ResourceFunctionExecuting);
        assert!(error.message.contains("relay range write failed"));
        assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1);
        let response = bounded(fixture.request(2, 8192).send()).await.unwrap();
        let token = response.headers()["x-c2-call-token"]
            .to_str()
            .unwrap()
            .to_owned();
        let budget = c2_mem::retention::PayloadBufferBudget::new(16 * 1024);
        let mut receiver = crate::client::HttpResponseStream::start(
            response,
            reqwest::Client::builder().no_proxy().build().unwrap(),
            format!("http://{}", fixture.socket),
            token,
            budget.clone(),
            8192,
            Box::new(()),
            crate::client::HttpStreamCancellation::new(),
        );
        let held = bounded(receiver.recv()).await.unwrap().unwrap();
        assert_eq!(held.as_ref(), &[0; 8192]);
        let error = match bounded(receiver.recv()).await {
            Err(crate::client::HttpError::LocalCallRejected(error)) => error,
            _ => panic!("expected original resource error"),
        };
        assert_eq!(error.code, c2_error::ErrorCode::ResourceFunctionExecuting);
        assert!(error.message.contains("relay range write failed"));
        let terminal = bounded(receiver.completion()).await.unwrap();
        assert_eq!(terminal.complete_items, Some(1));
        assert_eq!(budget.snapshot().used_bytes, 8192);
        assert_eq!(held.as_ref(), &[0; 8192]);
        drop(held);
        assert_eq!(budget.snapshot().used_bytes, 0);
        assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 2);
        drop(receiver);
        fixture.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hop_real_http_stream_inline_and_whole_shm() {
    for shm in [false, true] {
        let fixture = Fixture::start(shm, 256 * 1024).await;
        for (count, size) in [(0, 0), (1, 0), (3, 64), (2, 128 * 1024)] {
            let response = bounded(fixture.request(count, size).send()).await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], stream_wire::MEDIA_TYPE);
            assert!(response.headers().contains_key("x-c2-call-token"));
            let bytes = bounded(response.bytes()).await.unwrap();
            parse_records(&bytes, count as usize, size as usize);
        }
        assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 4);
        assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 4);
        let peak_shm = fixture
            .server
            .memory_budget_snapshot()
            .budget
            .shm
            .peak_bytes;
        if shm {
            assert!(
                peak_shm >= 128 * 1024,
                "large item must use actual SHM backing"
            );
        } else {
            assert_eq!(peak_shm, 0, "inline case must not allocate SHM backing");
        }
        fixture.stop().await;
    }
}

/// Incremental final-consumer decoder. C advances only after an ITEM's entire
/// payload has arrived through reqwest's real HTTP/1 response body.
struct Consumer {
    buffer: Vec<u8>,
    complete: usize,
    terminal: bool,
    remaining: Option<usize>,
    max_ahead: usize,
}
impl Consumer {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            complete: 0,
            terminal: false,
            remaining: None,
            max_ahead: 0,
        }
    }
    fn push(&mut self, bytes: &[u8], produced: usize, item_size: usize, observed: &Observed) {
        self.max_ahead = self.max_ahead.max(produced.saturating_sub(self.complete));
        self.buffer.extend_from_slice(bytes);
        loop {
            if self.buffer.len() < stream_wire::RECORD_HEADER_LEN {
                break;
            }
            let (kind, length) =
                stream_wire::decode_header(self.buffer[..9].try_into().unwrap()).unwrap();
            let length = usize::try_from(length).unwrap();
            if self.buffer.len() < 9 + length {
                break;
            }
            match kind {
                HttpStreamRecordKind::ItemBegin => {
                    assert!(self.remaining.is_none());
                    assert_eq!(length, 8);
                    let total = u64::from_le_bytes(self.buffer[9..17].try_into().unwrap()) as usize;
                    assert_eq!(total, item_size);
                    if total == 0 {
                        self.complete += 1;
                        let _progress = observed.progress_gate.lock();
                        observed.consumed.fetch_add(1, Ordering::SeqCst);
                    } else {
                        self.remaining = Some(total);
                    }
                }
                HttpStreamRecordKind::ItemData => {
                    let remaining = self.remaining.as_mut().expect("open item");
                    assert!(length <= *remaining);
                    assert!(
                        self.buffer[9..9 + length]
                            .iter()
                            .all(|&byte| byte == self.complete as u8)
                    );
                    *remaining -= length;
                    if *remaining == 0 {
                        self.remaining = None;
                        self.complete += 1;
                        let _progress = observed.progress_gate.lock();
                        observed.consumed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                HttpStreamRecordKind::CallEnd => {
                    assert!(self.remaining.is_none());
                    let terminal =
                        stream_wire::decode_terminal(&self.buffer[9..9 + length]).unwrap();
                    assert_eq!(terminal.status, HttpStreamStatus::Success);
                    assert_eq!(terminal.complete_items, Some(self.complete as u64));
                    self.terminal = true;
                }
                HttpStreamRecordKind::Failure => panic!("unexpected dispatch-uncertain record"),
            }
            self.buffer.drain(..9 + length);
        }
        self.max_ahead = self.max_ahead.max(produced.saturating_sub(self.complete));
    }
}

async fn slow_reader_case(
    shm: bool,
    repetition: usize,
    count: u32,
    size: u32,
    pause_limit: Duration,
) -> serde_json::Value {
    let mut fixture = Fixture::start(shm, 256 * 1024).await;
    let response = bounded(fixture.request(count, size).send()).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], stream_wire::MEDIA_TYPE);
    fixture.armed.store(true, Ordering::SeqCst);
    // No response-body read occurs before actual write stall or natural
    // source exhaustion. Large item-count caps probe beyond the original
    // small/empty runs; a naturally exhausted row establishes no bound.
    let stalled = tokio::select! {
        result = fixture.pending.recv() => result.is_some(),
        _ = tokio::time::timeout(pause_limit, async {
            while fixture.observed.finished.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        }) => {
            assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1,
                "source neither stalled nor exhausted within the pause safety cap");
            false
        },
    };
    let at_pause = fixture.observed.produced.load(Ordering::SeqCst);
    let consumed_at_pause = fixture.observed.consumed.load(Ordering::SeqCst);
    assert_eq!(
        consumed_at_pause, 0,
        "reader has not received a complete ITEM"
    );
    let natural = fixture.observed.finished.load(Ordering::SeqCst) == 1;
    if size > 4096 {
        assert!(stalled, "large ITEM must produce a real TCP write stall");
        assert!(
            !natural,
            "source naturally exhausted before the socket stalled"
        );
    }
    let mut consumer = Consumer::new();
    consumer.max_ahead = at_pause;
    let mut response = response;
    while let Some(chunk) = bounded(response.chunk()).await.unwrap() {
        consumer.push(
            &chunk,
            fixture.observed.produced.load(Ordering::SeqCst),
            size as usize,
            &fixture.observed,
        );
    }
    assert_eq!(consumer.complete, count as usize);
    assert_eq!(
        fixture.observed.consumed.load(Ordering::SeqCst),
        count as usize
    );
    assert!(consumer.terminal);
    assert!(consumer.buffer.is_empty());
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1);
    let record = serde_json::json!({
        "carrier": if shm { "whole_shm" } else { "inline" },
        "repetition": repetition,
        "item_bytes": size, "items": count,
        "pause_safety_cap_seconds": pause_limit.as_secs(),
        "relay_budget_bytes": fixture.budget_bytes,
        "relay_copy_window_bytes": 4096,
        "server_ipc_chunk_bytes": 4096,
        "upstream_ipc_chunk_bytes": 4096,
        "remote_payload_chunk_bytes": 4096,
        "tcp_write_pending_observed": stalled,
        "source_naturally_exhausted_while_reader_paused": natural,
        "P_at_pause_boundary": at_pause,
        "C_at_pause_boundary": consumed_at_pause,
        "max_event_P_minus_C": fixture.observed.peak_ahead.load(Ordering::SeqCst),
        "max_read_sample_P_minus_C": consumer.max_ahead,
        "backpressure_peak_P_minus_C": (!natural && stalled).then(|| fixture.observed.peak_ahead.load(Ordering::SeqCst)),
        "natural_exhaustion_P_minus_C": natural.then_some(at_pause),
        "server_shm_peak_bytes": fixture.server.memory_budget_snapshot().budget.shm.peak_bytes,
        "final_P": fixture.observed.produced.load(Ordering::SeqCst),
        "final_C": consumer.complete,
    });
    eprintln!("[block8-slow-read] {record}");
    fixture.stop().await;
    record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_reader_large_items_stall_tcp_and_bound_producer_distance() {
    // A strict distance below the finite source length rules out exhausting
    // the source into HTTP buffers and mistaking that for backpressure.
    const COUNT: u32 = 64;
    for shm in [false, true] {
        let record = slow_reader_case(shm, 0, COUNT, 128 * 1024, WAIT).await;
        assert!(record["tcp_write_pending_observed"].as_bool().unwrap());
        let ahead = record["max_event_P_minus_C"].as_u64().unwrap();
        assert!(
            ahead < u64::from(COUNT),
            "producer distance reached the source safety cap: {record}"
        );
    }
}

// Full measurement, deliberately separate from the regression above. Run:
// CARGO_TARGET_DIR="$PWD/core/target" cargo test --manifest-path core/Cargo.toml \
//   -p c2-http --features relay --lib \
//   relay::stream_tests::slow_reader_records_real_tcp_stall_and_producer_distance \
//   -- --exact --ignored --nocapture --test-threads=1
// Both upstream carriers run twice for empty, 64-byte and 128-KiB items. The
// final consumer does not read the HTTP body until the real TCP poll_write
// returns Pending or the resource finishes. P is counted on each yield, C on
// each complete item received, with one test lock capturing max(P-C) at those
// events. Natural exhaustion and the 120-second safety cap establish no bound.
// JSON rows record both the event peak and read samples, final counts, carrier,
// windows and budgets in target/block8a-fix-validation/slow-read.json. These
// observations depend on the OS's HTTP/TCP buffers; no small-item step bound
// is asserted. The metered socket delegates all bytes and wakeups unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "on-demand slow-reader measurement; run command is in the adjacent test comments"]
async fn slow_reader_records_real_tcp_stall_and_producer_distance() {
    let mut records = Vec::new();
    for shm in [false, true] {
        for repetition in 0..2 {
            for (count, size) in [(200_000_u32, 0_u32), (100_000, 64), (512, 128 * 1024)] {
                records.push(
                    slow_reader_case(shm, repetition, count, size, Duration::from_secs(120)).await,
                );
            }
        }
    }
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/block8a-fix-validation/slow-read.json");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(output, serde_json::to_vec_pretty(&serde_json::json!({
        "method": "real HTTP/1 over TCP; final consumer paused until test-only AsyncWrite observed actual poll_write Pending or resource exhaustion; P advances on each completed resource yield, C after each entire HTTP ITEM reaches the final consumer; a test-only gate serializes both event counters and captures their peak",
        "crate_version": env!("CARGO_PKG_VERSION"),
        "http_version": "HTTP/1.1",
        "ipc_protocol_version": 16,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "interpretation": "observations for these runs only; only rows with actual TCP write Pending before source exhaustion demonstrate backpressure; item count and the 120-second pause timeout are test safety caps",
        "cases": records,
    })).unwrap()).unwrap();
}

async fn wait_for_admission(state: &RelayState) {
    bounded(async {
        loop {
            if state.forwarding.snapshot().used_operations == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_wait_precedes_upstream_lease_then_reconnects_after_idle_evict() {
    let fixture = Fixture::start(false, 32 * 1024).await;
    let (lease, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry))
        .await
        .unwrap_or_else(|_| panic!("live pooled upstream acquire failed"));
    let old_client = lease.client();
    drop(lease);
    let budget = fixture.state.forwarding.payload_budget();
    let held = budget.try_reserve(fixture.budget_bytes).unwrap();
    let request = fixture.request(1, 64);
    let pending = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_admission(&fixture.state).await;
    assert_eq!(
        fixture.observed.opened.load(Ordering::SeqCst),
        0,
        "B waiter must not acquire or dispatch before reservation"
    );
    let evicted = fixture.state.evict_idle(0);
    assert!(
        evicted.iter().any(|(_, client)| client
            .as_ref()
            .is_some_and(|client| Arc::ptr_eq(client, &old_client))),
        "idle pooled client must remain evictable while new call waits for B"
    );
    for (_, client) in evicted {
        if let Some(client) = client {
            fixture.state.clients.close(&client);
        }
    }
    assert!(!old_client.is_connected());
    drop(held);
    let response = bounded(pending).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = bounded(response.bytes()).await.unwrap();
    parse_records(&bytes, 1, 64);
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_wait_rechecks_route_before_any_dispatch() {
    let fixture = Fixture::start(false, 32 * 1024).await;
    let budget = fixture.state.forwarding.payload_budget();
    let held = budget.try_reserve(fixture.budget_bytes).unwrap();
    let request = fixture.request(1, 64);
    let pending = tokio::spawn(async move { request.send().await.unwrap() });
    wait_for_admission(&fixture.state).await;
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    match fixture
        .state
        .unregister_upstream("portable", fixture.server.server_id())
    {
        super::state::UnregisterResult::Removed { client, .. } => {
            if let Some(client) = client {
                fixture.state.clients.close(&client);
            }
        }
        _ => panic!("test route did not unregister"),
    }
    drop(held);
    let response = bounded(pending).await.unwrap();
    assert_ne!(response.status(), 200);
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_body_while_next_pull_waits_for_b_cancels_and_preserves_shared_unary() {
    use axum::body::{Body, HttpBody};
    use axum::http::Request;
    use tower::ServiceExt;

    for shm in [false, true] {
        let fixture = Fixture::start(shm, 32 * 1024).await;
        let input = [1_u32.to_le_bytes(), (128 * 1024_u32).to_le_bytes()].concat();
        let request = Request::builder()
            .method("POST")
            .uri("/portable/echo")
            .header("accept", stream_wire::MEDIA_TYPE)
            .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
            .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
            .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
            .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
            .header(
                "x-c2-expected-signature-hash",
                &fixture.entry.signature_hash,
            )
            .header("x-c2-route-uid", &fixture.entry.route_uid)
            .header("content-length", "8")
            .body(Body::from(input))
            .unwrap();
        let response = bounded(build_router(fixture.state.clone()).oneshot(request))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        let header = bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut body).poll_frame(cx)
        }))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        let (kind, length) = stream_wire::decode_header(header[..9].try_into().unwrap()).unwrap();
        assert_eq!(kind, HttpStreamRecordKind::ItemBegin);
        assert_eq!(length, 8);
        assert_eq!(
            u64::from_le_bytes(header[9..17].try_into().unwrap()),
            128 * 1024
        );
        let data_header = bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut body).poll_frame(cx)
        }))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        assert_eq!(
            stream_wire::decode_header(data_header.as_ref().try_into().unwrap()).unwrap(),
            (HttpStreamRecordKind::ItemData, 4096)
        );
        let first = bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut body).poll_frame(cx)
        }))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        assert_eq!(first.len(), 4096);
        assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 0);
        let budget = fixture.state.forwarding.payload_budget();
        let used = budget.snapshot().used_bytes;
        let held = budget.try_reserve(fixture.budget_bytes - used).unwrap();
        {
            let waiting = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
            tokio::pin!(waiting);
            assert!(
                matches!(futures::poll!(&mut waiting), Poll::Pending),
                "the next Body poll must wait for B before another IPC read"
            );
        }
        drop(body); // The driver must see abandonment while its IPC pull is pending.
        let (lease, binding) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry))
            .await
            .unwrap_or_else(|_| panic!("shared upstream unavailable after Body drop"));
        let unary = bounded(lease.client().call_bound(&binding, "ping", b""))
            .await
            .unwrap();
        assert_eq!(unary.into_owned_bytes().unwrap(), b"pong");
        drop(lease);
        drop(held);
        drop(first);
        bounded(async {
            while fixture.observed.finished.load(Ordering::SeqCst) != 1
                || fixture.state.upstream_memory_snapshot().shm.used_bytes != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        fixture.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_budget_wait_stops_before_ipc_dispatch() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let fixture = Fixture::start(false, 32 * 1024).await;
    let budget = fixture.state.forwarding.payload_budget();
    let held = budget.try_reserve(fixture.budget_bytes).unwrap();
    let input = [1_u32.to_le_bytes(), 64_u32.to_le_bytes()].concat();
    let request = Request::builder()
        .method("POST")
        .uri("/portable/echo")
        .header("accept", stream_wire::MEDIA_TYPE)
        .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
        .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
        .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
        .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
        .header(
            "x-c2-expected-signature-hash",
            &fixture.entry.signature_hash,
        )
        .header("x-c2-route-uid", &fixture.entry.route_uid)
        .header("content-length", "8")
        .body(Body::from(input))
        .unwrap();
    let app = build_router(fixture.state.clone());
    let pending = tokio::spawn(async move { app.oneshot(request).await });
    wait_for_admission(&fixture.state).await;
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    pending.abort();
    assert!(bounded(pending).await.unwrap_err().is_cancelled());
    bounded(async {
        while fixture.state.forwarding.snapshot().used_operations != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    drop(held);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_stream_terminal_is_delivered_while_another_http_response_fills_b() {
    use axum::body::{Body, HttpBody};
    use axum::http::Request;
    use c2_error::ErrorCode;
    use tower::ServiceExt;

    let fixture = Fixture::start(false, 32 * 1024).await;
    let stream_request = Request::builder()
        .method("POST")
        .uri("/portable/echo")
        .header("accept", stream_wire::MEDIA_TYPE)
        .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
        .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
        .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
        .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
        .header(
            "x-c2-expected-signature-hash",
            &fixture.entry.signature_hash,
        )
        .header("x-c2-route-uid", &fixture.entry.route_uid)
        .header("content-length", "8")
        .body(Body::from(
            [2_u32.to_le_bytes(), 64_u32.to_le_bytes()].concat(),
        ))
        .unwrap();
    let response = bounded(build_router(fixture.state.clone()).oneshot(stream_request))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let token = response.headers()["x-c2-call-token"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut stream_body = response.into_body();
    let header = bounded(std::future::poll_fn(|cx| {
        Pin::new(&mut stream_body).poll_frame(cx)
    }))
    .await
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    let (kind, length) = stream_wire::decode_header(header[..9].try_into().unwrap()).unwrap();
    assert_eq!(kind, HttpStreamRecordKind::ItemBegin);
    assert_eq!(length, 8);
    assert_eq!(u64::from_le_bytes(header[9..17].try_into().unwrap()), 64);
    let data_header = bounded(std::future::poll_fn(|cx| {
        Pin::new(&mut stream_body).poll_frame(cx)
    }))
    .await
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    assert_eq!(
        stream_wire::decode_header(data_header.as_ref().try_into().unwrap()).unwrap(),
        (HttpStreamRecordKind::ItemData, 64)
    );
    let item = bounded(std::future::poll_fn(|cx| {
        Pin::new(&mut stream_body).poll_frame(cx)
    }))
    .await
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    assert_eq!(item.as_ref(), &[0; 64]);
    drop(item);
    drop(header);

    // Keep the actual budgeted Bytes from a second HTTP response alive. Each
    // unary Body pull copies through the same relay B as the stream Body.
    let unary_request = Request::builder()
        .method("POST")
        .uri("/portable/ping")
        .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
        .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
        .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
        .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
        .header(
            "x-c2-expected-signature-hash",
            &fixture.entry.signature_hash,
        )
        .header("x-c2-route-uid", &fixture.entry.route_uid)
        .header("content-length", "4")
        .body(Body::from((32 * 1024_u32).to_le_bytes().to_vec()))
        .unwrap();
    let unary_response = bounded(build_router(fixture.state.clone()).oneshot(unary_request))
        .await
        .unwrap();
    assert_eq!(unary_response.status(), 200);
    let mut unary_body = unary_response.into_body();
    let mut retained = Vec::new();
    while let Some(frame) = bounded(std::future::poll_fn(|cx| {
        Pin::new(&mut unary_body).poll_frame(cx)
    }))
    .await
    {
        retained.push(frame.unwrap().into_data().unwrap());
    }
    assert_eq!(
        retained.iter().map(|chunk| chunk.len()).sum::<usize>(),
        32 * 1024
    );
    let budget = fixture.state.forwarding.payload_budget();
    assert_eq!(budget.snapshot().used_bytes, fixture.budget_bytes);

    let cancel = Request::builder()
        .method("DELETE")
        .uri(format!("/_calls/{token}"))
        .body(Body::empty())
        .unwrap();
    let cancelled = bounded(build_router(fixture.state.clone()).oneshot(cancel))
        .await
        .unwrap();
    assert_eq!(cancelled.status(), 204);

    let terminal_record = bounded(std::future::poll_fn(|cx| {
        Pin::new(&mut stream_body).poll_frame(cx)
    }))
    .await
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    let (kind, length) =
        stream_wire::decode_header(terminal_record[..9].try_into().unwrap()).unwrap();
    assert_eq!(kind, HttpStreamRecordKind::CallEnd);
    assert_eq!(length as usize, terminal_record.len() - 9);
    let terminal = stream_wire::decode_terminal(&terminal_record[9..]).unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::Failed);
    assert_eq!(terminal.complete_items, Some(1));
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert_eq!(budget.snapshot().used_bytes, fixture.budget_bytes);
    assert!(
        retained
            .iter()
            .all(|chunk| chunk.iter().all(|&byte| byte == 7))
    );
    assert!(
        bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut stream_body).poll_frame(cx)
        }))
        .await
        .is_none()
    );
    drop(retained);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_data_header_refunds_its_payload_before_blocked_cleanup() {
    use axum::body::{Body, HttpBody};
    use axum::http::Request;
    use tower::ServiceExt;
    let fixture = Fixture::start(false, 16 * 1024).await;
    let gate = Arc::new(FinishGate {
        entered: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
    });
    *fixture.observed.finish_gate.lock() = Some(gate.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/portable/echo")
        .header("accept", stream_wire::MEDIA_TYPE)
        .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
        .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
        .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
        .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
        .header(
            "x-c2-expected-signature-hash",
            &fixture.entry.signature_hash,
        )
        .header("x-c2-route-uid", &fixture.entry.route_uid)
        .header("content-length", "8")
        .body(Body::from(
            [1_u32.to_le_bytes(), 8192_u32.to_le_bytes()].concat(),
        ))
        .unwrap();
    let response = bounded(build_router(fixture.state.clone()).oneshot(request))
        .await
        .unwrap();
    let mut body = response.into_body();
    for expected in [
        HttpStreamRecordKind::ItemBegin,
        HttpStreamRecordKind::ItemData,
    ] {
        let header = bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut body).poll_frame(cx)
        }))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        assert_eq!(
            stream_wire::decode_header(header[..9].try_into().unwrap())
                .unwrap()
                .0,
            expected
        );
    }
    assert_eq!(
        fixture
            .state
            .forwarding
            .payload_budget()
            .snapshot()
            .used_bytes,
        4096
    );
    let release = ReleaseFinish(gate.clone());
    drop(body);
    gate.entered.wait();
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .state
            .forwarding
            .payload_budget()
            .snapshot()
            .used_bytes,
        0,
        "an abandoned local payload must not stay charged through unrelated cleanup"
    );
    drop(release);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_cancel_after_lifecycle_cancel_finishes_without_another_copy_window() {
    use axum::body::{Body, HttpBody};
    use axum::http::Request;
    use tower::ServiceExt;
    for shm in [false, true] {
        let fixture = Fixture::start(shm, 32 * 1024).await;
        let request = Request::builder()
            .method("POST")
            .uri("/portable/echo")
            .header("accept", stream_wire::MEDIA_TYPE)
            .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
            .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
            .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
            .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
            .header(
                "x-c2-expected-signature-hash",
                &fixture.entry.signature_hash,
            )
            .header("x-c2-route-uid", &fixture.entry.route_uid)
            .header("content-length", "8")
            .body(Body::from(
                [1_u32.to_le_bytes(), (128 * 1024_u32).to_le_bytes()].concat(),
            ))
            .unwrap();
        let response = bounded(build_router(fixture.state.clone()).oneshot(request))
            .await
            .unwrap();
        let token = response.headers()["x-c2-call-token"]
            .to_str()
            .unwrap()
            .to_owned();
        let mut body = response.into_body();
        // BEGIN, DATA header, then the first charged 4096-byte payload.
        for _ in 0..2 {
            bounded(std::future::poll_fn(|cx| {
                Pin::new(&mut body).poll_frame(cx)
            }))
            .await
            .unwrap()
            .unwrap();
        }
        let retained = bounded(std::future::poll_fn(|cx| {
            Pin::new(&mut body).poll_frame(cx)
        }))
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
        assert_eq!(retained.len(), 4096);
        let budget = fixture.state.forwarding.payload_budget();
        let other = budget
            .try_reserve(fixture.budget_bytes - budget.snapshot().used_bytes)
            .unwrap();
        {
            let waiting = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
            tokio::pin!(waiting);
            assert!(matches!(futures::poll!(&mut waiting), Poll::Pending));
            fixture.state.forwarding.close(); // First reason is lifecycle.
            let cancel = Request::builder()
                .method("DELETE")
                .uri(format!("/_calls/{token}"))
                .body(Body::empty())
                .unwrap();
            let response = bounded(build_router(fixture.state.clone()).oneshot(cancel))
                .await
                .unwrap();
            assert_eq!(response.status(), 204);
            let record = bounded(waiting)
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            let (kind, length) =
                stream_wire::decode_header(record[..9].try_into().unwrap()).unwrap();
            assert_eq!(kind, HttpStreamRecordKind::CallEnd);
            assert_eq!(length as usize, record.len() - 9);
            let terminal = stream_wire::decode_terminal(&record[9..]).unwrap();
            assert_eq!(terminal.status, HttpStreamStatus::Failed);
            assert!(terminal.error.unwrap().message.contains("relay shutdown"));
            assert_eq!(terminal.complete_items, Some(u64::from(shm)));
            assert_eq!(budget.snapshot().used_bytes, fixture.budget_bytes);
        }
        drop(other);
        drop(retained);
        drop(body);
        fixture.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_http_call_modes_are_not_executed_and_keep_upstream() {
    use crate::client::{
        HttpCallControl, HttpCallPhase, HttpError, HttpStreamCancellation, RelayAwareClientConfig,
        RelayAwareHttpClient,
    };
    use c2_error::ErrorCode;
    let fixture = Fixture::start(false, 64 * 1024).await;
    let (lease, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry))
        .await
        .unwrap_or_else(|_| panic!("upstream acquire failed"));
    let original = lease.client();
    let original_observation = original.connection_observation().unwrap();
    drop(lease);

    let response = bounded(
        fixture
            .method_request("echo", false)
            .body([1u32.to_le_bytes(), 64u32.to_le_bytes()].concat())
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 406);
    let error: serde_json::Value =
        serde_json::from_slice(&bounded(response.bytes()).await.unwrap()).unwrap();
    assert_eq!(error["code"], 716);
    assert_eq!(error["name"], "UnsupportedCallMode");
    assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
    assert_eq!(error["details"]["call_mode"], "unary");
    assert_eq!(error["details"]["method_mode"], "server_stream");
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.observed.produced.load(Ordering::SeqCst), 0);
    let response = bounded(
        fixture
            .method_request("ping", false)
            .body(Vec::new())
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(bounded(response.bytes()).await.unwrap().as_ref(), b"pong");

    let before = fixture.observed.unary_calls.load(Ordering::SeqCst);
    let response = bounded(fixture.method_request("ping", true).body(Vec::new()).send())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], stream_wire::MEDIA_TYPE);
    let body = bounded(response.bytes()).await.unwrap();
    let (kind, len) = stream_wire::decode_header(body[..9].try_into().unwrap()).unwrap();
    assert_eq!(kind, HttpStreamRecordKind::CallEnd);
    assert_eq!(
        len as usize,
        body.len() - 9,
        "one terminal and no other records"
    );
    let terminal = stream_wire::decode_terminal(&body[9..]).unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::NotExecuted);
    assert_eq!(terminal.complete_items, None);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::UnsupportedCallMode);
    assert_eq!(fixture.observed.unary_calls.load(Ordering::SeqCst), before);
    let response = bounded(
        fixture
            .method_request("ping", false)
            .body(Vec::new())
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(bounded(response.bytes()).await.unwrap().as_ref(), b"pong");

    let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "portable".into(),
        crm_ns: descriptor.crm_namespace().into(),
        crm_name: descriptor.crm_name().into(),
        crm_ver: descriptor.crm_version().into(),
        abi_hash: descriptor.abi_hash().into(),
        signature_hash: descriptor.signature_hash().into(),
    };
    let client = RelayAwareHttpClient::new(
        &format!("http://{}", fixture.socket),
        expected,
        false,
        RelayAwareClientConfig::default(),
    )
    .unwrap()
    .with_http_only();
    let budget = c2_mem::retention::PayloadBufferBudget::new(64 * 1024);
    let control =
        HttpCallControl::new(|| Ok(()), || Ok(())).with_payload_budget(budget.clone(), None);
    let error = bounded(client.call_controlled_async(
        "echo",
        Arc::new([1u32.to_le_bytes(), 64u32.to_le_bytes()].concat()),
        &control,
    ))
    .await
    .unwrap_err();
    assert_eq!(error.phase(), HttpCallPhase::PreDispatch);
    assert!(error.is_retry_safe());
    let (status, envelope) = match error.source_error() {
        HttpError::ServerError(status, body) => (*status, body.as_bytes()),
        HttpError::ResponseError(status, body) => (*status, body.as_ref()),
        other => panic!("expected HTTP error envelope: {other:?}"),
    };
    assert_eq!(status, 406);
    let envelope: serde_json::Value = serde_json::from_slice(envelope).unwrap();
    assert_eq!(envelope["code"], 716);
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    let before = fixture.observed.unary_calls.load(Ordering::SeqCst);
    let mut stream = bounded(client.stream_controlled_async(
        "ping",
        Arc::new(Vec::<u8>::new()),
        budget.try_reserve(1).unwrap(),
        Box::new(()),
        HttpStreamCancellation::new(),
        &control,
    ))
    .await
    .unwrap();
    let error = bounded(stream.recv()).await.unwrap_err();
    let HttpError::LocalCallRejected(error) = error else {
        panic!("expected NotExecuted")
    };
    assert_eq!(error.code, ErrorCode::UnsupportedCallMode);
    assert_eq!(error.details["transport_phase"], "pre_dispatch");
    assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
    let terminal = bounded(stream.completion()).await.unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::NotExecuted);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::UnsupportedCallMode);
    assert_eq!(fixture.observed.unary_calls.load(Ordering::SeqCst), before);
    assert_eq!(
        bounded(client.call_controlled_async("ping", Arc::new(Vec::<u8>::new()), &control))
            .await
            .unwrap()
            .as_ref(),
        b"pong"
    );
    drop(stream);
    drop(client);
    let (lease, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry))
        .await
        .unwrap_or_else(|_| panic!("upstream reacquire failed"));
    assert!(Arc::ptr_eq(&original, &lease.client()));
    assert!(original.is_connected());
    assert_eq!(
        original.connection_observation().unwrap().connection_id(),
        original_observation.connection_id()
    );
    drop(lease);
    fixture.stop().await;
}

// Requirement 9b4.1: stream payload Bytes, including a slice, keep their
// original allocation charged after the response Body has reached CALL_END.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_body_last_bytes_reference_refunds_b() {
    use axum::{body::Body, http::Request};
    use futures::StreamExt;
    use tower::ServiceExt;

    const LEN: usize = 4096;
    for shm in [false, true] {
        let fixture = Fixture::start(shm, 32 * 1024).await;
        let request = Request::builder()
            .method("POST")
            .uri("/portable/echo")
            .header("accept", stream_wire::MEDIA_TYPE)
            .header("x-c2-expected-crm-ns", &fixture.entry.crm_ns)
            .header("x-c2-expected-crm-name", &fixture.entry.crm_name)
            .header("x-c2-expected-crm-ver", &fixture.entry.crm_ver)
            .header("x-c2-expected-abi-hash", &fixture.entry.abi_hash)
            .header(
                "x-c2-expected-signature-hash",
                &fixture.entry.signature_hash,
            )
            .header("x-c2-route-uid", &fixture.entry.route_uid)
            .header("content-length", "8")
            .body(Body::from(
                [1u32.to_le_bytes(), (LEN as u32).to_le_bytes()].concat(),
            ))
            .unwrap();
        let response = bounded(build_router(fixture.state.clone()).oneshot(request))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut body = response.into_body().into_data_stream();
        let begin = bounded(body.next()).await.unwrap().unwrap();
        assert_eq!(begin.as_ref(), stream_wire::encode_item_begin(LEN as u64));
        let header = bounded(body.next()).await.unwrap().unwrap();
        assert_eq!(
            header.as_ref(),
            stream_wire::encode_header(HttpStreamRecordKind::ItemData, LEN as u64)
        );
        let bytes = bounded(body.next()).await.unwrap().unwrap();
        assert_eq!(bytes.as_ref(), &[0; LEN]);
        let clone = bytes.clone();
        let slice = clone.slice(7..12);
        drop(bytes);
        let end = bounded(body.next()).await.unwrap().unwrap();
        let terminal = stream_wire::decode_terminal(&end[9..]).unwrap();
        assert_eq!(terminal.status, HttpStreamStatus::Success);
        assert_eq!(terminal.complete_items, Some(1));
        assert!(bounded(body.next()).await.is_none());
        drop(body);
        fixture.state.forwarding.close();
        bounded(fixture.state.forwarding.drain()).await;
        let budget = fixture.state.forwarding.payload_budget();
        assert_eq!(fixture.state.forwarding.snapshot().used_operations, 0);
        assert_eq!(budget.snapshot().used_bytes, LEN as u64);
        drop(clone);
        assert_eq!(budget.snapshot().used_bytes, LEN as u64);
        assert_eq!(slice.as_ref(), &[0; 5]);
        drop(slice);
        assert_eq!(budget.snapshot().used_bytes, 0);
        fixture.stop().await;
    }
}

// Requirement 9b4.2: a real one-hop relay reaches the public client with an
// item larger than its B; local refusal cannot replace the resource's end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_oversized_item_keeps_real_terminal_and_refunds_b() {
    use crate::client::{
        HttpCallControl, HttpError, HttpStreamCancellation, RelayAwareClientConfig,
        RelayAwareHttpClient,
    };
    use c2_error::ErrorCode;
    use c2_mem::retention::PayloadBufferBudget;

    let fixture = Fixture::start(true, 64 * 1024).await;
    let gate = Arc::new(FinishGate {
        entered: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
    });
    *fixture.observed.finish_gate.lock() = Some(gate.clone());
    let client = RelayAwareHttpClient::new(
        &format!("http://{}", fixture.socket),
        c2_contract::ExpectedRouteContract {
            route_name: fixture.entry.name.clone(),
            crm_ns: fixture.entry.crm_ns.clone(),
            crm_name: fixture.entry.crm_name.clone(),
            crm_ver: fixture.entry.crm_ver.clone(),
            abi_hash: fixture.entry.abi_hash.clone(),
            signature_hash: fixture.entry.signature_hash.clone(),
        },
        false,
        RelayAwareClientConfig::default(),
    )
    .unwrap()
    .with_http_only();
    let budget = PayloadBufferBudget::new(4096);
    let control =
        HttpCallControl::new(|| Ok(()), || Ok(())).with_payload_budget(budget.clone(), None);
    let mut stream = bounded(client.stream_controlled_async(
        "echo",
        Arc::new([1u32.to_le_bytes(), 8192u32.to_le_bytes()].concat()),
        budget.try_reserve(1).unwrap(),
        Box::new(()),
        HttpStreamCancellation::new(),
        &control,
    ))
    .await
    .unwrap();
    gate.entered.wait();
    let release = ReleaseFinish(gate);
    let error = bounded(stream.recv()).await.unwrap_err();
    let HttpError::LocalCallRejected(error) = error else {
        panic!("expected local item capacity refusal: {error:?}")
    };
    assert_eq!(error.code, ErrorCode::CallCapacityExceeded);
    assert_eq!(error.details["stage"], "payload_buffer_admission");
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 0);
    {
        let terminal = stream.completion();
        tokio::pin!(terminal);
        assert!(futures::poll!(&mut terminal).is_pending());
    }
    drop(release);
    let terminal = bounded(stream.completion()).await.unwrap();
    assert_ne!(terminal.status, HttpStreamStatus::NotExecuted);
    assert_eq!(terminal.complete_items, Some(1));
    if let Some(error) = terminal.error {
        assert_eq!(error.code, ErrorCode::CallCancelled);
    }
    assert_eq!(fixture.observed.produced.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(budget.snapshot().rejected_allocations, 1);
    drop(stream);
    drop(client);
    fixture.stop().await;
}

// Requirement 9b4.5: observe the actual relay/upstream IPC connection through
// its close barrier, so an extra OPEN, CALL_END or CREDIT cannot escape count.
// LocalStream selects UDS on Unix and Named Pipes on Windows for the same proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_unary_upstream_has_one_open_one_call_end_no_credit() {
    use c2_wire::rpc::RpcRecordKind;

    let frames = RpcFrames::default();
    let fixture = Fixture::start_with_frame_tap(false, 64 * 1024, Some(frames.clone())).await;
    let response = bounded(
        fixture
            .method_request("ping", false)
            .body(Vec::new())
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(bounded(response.bytes()).await.unwrap().as_ref(), b"pong");
    assert_eq!(fixture.observed.unary_calls.load(Ordering::SeqCst), 1);
    fixture.stop().await;
    let frames = frames.lock();
    assert_eq!(
        frames.len(),
        2,
        "the whole IPC lifetime has exactly two RPC records: {frames:?}"
    );
    assert_eq!(frames[0].0, false);
    assert_eq!(frames[0].2, RpcRecordKind::Open);
    assert_eq!(frames[1], (true, frames[0].1, RpcRecordKind::CallEnd));
}
