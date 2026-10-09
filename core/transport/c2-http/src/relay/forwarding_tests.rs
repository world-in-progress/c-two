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

fn request_head(
    socket: std::net::SocketAddr,
    entry: &RouteEntry,
    method: &str,
    framing: &str,
) -> String {
    format!(
        "POST /grid/{method} HTTP/1.1\r\nHost: {socket}\r\n{framing}\r\nConnection: close\r\nx-c2-expected-crm-ns: test.echo\r\nx-c2-expected-crm-name: Echo\r\nx-c2-expected-crm-ver: 0.1.0\r\nx-c2-expected-abi-hash: {TEST_ABI_HASH}\r\nx-c2-expected-signature-hash: {TEST_SIGNATURE_HASH}\r\nx-c2-route-uid: {}\r\nx-c2-route-revision: {}\r\n\r\n",
        entry.route_uid, entry.route_revision
    )
}

async fn request_capacity_rejection(
    socket: std::net::SocketAddr,
    entry: &RouteEntry,
    input_len: usize,
) -> Vec<u8> {
    let mut connection = bounded(tokio::net::TcpStream::connect(socket))
        .await
        .unwrap();
    // Capacity is decided from headers before Body is polled. An eager,
    // separately written upload can race Hyper closing an unread H1 body and
    // lose the response to a TCP reset. Use the real H1 continue handshake:
    // advertise the full input charge, but do not send it without a 100.
    // This rejection-only helper requires a complete final response; a 100,
    // reset, EOF or timeout remains a failure, never an excuse to retry.
    let head = request_head(
        socket,
        entry,
        "ping",
        &format!("Content-Length: {input_len}\r\nExpect: 100-continue"),
    );
    bounded(connection.write_all(head.as_bytes()))
        .await
        .unwrap();
    bounded(read_framed_reply(&mut connection)).await.unwrap()
}

// Exercise production header-only capacity rejection through Axum/Hyper over
// real TCP, without an IPC listener. Both operation and byte exhaustion must
// respond before requesting any upload; the disconnect cases below still own
// the real shared IPC, callback counts, carriers and drain/stop proof.
#[tokio::test]
async fn h1_capacity_rejection_precedes_continue_and_upload() {
    capacity_rejection_case(CapacityRequest::Continue).await;
}

#[tokio::test]
async fn capacity_rejection_does_not_poll_body() {
    capacity_rejection_case(CapacityRequest::Direct).await;
}

#[tokio::test]
async fn controlled_eager_http_receives_production_capacity_rejection() {
    capacity_rejection_case(CapacityRequest::ControlledEager).await;
}

#[tokio::test]
async fn eager_capacity_rejection_consumes_declared_body_without_admission() {
    capacity_rejection_case(CapacityRequest::DirectEager).await;
}

