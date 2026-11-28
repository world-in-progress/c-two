//! End-to-end forwarding across authenticated mesh peers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use c2_config::{BridgeConfig, CallMetadata, DomainLimits, LocalEndpointContext, RelayConfig};
use c2_contract::ExpectedRouteContract;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, ResponsePool, RouteBuildSpec,
    SchedulerLimits, Server, ServerIdentity, ServerIpcConfig,
};

use super::{
    RelayServer,
    test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH},
};
use crate::{
    bridge::BridgeServer,
    client::{RelayAwareClientConfig, RelayAwareHttpClient, RelayResolvedTarget},
};

const WAIT: Duration = Duration::from_secs(30);

struct Echo(Arc<AtomicU64>);

impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        input: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        if input == b"fail" {
            return Err(CrmError::UserError(
                c2_error::C2Error::new(
                    c2_error::ErrorCode::ResourceFunctionExecuting,
                    "resource error",
                )
                .to_wire_bytes(),
            ));
        }
        Ok(ResponseMeta::Inline(input))
    }
}

fn expected() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test.echo".into(),
        crm_name: "Echo".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: TEST_ABI_HASH.into(),
        signature_hash: TEST_SIGNATURE_HASH.into(),
    }
}

fn contract_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("x-c2-expected-crm-ns", "test.echo")
        .header("x-c2-expected-crm-name", "Echo")
        .header("x-c2-expected-crm-ver", "0.1.0")
        .header("x-c2-expected-abi-hash", TEST_ABI_HASH)
        .header("x-c2-expected-signature-hash", TEST_SIGNATURE_HASH)
}

