//! Real HTTP exchanges for the explicit per-call control seam.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use super::{
    HttpCallControl, HttpCallError, HttpCallPhase, HttpError, RelayAwareClientConfig,
    RelayAwareHttpClient, RelayRouteInfo,
};

const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SIGNATURE_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn expected() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test.controlled".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: ABI_HASH.into(),
        signature_hash: SIGNATURE_HASH.into(),
    }
}

fn route(url: &str) -> RelayRouteInfo {
    let contract = expected();
    RelayRouteInfo {
        name: contract.route_name,
        relay_url: url.into(),
        route_uid: "controlled-grid-route".into(),
        route_revision: 7,
        ipc_address: None,
        server_id: None,
        server_instance_id: None,
        crm_ns: contract.crm_ns,
        crm_name: contract.crm_name,
        crm_ver: contract.crm_ver,
        abi_hash: contract.abi_hash,
        signature_hash: contract.signature_hash,
        max_payload_size: 8 * 1024 * 1024,
    }
}

fn rejected() -> C2Error {
    C2Error::new(ErrorCode::ClientCallingResource, "owner deadline expired").with_details(
        BTreeMap::from([
            ("reason".into(), "deadline".into()),
            ("logical_call".into(), "same-scope".into()),
        ]),
    )
}

fn assert_local(error: &HttpCallError) {
    assert_local_source(error.source_error());
}

fn assert_local_source(error: &HttpError) {
    match error {
        HttpError::LocalCallRejected(error) => assert_eq!(error, &rejected()),
        other => panic!("expected intact owner error, got {other:?}"),
    }
}

fn allowed() -> HttpCallControl {
    HttpCallControl::new(|| Ok(()), |_| Ok(()))
}

fn record_dispatches(phases: &Arc<Mutex<Vec<Option<HttpCallPhase>>>>) -> HttpCallControl {
    let phases = Arc::clone(phases);
    HttpCallControl::new(
        || Ok(()),
        move |phase| {
            phases.lock().push(phase);
            Ok(())
        },
    )
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { url, task }
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    resume: Notify,
}

impl Gate {
    async fn wait(&self) {
        self.entered.notify_one();
        self.resume.notified().await;
    }
}

#[derive(Clone)]
struct Registry {
    routes: Vec<RelayRouteInfo>,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
}

async fn resolve(State(state): State<Registry>) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if let Some(gate) = state.gate {
        gate.wait().await;
    }
    Json(state.routes).into_response()
}

async fn registry(urls: &[&str], gate: Option<Arc<Gate>>) -> (Server, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = Registry {
        routes: urls.iter().map(|url| route(url)).collect(),
        calls: Arc::clone(&calls),
        gate,
    };
    let server = serve(
        Router::new()
            .route("/_resolve/{route}", get(resolve))
            .with_state(state),
    )
    .await;
    (server, calls)
}

#[derive(Clone, Copy)]
enum Reply {
    Echo,
    Stale,
    Crm,
    Capacity,
    CapacityPreDispatch,
    Uncertain,
    GenericBadGateway,
}

#[derive(Clone)]
struct Data {
    calls: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    reply: Arc<Mutex<Reply>>,
    probe_gate: Option<Arc<Gate>>,
    call_gate: Option<Arc<Gate>>,
}

impl Data {
    fn new(reply: Reply) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            probes: Arc::new(AtomicUsize::new(0)),
            received: Arc::new(Mutex::new(Vec::new())),
            reply: Arc::new(Mutex::new(reply)),
            probe_gate: None,
            call_gate: None,
        }
    }
}

fn semantic_error(code: ErrorCode, phase: Option<&str>) -> Json<c2_error::C2ErrorEnvelope> {
    let mut error = C2Error::new(code, "test relay refusal");
    error.details.insert("route".into(), "grid".into());
    if let Some(phase) = phase {
        error.details.insert("dispatch_phase".into(), phase.into());
    }
    Json(error.envelope())
}