#[tokio::test]
async fn capacity_freed_during_rejected_body_cleanup_does_not_readmit() {
    capacity_rejection_case(CapacityRequest::DirectEagerRelease).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CapacityRequest {
    Continue,
    ControlledEager,
    Direct,
    DirectEager,
    DirectEagerRelease,
}

async fn capacity_rejection_case(kind: CapacityRequest) {
    let network = matches!(
        kind,
        CapacityRequest::Continue | CapacityRequest::ControlledEager
    );
    let input_len = if matches!(
        kind,
        CapacityRequest::ControlledEager
            | CapacityRequest::DirectEager
            | CapacityRequest::DirectEagerRelease
    ) {
        4 * 1024 * 1024
    } else {
        INPUT_LEN
    };
    for operations in [1, 2] {
        let listener = if network {
            Some(
                bounded(tokio::net::TcpListener::bind("127.0.0.1:0"))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let socket = listener
            .as_ref()
            .map(|listener| listener.local_addr().unwrap())
            .unwrap_or_else(|| "127.0.0.1:9".parse().unwrap());
        let state = Arc::new(RelayState::new_with_execution_limits(
            Arc::new(RelayConfig {
                relay_id: "capacity-http-test".into(),
                ..RelayConfig::default()
            }),
            Arc::new(NoopDisseminator),
            LocalEndpointContext::default_for_platform().unwrap(),
            CallExecutionLimits {
                max_outstanding_calls: operations,
                retained_input_budget_bytes: input_len as u64,
            },
        ));
        let entry = RouteEntry {
            name: "grid".into(),
            relay_id: state.config().relay_id.clone(),
            relay_url: format!("http://{socket}"),
            server_id: Some("capacity-server".into()),
            server_instance_id: Some("capacity-instance".into()),
            ipc_address: Some("ipc://capacity-must-not-dispatch".into()),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: input_len as u64,
            route_uid: "capacity-route".into(),
            route_revision: 1,
            locality: super::types::Locality::Local,
            registered_at: 1000.0,
        };
        state.with_route_table_mut(|table| assert!(table.register_route(entry.clone())));
        let (release, released) = oneshot::channel();
        let release = Arc::new(parking_lot::Mutex::new(Some(release)));
        let pending = state
            .forwarding
            .spawn(input_len as u64, |permit| async move {
                let _ = released.await;
                drop(permit);
            })
            .unwrap();
        let (stop, stopped) = oneshot::channel();
        let resolves = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let mut app = build_router(state.clone());
        if kind == CapacityRequest::ControlledEager {
            let observed = resolves.clone();
            let observed_posts = posts.clone();
            app = app.layer(axum::middleware::from_fn(
                move |request: axum::http::Request<axum::body::Body>,
                      next: axum::middleware::Next| {
                    let observed = observed.clone();
                    let observed_posts = observed_posts.clone();
                    async move {
                        assert!(!request.headers().contains_key("expect"));
                        assert!(!request.headers().contains_key("connection"));
                        if request.uri().path() == "/_resolve/grid" {
                            observed.fetch_add(1, Ordering::SeqCst);
                        }
                        if request.method() == axum::http::Method::POST
                            && request.uri().path() == "/grid/ping"
                        {
                            observed_posts.fetch_add(1, Ordering::SeqCst);
                        }
                        next.run(request).await
                    }
                },
            ));
        }
        let mut http_task = listener.map(|listener| {
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
            })
        });
        let result = AssertUnwindSafe(async {
            let envelope: serde_json::Value = if kind == CapacityRequest::ControlledEager {
                use crate::client::{
                    HttpCallControl, HttpError, RelayAwareClientConfig, RelayAwareHttpClient,
                };
                let expected = c2_contract::ExpectedRouteContract {
                    route_name: entry.name.clone(),
                    crm_ns: entry.crm_ns.clone(),
                    crm_name: entry.crm_name.clone(),
                    crm_ver: entry.crm_ver.clone(),
                    abi_hash: entry.abi_hash.clone(),
                    signature_hash: entry.signature_hash.clone(),
                };
                let config = RelayAwareClientConfig {
                    max_attempts: 3,
                    ..Default::default()
                };
                let client = RelayAwareHttpClient::new(&entry.relay_url, expected, false, config)
                    .unwrap()
                    .with_http_only();
                let dispatches = Arc::new(AtomicUsize::new(0));
                let observed = dispatches.clone();
                let control = HttpCallControl::new(
                    || Ok(()),
                    move |previous| {
                        assert!(
                            previous.is_none(),
                            "capacity refusal cannot authorize a repeated POST"
                        );
                        observed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                );
                // Exercise the actual SDK transport: an eager owned body,
                // no Expect or Connection override, and no hidden POST retry.
                // This performs production resolve and POST, without the
                // separate connect-time probe. Three acquisition attempts
                // must still produce exactly one business dispatch.
                let mut envelope = serde_json::Value::Null;
                for calls in 1..=2 {
                    let error = bounded(client.call_controlled_async(
                        "ping",
                        Arc::new(vec![7; input_len]),
                        &control,
                    ))
                    .await
                    .unwrap_err();
                    assert_eq!(dispatches.load(Ordering::SeqCst), calls);
                    assert_eq!(
                        posts.load(Ordering::SeqCst),
                        calls,
                        "the HTTP client must not hide a POST replay"
                    );
                    // The HTTP wrapper conservatively preserves uncertainty;
                    // the canonical capacity envelope still says pre_dispatch.
                    assert_eq!(
                        error.phase(),
                        crate::client::HttpCallPhase::DispatchUncertain
                    );
                    let HttpError::ServerError(status, body) = error.source_error() else {
                        panic!("eager SDK call lost the production capacity response: {error:?}");
                    };
                    assert_eq!(*status, 502);
                    envelope = serde_json::from_str(body).unwrap();
                    assert_eq!(envelope["code"], 717);
                    assert_eq!(envelope["details"]["dispatch_phase"], "pre_dispatch");
                    assert_eq!(
                        state.forwarding.snapshot().rejected_reservations,
                        calls as u64
                    );
                    assert!(state.local_route("grid").is_some());
                    assert_eq!(
                        resolves.load(Ordering::SeqCst),
                        1,
                        "capacity must not invalidate the resolved route"
                    );
                }
                envelope
            } else if network {
                let reply = request_capacity_rejection(socket, &entry, input_len).await;
                assert!(
                    reply.starts_with(b"HTTP/1.1 502 "),
                    "{}",
                    String::from_utf8_lossy(&reply)
                );
                let header_end = reply
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap();
                serde_json::from_slice(&reply[header_end + 4..]).unwrap()
            } else {
                use tower::ServiceExt;
                let mut framing = format!("Content-Length: {input_len}");
                if kind == CapacityRequest::Direct {
                    framing.push_str("\r\nExpect: 100-continue");
                }
                let head = request_head(socket, &entry, "ping", &framing);
                let mut request = axum::http::Request::builder()
                    .method("POST")
                    .uri("/grid/ping");
                for line in head
                    .split("\r\n")
                    .skip(1)
                    .take_while(|line| !line.is_empty())
                {
                    let (name, value) = reply_header(line.as_bytes()).unwrap();
                    request = request.header(name, value);
                }
                let frames = Arc::new(AtomicUsize::new(0));
                let eof = Arc::new(AtomicUsize::new(0));
                let eager = matches!(
                    kind,
                    CapacityRequest::DirectEager | CapacityRequest::DirectEagerRelease
                );
                let body = if eager {
                    let frames = frames.clone();
                    let eof = eof.clone();
                    let release = release.clone();
                    let state = state.clone();
                    let bytes = bytes::Bytes::from(vec![7; input_len]);
                    axum::body::Body::from_stream(futures::stream::unfold(
                        (bytes, 0),
                        move |(bytes, offset)| {
                            let frames = frames.clone();
                            let eof = eof.clone();
                            let release = release.clone();
                            let state = state.clone();
                            async move {
                                if offset == 0 && kind == CapacityRequest::DirectEagerRelease {
                                    assert_eq!(
                                        state.forwarding.snapshot().rejected_reservations,
                                        1
                                    );
                                    release.lock().take().unwrap().send(()).unwrap();
                                    eventually(|| state.forwarding.snapshot().used_operations == 0)
                                        .await;
                                }
                                if offset == bytes.len() {
                                    eof.fetch_add(1, Ordering::SeqCst);
                                    return None;
                                }
                                let end = (offset + 64 * 1024).min(bytes.len());
                                frames.fetch_add(1, Ordering::SeqCst);
                                Some((
                                    Ok::<_, std::io::Error>(bytes.slice(offset..end)),
                                    (bytes, end),
                                ))
                            }
                        },
                    ))
                } else {
                    axum::body::Body::from_stream(futures::stream::once(async {
                        panic!("early capacity rejection must not poll the request body");
                        #[allow(unreachable_code)]
                        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::new())
                    }))
                };
                let response = bounded(app.oneshot(request.body(body).unwrap()))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                if eager {
                    assert_eq!(frames.load(Ordering::SeqCst), input_len / (64 * 1024));
                    assert_eq!(
                        eof.load(Ordering::SeqCst),
                        1,
                        "transport must reach EOF before replying"
                    );
                }
                let body = bounded(axum::body::to_bytes(response.into_body(), MAX_REPLY))
                    .await
                    .unwrap();
                serde_json::from_slice(&body).unwrap()
            };
            assert_eq!(envelope["version"], 1);
            assert_eq!(envelope["code"], 717);
            assert_eq!(envelope["name"], "CallCapacityExceeded");
            assert_eq!(envelope["details"]["dispatch_phase"], "pre_dispatch");
            assert_eq!(envelope["details"]["route"], "grid");
            assert_eq!(
                state.forwarding.snapshot().rejected_reservations,
                if kind == CapacityRequest::ControlledEager {
                    2
                } else {
                    1
                }
            );
            let freed = kind == CapacityRequest::DirectEagerRelease;
            assert_eq!(
                state.forwarding.snapshot().used_operations,
                if freed { 0 } else { 1 }
            );
            assert_eq!(
                state.forwarding.snapshot().used_retained_bytes,
                if freed { 0 } else { input_len as u64 }
            );
            assert!(state.local_route("grid").is_some());
        })
        .catch_unwind()
        .await;
        // Even assertion failure releases the capacity owner and joins the
        // actual listener. Bound shutdown, aborting/joining on a stuck server.
        if let Some(release) = release.lock().take() {
            let _ = release.send(());
        }
        bounded(pending).await.unwrap();
        state.forwarding.close();
        bounded(state.forwarding.drain()).await;
        let _ = stop.send(());
        if let Some(http_task) = http_task.as_mut() {
            let stopped = tokio::time::timeout(STEP, &mut *http_task).await;
            if stopped.is_err() {
                http_task.abort();
                let _ = http_task.await;
            }
            stopped
                .expect("capacity HTTP listener did not stop")
                .unwrap()
                .unwrap();
        }
        assert_eq!(state.forwarding.snapshot().used_operations, 0);
        assert_eq!(state.forwarding.snapshot().used_retained_bytes, 0);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
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
        b"HTTP/1.1 100 Continue\r\n\r\n".as_slice(),
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
        request_head(self.socket, &self.entry, method, framing)
    }

    async fn request_capacity_rejection(&self, input_len: usize) -> Vec<u8> {
        let reply = request_capacity_rejection(self.socket, &self.entry, input_len).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_http_capacity_freed_during_rejected_upload_never_dispatches() {
    let mut fixture = Fixture::start(Carrier::Inline, 1, INPUT_LEN as u64).await;
    let result = AssertUnwindSafe(async {
        let mut admitted = bounded(tokio::net::TcpStream::connect(fixture.socket))
            .await
            .unwrap();
        bounded(
            admitted.write_all(
                fixture
                    .head("target", &format!("Content-Length: {INPUT_LEN}"))
                    .as_bytes(),
            ),
        )
        .await
        .unwrap();
        bounded(admitted.write_all(&vec![7; INPUT_LEN]))
            .await
            .unwrap();
        bounded(fixture.probe.entered[1].notified()).await;
        assert_eq!(fixture.state.forwarding.snapshot().used_operations, 1);

        let mut rejected = bounded(tokio::net::TcpStream::connect(fixture.socket))
            .await
            .unwrap();
        bounded(
            rejected.write_all(
                fixture
                    .head("ping", &format!("Content-Length: {INPUT_LEN}"))
                    .as_bytes(),
            ),
        )
        .await
        .unwrap();
        bounded(rejected.write_all(&[7])).await.unwrap();
        eventually(|| fixture.state.forwarding.snapshot().rejected_reservations == 1).await;
        fixture.release(1);
        let reply = bounded(read_framed_reply(&mut admitted)).await.unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 200 "));
        eventually(|| fixture.state.forwarding.snapshot().used_operations == 0).await;
        assert!(
            !fixture.state.forwarding.snapshot().closed,
            "domain must stay open while B finishes uploading"
        );
        assert_eq!(fixture.state.forwarding.snapshot().used_retained_bytes, 0);

        bounded(rejected.write_all(&vec![7; INPUT_LEN - 1]))
            .await
            .unwrap();
        let reply = bounded(read_framed_reply(&mut rejected)).await.unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 502 "));
        let end = reply
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&reply[end + 4..]).unwrap();
        assert_eq!(error["code"], 717);
        assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
        assert_eq!(fixture.state.forwarding.snapshot().rejected_reservations, 1);
        assert_eq!(fixture.state.forwarding.snapshot().used_operations, 0);
        assert_eq!(fixture.state.forwarding.snapshot().used_retained_bytes, 0);
        assert_eq!(
            fixture
                .probe
                .calls
                .each_ref()
                .map(|count| count.load(Ordering::SeqCst)),
            [0, 1, 0]
        );
        assert!(fixture.state.local_route("grid").is_some());
        assert!(fixture.client.is_connected());
        let (lease, _, _) = bounded(fixture.state.acquire_upstream_for_route(&fixture.entry))
            .await
            .unwrap_or_else(|_| panic!("healthy shared IPC must remain available"));
        assert!(Arc::ptr_eq(&lease.client(), &fixture.client));
        drop(lease);
    })
    .catch_unwind()
    .await;
    fixture.stop().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
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
        let rejected = fixture.request_capacity_rejection(INPUT_LEN).await;
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
        let reply = bounded(read_framed_reply(&mut upload)).await.unwrap();
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
        let rejected = fixture.request_capacity_rejection(LEN).await;
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
        let reply = bounded(read_framed_reply(&mut upload)).await.unwrap();
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
