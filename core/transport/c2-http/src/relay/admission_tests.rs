use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, StatusCode, header},
};
use c2_config::{DomainLimits, LocalEndpointContext, RelayConfig};
use c2_mem::DomainBudget;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits, Server,
    ServerIpcConfig,
};
use futures::StreamExt;
use tower::ServiceExt;

use super::router::build_local_router;
use super::state::{RegisterCommitResult, RelayState};
use super::test_support::{NoopDisseminator, TEST_ABI_HASH, TEST_SIGNATURE_HASH};

const RESPONSE_BYTES: usize = 2 * c2_config::CHUNK_SIZE as usize;
const WAIT: Duration = Duration::from_secs(5);

struct ProduceResponse;

impl CrmCallback for ProduceResponse {
    fn invoke(
        &self,
        _route_name: &str,
        _method_idx: u16,
        request: Vec<u8>,
        _response_pool: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        assert!(request.is_empty());
        Ok(ResponseMeta::Inline(vec![b'x'; RESPONSE_BYTES]))
    }
}

fn budget(memory: u64, file_spill: u64, operations: u64) -> DomainBudget {
    let limits = DomainLimits {
        memory_budget_bytes: memory,
        file_spill_budget_bytes: file_spill,
        max_outstanding_calls: operations,
    };
    limits.validate().unwrap();
    DomainBudget::from_limits(&limits)
}

fn state(context: LocalEndpointContext, budget: DomainBudget) -> Arc<RelayState> {
    Arc::new(RelayState::new_with_budget(
        Arc::new(RelayConfig::default()),
        Arc::new(NoopDisseminator),
        context,
        budget,
        c2_config::IDLE_CONNECTION_TIMEOUT,
    ))
}

struct Fixture {
    state: Arc<RelayState>,
    server: Arc<Server>,
    #[cfg(unix)]
    _root: super::test_support::EndpointTestRoot,
}

impl Fixture {
    async fn start(id: &str, memory: u64, file_spill: u64, operations: u64) -> Self {
        #[cfg(unix)]
        let root = super::test_support::EndpointTestRoot::new();
        #[cfg(unix)]
        let context = root.context();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let state = state(context.clone(), budget(memory, file_spill, operations));
        let server_id = format!("{id}-{}", std::process::id());
        let instance = format!("{server_id}-instance");
        let address = format!("ipc://{server_id}");
        let endpoint = context.endpoint(&address).unwrap();
        // A small host domain forces this large reply through chunked IPC;
        // the relay must reassemble it before serving its HTTP body.
        let server = Arc::new(
            Server::new_with_identity_endpoint_and_budget(
                endpoint.clone(),
                ServerIpcConfig::default(),
                c2_server::ServerIdentity {
                    server_id: server_id.clone(),
                    server_instance_id: instance.clone(),
                },
                budget(64 * 1024, 0, 64),
            )
            .unwrap(),
        );
        let route = server
            .build_route(
                RouteBuildSpec {
                    name: "output".into(),
                    crm_ns: "test.admission".into(),
                    crm_name: "Output".into(),
                    crm_ver: "0.1.0".into(),
                    abi_hash: TEST_ABI_HASH.into(),
                    signature_hash: TEST_SIGNATURE_HASH.into(),
                    method_names: vec!["produce".into()],
                    access_map: Default::default(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(ProduceResponse),
            )
            .unwrap();
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let running = server.clone();
        tokio::spawn(async move { running.run().await.unwrap() });
        server.wait_until_responsive(WAIT).await.unwrap();

        let mut attestor = c2_ipc::IpcClient::with_endpoint(endpoint).unwrap();
        attestor.connect().await.unwrap();
        let contract = attestor.route_contract("output").unwrap();
        let binding = attestor
            .attest_route_for_registration(&contract)
            .await
            .unwrap();
        attestor.close().await;
        assert!(matches!(
            test_commit_registration!(
                state,
                "output".into(),
                server_id,
                instance,
                address,
                "test.admission".into(),
                "Output".into(),
                "0.1.0".into(),
                TEST_ABI_HASH.into(),
                TEST_SIGNATURE_HASH.into(),
                binding.max_payload_size,
                binding.route_uid,
                None,
            ),
            RegisterCommitResult::Registered { .. }
        ));

        Self {
            state,
            server,
            #[cfg(unix)]
            _root: root,
        }
    }

    async fn stop(self) {
        self.server.shutdown_and_wait(WAIT).await.unwrap();
    }
}

fn call_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-c2-expected-crm-ns", "test.admission")
        .header("x-c2-expected-crm-name", "Output")
        .header("x-c2-expected-crm-ver", "0.1.0")
        .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
        .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .unwrap()
}

async fn capacity_refusal(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["code"], 717);
    assert_eq!(error["details"]["stage"], "call_execution_admission");
    assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
}

async fn collect_output(mut body: axum::body::BodyDataStream, width: usize) -> Bytes {
    let mut received = Vec::new();
    while let Some(frame) = body.next().await {
        let frame = frame.unwrap();
        assert!(!frame.is_empty() && frame.len() <= width);
        received.extend_from_slice(&frame);
    }
    assert_eq!(received.len(), RESPONSE_BYTES);
    assert!(received.iter().all(|&byte| byte == b'x'));
    received.into()
}