async fn business(
    State(state): State<Data>,
    Path((_route, _method)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.received.lock().push((headers, body.clone()));
    if let Some(gate) = state.call_gate {
        gate.wait().await;
    }
    match *state.reply.lock() {
        Reply::Echo => (StatusCode::OK, body).into_response(),
        Reply::Stale => (
            StatusCode::CONFLICT,
            semantic_error(ErrorCode::RouteStale, None),
        )
            .into_response(),
        Reply::Crm => (StatusCode::INTERNAL_SERVER_ERROR, b"crm-failure".to_vec()).into_response(),
        Reply::Capacity => (
            StatusCode::TOO_MANY_REQUESTS,
            semantic_error(ErrorCode::ResourceUnavailable, None),
        )
            .into_response(),
        Reply::CapacityPreDispatch => (
            StatusCode::BAD_GATEWAY,
            semantic_error(ErrorCode::ResourceUnavailable, Some("pre_dispatch")),
        )
            .into_response(),
        Reply::Uncertain => (
            StatusCode::BAD_GATEWAY,
            semantic_error(ErrorCode::ResourceUnavailable, Some("dispatch_uncertain")),
        )
            .into_response(),
        Reply::GenericBadGateway => {
            (StatusCode::BAD_GATEWAY, "proxy lost upstream").into_response()
        }
    }
}

async fn probe(State(state): State<Data>) -> StatusCode {
    state.probes.fetch_add(1, Ordering::SeqCst);
    if let Some(gate) = state.probe_gate {
        gate.wait().await;
    }
    StatusCode::OK
}

async fn data_server(state: &Data) -> Server {
    serve(
        Router::new()
            .route("/{route}/{method}", post(business))
            .route("/_probe/{route}", get(probe))
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone()),
    )
    .await
}