async fn run_mesh_forwarding(relay_only: bool) {
    let files = tempfile::tempdir().unwrap();
    let bridge = BridgeServer::start(BridgeConfig {
        bind: "127.0.0.1:0".into(),
        quic_bind: Some("127.0.0.1:0".into()),
        state_dir: Some(files.path().join("bridge")),
        ..BridgeConfig::default()
    })
    .await
    .unwrap();
    #[cfg(unix)]
    let root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = root.context();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();

    let address = format!(
        "ipc://mesh-origin-{}-{}",
        std::process::id(),
        u8::from(relay_only)
    );
    let endpoint = context.endpoint(&address).unwrap();
    let host = Arc::new(
        Server::new_with_identity_and_endpoint(
            endpoint,
            ServerIpcConfig {
                outcome_max_retained_bytes: Some(1024),
                ..ServerIpcConfig::default()
            },
            ServerIdentity {
                server_id: "mesh-origin".into(),
                server_instance_id: "mesh-origin-instance".into(),
            },
        )
        .unwrap()
        .with_admission_hook(Arc::new(|context: &c2_server::AdmissionContext<'_>| {
            if context.metadata().get("gate") == Some("deny") {
                c2_server::AdmissionDecision::deny("not admitted")
            } else {
                c2_server::AdmissionDecision::allow()
            }
        })),
    );
    let invocations = Arc::new(AtomicU64::new(0));
    let route = host
        .build_route(
            RouteBuildSpec {
                name: "grid".into(),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                method_names: vec!["echo".into()],
                access_map: Default::default(),
                concurrency_mode: ConcurrencyMode::ReadParallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(Echo(invocations.clone())),
        )
        .unwrap();
    let reservation = host.reserve_route(route).await.unwrap();
    host.commit_reserved_route(reservation).await.unwrap();
    host.begin_start_attempt().unwrap();
    let host_task = tokio::spawn({
        let host = host.clone();
        async move { host.run().await }
    });
    host.wait_until_responsive(WAIT).await.unwrap();

    let base = RelayConfig {
        bind: "127.0.0.1:0".into(),
        bridge: Some(bridge.bridge_url_template()),
        bridge_token: Some(bridge.token().to_owned()),
        cluster_secret: Some("mesh-forward-test".into()),
        seed_retry_interval: Duration::from_millis(200),
        heartbeat_interval: Duration::from_secs(1),
        ..RelayConfig::default()
    };
    let start = |config: RelayConfig, context: LocalEndpointContext| {
        if relay_only {
            RelayServer::start_relay_only_for_test(config, context)
        } else {
            RelayServer::start_resolved(config, context)
        }
    };
    let mut origin = tokio::task::block_in_place(|| {
        start(
            RelayConfig {
                state_dir: Some(files.path().join("origin")),
                ..base.clone()
            },
            context.clone(),
        )
    })
    .unwrap();
    tokio::task::block_in_place(|| origin.register_upstream("grid", "mesh-origin", &address))
        .unwrap();
    let origin_id = hex::encode(origin.iroh_endpoint_id().unwrap().as_bytes());
    let mut anchor = tokio::task::block_in_place(|| {
        start(
            RelayConfig {
                state_dir: Some(files.path().join("anchor")),
                seeds: vec![format!("iroh://{origin_id}")],
                domain_limits: DomainLimits {
                    memory_budget_bytes: 64 * 1024,
                    file_spill_budget_bytes: 0,
                    max_outstanding_calls: 8,
                },
                ..base
            },
            context,
        )
    })
    .unwrap();

    let anchor_url = format!("http://{}", anchor.local_addr());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let resolve_url = format!(
        "{anchor_url}/_resolve?route=grid&crm_ns=test.echo&crm_name=Echo&crm_ver=0.1.0&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
    );
    tokio::time::timeout(WAIT, async {
        loop {
            let response = client.get(&resolve_url).send().await.unwrap();
            if response.status().is_success() {
                let routes: serde_json::Value = response.json().await.unwrap();
                assert_eq!(routes[0]["proxy_via_anchor"], true);
                assert!(routes[0]["relay_url"].is_null());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("mesh route did not converge");

    let probe = contract_headers(client.get(format!("{anchor_url}/_probe?route=grid")))
        .send()
        .await
        .unwrap();
    assert_eq!(probe.status(), 200);
    let forbidden = contract_headers(client.get(format!("{anchor_url}/_probe?route=grid")))
        .header("x-c2-relay-origin-only", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 502);
    let error: serde_json::Value = forbidden.json().await.unwrap();
    assert_eq!(error["code"], 702);
    assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");

    let aware = RelayAwareHttpClient::new(
        &anchor_url,
        expected(),
        &Default::default(),
        RelayAwareClientConfig::default(),
    )
    .unwrap()
    .with_http_only();
    let selected =
        tokio::task::block_in_place(|| aware.resolve_http_target_with_deadline(Default::default()))
            .unwrap();
    assert_eq!(
        selected,
        RelayResolvedTarget::Http {
            relay_url: anchor_url.clone()
        }
    );
    assert_eq!(
        aware
            .call_async("echo", b"small", &CallMetadata::empty())
            .await
            .unwrap(),
        b"small"
    );

    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let monitor = tokio::spawn({
        let state = anchor.state_for_test();
        let done = done.clone();
        let peak = peak.clone();
        async move {
            while !done.load(Ordering::Acquire) {
                peak.fetch_max(state.domain_snapshot().memory.used_bytes, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }
    });
    let payload = vec![0x5a; 256 * 1024];
    let response = contract_headers(client.post(format!(
        "{anchor_url}/_c2/calls/call?route=grid&method=echo"
    )))
    .body(payload.clone())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), payload);
    done.store(true, Ordering::Release);
    monitor.await.unwrap();
    assert!(peak.load(Ordering::Relaxed) <= 64 * 1024);
    tokio::time::timeout(WAIT, async {
        while anchor.domain_snapshot().memory.used_bytes != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("forwarding memory was not released");

    let outcome =
        contract_headers(client.get(format!("{anchor_url}/_c2/calls/outcome?route=grid")))
            .header("x-c2-call-id", "unknown-call")
            .send()
            .await
            .unwrap();
    assert_eq!(outcome.status(), 200);
    assert_eq!(outcome.headers().get("cache-control").unwrap(), "no-store");
    let unknown: serde_json::Value = outcome.json().await.unwrap();
    assert_eq!(unknown["status"], "unknown");

    let keyed_url = format!("{anchor_url}/_c2/calls/keyed?route=grid&method=echo");
    let recover_url = format!("{anchor_url}/_c2/calls/recover?route=grid&method=echo");
    for _ in 0..2 {
        let result = contract_headers(client.post(&keyed_url))
            .header("x-c2-call-id", "same-call")
            .body("keyed")
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 200);
        assert_eq!(result.bytes().await.unwrap().as_ref(), b"keyed");
    }
    let recovered = contract_headers(client.post(&recover_url))
        .header("x-c2-call-id", "same-call")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), 200);
    assert_eq!(recovered.bytes().await.unwrap().as_ref(), b"keyed");
    let conflict = contract_headers(client.post(&keyed_url))
        .header("x-c2-call-id", "same-call")
        .body("different")
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    let error: serde_json::Value = conflict.json().await.unwrap();
    assert_eq!(error["code"], 721);
    let missing = contract_headers(client.post(&recover_url))
        .header("x-c2-call-id", "missing-call")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 409);
    let error: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(error["code"], 723);
    let failed = contract_headers(client.post(format!(
        "{anchor_url}/_c2/calls/call?route=grid&method=echo"
    )))
    .body("fail")
    .send()
    .await
    .unwrap();
    assert_eq!(failed.status(), 500);
    let body = failed.bytes().await.unwrap();
    let error = c2_error::C2Error::from_wire_bytes(&body).unwrap().unwrap();
    assert_eq!(error.code, c2_error::ErrorCode::ResourceFunctionExecuting);
    assert_eq!(error.message, "resource error");
    let rejected = contract_headers(client.post(format!(
        "{anchor_url}/_c2/calls/call?route=grid&method=echo"
    )))
    .header("x-c2-meta-gate", "deny")
    .body("blocked")
    .send()
    .await
    .unwrap();
    assert_eq!(rejected.status(), 403);
    let error: serde_json::Value = rejected.json().await.unwrap();
    assert_eq!(error["code"], 724);
    assert_eq!(error["details"]["dispatch_phase"], "pre_dispatch");
    let big = contract_headers(client.post(&keyed_url))
        .header("x-c2-call-id", "large-call")
        .body(vec![9; 2048])
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 200);
    assert_eq!(big.bytes().await.unwrap().len(), 2048);
    let unavailable = contract_headers(client.post(&recover_url))
        .header("x-c2-call-id", "large-call")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), 500);
    let body = unavailable.bytes().await.unwrap();
    let error = c2_error::C2Error::from_wire_bytes(&body).unwrap().unwrap();
    assert_eq!(error.code, c2_error::ErrorCode::CallResultUnavailable);
    assert_eq!(invocations.load(Ordering::Relaxed), 5);

    tokio::task::block_in_place(|| {
        anchor.stop().unwrap();
        origin.stop().unwrap();
    });
    host.shutdown_and_wait(WAIT).await.unwrap();
    host_task.await.unwrap().unwrap();
    bridge.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_works_with_direct_iroh_paths() {
    run_mesh_forwarding(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_works_via_private_bridge_without_ip_transports() {
    run_mesh_forwarding(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_relay_iroh_route_conflict_and_withdrawal_recovery() {
    let files = tempfile::tempdir().unwrap();
    let bridge = BridgeServer::start(BridgeConfig {
        bind: "127.0.0.1:0".into(),
        quic_bind: Some("127.0.0.1:0".into()),
        state_dir: Some(files.path().join("bridge")),
        ..BridgeConfig::default()
    })
    .await
    .unwrap();
    #[cfg(unix)]
    let root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = root.context();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();

    let start_host = |label: &str| {
        let context = context.clone();
        let address = format!("ipc://mesh-conflict-{}-{label}", std::process::id());
        let server_id = format!("mesh-conflict-{label}");
        let instance_id = format!("mesh-conflict-{label}-instance");
        let invocations = Arc::new(AtomicU64::new(0));
        async move {
            let endpoint = context.endpoint(&address).unwrap();
            let host = Arc::new(
                Server::new_with_identity_and_endpoint(
                    endpoint,
                    ServerIpcConfig::default(),
                    ServerIdentity {
                        server_id: server_id.clone(),
                        server_instance_id: instance_id,
                    },
                )
                .unwrap(),
            );
            let route = host
                .build_route(
                    RouteBuildSpec {
                        name: "grid".into(),
                        crm_ns: "test.echo".into(),
                        crm_name: "Echo".into(),
                        crm_ver: "0.1.0".into(),
                        abi_hash: TEST_ABI_HASH.into(),
                        signature_hash: TEST_SIGNATURE_HASH.into(),
                        method_names: vec!["echo".into()],
                        access_map: Default::default(),
                        concurrency_mode: ConcurrencyMode::ReadParallel,
                        limits: SchedulerLimits::default(),
                    },
                    Arc::new(Echo(invocations.clone())),
                )
                .unwrap();
            let reservation = host.reserve_route(route).await.unwrap();
            host.commit_reserved_route(reservation).await.unwrap();
            host.begin_start_attempt().unwrap();
            let task = tokio::spawn({
                let host = host.clone();
                async move { host.run().await }
            });
            host.wait_until_responsive(WAIT).await.unwrap();
            (host, task, server_id, address, invocations)
        }
    };

    let (host_a, task_a, server_a, addr_a, calls_a) = start_host("a").await;
    let (host_b, task_b, server_b, addr_b, calls_b) = start_host("b").await;

    let base = RelayConfig {
        bind: "127.0.0.1:0".into(),
        bridge: Some(bridge.bridge_url_template()),
        bridge_token: Some(bridge.token().to_owned()),
        cluster_secret: Some("mesh-conflict-secret".into()),
        seed_retry_interval: Duration::from_millis(200),
        heartbeat_interval: Duration::from_secs(1),
        ..RelayConfig::default()
    };
    let mut origin_a = tokio::task::block_in_place(|| {
        RelayServer::start_resolved(
            RelayConfig {
                state_dir: Some(files.path().join("origin-a")),
                ..base.clone()
            },
            context.clone(),
        )
    })
    .unwrap();
    let mut origin_b = tokio::task::block_in_place(|| {
        RelayServer::start_resolved(
            RelayConfig {
                state_dir: Some(files.path().join("origin-b")),
                ..base.clone()
            },
            context.clone(),
        )
    })
    .unwrap();
    tokio::task::block_in_place(|| {
        origin_a.register_upstream("grid", &server_a, &addr_a).unwrap();
        origin_b.register_upstream("grid", &server_b, &addr_b).unwrap();
    });
    let id_a = hex::encode(origin_a.iroh_endpoint_id().unwrap().as_bytes());
    let id_b = hex::encode(origin_b.iroh_endpoint_id().unwrap().as_bytes());

    // Origin B joins Origin A so the mesh learns both conflicting registrations,
    // and Anchor C joins Origin A to receive the full 3-relay conflicting view.
    let mut anchor = tokio::task::block_in_place(|| {
        RelayServer::start_resolved(
            RelayConfig {
                state_dir: Some(files.path().join("anchor-c")),
                seeds: vec![format!("iroh://{id_a}")],
                ..base.clone()
            },
            context.clone(),
        )
    })
    .unwrap();
    // Connect Origin B into the same mesh via Anchor C's iroh endpoint.
    let id_c = hex::encode(anchor.iroh_endpoint_id().unwrap().as_bytes());
    let origin_b_addr = origin_b.state_for_test().local_peer_address();
    let anchor_addr = anchor.state_for_test().local_peer_address();
    let origin_b_id = origin_b.state_for_test().relay_id().to_owned();
    let anchor_id = anchor.state_for_test().relay_id().to_owned();
    let join_b_to_c = super::peer::PeerEnvelope::new(
        &origin_b_id,
        super::peer::PeerMessage::RelayJoin {
            relay_id: origin_b_id.clone(),
            address: origin_b_addr.clone(),
        },
    );
    let _ = origin_b
        .state_for_test()
        .peer_client()
        .post_control(&anchor_addr, "/_peer/join", &join_b_to_c)
        .await
        .unwrap();
    let join_c_to_b = super::peer::PeerEnvelope::new(
        &anchor_id,
        super::peer::PeerMessage::RelayJoin {
            relay_id: anchor_id.clone(),
            address: anchor_addr.clone(),
        },
    );
    let _ = anchor
        .state_for_test()
        .peer_client()
        .post_control(&origin_b_addr, "/_peer/join", &join_c_to_b)
        .await
        .unwrap();
    let _ = id_c;
    // Push Origin B's route announcement to Anchor C over iroh control lane.
    let entry_b = origin_b.state_for_test().local_route("grid").unwrap();
    super::gossip::broadcast_route_announce(&origin_b.state_for_test(), &entry_b);

    let anchor_url = format!("http://{}", anchor.local_addr());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let resolve_url = format!(
        "{anchor_url}/_resolve?route=grid&crm_ns=test.echo&crm_name=Echo&crm_ver=0.1.0&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
    );
    tokio::time::timeout(WAIT, async {
        loop {
            let response = client.get(&resolve_url).send().await.unwrap();
            if response.status() == 409 {
                let error: serde_json::Value = response.json().await.unwrap();
                assert_eq!(error["code"], 719);
                let regs: Vec<serde_json::Value> =
                    serde_json::from_str(error["details"]["registrations"].as_str().unwrap())
                        .unwrap();
                assert_eq!(regs.len(), 2);
                let urls: std::collections::BTreeSet<_> = regs
                    .iter()
                    .map(|r| r["relay_url"].as_str().unwrap().to_owned())
                    .collect();
                assert!(urls.contains(&format!("iroh://{id_a}")), "{urls:?}");
                assert!(urls.contains(&format!("iroh://{id_b}")), "{urls:?}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("three-relay iroh route conflict did not converge");

    let probe_conflict = contract_headers(client.get(format!("{anchor_url}/_probe?route=grid")))
        .send()
        .await
        .unwrap();
    assert_eq!(probe_conflict.status(), 409);
    let call_conflict = contract_headers(client.post(format!(
        "{anchor_url}/_c2/calls/call?route=grid&method=echo"
    )))
    .body("blocked")
    .send()
    .await
    .unwrap();
    assert_eq!(call_conflict.status(), 409);
    assert_eq!(calls_a.load(Ordering::Relaxed), 0);
    assert_eq!(calls_b.load(Ordering::Relaxed), 0);

    tokio::task::block_in_place(|| origin_b.unregister_upstream("grid", &server_b)).unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let response = client.get(&resolve_url).send().await.unwrap();
            if response.status() == 200 {
                let routes: serde_json::Value = response.json().await.unwrap();
                assert_eq!(routes[0]["proxy_via_anchor"], true);
                assert!(routes[0]["relay_url"].is_null());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("conflict withdrawal did not converge");

    let ok = contract_headers(client.post(format!(
        "{anchor_url}/_c2/calls/call?route=grid&method=echo"
    )))
    .body("after-withdraw")
    .send()
    .await
    .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.bytes().await.unwrap().as_ref(), b"after-withdraw");
    assert_eq!(calls_a.load(Ordering::Relaxed), 1);
    assert_eq!(calls_b.load(Ordering::Relaxed), 0);

    tokio::task::block_in_place(|| {
        anchor.stop().unwrap();
        origin_b.stop().unwrap();
        origin_a.stop().unwrap();
    });
    host_b.shutdown_and_wait(WAIT).await.unwrap();
    task_b.await.unwrap().unwrap();
    host_a.shutdown_and_wait(WAIT).await.unwrap();
    task_a.await.unwrap().unwrap();
    bridge.stop().await.unwrap();
}

struct GatedEcho {
    entered: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl CrmCallback for GatedEcho {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        input: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if input == b"hold" {
            self.entered.store(true, Ordering::SeqCst);
            let (lock, cv) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
        }
        Ok(ResponseMeta::Inline(input))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn iroh_control_lane_stays_responsive_during_blocked_data_call() {
    let files = tempfile::tempdir().unwrap();
    let bridge = BridgeServer::start(BridgeConfig {
        bind: "127.0.0.1:0".into(),
        quic_bind: Some("127.0.0.1:0".into()),
        state_dir: Some(files.path().join("bridge")),
        ..BridgeConfig::default()
    })
    .await
    .unwrap();
    #[cfg(unix)]
    let root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = root.context();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let address = format!("ipc://mesh-ctrl-lane-{}", std::process::id());
    let host = Arc::new(
        Server::new_with_identity_and_endpoint(
            context.endpoint(&address).unwrap(),
            ServerIpcConfig::default(),
            ServerIdentity {
                server_id: "mesh-ctrl-host".into(),
                server_instance_id: "mesh-ctrl-instance".into(),
            },
        )
        .unwrap(),
    );
    let route = host
        .build_route(
            RouteBuildSpec {
                name: "grid".into(),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: TEST_ABI_HASH.into(),
                signature_hash: TEST_SIGNATURE_HASH.into(),
                method_names: vec!["echo".into()],
                access_map: Default::default(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(GatedEcho {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .unwrap();
    let reservation = host.reserve_route(route).await.unwrap();
    host.commit_reserved_route(reservation).await.unwrap();
    host.begin_start_attempt().unwrap();
    let host_task = tokio::spawn({
        let host = host.clone();
        async move { host.run().await }
    });
    host.wait_until_responsive(WAIT).await.unwrap();

    let base = RelayConfig {
        bind: "127.0.0.1:0".into(),
        bridge: Some(bridge.bridge_url_template()),
        bridge_token: Some(bridge.token().to_owned()),
        cluster_secret: Some("mesh-ctrl-secret".into()),
        seed_retry_interval: Duration::from_millis(200),
        heartbeat_interval: Duration::from_secs(1),
        ..RelayConfig::default()
    };
    let mut origin = tokio::task::block_in_place(|| {
        RelayServer::start_relay_only_for_test(
            RelayConfig {
                state_dir: Some(files.path().join("origin")),
                ..base.clone()
            },
            context.clone(),
        )
    })
    .unwrap();
    tokio::task::block_in_place(|| origin.register_upstream("grid", "mesh-ctrl-host", &address))
        .unwrap();
    let origin_id = hex::encode(origin.iroh_endpoint_id().unwrap().as_bytes());
    let mut anchor = tokio::task::block_in_place(|| {
        RelayServer::start_relay_only_for_test(
            RelayConfig {
                state_dir: Some(files.path().join("anchor")),
                seeds: vec![format!("iroh://{origin_id}")],
                domain_limits: DomainLimits {
                    memory_budget_bytes: 128 * 1024,
                    file_spill_budget_bytes: 0,
                    max_outstanding_calls: 1,
                },
                ..base
            },
            context,
        )
    })
    .unwrap();

    let anchor_url = format!("http://{}", anchor.local_addr());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let resolve_url = format!(
        "{anchor_url}/_resolve?route=grid&crm_ns=test.echo&crm_name=Echo&crm_ver=0.1.0&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
    );
    tokio::time::timeout(WAIT, async {
        loop {
            if client.get(&resolve_url).send().await.unwrap().status() == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();

    // Occupy the single data-lane business slot with a blocked keyed call.
    let blocked_call = tokio::spawn({
        let client = client.clone();
        let url = format!("{anchor_url}/_c2/calls/keyed?route=grid&method=echo");
        async move {
            let response = contract_headers(client.post(&url))
                .header("x-c2-call-id", "blocked-call-1")
                .body("hold")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap()
        }
    });
    tokio::time::timeout(WAIT, async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("blocked call did not enter resource");

    // Control lane /_probe and /_c2/calls/outcome must succeed immediately even
    // while max_outstanding_calls == 1 is saturated on the data lane.
    let probe = tokio::time::timeout(
        Duration::from_secs(5),
        contract_headers(client.get(format!("{anchor_url}/_probe?route=grid"))).send(),
    )
    .await
    .expect("control-lane /_probe starved behind blocked data call")
    .unwrap();
    assert_eq!(probe.status(), 200);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        contract_headers(client.get(format!("{anchor_url}/_c2/calls/outcome?route=grid")))
            .header("x-c2-call-id", "blocked-call-1")
            .send(),
    )
    .await
    .expect("control-lane outcome query starved behind blocked data call")
    .unwrap();
    assert_eq!(outcome.status(), 200);
    assert_eq!(outcome.headers().get("cache-control").unwrap(), "no-store");
    let body: serde_json::Value = outcome.json().await.unwrap();
    assert_eq!(body["status"], "in_flight");

    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    assert_eq!(blocked_call.await.unwrap().as_ref(), b"hold");

    tokio::task::block_in_place(|| {
        anchor.stop().unwrap();
        origin.stop().unwrap();
    });
    host.shutdown_and_wait(WAIT).await.unwrap();
    host_task.await.unwrap().unwrap();
    bridge.stop().await.unwrap();
}