#[tokio::test]
async fn removed_echo_route_returns_not_found() {
    let state = state(
        LocalEndpointContext::default_for_platform().unwrap(),
        budget(16 * 1024, 0, 1),
    );
    let response = build_local_router(state)
        .oneshot(call_request("/_echo"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn full_memory_budget_answers_pre_admission_errors_without_waiting() {
    let memory = 16 * 1024;
    let budget = budget(memory, 0, 1);
    let app = build_local_router(state(
        LocalEndpointContext::default_for_platform().unwrap(),
        budget.clone(),
    ));
    let missing = app
        .clone()
        .oneshot(call_request("/_c2/calls/call?route=missing&method=produce"))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    drop(missing);

    let occupied = budget.try_reserve_memory(memory).unwrap();
    let mut request =
        Box::pin(app.oneshot(call_request("/_c2/calls/call?route=missing&method=produce")));
    let std::task::Poll::Ready(Ok(refused)) = futures::poll!(&mut request) else {
        panic!("a pre-admission refusal must not wait on the memory budget");
    };
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(
        refused.into_body(),
        crate::http_wire::MAX_CONTROL_ERROR_BYTES,
    )
    .await
    .unwrap();
    let error = crate::decode_control_error(404, &body).unwrap();
    assert_eq!(error.code, c2_error::ErrorCode::ResourceNotFound);
    assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
    assert_eq!(budget.snapshot().memory.used_bytes, memory);
    assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
    drop(occupied);
}

#[tokio::test]
async fn zero_memory_budget_immediately_refuses_even_a_zero_byte_call() {
    let fixture = Fixture::start("adm-zero", 0, 0, 0).await;
    let mut request = Box::pin(
        build_local_router(fixture.state.clone())
            .oneshot(call_request("/_c2/calls/call?route=output&method=produce")),
    );
    let std::task::Poll::Ready(Ok(refused)) = futures::poll!(&mut request) else {
        panic!("a closed domain must refuse admission immediately");
    };
    capacity_refusal(refused).await;
    assert_eq!(fixture.state.call_execution_snapshot().used_operations, 0);
    fixture.stop().await;
}

#[tokio::test]
async fn unary_response_holds_slot_through_eof_abort_and_last_transport_frame() {
    let fixture = Fixture::start("adm-slot", 4 * RESPONSE_BYTES as u64, 0, 1).await;
    let app = build_local_router(fixture.state.clone());
    for abort in [false, true] {
        let response = app
            .clone()
            .oneshot(call_request("/_c2/calls/call?route=output&method=produce"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.state.call_execution_snapshot().used_operations, 1);

        let mut body = response.into_body().into_data_stream();
        let mut last = body.next().await.unwrap().unwrap();
        if !abort {
            let mut received = last.len();
            drop(last);
            last = body.next().await.unwrap().unwrap();
            received += last.len();
            assert_eq!(received, RESPONSE_BYTES);
            assert!(body.next().await.is_none());
        }
        drop(body);
        assert_eq!(fixture.state.call_execution_snapshot().used_operations, 1);
        capacity_refusal(
            app.clone()
                .oneshot(call_request("/_c2/calls/call?route=output&method=produce"))
                .await
                .unwrap(),
        )
        .await;
        if abort {
            fixture.state.forwarding.close();
            let mut drain = Box::pin(fixture.state.forwarding.drain());
            assert!(futures::poll!(&mut drain).is_pending());
            drop(last);
            drain.await;
        } else {
            drop(last);
        }
        assert_eq!(fixture.state.call_execution_snapshot().used_operations, 0);
    }
    fixture.stop().await;
}

#[tokio::test]
async fn tiny_memory_budget_recycles_download_window_while_a_second_call_waits() {
    let memory = 8 * 1024;
    let fixture = Fixture::start("adm-window", memory, RESPONSE_BYTES as u64 + 64, 2).await;
    let state = fixture.state.clone();
    let app = build_local_router(state.clone());
    tokio::time::timeout(WAIT, async {
        let first = app
            .clone()
            .oneshot(call_request("/_c2/calls/call?route=output&method=produce"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()[header::CONTENT_LENGTH],
            RESPONSE_BYTES.to_string()
        );
        let snapshot = state.domain_snapshot();
        assert_eq!(snapshot.memory.used_bytes, memory);
        assert!(snapshot.file_spill.used_bytes >= RESPONSE_BYTES as u64);

        let mut second =
            Box::pin(app.oneshot(call_request("/_c2/calls/call?route=output&method=produce")));
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(state.call_execution_snapshot().used_operations, 2);

        collect_output(first.into_body().into_data_stream(), memory as usize).await;
        let second = second.await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        collect_output(second.into_body().into_data_stream(), memory as usize).await;
        let snapshot = state.domain_snapshot();
        assert_eq!(snapshot.memory.peak_bytes, memory);
        assert_eq!(snapshot.memory.used_bytes, 0);
        assert_eq!(snapshot.file_spill.used_bytes, 0);
        assert_eq!(snapshot.operations.used_operations, 0);
    })
    .await
    .expect("a second call waiting on shared memory must not block window reuse");
    fixture.stop().await;
}