fn client(registry: &Server) -> RelayAwareHttpClient {
    RelayAwareHttpClient::new(
        &registry.url,
        expected(),
        false,
        RelayAwareClientConfig::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn initial_active_guard_preserves_error_without_resolve_or_post() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let (registry, resolves) = registry(&[&server.url], None).await;
    let dispatches = Arc::new(AtomicUsize::new(0));
    let dispatch_count = Arc::clone(&dispatches);
    let control = HttpCallControl::new(
        || Err(rejected()),
        move |_| {
            dispatch_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let error = client(&registry)
        .call_controlled_async("step", Arc::new(b"input".to_vec()), &control)
        .await
        .unwrap_err();

    assert_local(&error);
    assert_eq!(resolves.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(data.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dispatch_guard_rejection_keeps_resolved_route_for_later_call() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let (registry, resolves) = registry(&[&server.url], None).await;
    let client = client(&registry);
    let control = HttpCallControl::new(
        || Ok(()),
        |phase| {
            assert_eq!(phase, None);
            Err(rejected())
        },
    );
    let error = client
        .call_controlled_async("step", Arc::new(b"rejected".to_vec()), &control)
        .await
        .unwrap_err();

    assert_local(&error);
    assert_eq!(data.calls.load(Ordering::SeqCst), 0);
    let output = client
        .call_controlled_async("step", Arc::new(b"allowed".to_vec()), &allowed())
        .await
        .unwrap();
    assert_eq!(output, b"allowed");
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owner_deadline_rejection_after_route_wait_never_dispatches() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let gate = Arc::new(Gate::default());
    let (registry, resolves) = registry(&[&server.url], Some(Arc::clone(&gate))).await;
    let client = client(&registry);
    // The owner supplies its terminal deadline observation. HTTP neither
    // constructs nor resets a deadline; events make this test deterministic.
    let expired = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&expired);
    let dispatches = Arc::new(AtomicUsize::new(0));
    let dispatch_count = Arc::clone(&dispatches);
    let control = HttpCallControl::new(
        move || {
            if observed.load(Ordering::SeqCst) {
                Err(rejected())
            } else {
                Ok(())
            }
        },
        move |_| {
            dispatch_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let call = client.call_controlled_async("step", Arc::new(vec![1]), &control);
    let expire_while_resolving = async {
        gate.entered.notified().await;
        expired.store(true, Ordering::SeqCst);
        gate.resume.notify_one();
    };
    let (result, ()) = tokio::join!(call, expire_while_resolving);

    assert_local(&result.unwrap_err());
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(data.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn owner_rejection_after_probe_wait_is_not_business_dispatch() {
    let gate = Arc::new(Gate::default());
    let mut data = Data::new(Reply::Echo);
    data.probe_gate = Some(Arc::clone(&gate));
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let client = client(&registry);
    let expired = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&expired);
    let control = HttpCallControl::new(
        move || {
            if observed.load(Ordering::SeqCst) {
                Err(rejected())
            } else {
                Ok(())
            }
        },
        |_| panic!("a route probe must not invoke the business dispatch guard"),
    );
    let resolve = client.resolve_http_target_controlled_async(&control);
    let expire_while_probing = async {
        gate.entered.notified().await;
        expired.store(true, Ordering::SeqCst);
        gate.resume.notify_one();
    };
    let (result, ()) = tokio::join!(resolve, expire_while_probing);

    assert_local_source(&result.unwrap_err());
    assert_eq!(data.probes.load(Ordering::SeqCst), 1);
    assert_eq!(data.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn authoritative_stale_post_retries_with_predispatch_proof() {
    let stale = Data::new(Reply::Stale);
    let live = Data::new(Reply::Echo);
    let stale_server = data_server(&stale).await;
    let live_server = data_server(&live).await;
    let (registry, resolves) = registry(&[&stale_server.url, &live_server.url], None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let control = record_dispatches(&phases);
    let client = client(&registry);
    let output = client
        .call_controlled_async("step", Arc::new(b"same-input".to_vec()), &control)
        .await
        .unwrap();

    assert_eq!(output, b"same-input");
    assert_eq!(*phases.lock(), vec![None, Some(HttpCallPhase::PreDispatch)]);
    assert_eq!(stale.calls.load(Ordering::SeqCst), 1);
    assert_eq!(live.calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retry_dispatch_guard_can_reject_without_sending_next_post() {
    let stale = Data::new(Reply::Stale);
    let live = Data::new(Reply::Echo);
    let stale_server = data_server(&stale).await;
    let live_server = data_server(&live).await;
    let (registry, _) = registry(&[&stale_server.url, &live_server.url], None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&phases);
    let control = HttpCallControl::new(
        || Ok(()),
        move |phase| {
            observed.lock().push(phase);
            if phase.is_some() {
                Err(rejected())
            } else {
                Ok(())
            }
        },
    );
    let error = client(&registry)
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await
        .unwrap_err();

    assert_local(&error);
    assert_eq!(*phases.lock(), vec![None, Some(HttpCallPhase::PreDispatch)]);
    assert_eq!(stale.calls.load(Ordering::SeqCst), 1);
    assert_eq!(live.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn uncertain_and_crm_failures_never_retry_or_forge_proof() {
    for reply in [Reply::Crm, Reply::Uncertain, Reply::GenericBadGateway] {
        let failed = Data::new(reply);
        let live = Data::new(Reply::Echo);
        let failed_server = data_server(&failed).await;
        let live_server = data_server(&live).await;
        let (registry, resolves) = registry(&[&failed_server.url, &live_server.url], None).await;
        let phases = Arc::new(Mutex::new(Vec::new()));
        let error = client(&registry)
            .call_controlled_async("step", Arc::new(vec![1]), &record_dispatches(&phases))
            .await
            .unwrap_err();

        assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
        match reply {
            Reply::Crm => {
                assert!(
                    matches!(error.source_error(), HttpError::CrmError(bytes) if bytes == b"crm-failure")
                );
            }
            _ => assert!(matches!(
                error.source_error(),
                HttpError::ServerError(502, _)
            )),
        }
        assert_eq!(*phases.lock(), vec![None]);
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        assert_eq!(failed.calls.load(Ordering::SeqCst), 1);
        assert_eq!(live.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn network_loss_after_post_does_not_send_again() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&calls);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Receive the actual POST before simulating a connection loss. No
            // HTTP response exists from which to derive pre-dispatch proof.
            let mut request = Vec::new();
            let mut bytes = [0u8; 1024];
            loop {
                let count = socket.read(&mut bytes).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(request.starts_with(b"POST /grid/step HTTP/1.1\r\n"));
            accepted.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    let lost_server = Server { url, task };
    let live = Data::new(Reply::Echo);
    let live_server = data_server(&live).await;
    let (registry, resolves) = registry(&[&lost_server.url, &live_server.url], None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let error = client(&registry)
        .call_controlled_async(
            "step",
            Arc::new(b"lost".to_vec()),
            &record_dispatches(&phases),
        )
        .await
        .unwrap_err();

    assert!(matches!(error.source_error(), HttpError::Transport(_)));
    assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
    assert_eq!(*phases.lock(), vec![None]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(live.calls.load(Ordering::SeqCst), 0);
}

async fn assert_capacity_preserves_route(reply: Reply, status: u16) {
    let data = Data::new(reply);
    let server = data_server(&data).await;
    let (registry, resolves) = registry(&[&server.url], None).await;
    let client = client(&registry);
    let phases = Arc::new(Mutex::new(Vec::new()));
    let control = record_dispatches(&phases);
    let error = client
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await
        .unwrap_err();

    assert!(matches!(error.source_error(), HttpError::ServerError(code, _) if *code == status));
    assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
    *data.reply.lock() = Reply::Echo;
    assert_eq!(
        client
            .call_controlled_async("step", Arc::new(vec![2]), &control)
            .await
            .unwrap(),
        vec![2]
    );
    assert_eq!(*phases.lock(), vec![None, None]);
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(data.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn capacity_refusal_preserves_cache_and_never_provides_retry_proof() {
    assert_capacity_preserves_route(Reply::Capacity, 429).await;
}

#[tokio::test]
async fn predispatched_capacity_refusal_is_not_a_stale_route_proof() {
    assert_capacity_preserves_route(Reply::CapacityPreDispatch, 502).await;
}

#[tokio::test]
async fn concurrent_calls_keep_their_control_hooks_independent() {
    let gate = Arc::new(Gate::default());
    let mut data = Data::new(Reply::Echo);
    data.call_gate = Some(Arc::clone(&gate));
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let client = client(&registry);
    let good_phases = Arc::new(Mutex::new(Vec::new()));
    let good = record_dispatches(&good_phases);
    let bad_phases = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&bad_phases);
    let bad = HttpCallControl::new(
        || Ok(()),
        move |phase| {
            observed.lock().push(phase);
            Err(rejected())
        },
    );
    let good_call = client.call_controlled_async("step", Arc::new(b"good".to_vec()), &good);
    let reject_other_call = async {
        gate.entered.notified().await;
        let result = client
            .call_controlled_async("step", Arc::new(b"bad".to_vec()), &bad)
            .await;
        gate.resume.notify_one();
        result
    };
    let (good_result, bad_result) = tokio::join!(good_call, reject_other_call);

    assert_eq!(good_result.unwrap(), b"good");
    assert_local(&bad_result.unwrap_err());
    assert_eq!(*good_phases.lock(), vec![None]);
    assert_eq!(*bad_phases.lock(), vec![None]);
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
    assert_eq!(data.received.lock()[0].1, b"good".as_slice());
}

fn assert_headers(headers: &HeaderMap, len: usize) {
    for (key, expected) in [
        ("x-c2-expected-crm-ns", "test.controlled"),
        ("x-c2-expected-crm-name", "Grid"),
        ("x-c2-expected-crm-ver", "0.1.0"),
        ("x-c2-expected-abi-hash", ABI_HASH),
        ("x-c2-expected-signature-hash", SIGNATURE_HASH),
        ("x-c2-route-uid", "controlled-grid-route"),
        ("x-c2-route-revision", "7"),
        ("content-type", "application/octet-stream"),
    ] {
        assert_eq!(headers[key], expected, "header {key}");
    }
    assert_eq!(headers["content-length"], len.to_string());
    assert!(headers.get("transfer-encoding").is_none());
}

#[tokio::test]
async fn owned_payloads_preserve_content_length_contract_headers_and_contents() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let client = RelayAwareHttpClient::new(
        &registry.url,
        expected(),
        false,
        RelayAwareClientConfig {
            remote_payload_chunk_size: 64 * 1024,
            // Controlled data transfers ignore this legacy reqwest timeout.
            // A one-nanosecond setting would expire any legacy exchange.
            call_timeout_secs: 0.000_000_001,
            ..Default::default()
        },
    )
    .unwrap();
    for len in [0, 1, 64 * 1024, 64 * 1024 + 1, 2 * 1024 * 1024 + 17] {
        let input = Arc::new(
            (0..len)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let weak = Arc::downgrade(&input);
        let expected = input.as_ref().clone();
        let result = client
            .call_controlled_async("step", input, &allowed())
            .await
            .unwrap();
        assert_eq!(result, expected);
        let received = data.received.lock();
        let (headers, body) = received.last().unwrap();
        assert_headers(headers, len);
        assert_eq!(body.as_ref(), expected.as_slice());
        // Neither this client nor its pool retains a completed call's owner.
        assert!(weak.upgrade().is_none());
    }
    assert_eq!(data.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn test_owned_driver_finishes_after_application_waiter_leaves() {
    let gate = Arc::new(Gate::default());
    let mut data = Data::new(Reply::Echo);
    data.call_gate = Some(Arc::clone(&gate));
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let client = client(&registry);
    let expired = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&expired);
    let control = HttpCallControl::new(
        move || {
            if observed.load(Ordering::SeqCst) {
                Err(rejected())
            } else {
                Ok(())
            }
        },
        |phase| {
            assert_eq!(phase, None);
            Ok(())
        },
    );
    let len = 64 * 1024 + 7;
    let input = Arc::new(vec![9; len]);
    let pointer = input.as_ptr();
    let weak = Arc::downgrade(&input);
    let (application_sender, application_waiter) = tokio::sync::oneshot::channel();
    // Only this test supplies an owned driver, as the future Core caller will.
    // c2-http itself never spawns or tracks an RPC background task.
    let driver = tokio::spawn(async move {
        let result = client.call_controlled_async("step", input, &control).await;
        let valid = result
            .as_ref()
            .is_ok_and(|bytes| bytes.len() == len && bytes.iter().all(|byte| *byte == 9));
        let _ = application_sender.send(result);
        valid
    });
    gate.entered.notified().await;
    drop(application_waiter);
    expired.store(true, Ordering::SeqCst);
    {
        let retained = weak.upgrade().expect("dispatched task retains its input");
        assert_eq!(retained.as_ptr(), pointer);
        assert_eq!(retained.len(), len);
    }
    gate.resume.notify_one();

    assert!(
        driver.await.unwrap(),
        "the driver must finish the dispatched call"
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
}

// A small advertised native message limit deliberately differs from both the
// request batching policy and the default limit. Both HTTP-only (explicit
// relay) and relay-aware calls must consume the selected route's value.
const RESPONSE_LIMIT: u64 = 8;

#[derive(Clone)]
struct ResponseData {
    status: StatusCode,
    chunked: bool,
    calls: Arc<AtomicUsize>,
    late: Option<Arc<Gate>>,
    tail: Option<Arc<Gate>>,
}

async fn response_business(
    State(state): State<ResponseData>,
    Path((_route, method)): Path<(String, String)>,
    _body: Bytes,
) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if method == "good" {
        return (StatusCode::OK, b"12345678".to_vec()).into_response();
    }
    if let Some(gate) = &state.late {
        gate.wait().await;
    }
    let body = if state.chunked {
        axum::body::Body::from_stream(futures::stream::unfold(
            (0, state.tail),
            |(index, tail)| async move {
                if index == 3 {
                    if let Some(gate) = tail {
                        // No EOF: rejection must happen at the crossing chunk,
                        // rather than after collecting the complete response.
                        gate.wait().await;
                    }
                    return None;
                }
                Some((
                    Ok::<_, std::convert::Infallible>(Bytes::from_static(b"123")),
                    (index + 1, tail),
                ))
            },
        ))
    } else {
        axum::body::Body::from(vec![b'x'; 9])
    };
    (state.status, body).into_response()
}

async fn response_fixture(state: ResponseData) -> (Server, Server, RelayAwareHttpClient) {
    let server = serve(
        Router::new()
            .route("/{route}/{method}", post(response_business))
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .with_state(state),
    )
    .await;
    let mut selected = route(&server.url);
    selected.max_payload_size = RESPONSE_LIMIT;
    let registry = serve(Router::new().route(
        "/_resolve/{route}",
        get(move || {
            let selected = selected.clone();
            async move { Json(vec![selected]) }
        }),
    ))
    .await;
    let client = client(&registry);
    (server, registry, client)
}

fn assert_response_limit(error: &HttpCallError) {
    assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
    assert!(!error.is_retry_safe());
    let HttpError::ServerError(_, body) = error.source_error() else {
        panic!("expected canonical response protocol error, got {error:?}");
    };
    let error = C2Error::from_envelope(serde_json::from_str(body).unwrap()).unwrap();
    assert_eq!(error.code, ErrorCode::ProtocolViolation);
    assert_eq!(error.details["reason"], "response_payload_too_large");
    assert_eq!(
        error.details["max_payload_size"],
        RESPONSE_LIMIT.to_string()
    );
}

#[tokio::test]
async fn response_message_limit_covers_success_crm_and_other_errors_before_eof() {
    for status in [
        StatusCode::OK,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::CONFLICT,
        StatusCode::BAD_GATEWAY,
    ] {
        for chunked in [false, true] {
            for http_only in [false, true] {
                let calls = Arc::new(AtomicUsize::new(0));
                let tail = Arc::new(Gate::default());
                let (_server, _registry, client) = response_fixture(ResponseData {
                    status,
                    chunked,
                    calls: calls.clone(),
                    late: None,
                    tail: Some(tail.clone()),
                })
                .await;
                let client = if http_only {
                    client.with_http_only()
                } else {
                    client
                };
                let control = allowed();
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    client.call_controlled_async("large", Arc::new(vec![1]), &control),
                )
                .await;
                // Unblock the fixture even when running against the faulty reader.
                tail.resume.notify_one();
                let error = result
                    .expect("reader must reject without waiting for EOF")
                    .unwrap_err();
                assert_response_limit(&error);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "oversize must not replay POST"
                );
                assert_eq!(
                    client
                        .call_controlled_async("good", Arc::new(vec![2]), &allowed())
                        .await
                        .unwrap(),
                    b"12345678"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            }
        }
    }
}

#[tokio::test]
async fn response_message_limit_also_covers_borrowed_legacy_calls() {
    let (_server, _registry, client) = response_fixture(ResponseData {
        status: StatusCode::OK,
        chunked: false,
        calls: Arc::new(AtomicUsize::new(0)),
        late: None,
        tail: None,
    })
    .await;
    let error = client.call_async("large", &[1]).await.unwrap_err();
    let HttpError::ServerError(_, body) = error else {
        panic!("expected protocol error: {error:?}");
    };
    assert_eq!(
        C2Error::from_envelope(serde_json::from_str(&body).unwrap())
            .unwrap()
            .code,
        ErrorCode::ProtocolViolation
    );
}

struct ReservedResponseInput {
    bytes: Vec<u8>,
    _permit: c2_mem::RetentionPermit,
}

impl AsRef<[u8]> for ReservedResponseInput {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[tokio::test]
async fn late_response_limit_keeps_owned_input_until_real_driver_completion() {
    for status in [
        StatusCode::OK,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        let late = Arc::new(Gate::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (_server, _registry, client) = response_fixture(ResponseData {
            status,
            chunked: true,
            calls: calls.clone(),
            late: Some(late.clone()),
            tail: None,
        })
        .await;
        let client = Arc::new(client);
        let budget = c2_mem::RetentionBudget::new(1, 4);
        let input = Arc::new(ReservedResponseInput {
            bytes: vec![1; 4],
            _permit: budget.reserve(4).unwrap(),
        });
        let weak = Arc::downgrade(&input);
        let expired = Arc::new(AtomicBool::new(false));
        let observed = expired.clone();
        let active_checks = Arc::new(AtomicUsize::new(0));
        let checks = active_checks.clone();
        let control = HttpCallControl::new(
            move || {
                checks.fetch_add(1, Ordering::SeqCst);
                if observed.load(Ordering::SeqCst) {
                    Err(rejected())
                } else {
                    Ok(())
                }
            },
            |_| Ok(()),
        );
        let driver_client = client.clone();
        let (delivery, waiter) = tokio::sync::oneshot::channel();
        let driver = tokio::spawn(async move {
            let result = driver_client
                .call_controlled_async("large", input, &control)
                .await;
            (result, delivery.send(()).is_ok())
        });
        late.entered.notified().await;
        // The application waits once. Expiry drops only its delivery channel;
        // the independent driver continues owning the actual transport/input.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), waiter)
                .await
                .is_err()
        );
        expired.store(true, Ordering::SeqCst);
        let checks_at_expiry = active_checks.load(Ordering::SeqCst);
        assert!(weak.upgrade().is_some());
        assert_eq!(budget.snapshot().used_operations, 1);
        assert_eq!(budget.snapshot().used_retained_bytes, 4);
        assert_eq!(
            client
                .call_controlled_async("good", Arc::new(vec![2]), &allowed())
                .await
                .unwrap(),
            b"12345678"
        );
        late.resume.notify_one();
        // Controller observation is separate from the application's waiter.
        let (result, delivered) = tokio::time::timeout(std::time::Duration::from_secs(2), driver)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !delivered,
            "late completion cannot deliver a second caller outcome"
        );
        assert_response_limit(&result.unwrap_err());
        assert_eq!(
            active_checks.load(Ordering::SeqCst),
            checks_at_expiry,
            "dispatched response must not recheck caller deadline"
        );
        assert!(weak.upgrade().is_none());
        assert_eq!(budget.snapshot().used_operations, 0);
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one large POST and one healthy sibling"
        );
    }
}

// Exercise the actual reqwest body reader without a listening socket. These
// complement, and never replace, the real HTTP exchanges above.
fn response_from_chunks(
    status: u16,
    known_length: bool,
    polls: Arc<AtomicUsize>,
) -> reqwest::Response {
    let body = if known_length {
        reqwest::Body::from(vec![b'x'; 9])
    } else {
        reqwest::Body::wrap_stream(futures::stream::unfold(0, move |index| {
            let polls = polls.clone();
            async move {
                polls.fetch_add(1, Ordering::SeqCst);
                if index == 3 {
                    std::future::pending::<()>().await;
                }
                Some((
                    Ok::<_, std::convert::Infallible>(Bytes::from_static(b"123")),
                    index + 1,
                ))
            }
        }))
    };
    let mut builder = axum::http::Response::builder().status(status);
    if known_length {
        builder = builder.header("content-length", "9");
    }
    builder.body(body).unwrap().into()
}

async fn check_response_reader(status: u16, known_length: bool) {
    let polls = Arc::new(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        crate::payload::read_http_response(
            response_from_chunks(status, known_length, polls.clone()),
            RESPONSE_LIMIT,
        ),
    )
    .await
    .expect("reader must reject before polling EOF");
    let error = result.unwrap_err();
    let HttpError::ServerError(_, body) = error else {
        panic!("expected protocol error: {error:?}");
    };
    let error = C2Error::from_envelope(serde_json::from_str(&body).unwrap()).unwrap();
    assert_eq!(error.code, ErrorCode::ProtocolViolation);
    assert_eq!(error.details["reason"], "response_payload_too_large");
    if !known_length {
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }
}

#[tokio::test]
async fn response_reader_bounds_200() {
    check_response_reader(200, true).await;
}
#[tokio::test]
async fn response_reader_bounds_500() {
    check_response_reader(500, true).await;
}
#[tokio::test]
async fn response_reader_bounds_other_error() {
    check_response_reader(502, true).await;
}

#[tokio::test]
async fn response_reader_bounds_chunked_200() {
    check_response_reader(200, false).await;
}
#[tokio::test]
async fn response_reader_bounds_chunked_500() {
    check_response_reader(500, false).await;
}
#[tokio::test]
async fn response_reader_bounds_chunked_other_error() {
    check_response_reader(502, false).await;
}

#[tokio::test]
async fn response_reader_preserves_compliant_bodies_and_uses_actual_body_hint() {
    for status in [200, 500, 502] {
        for len in [0, 8] {
            for streamed in [false, true] {
                let expected = vec![b'x'; len];
                let body = if streamed {
                    reqwest::Body::wrap_stream(futures::stream::iter(
                        expected
                            .chunks(3)
                            .map(|chunk| {
                                Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(chunk))
                            })
                            .collect::<Vec<_>>(),
                    ))
                } else {
                    reqwest::Body::from(expected.clone())
                };
                let response: reqwest::Response = axum::http::Response::builder()
                    .status(status)
                    .body(body)
                    .unwrap()
                    .into();
                let result = crate::payload::read_http_response(response, RESPONSE_LIMIT).await;
                match (status, result) {
                    (200, Ok(bytes)) => assert_eq!(bytes, expected),
                    (500, Err(HttpError::CrmError(bytes))) => assert_eq!(bytes, expected),
                    (502, Err(HttpError::ServerError(502, text))) => {
                        assert_eq!(text.as_bytes(), expected)
                    }
                    (_, other) => panic!("unexpected compliant result: {other:?}"),
                }
            }
        }
    }
    // A legal 304 response's Content-Length describes the selected
    // representation, not its (empty) message body. The reader must consult
    // the body hint. The separate socket fixture below proves HTTP parsing.
    let response: reqwest::Response = axum::http::Response::builder()
        .status(304)
        .header("content-length", u64::MAX.to_string())
        .body(reqwest::Body::from(Vec::new()))
        .unwrap()
        .into();
    assert_eq!(response.content_length(), Some(0));
    assert!(
        matches!(crate::payload::read_http_response(response, RESPONSE_LIMIT).await, Err(HttpError::ServerError(304, text)) if text.is_empty())
    );

    for len in [0, 1] {
        let response: reqwest::Response =
            axum::http::Response::new(reqwest::Body::from(vec![0; len])).into();
        let bytes = crate::payload::read_http_response(response, u64::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.len(), len);
        assert!(
            bytes.capacity() < 1024,
            "a huge limit must not drive allocation"
        );
    }
}

#[tokio::test]
async fn response_content_length_metadata_can_exceed_limit_for_legal_empty_body() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let count = socket.read(&mut bytes).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&bytes[..count]);
        }
        socket
            .write_all(
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 999999\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-length"], "999999");
    assert_eq!(response.content_length(), Some(0));
    assert!(
        matches!(crate::payload::read_http_response(response, RESPONSE_LIMIT).await, Err(HttpError::ServerError(304, text)) if text.is_empty())
    );
    server.await.unwrap();
}
