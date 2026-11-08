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
    HttpCallControl, HttpCallError, HttpCallPhase, HttpError, HttpStreamCancellation,
    RelayAwareClientConfig, RelayAwareHttpClient, RelayControlClient, RelayRouteInfo,
};

const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SIGNATURE_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
static NEXT_LIVE_FOLLOW: AtomicUsize = AtomicUsize::new(0);

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
    HttpCallControl::new(|| Ok(()), || Ok(()))
}

fn record_dispatches(phases: &Arc<Mutex<Vec<Option<HttpCallPhase>>>>) -> HttpCallControl {
    let phases = Arc::clone(phases);
    HttpCallControl::new(
        || Ok(()),
        move || {
            phases.lock().push(None);
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

async fn registry_with_max_payload_size(url: &str, max_payload_size: u64) -> Server {
    let mut selected = route(url);
    selected.max_payload_size = max_payload_size;
    serve(Router::new().route(
        "/_resolve/{route}",
        get(move || {
            let selected = selected.clone();
            async move { Json(vec![selected]) }
        }),
    ))
    .await
}

#[tokio::test]
async fn selected_relay_accepts_larger_unary_and_stream_after_same_contract_reregistration() {
    use crate::stream_wire::{
        HttpStreamRecordKind, HttpStreamStatus, HttpStreamTerminal, encode_header, encode_terminal,
    };

    let registration = Arc::new(Mutex::new(Vec::<RelayRouteInfo>::new()));
    let active = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let relay = serve(
        Router::new()
            .route(
                "/_resolve/{route}",
                get({
                    let registration = registration.clone();
                    move || {
                        let registration = registration.clone();
                        async move { Json(registration.lock().clone()) }
                    }
                }),
            )
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .route(
                "/{route}/{method}",
                post({
                    let active = active.clone();
                    let calls = calls.clone();
                    move |Path((_route, method)): Path<(String, String)>| {
                        let active = active.clone();
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            if !active.load(Ordering::SeqCst) {
                                return StatusCode::NOT_FOUND.into_response();
                            }
                            if method == "unary" {
                                return (StatusCode::OK, b"larger result".to_vec()).into_response();
                            }
                            let mut record = crate::stream_wire::encode_item_begin(13).to_vec();
                            record.extend_from_slice(&encode_header(
                                HttpStreamRecordKind::ItemData,
                                13,
                            ));
                            record.extend_from_slice(b"larger result");
                            record.extend_from_slice(
                                &encode_terminal(&HttpStreamTerminal {
                                    status: HttpStreamStatus::Success,
                                    complete_items: Some(1),
                                    error: None,
                                })
                                .unwrap(),
                            );
                            (
                                StatusCode::OK,
                                [
                                    ("content-type", crate::stream_wire::MEDIA_TYPE),
                                    ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                                ],
                                record,
                            )
                                .into_response()
                        }
                    }
                }),
            ),
    )
    .await;
    let mut old = route(&relay.url);
    old.max_payload_size = 4;
    *registration.lock() = vec![old];
    let client = acquired_client(&relay).await;

    registration.lock().clear();
    let mut renewed = route(&relay.url);
    renewed.max_payload_size = 32;
    *registration.lock() = vec![renewed];
    active.store(true, Ordering::SeqCst);

    let unary = client
        .call_controlled_async("unary", Arc::new(vec![1]), &allowed())
        .await
        .unwrap();
    assert_eq!(unary.as_ref(), b"larger result");
    drop(unary);

    let budget = c2_mem::retention::PayloadBufferBudget::new(128);
    let headroom = budget.try_reserve(1).unwrap();
    let control = allowed().with_payload_budget(budget, None);
    let mut stream = client
        .stream_controlled_async(
            "stream",
            Arc::new(vec![1]),
            headroom,
            Box::new(()),
            HttpStreamCancellation::new(),
            &control,
        )
        .await
        .unwrap();
    assert_eq!(
        stream.recv().await.unwrap().unwrap().as_ref(),
        b"larger result"
    );
    assert!(stream.recv().await.unwrap().is_none());
    assert_eq!(stream.completion().await.unwrap().complete_items, Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

struct LiveFollowResource;

impl c2_server::CrmCallback for LiveFollowResource {
    fn invoke(
        &self,
        _: &str,
        method: u16,
        _: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<c2_server::ResponseMeta, c2_server::CrmError> {
        assert_eq!(method, 0);
        Ok(c2_server::ResponseMeta::Inline(b"larger result".to_vec()))
    }
}

impl c2_server::CrmStreamCallback for LiveFollowResource {
    fn open_stream(
        &self,
        _: &str,
        method: u16,
        _: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<Box<dyn c2_server::CrmStream>, c2_server::CrmError> {
        assert_eq!(method, 1);
        Ok(Box::new(LiveFollowStream(false)))
    }
}

struct LiveFollowStream(bool);

impl c2_server::CrmStream for LiveFollowStream {
    fn step(&mut self) -> Result<Option<Box<dyn c2_server::CrmStreamItem>>, c2_server::CrmError> {
        if std::mem::replace(&mut self.0, true) {
            Ok(None)
        } else {
            Ok(Some(Box::new(b"larger result".to_vec())))
        }
    }

    fn finish(self: Box<Self>) -> Result<(), c2_server::CrmError> {
        Ok(())
    }
}

async fn live_follow_server(
    context: &c2_config::LocalEndpointContext,
    descriptor: &c2_contract::ValidatedContractDescriptor,
    id: &str,
    limit: u64,
) -> (
    Arc<c2_server::Server>,
    tokio::task::JoinHandle<Result<(), c2_server::ServerError>>,
) {
    use c2_server::{
        ConcurrencyMode, RouteBuildSpec, SchedulerLimits, ServerIdentity, ServerIpcConfig,
    };
    let address = format!("ipc://{id}");
    let mut config = ServerIpcConfig::default();
    config.max_payload_size = limit;
    let server = Arc::new(
        c2_server::Server::new_with_identity_and_endpoint(
            context.endpoint(&address).unwrap(),
            config,
            ServerIdentity {
                server_id: id.into(),
                server_instance_id: format!("{id}-instance"),
            },
        )
        .unwrap(),
    );
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
                access_map: std::collections::HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(LiveFollowResource),
            descriptor,
        )
        .unwrap();
    let reservation = server.reserve_route(built).await.unwrap();
    server.commit_reserved_route(reservation).await.unwrap();
    server.begin_start_attempt().unwrap();
    let task = tokio::spawn({
        let server = server.clone();
        async move { server.run().await }
    });
    server
        .wait_until_responsive(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    (server, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_relay_reregistration_larger_limit_preserves_old_client_unary_and_stream() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
    #[cfg(windows)]
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let descriptor = c2_contract::ValidatedContractDescriptor::from_json(include_bytes!(
        "../../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json"
    ))
    .unwrap();
    let id = format!(
        "http-follow-{}-{}",
        std::process::id(),
        NEXT_LIVE_FOLLOW.fetch_add(1, Ordering::Relaxed)
    );
    let old_id = format!("{id}-old");
    let new_id = format!("{id}-new");
    let (old_server, old_task) = live_follow_server(&context, &descriptor, &old_id, 4).await;
    let (new_server, new_task) = live_follow_server(&context, &descriptor, &new_id, 32).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    drop(listener);
    let url = format!("http://{bind}");
    let relay = tokio::task::spawn_blocking({
        let context = context.clone();
        let bind = bind.clone();
        let url = url.clone();
        move || {
            crate::relay::RelayServer::start_with_context(
                c2_config::RelayConfig {
                    bind,
                    advertise_url: url,
                    idle_timeout_secs: 0,
                    ..c2_config::RelayConfig::default()
                },
                context,
            )
            .unwrap()
        }
    })
    .await
    .unwrap();
    let relay = Arc::new(Mutex::new(relay));
    tokio::task::spawn_blocking({
        let relay = relay.clone();
        let old_id = old_id.clone();
        move || {
            relay
                .lock()
                .register_upstream("portable", &old_id, &format!("ipc://{old_id}"))
                .unwrap()
        }
    })
    .await
    .unwrap();

    let expected = c2_contract::ExpectedRouteContract {
        route_name: "portable".into(),
        crm_ns: descriptor.crm_namespace().into(),
        crm_name: descriptor.crm_name().into(),
        crm_ver: descriptor.crm_version().into(),
        abi_hash: descriptor.abi_hash().into(),
        signature_hash: descriptor.signature_hash().into(),
    };
    let client = tokio::task::spawn_blocking({
        let url = url.clone();
        move || {
            let client =
                RelayAwareHttpClient::new(&url, expected, false, RelayAwareClientConfig::default())
                    .unwrap()
                    .with_http_only();
            client
                .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
                .unwrap();
            client
        }
    })
    .await
    .unwrap();

    tokio::task::spawn_blocking({
        let relay = relay.clone();
        let old_id = old_id.clone();
        let new_id = new_id.clone();
        move || {
            relay
                .lock()
                .unregister_upstream("portable", &old_id)
                .unwrap();
            relay
                .lock()
                .register_upstream("portable", &new_id, &format!("ipc://{new_id}"))
                .unwrap();
        }
    })
    .await
    .unwrap();

    assert_eq!(
        client
            .call_controlled_async("ping", Arc::new(Vec::new()), &allowed())
            .await
            .unwrap()
            .as_ref(),
        b"larger result"
    );
    let budget = c2_mem::retention::PayloadBufferBudget::new(128);
    let headroom = budget.try_reserve(1).unwrap();
    let control = allowed().with_payload_budget(budget, None);
    let mut stream = client
        .stream_controlled_async(
            "echo",
            Arc::new(vec![0; 8]),
            headroom,
            Box::new(()),
            HttpStreamCancellation::new(),
            &control,
        )
        .await
        .unwrap();
    assert_eq!(
        stream.recv().await.unwrap().unwrap().as_ref(),
        b"larger result"
    );
    assert!(stream.recv().await.unwrap().is_none());
    assert_eq!(stream.completion().await.unwrap().complete_items, Some(1));
    drop(stream);
    tokio::task::spawn_blocking(move || relay.lock().stop().unwrap())
        .await
        .unwrap();
    old_server
        .shutdown_and_wait(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    new_server
        .shutdown_and_wait(std::time::Duration::from_secs(2))
        .await
        .unwrap();
    old_task.await.unwrap().unwrap();
    new_task.await.unwrap().unwrap();
}

#[derive(Clone, Copy)]
enum Reply {
    Echo,
    NotFound,
    Crm,
    Capacity,
    CapacityPreDispatch,
    Uncertain,
    GenericBadGateway,
}

#[derive(Clone)]
struct Data {
    calls: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    reply: Arc<Mutex<Reply>>,
    call_gate: Option<Arc<Gate>>,
}

impl Data {
    fn new(reply: Reply) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            received: Arc::new(Mutex::new(Vec::new())),
            reply: Arc::new(Mutex::new(reply)),
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
        Reply::NotFound => (
            StatusCode::NOT_FOUND,
            semantic_error(ErrorCode::ResourceNotFound, Some("pre_dispatch")),
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

async fn data_server(state: &Data) -> Server {
    serve(
        Router::new()
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .route("/{route}/{method}", post(business))
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

async fn acquired_client(registry: &Server) -> RelayAwareHttpClient {
    let client = client(registry);
    tokio::task::spawn_blocking(move || {
        client
            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
            .unwrap();
        client
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn selected_http_target_stays_pinned_when_another_client_refreshes_shared_resolution() {
    let old_data = Data::new(Reply::Echo);
    let new_data = Data::new(Reply::Echo);
    let old_server = data_server(&old_data).await;
    let new_server = data_server(&new_data).await;
    let routes = Arc::new(Mutex::new(vec![route(&old_server.url)]));
    let resolves = Arc::new(AtomicUsize::new(0));
    let registry = serve(Router::new().route(
        "/_resolve/{route}",
        get({
            let routes = Arc::clone(&routes);
            let resolves = Arc::clone(&resolves);
            move || {
                let routes = Arc::clone(&routes);
                let resolves = Arc::clone(&resolves);
                async move {
                    resolves.fetch_add(1, Ordering::SeqCst);
                    Json(routes.lock().clone())
                }
            }
        }),
    ))
    .await;
    let control = Arc::new(RelayControlClient::new(&registry.url, false).unwrap());
    let old_client = Arc::new(
        RelayAwareHttpClient::new_with_control(
            Arc::clone(&control),
            expected(),
            false,
            RelayAwareClientConfig::default(),
        )
        .unwrap(),
    );
    let new_client = Arc::new(
        RelayAwareHttpClient::new_with_control(
            Arc::clone(&control),
            expected(),
            false,
            RelayAwareClientConfig::default(),
        )
        .unwrap(),
    );
    let selected = Arc::clone(&old_client);
    tokio::task::spawn_blocking(move || {
        selected.resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
    })
    .await
    .unwrap()
    .unwrap();

    let mut replacement = route(&new_server.url);
    replacement.max_payload_size = 1;
    *routes.lock() = vec![replacement];
    control.invalidate("grid");
    let selected = Arc::clone(&new_client);
    tokio::task::spawn_blocking(move || {
        selected.resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resolves.load(Ordering::SeqCst), 2);

    let result = old_client
        .call_controlled_async("step", Arc::new(b"old-target".to_vec()), &allowed())
        .await
        .unwrap();
    assert_eq!(result.as_ref(), b"old-target");
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
    assert_eq!(old_data.calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_data.calls.load(Ordering::SeqCst), 0);
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
        move || {
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
    let client = acquired_client(&registry).await;
    let control = HttpCallControl::new(|| Ok(()), || Err(rejected()));
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
    assert_eq!(output.as_ref(), b"allowed");
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owner_deadline_rejection_after_acquisition_never_dispatches() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let (registry, resolves) = registry(&[&server.url], None).await;
    let client = acquired_client(&registry).await;
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
        move || {
            dispatch_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    expired.store(true, Ordering::SeqCst);
    let result = client
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await;

    assert_local(&result.unwrap_err());
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(data.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn authoritative_not_found_post_reports_predispatch_without_retaining_replay_input() {
    let missing = Data::new(Reply::NotFound);
    let live = Data::new(Reply::Echo);
    let missing_server = data_server(&missing).await;
    let live_server = data_server(&live).await;
    let (registry, resolves) = registry(&[&missing_server.url, &live_server.url], None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let control = record_dispatches(&phases);
    let client = acquired_client(&registry).await;
    let error = client
        .call_controlled_async("step", Arc::new(b"same-input".to_vec()), &control)
        .await
        .unwrap_err();

    assert_eq!(error.phase(), HttpCallPhase::PreDispatch);
    assert_eq!(*phases.lock(), vec![None]);
    assert_eq!(missing.calls.load(Ordering::SeqCst), 1);
    assert_eq!(live.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
}

fn assert_not_found_control_error(error: &HttpCallError) {
    assert_eq!(error.phase(), HttpCallPhase::PreDispatch);
    assert!(error.is_retry_safe());
    let HttpError::ResponseError(404, body) = error.source_error() else {
        panic!("expected the relay's 404 error envelope, got {error:?}");
    };
    let envelope = serde_json::from_slice(body.as_ref()).unwrap();
    let decoded = C2Error::from_envelope(envelope).unwrap();
    assert_eq!(decoded.code, ErrorCode::ResourceNotFound);
}

#[tokio::test]
async fn unary_route_not_found_control_envelope_exceeds_one_byte_business_limit() {
    let missing = Data::new(Reply::NotFound);
    let server = data_server(&missing).await;
    let registry = registry_with_max_payload_size(&server.url, 1).await;
    let control = allowed();

    let error = acquired_client(&registry)
        .await
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await
        .unwrap_err();

    assert_not_found_control_error(&error);
    assert_eq!(missing.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_route_not_found_control_envelope_exceeds_one_byte_business_limit() {
    let missing = Data::new(Reply::NotFound);
    let server = data_server(&missing).await;
    let registry = registry_with_max_payload_size(&server.url, 1).await;
    let budget = c2_mem::retention::PayloadBufferBudget::new(4096);
    let headroom = budget.try_reserve(1).unwrap();
    let control = allowed().with_payload_budget(budget, None);

    let error = acquired_client(&registry)
        .await
        .stream_controlled_async(
            "step",
            Arc::new(vec![1]),
            headroom,
            Box::new(()),
            HttpStreamCancellation::new(),
            &control,
        )
        .await
        .err()
        .expect("missing route must reject the stream before dispatch");

    assert_not_found_control_error(&error);
    assert_eq!(missing.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn consumed_input_does_not_enter_a_second_dispatch_guard() {
    let missing = Data::new(Reply::NotFound);
    let live = Data::new(Reply::Echo);
    let missing_server = data_server(&missing).await;
    let live_server = data_server(&live).await;
    let (registry, _) = registry(&[&missing_server.url, &live_server.url], None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&phases);
    let control = HttpCallControl::new(
        || Ok(()),
        move || {
            observed.lock().push(None::<HttpCallPhase>);
            Ok(())
        },
    );
    let error = acquired_client(&registry)
        .await
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await
        .unwrap_err();

    assert_eq!(error.phase(), HttpCallPhase::PreDispatch);
    assert_eq!(*phases.lock(), vec![None]);
    assert_eq!(missing.calls.load(Ordering::SeqCst), 1);
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
        let error = acquired_client(&registry)
            .await
            .call_controlled_async("step", Arc::new(vec![1]), &record_dispatches(&phases))
            .await
            .unwrap_err();

        assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
        match reply {
            Reply::Crm => {
                assert!(
                    matches!(error.source_error(), HttpError::CrmError(bytes) if bytes.as_ref() == b"crm-failure")
                );
            }
            _ => assert!(matches!(
                error.source_error(),
                HttpError::ResponseError(502, _)
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
            if request.starts_with(b"GET /_probe/grid HTTP/1.1\r\n") {
                use tokio::io::AsyncWriteExt;
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                continue;
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
    let error = acquired_client(&registry)
        .await
        .call_controlled_async(
            "step",
            Arc::new(b"lost".to_vec()),
            &record_dispatches(&phases),
        )
        .await
        .unwrap_err();

    assert!(matches!(error.source_error(), HttpError::Request(_)));
    assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
    assert_eq!(*phases.lock(), vec![None]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(live.calls.load(Ordering::SeqCst), 0);
}

async fn assert_capacity_preserves_route(reply: Reply, status: u16, phase: HttpCallPhase) {
    let data = Data::new(reply);
    let server = data_server(&data).await;
    let (registry, resolves) = registry(&[&server.url], None).await;
    let client = acquired_client(&registry).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let control = record_dispatches(&phases);
    let error = client
        .call_controlled_async("step", Arc::new(vec![1]), &control)
        .await
        .unwrap_err();

    assert!(matches!(error.source_error(), HttpError::ResponseError(code, _) if *code == status));
    assert_eq!(error.phase(), phase);
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
    *data.reply.lock() = Reply::Echo;
    assert_eq!(
        client
            .call_controlled_async("step", Arc::new(vec![2]), &control)
            .await
            .unwrap()
            .as_ref(),
        vec![2]
    );
    assert_eq!(*phases.lock(), vec![None, None]);
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
    assert_eq!(data.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn capacity_refusal_preserves_cache_and_never_provides_retry_proof() {
    assert_capacity_preserves_route(Reply::Capacity, 429, HttpCallPhase::DispatchUncertain).await;
}

#[tokio::test]
async fn predispatched_capacity_refusal_does_not_prove_route_not_found() {
    assert_capacity_preserves_route(Reply::CapacityPreDispatch, 502, HttpCallPhase::PreDispatch)
        .await;
}

#[tokio::test]
async fn concurrent_calls_keep_their_control_hooks_independent() {
    let gate = Arc::new(Gate::default());
    let mut data = Data::new(Reply::Echo);
    data.call_gate = Some(Arc::clone(&gate));
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let client = acquired_client(&registry).await;
    let good_phases = Arc::new(Mutex::new(Vec::new()));
    let good = record_dispatches(&good_phases);
    let bad_phases = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&bad_phases);
    let bad = HttpCallControl::new(
        || Ok(()),
        move || {
            observed.lock().push(None::<HttpCallPhase>);
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

    assert_eq!(good_result.unwrap().as_ref(), b"good");
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
        ("content-type", "application/octet-stream"),
    ] {
        assert_eq!(headers[key], expected, "header {key}");
    }
    assert!(headers.get("x-c2-route-uid").is_none());
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
    let client = tokio::task::spawn_blocking(move || {
        client
            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
            .unwrap();
        client
    })
    .await
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
        assert_eq!(result.as_ref(), expected);
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
    let client = acquired_client(&registry).await;
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
        || Ok(()),
    );
    let len = 64 * 1024 + 7;
    let input = Arc::new(vec![9; len]);
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
    assert!(
        weak.strong_count() <= 1,
        "there is no second owner retained for a replay"
    );
    gate.resume.notify_one();

    assert!(
        driver.await.unwrap(),
        "the driver must finish the dispatched call"
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(data.calls.load(Ordering::SeqCst), 1);
}

// A small advertised limit must not constrain responses after registration changes.
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
    let client = acquired_client(&registry).await;
    (server, registry, client)
}

fn assert_response_budget(error: &HttpCallError) {
    assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
    assert!(!error.is_retry_safe());
    let HttpError::LocalCallRejected(error) = error.source_error() else {
        panic!("expected local response budget error, got {error:?}");
    };
    assert_eq!(error.code, ErrorCode::CallCapacityExceeded);
    assert_eq!(error.details["stage"], "payload_buffer_admission");
}

#[tokio::test]
async fn business_response_budget_covers_success_and_crm_before_eof() {
    for status in [StatusCode::OK, StatusCode::INTERNAL_SERVER_ERROR] {
        for chunked in [false, true] {
            for http_only in [false, true] {
                let calls = Arc::new(AtomicUsize::new(0));
                let tail = Arc::new(Gate::default());
                let (_server, registry, acquired) = response_fixture(ResponseData {
                    status,
                    chunked,
                    calls: calls.clone(),
                    late: None,
                    tail: Some(tail.clone()),
                })
                .await;
                let client = if http_only {
                    let client = client(&registry).with_http_only();
                    tokio::task::spawn_blocking(move || {
                        client
                            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
                            .unwrap();
                        client
                    })
                    .await
                    .unwrap()
                } else {
                    acquired
                };
                let budget = c2_mem::retention::PayloadBufferBudget::new(RESPONSE_LIMIT);
                let control = allowed().with_payload_budget(budget, None);
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
                assert_response_budget(&error);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "oversize must not replay POST"
                );
                assert_eq!(
                    client
                        .call_controlled_async("good", Arc::new(vec![2]), &allowed())
                        .await
                        .unwrap()
                        .as_ref(),
                    b"12345678"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            }
        }
    }
}

#[tokio::test]
async fn relay_control_error_uses_protocol_limit_and_retains_b_charge() {
    for status in [StatusCode::CONFLICT, StatusCode::BAD_GATEWAY] {
        for chunked in [false, true] {
            let (_server, _registry, client) = response_fixture(ResponseData {
                status,
                chunked,
                calls: Arc::new(AtomicUsize::new(0)),
                late: None,
                tail: None,
            })
            .await;
            let budget = c2_mem::retention::PayloadBufferBudget::new(64);
            let control = allowed().with_payload_budget(budget.clone(), None);
            let error = client
                .call_controlled_async("large", Arc::new(vec![1]), &control)
                .await
                .unwrap_err();
            assert_eq!(error.phase(), HttpCallPhase::DispatchUncertain);
            let HttpError::ResponseError(actual_status, body) = error.source_error() else {
                panic!("expected relay control error, got {error:?}");
            };
            assert_eq!(*actual_status, status.as_u16());
            assert_eq!(body.len(), 9);
            assert_eq!(budget.snapshot().used_bytes, 9);
            drop(error);
            assert_eq!(budget.snapshot().used_bytes, 0);
        }
    }
}

#[tokio::test]
async fn borrowed_legacy_calls_accept_responses_above_registration_limit() {
    let (_server, _registry, client) = response_fixture(ResponseData {
        status: StatusCode::OK,
        chunked: false,
        calls: Arc::new(AtomicUsize::new(0)),
        late: None,
        tail: None,
    })
    .await;
    assert_eq!(
        client.call_async("large", &[1]).await.unwrap(),
        vec![b'x'; 9]
    );
}

struct ReservedResponseInput {
    bytes: Vec<u8>,
    _permit: c2_mem::retention::PayloadBufferReservation,
}

impl AsRef<[u8]> for ReservedResponseInput {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[tokio::test]
async fn late_response_completion_keeps_only_actual_request_reader_and_native_driver() {
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
        let budget = c2_mem::retention::PayloadBufferBudget::new(4);
        let input = Arc::new(ReservedResponseInput {
            bytes: vec![1; 4],
            _permit: budget.reserve(4).await.unwrap(),
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
            || Ok(()),
        )
        .with_payload_budget(budget.clone(), None);
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
        let actual_reader = weak.upgrade();
        assert!(
            weak.strong_count() <= if actual_reader.is_some() { 2 } else { 0 },
            "only the transport reader and this observation may retain input"
        );
        if actual_reader.is_some() {
            assert_eq!(
                budget.snapshot().used_bytes,
                4,
                "live transport storage remains billed"
            );
        }
        drop(actual_reader);
        assert_eq!(
            client
                .call_controlled_async("good", Arc::new(vec![2]), &allowed())
                .await
                .unwrap()
                .as_ref(),
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
        let error = result.unwrap_err();
        assert_response_budget(&error);
        assert_eq!(
            active_checks.load(Ordering::SeqCst),
            checks_at_expiry,
            "dispatched response must not recheck caller deadline"
        );
        assert!(weak.upgrade().is_none());
        assert_eq!(budget.snapshot().used_bytes, 0);
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
    let budget = c2_mem::retention::PayloadBufferBudget::new(RESPONSE_LIMIT);
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        crate::payload::read_http_response(
            response_from_chunks(status, known_length, polls.clone()),
            &budget,
            None,
        ),
    )
    .await
    .expect("reader must reject before polling EOF");
    let error = result.unwrap_err();
    let HttpError::LocalCallRejected(error) = error else {
        panic!("expected payload budget error: {error:?}");
    };
    assert_eq!(error.code, ErrorCode::CallCapacityExceeded);
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
async fn relay_control_error_declared_length_above_protocol_limit_rejects_before_body_poll() {
    use tokio::io::AsyncWriteExt;

    let limit = crate::stream_wire::MAX_TERMINAL_PAYLOAD_BYTES;
    for (status, reason) in [
        (200, "OK"),
        (500, "Internal Server Error"),
        (502, "Bad Gateway"),
    ] {
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
            socket.write_all(format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                limit + 1,
            ).as_bytes()).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), Some(limit + 1));
        let budget = c2_mem::retention::PayloadBufferBudget::new(64);
        let error = crate::payload::read_http_response(response, &budget, None)
            .await
            .unwrap_err();
        let HttpError::ServerError(actual, body) = error else {
            panic!("expected response protocol error, got {error:?}");
        };
        assert_eq!(actual, status);
        let decoded = C2Error::from_envelope(serde_json::from_str(&body).unwrap()).unwrap();
        assert_eq!(decoded.code, ErrorCode::ProtocolViolation);
        assert_eq!(decoded.details["reason"], "response_payload_too_large");
        assert_eq!(decoded.details["max_payload_size"], limit.to_string());
        assert_eq!(budget.snapshot().peak_bytes, 0);
        server.await.unwrap();
    }
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
async fn chunked_relay_control_error_respects_b_before_waiting_for_eof() {
    let polls = Arc::new(AtomicUsize::new(0));
    let budget = c2_mem::retention::PayloadBufferBudget::new(4);
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        crate::payload::read_http_response(
            response_from_chunks(502, false, polls.clone()),
            &budget,
            None,
        ),
    )
    .await
    .expect("B rejection must precede the pending EOF");
    let error = result.unwrap_err();
    let HttpError::LocalCallRejected(details) = &error else {
        panic!("expected B capacity rejection, got {error:?}");
    };
    assert_eq!(details.code, ErrorCode::CallCapacityExceeded);
    assert_eq!(details.details["transport_phase"], "dispatch_uncertain");
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(budget.snapshot().used_bytes, 0);
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
                let result = read_response(response).await;
                match (status, result) {
                    (200, Ok(bytes)) => assert_eq!(bytes.as_ref(), expected),
                    (500, Err(HttpError::CrmError(bytes))) => assert_eq!(bytes.as_ref(), expected),
                    (502, Err(HttpError::ResponseError(502, text))) => {
                        assert_eq!(text.as_ref(), expected)
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
        matches!(read_response(response).await, Err(HttpError::ResponseError(304, text)) if text.is_empty())
    );

    for len in [0, 1] {
        let response: reqwest::Response =
            axum::http::Response::new(reqwest::Body::from(vec![0; len])).into();
        let bytes = read_response(response).await.unwrap();
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
        matches!(read_response(response).await, Err(HttpError::ResponseError(304, text)) if text.is_empty())
    );
    server.await.unwrap();
}

async fn read_response(
    response: reqwest::Response,
) -> Result<c2_mem::retention::BudgetedBuffer, HttpError> {
    let budget =
        c2_mem::retention::PayloadBufferBudget::new(c2_config::DEFAULT_PAYLOAD_BUFFER_BUDGET_BYTES);
    crate::payload::read_http_response(response, &budget, None).await
}

#[tokio::test]
async fn completed_request_budget_is_reused_by_its_known_length_response() {
    let data = Data::new(Reply::Echo);
    let server = data_server(&data).await;
    let (registry, _) = registry(&[&server.url], None).await;
    let budget = c2_mem::retention::PayloadBufferBudget::new(7);
    let input = Arc::new(ReservedResponseInput {
        bytes: vec![7; 7],
        _permit: budget.reserve(7).await.unwrap(),
    });
    let control = allowed().with_payload_budget(budget.clone(), None);
    let client = RelayAwareHttpClient::new(
        &registry.url,
        expected(),
        false,
        RelayAwareClientConfig {
            remote_payload_chunk_size: 3,
            ..Default::default()
        },
    )
    .unwrap();
    let client = tokio::task::spawn_blocking(move || {
        client
            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
            .unwrap();
        client
    })
    .await
    .unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.call_controlled_async("step", input, &control),
    )
    .await
    .expect("the request's ended reader must not block response admission")
    .unwrap();
    assert_eq!(output.as_ref(), &[7; 7]);
    assert_eq!(budget.snapshot().used_bytes, 7);
    assert_eq!(budget.snapshot().peak_bytes, 7);
    drop(output);
    assert_eq!(budget.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn controlled_stream_uses_one_bound_post_and_stream_accept_header() {
    use crate::stream_wire::{
        HttpStreamRecordKind, HttpStreamStatus, HttpStreamTerminal, encode_header, encode_terminal,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let post_calls = calls.clone();
    let post_headers = seen_headers.clone();
    let server = serve(
        Router::new()
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .route(
                "/{route}/{method}",
                post(move |headers: HeaderMap, body: Bytes| {
                    let calls = post_calls.clone();
                    let seen = post_headers.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        *seen.lock() = Some(headers);
                        assert_eq!(body.as_ref(), b"input");
                        let mut record = crate::stream_wire::encode_item_begin(4).to_vec();
                        record.extend_from_slice(&encode_header(HttpStreamRecordKind::ItemData, 4));
                        record.extend_from_slice(b"item");
                        record.extend_from_slice(
                            &encode_terminal(&HttpStreamTerminal {
                                status: HttpStreamStatus::Success,
                                complete_items: Some(1),
                                error: None,
                            })
                            .unwrap(),
                        );
                        (
                            StatusCode::OK,
                            [
                                ("content-type", crate::stream_wire::MEDIA_TYPE),
                                ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                            ],
                            record,
                        )
                    }
                }),
            ),
    )
    .await;
    let newer = Data::new(Reply::Echo);
    let newer_server = data_server(&newer).await;
    let routes = Arc::new(Mutex::new(vec![route(&server.url)]));
    let resolves = Arc::new(AtomicUsize::new(0));
    let registry = serve(Router::new().route(
        "/_resolve/{route}",
        get({
            let routes = Arc::clone(&routes);
            let resolves = Arc::clone(&resolves);
            move || {
                let routes = Arc::clone(&routes);
                let resolves = Arc::clone(&resolves);
                async move {
                    resolves.fetch_add(1, Ordering::SeqCst);
                    Json(routes.lock().clone())
                }
            }
        }),
    ))
    .await;
    let budget = c2_mem::retention::PayloadBufferBudget::new(128);
    let input = Arc::new(ReservedResponseInput {
        bytes: b"input".to_vec(),
        _permit: budget.try_reserve(5).unwrap(),
    });
    let headroom = budget.try_reserve(1).unwrap();
    let control = allowed().with_payload_budget(budget.clone(), None);
    let relay_control = Arc::new(RelayControlClient::new(&registry.url, false).unwrap());
    let client = RelayAwareHttpClient::new_with_control(
        Arc::clone(&relay_control),
        expected(),
        false,
        RelayAwareClientConfig::default(),
    )
    .unwrap();
    let client = tokio::task::spawn_blocking(move || {
        client
            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
            .unwrap();
        client
    })
    .await
    .unwrap();
    let mut replacement = route(&newer_server.url);
    replacement.max_payload_size = 1;
    *routes.lock() = vec![replacement];
    relay_control.invalidate("grid");
    let newer_client = RelayAwareHttpClient::new_with_control(
        Arc::clone(&relay_control),
        expected(),
        false,
        RelayAwareClientConfig::default(),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        newer_client
            .resolve_http_target_with_deadline(c2_config::ConnectDeadline::default())
            .unwrap();
    })
    .await
    .unwrap();
    let mut stream = client
        .stream_controlled_async(
            "step",
            input,
            headroom,
            Box::new(()),
            HttpStreamCancellation::new(),
            &control,
        )
        .await
        .unwrap();
    assert_eq!(stream.recv().await.unwrap().unwrap().as_ref(), b"item");
    assert!(stream.recv().await.unwrap().is_none());
    assert_eq!(stream.completion().await.unwrap().complete_items, Some(1));
    let headers = seen_headers.lock().clone().unwrap();
    assert_eq!(headers["accept"], crate::stream_wire::MEDIA_TYPE);
    assert!(headers.get("x-c2-route-uid").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(newer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolves.load(Ordering::SeqCst), 2);
}
