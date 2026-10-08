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
