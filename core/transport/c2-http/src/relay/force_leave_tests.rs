//! Decision A: "每个对端按自己看到的状态同样处理".
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use c2_config::RelayConfig;
use tower::ServiceExt;

use super::authority::RouteAuthority;
use super::disseminator::{Disseminator, FullBroadcast};
use super::peer::{PROTOCOL_VERSION, PeerEnvelope, PeerMessage, validate_route_state_envelope};
use super::router::build_router;
use super::state::RelayState;
use super::test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH};
use super::types::*;

#[derive(Default)]
struct RecordingDisseminator(Mutex<Vec<PeerEnvelope>>);
impl Disseminator for RecordingDisseminator {
    fn broadcast(
        &self,
        envelope: PeerEnvelope,
        _: &[PeerSnapshot],
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.0.lock().unwrap().push(envelope);
        None
    }
}

fn state(id: &str) -> (Arc<RelayState>, Arc<RecordingDisseminator>) {
    let recorder = Arc::new(RecordingDisseminator::default());
    (
        Arc::new(RelayState::new(
            Arc::new(RelayConfig {
                relay_id: id.into(),
                advertise_url: format!("http://{id}:8080"),
                ..Default::default()
            }),
            recorder.clone(),
        )),
        recorder,
    )
}

fn peer(state: &RelayState, id: &str, status: PeerStatus) {
    state.register_peer(PeerInfo {
        relay_id: id.into(),
        url: format!("http://{id}:8080"),
        route_count: 0,
        last_heartbeat: Instant::now(),
        status,
    });
}

fn route(name: &str, owner: &str, locality: Locality) -> RouteEntry {
    RouteEntry {
        name: name.into(),
        relay_id: owner.into(),
        relay_url: format!("http://{owner}:8080"),
        server_id: (locality == Locality::Local).then(|| format!("server-{owner}")),
        server_instance_id: (locality == Locality::Local).then(|| format!("instance-{owner}")),
        ipc_address: (locality == Locality::Local).then(|| format!("ipc://{owner}")),
        crm_ns: "test.ns".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: TEST_ABI_HASH.into(),
        signature_hash: TEST_SIGNATURE_HASH.into(),
        max_payload_size: 1024,
        route_uid: (locality == Locality::Local).then(|| format!("{name}-{owner}-uid")),
        locality,
        registered_at: 1000.0,
    }
}

fn target_records(state: &RelayState, id: &str, status: PeerStatus) {
    peer(state, id, status);
    state.with_route_table_mut(|table| {
        table.register_route(route("grid", id, Locality::Peer));
        table.register_route(route("other", id, Locality::Peer));
        table.unregister_route_with_tombstone("gone", id, 1001.0);
    });
}

fn assert_forgotten(state: &RelayState, id: &str) {
    assert!(!state.has_peer(id));
    assert!(state.list_routes().iter().all(|route| route.relay_id != id));
    assert!(
        state
            .full_snapshot()
            .tombstones
            .iter()
            .all(|entry| entry.relay_id != id)
    );
}

async fn post(state: Arc<RelayState>, path: &str, body: &str) -> (StatusCode, String) {
    let response = build_router(state)
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap(),
    )
}

fn force_message(sender: &str, target: &str) -> String {
    serde_json::json!({"protocol_version":7,"sender_relay_id":sender,
        "message":{"type":"RelayForceLeave","relay_id":target}})
    .to_string()
}

#[test]
fn force_leave_authority_deletes_only_target_records_for_suspect_and_dead() {
    for status in [PeerStatus::Suspect, PeerStatus::Dead] {
        let (state, _) = state("relay-a");
        target_records(&state, "relay-c", status);
        target_records(&state, "relay-b", PeerStatus::Alive);
        state.with_route_table_mut(|table| {
            table.register_route(route("local", "relay-a", Locality::Local))
        });
        RouteAuthority::new(&state)
            .force_leave_peer("relay-c")
            .unwrap();
        assert_forgotten(&state, "relay-c");
        assert!(state.has_peer("relay-b"));
        assert_eq!(state.list_routes().len(), 3);
        assert_eq!(state.full_snapshot().tombstones.len(), 1);
        assert!(
            RouteAuthority::new(&state)
                .force_leave_peer("relay-c")
                .is_err()
        );
    }
}

#[test]
fn force_leave_route_table_uses_current_status_at_mutation() {
    let (state, _) = state("relay-a");
    target_records(&state, "relay-c", PeerStatus::Suspect);
    state.with_route_table_mut(|table| {
        table.set_peer_status("relay-c", PeerStatus::Alive);
        assert!(table.force_leave_peer("relay-c").is_err());
        assert!(table.has_peer("relay-c"));
        table.set_peer_status("relay-c", PeerStatus::Dead);
        table.force_leave_peer("relay-c").unwrap();
    });
    assert_forgotten(&state, "relay-c");
}

#[test]
fn force_leave_authority_rejects_without_mutating() {
    let (state, _) = state("relay-a");
    target_records(&state, "relay-c", PeerStatus::Alive);
    let before =
        serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap();
    for id in [
        "relay-a",
        "relay-c",
        "unknown",
        "",
        "bad/relay",
        "bad\\relay",
        " relay-c",
        ".",
    ] {
        assert!(
            RouteAuthority::new(&state).force_leave_peer(id).is_err(),
            "{id}"
        );
        assert_eq!(
            serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn force_leave_admin_status_body_and_no_broadcast_on_rejection() {
    let (state, recorder) = state("relay-a");
    target_records(&state, "relay-c", PeerStatus::Alive);
    for (body, status) in [
        ("{", StatusCode::BAD_REQUEST),
        ("{}", StatusCode::BAD_REQUEST),
        (r#"{"relay_id":3}"#, StatusCode::BAD_REQUEST),
        (
            r#"{"relay_id":"relay-c","extra":1}"#,
            StatusCode::BAD_REQUEST,
        ),
        (r#"{"relay_id":"bad/relay"}"#, StatusCode::BAD_REQUEST),
        (r#"{"relay_id":"unknown"}"#, StatusCode::NOT_FOUND),
        (r#"{"relay_id":"relay-a"}"#, StatusCode::CONFLICT),
        (r#"{"relay_id":"relay-c"}"#, StatusCode::CONFLICT),
    ] {
        let (actual, text) = post(state.clone(), "/_admin/force-leave", body).await;
        assert_eq!(actual, status, "{body}: {text}");
        assert!(!text.is_empty());
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_err(),
            "expected plain text: {text}"
        );
        assert!(state.has_peer("relay-c"));
        assert!(recorder.0.lock().unwrap().is_empty());
    }
    state.with_route_table_mut(|table| table.set_peer_status("relay-c", PeerStatus::Suspect));
    let (status, text) = post(
        state.clone(),
        "/_admin/force-leave",
        r#"{"relay_id":"relay-c"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::json!({"force_left":"relay-c"})
    );
    assert_forgotten(&state, "relay-c");
    let envelopes = recorder.0.lock().unwrap();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].sender_relay_id, "relay-a");
    assert_eq!(
        serde_json::to_value(&envelopes[0].message).unwrap(),
        serde_json::json!({"type":"RelayForceLeave","relay_id":"relay-c"})
    );
    drop(envelopes);
    assert_eq!(
        post(state, "/_admin/force-leave", r#"{"relay_id":"relay-c"}"#)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn force_leave_wire_validation_distinguishes_deployer_from_graceful_leave() {
    let message: PeerEnvelope = serde_json::from_str(&force_message("relay-a", "relay-c")).unwrap();
    assert_eq!(PROTOCOL_VERSION, 7);
    assert!(validate_route_state_envelope(message.clone(), Some("relay-a")).is_ok());
    let wire = serde_json::to_string(&message).unwrap();
    assert_eq!(
        wire,
        serde_json::to_string(&serde_json::from_str::<PeerEnvelope>(&wire).unwrap()).unwrap()
    );
    for (sender, target) in [
        ("relay-a", "relay-a"),
        ("bad/relay", "relay-c"),
        ("relay-a", "bad/relay"),
    ] {
        let envelope = serde_json::from_str(&force_message(sender, target)).unwrap();
        assert!(validate_route_state_envelope(envelope, None).is_err());
    }
    for version in [6, 8] {
        let mut envelope = message.clone();
        envelope.protocol_version = version;
        assert!(validate_route_state_envelope(envelope, None).is_err());
    }
    let mut wire: serde_json::Value = serde_json::from_str(&wire).unwrap();
    wire["message"]["extra"] = 1.into();
    assert!(serde_json::from_value::<PeerEnvelope>(wire).is_err());
    assert!(
        validate_route_state_envelope(
            PeerEnvelope::new(
                "relay-a",
                PeerMessage::RelayLeave {
                    relay_id: "relay-c".into()
                }
            ),
            None
        )
        .is_err()
    );
}

#[tokio::test]
async fn force_leave_peer_obeys_local_view_and_rebroadcasts_only_on_removal() {
    for target_status in [PeerStatus::Alive, PeerStatus::Suspect, PeerStatus::Dead] {
        let (state, recorder) = state("relay-b");
        peer(&state, "relay-a", PeerStatus::Alive);
        target_records(&state, "relay-c", target_status);
        let message = force_message("relay-a", "relay-c");
        assert_eq!(
            post(state.clone(), "/_peer/force-leave", &message).await.0,
            StatusCode::OK
        );
        if target_status == PeerStatus::Alive {
            assert!(state.has_peer("relay-c"));
            assert!(recorder.0.lock().unwrap().is_empty());
        } else {
            assert_forgotten(&state, "relay-c");
            assert_eq!(recorder.0.lock().unwrap()[0].sender_relay_id, "relay-b");
            post(state.clone(), "/_peer/force-leave", &message).await;
            assert_eq!(recorder.0.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn force_leave_peer_rejects_untrusted_senders_and_invalid_targets() {
    let (state, recorder) = state("relay-b");
    target_records(&state, "relay-c", PeerStatus::Suspect);
    peer(&state, "suspect", PeerStatus::Suspect);
    peer(&state, "dead", PeerStatus::Dead);
    peer(&state, "relay-a", PeerStatus::Alive);
    let before =
        serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap();
    for (sender, target) in [
        ("unknown", "relay-c"),
        ("suspect", "relay-c"),
        ("dead", "relay-c"),
        ("relay-b", "relay-c"),
        ("relay-c", "relay-c"),
        ("bad/relay", "relay-c"),
        ("relay-a", "relay-b"),
        ("relay-a", "unknown"),
        ("relay-a", "bad/relay"),
    ] {
        assert_eq!(
            post(
                state.clone(),
                "/_peer/force-leave",
                &force_message(sender, target)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            serde_json::to_value(FullSyncSnapshot::from_internal(state.full_snapshot())).unwrap(),
            before,
            "{sender}/{target}"
        );
        assert!(recorder.0.lock().unwrap().is_empty());
    }
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut poll = tokio::time::interval(Duration::from_millis(5));
        while !condition() {
            poll.tick().await;
        }
    })
    .await
    .expect("mesh condition did not converge");
}

#[tokio::test]
async fn force_leave_digest_cannot_restore_state_from_another_owner() {
    let (a, _) = state("relay-a");
    let (b, _) = state("relay-b");
    peer(&a, "relay-b", PeerStatus::Alive);
    peer(&b, "relay-a", PeerStatus::Alive);
    target_records(&a, "relay-c", PeerStatus::Suspect);
    target_records(&b, "relay-c", PeerStatus::Alive);
    RouteAuthority::new(&a).force_leave_peer("relay-c").unwrap();
    let request = PeerEnvelope::new("relay-a", PeerMessage::DigestExchange { digest: vec![] });
    let (status, body) = post(
        b,
        "/_peer/digest",
        &serde_json::to_string(&request).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reply: PeerEnvelope = serde_json::from_str(&body).unwrap();
    let PeerMessage::DigestDiff { entries } = &reply.message else {
        panic!("expected digest diff")
    };
    assert!(entries.is_empty(), "relay-b must not send relay-c's state");
    post(a.clone(), "/_peer/digest", &body).await;
    // Even a peer that sends a third party's valid fields cannot restore it.
    let entry = route("grid", "relay-c", Locality::Peer);
    let foreign = serde_json::json!({"protocol_version":7,"sender_relay_id":"relay-b","message":{
        "type":"DigestDiff","entries":[{"state":"Active","name":entry.name,"relay_id":entry.relay_id,
        "relay_url":entry.relay_url,"crm_ns":entry.crm_ns,"crm_name":entry.crm_name,"crm_ver":entry.crm_ver,
        "abi_hash":entry.abi_hash,"signature_hash":entry.signature_hash,"max_payload_size":1024,
        "route_uid":entry.route_uid,"registered_at":1000.0}]}});
    post(a.clone(), "/_peer/digest", &foreign.to_string()).await;
    let deleted = serde_json::json!({"protocol_version":7,"sender_relay_id":"relay-b","message":{
        "type":"DigestDiff","entries":[{"state":"Deleted","name":"gone","relay_id":"relay-c","removed_at":1001.0}]}});
    post(a.clone(), "/_peer/digest", &deleted.to_string()).await;
    post(
        a.clone(),
        "/_peer/heartbeat",
        &serde_json::to_string(&PeerEnvelope::new(
            "relay-c",
            PeerMessage::Heartbeat {
                relay_id: "relay-c".into(),
                route_count: 2,
            },
        ))
        .unwrap(),
    )
    .await;
    assert_forgotten(&a, "relay-c");
}

#[tokio::test]
async fn force_leave_seed_rejoin_adopts_the_seeds_current_view() {
    // Full sync is bootstrap into the seed's view, not anti-entropy.
    for seed_remembers_target in [true, false] {
        let (seed, _) = state("relay-b");
        target_records(&seed, "relay-c", PeerStatus::Alive);
        if !seed_remembers_target {
            seed.with_route_table_mut(|table| {
                table.set_peer_status("relay-c", PeerStatus::Suspect)
            });
            RouteAuthority::new(&seed)
                .force_leave_peer("relay-c")
                .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = build_router(seed);
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let joiner = Arc::new(RelayState::new(
            Arc::new(RelayConfig {
                relay_id: "relay-a".into(),
                advertise_url: "http://127.0.0.1:1".into(),
                seeds: vec![url],
                heartbeat_interval: Duration::ZERO,
                anti_entropy_interval: Duration::ZERO,
                dead_peer_probe_interval: Duration::ZERO,
                seed_retry_interval: Duration::ZERO,
                ..Default::default()
            }),
            Arc::new(super::test_support::NoopDisseminator),
        ));
        target_records(&joiner, "relay-c", PeerStatus::Suspect);
        RouteAuthority::new(&joiner)
            .force_leave_peer("relay-c")
            .unwrap();
        assert!(joiner.list_peers().is_empty());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut tasks = super::background::spawn_background_tasks(joiner.clone(), cancel.clone());
        // Only seed join and tombstone GC are enabled; await the one-shot join.
        tokio::time::timeout(Duration::from_secs(5), tasks.remove(0))
            .await
            .unwrap()
            .unwrap();
        assert!(joiner.has_peer("relay-b"));
        assert_eq!(joiner.has_peer("relay-c"), seed_remembers_target);
        assert_eq!(
            joiner
                .list_routes()
                .iter()
                .any(|route| route.relay_id == "relay-c"),
            seed_remembers_target
        );
        assert_eq!(
            joiner
                .full_snapshot()
                .tombstones
                .iter()
                .any(|entry| entry.relay_id == "relay-c"),
            seed_remembers_target
        );
        cancel.cancel();
        for task in tasks {
            task.await.unwrap();
        }
        http.abort();
        assert!(http.await.unwrap_err().is_cancelled());
    }
}

struct GatedDisseminator {
    inner: FullBroadcast,
    gate: Arc<tokio::sync::Semaphore>,
    entered: Arc<tokio::sync::Semaphore>,
}
impl Disseminator for GatedDisseminator {
    fn broadcast(
        &self,
        envelope: PeerEnvelope,
        peers: &[PeerSnapshot],
    ) -> Option<tokio::task::JoinHandle<()>> {
        if matches!(envelope.message, PeerMessage::RelayForceLeave { .. }) {
            let gate = self.gate.clone();
            let entered = self.entered.clone();
            // Keep the production FullBroadcast mapping and HTTP transport.
            let peers = peers.to_vec();
            Some(tokio::spawn(async move {
                entered.add_permits(1);
                let permit = gate.acquire().await.unwrap();
                permit.forget();
                if let Some(task) =
                    FullBroadcast::with_proxy_policy(false).broadcast(envelope, &peers)
                {
                    task.await.unwrap();
                }
            }))
        } else {
            self.inner.broadcast(envelope, peers)
        }
    }
}

struct MeshRelay {
    state: Arc<RelayState>,
    url: String,
    http: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
    background: Vec<tokio::task::JoinHandle<()>>,
    clients: tokio::task::JoinHandle<()>,
}
impl MeshRelay {
    async fn start(
        id: &str,
        context: c2_config::LocalEndpointContext,
        disseminator: Arc<dyn Disseminator>,
        seeds: Vec<String>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(RelayState::new_with_context(
            Arc::new(RelayConfig {
                relay_id: id.into(),
                advertise_url: url.clone(),
                heartbeat_interval: Duration::from_millis(80),
                heartbeat_miss_threshold: 5,
                anti_entropy_interval: Duration::from_millis(50),
                dead_peer_probe_interval: Duration::from_millis(40),
                seed_retry_interval: Duration::from_millis(40),
                seeds,
                ..Default::default()
            }),
            disseminator,
            context,
        ));
        let domain = state.clients.clone();
        let clients = tokio::spawn(async move { domain.run().await });
        let cancel = tokio_util::sync::CancellationToken::new();
        let background = super::background::spawn_background_tasks(state.clone(), cancel.clone());
        let app = build_router(state.clone());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            state,
            url,
            http,
            cancel,
            background,
            clients,
        }
    }
    async fn stop_network(&mut self) {
        self.cancel.cancel();
        for task in self.background.drain(..) {
            task.await.unwrap();
        }
        self.http.abort();
        let _ = (&mut self.http).await;
    }
    async fn restore_network(&mut self) {
        let listener = tokio::net::TcpListener::bind(self.url.strip_prefix("http://").unwrap())
            .await
            .unwrap();
        let app = build_router(self.state.clone());
        self.http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        self.cancel = tokio_util::sync::CancellationToken::new();
        self.background =
            super::background::spawn_background_tasks(self.state.clone(), self.cancel.clone());
    }
    async fn finish(mut self) {
        self.stop_network().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            self.state.stop_upstream_controls().await;
            self.state.clients.shutdown().await;
            self.clients.await.unwrap();
        })
        .await
        .expect("relay cleanup did not finish");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_leave_three_relay_mesh_rejoins_without_seeds() {
    force_leave_three_relay_mesh(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_leave_three_relay_mesh_rejoins_with_seed() {
    force_leave_three_relay_mesh(true).await;
}

async fn force_leave_three_relay_mesh(c_has_seed: bool) {
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let context = c2_config::LocalEndpointContext::with_unix_root(root.path()).unwrap();
    #[cfg(windows)]
    let context = c2_config::LocalEndpointContext::default_for_platform().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let a_disseminator = Arc::new(GatedDisseminator {
        inner: FullBroadcast::with_proxy_policy(false),
        gate: gate.clone(),
        entered: entered.clone(),
    });
    let (a, b, mut c) = if c_has_seed {
        let a = MeshRelay::start("relay-a", context.clone(), a_disseminator, vec![]).await;
        let c = MeshRelay::start(
            "relay-c",
            context.clone(),
            Arc::new(FullBroadcast::with_proxy_policy(false)),
            vec![a.url.clone()],
        )
        .await;
        eventually(|| a.state.peer_is_alive("relay-c") && c.state.peer_is_alive("relay-a")).await;
        let b = MeshRelay::start(
            "relay-b",
            context.clone(),
            Arc::new(FullBroadcast::with_proxy_policy(false)),
            vec![c.url.clone()],
        )
        .await;
        (a, b, c)
    } else {
        let c = MeshRelay::start(
            "relay-c",
            context.clone(),
            Arc::new(FullBroadcast::with_proxy_policy(false)),
            vec![],
        )
        .await;
        let a = MeshRelay::start(
            "relay-a",
            context.clone(),
            a_disseminator,
            vec![c.url.clone()],
        )
        .await;
        eventually(|| a.state.peer_is_alive("relay-c") && c.state.peer_is_alive("relay-a")).await;
        let b = MeshRelay::start(
            "relay-b",
            context.clone(),
            Arc::new(FullBroadcast::with_proxy_policy(false)),
            vec![c.url.clone()],
        )
        .await;
        (a, b, c)
    };
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Initial membership also comes from the production seed/join paths.
    eventually(|| {
        [&a, &b, &c].iter().all(|node| {
            node.state
                .list_peers()
                .iter()
                .filter(|peer| peer.status == PeerStatus::Alive)
                .count()
                == 2
        })
    })
    .await;
    let mut grid = route("grid", "relay-c", Locality::Local);
    grid.relay_url = c.url.clone();
    let mut still = route("still", "relay-c", Locality::Local);
    still.relay_url = c.url.clone();
    c.state.with_route_table_mut(|table| {
        table.register_route(grid.clone());
        table.register_route(still.clone());
        table.unregister_route_with_tombstone("gone", "relay-c", 1001.0);
    });
    super::gossip::broadcast_route_announce(&c.state, &grid);
    super::gossip::broadcast_route_announce(&c.state, &still);
    eventually(|| {
        [&a, &b].iter().all(|node| {
            node.state
                .list_routes()
                .iter()
                .filter(|entry| entry.relay_id == "relay-c")
                .count()
                == 2
        })
    })
    .await;
    c.stop_network().await;
    eventually(|| {
        [&a, &b].iter().all(|node| {
            node.state
                .list_peers()
                .iter()
                .any(|peer| peer.relay_id == "relay-c" && peer.status != PeerStatus::Alive)
        })
    })
    .await;
    let response = http
        .post(format!("{}/_admin/force-leave", a.url))
        .json(&serde_json::json!({"relay_id":"relay-c"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"force_left":"relay-c"})
    );
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_forgotten(&a.state, "relay-c");
    assert!(
        b.state.has_peer("relay-c"),
        "test barrier must delay delivery"
    );
    // B still knows C, yet its production digest endpoint only sends B's own state.
    let digest = PeerEnvelope::new("relay-a", PeerMessage::DigestExchange { digest: vec![] });
    let reply: PeerEnvelope = http
        .post(format!("{}/_peer/digest", b.url))
        .json(&digest)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        http.post(format!("{}/_peer/digest", a.url))
            .json(&reply)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_forgotten(&a.state, "relay-c");
    gate.add_permits(1);
    eventually(|| !b.state.has_peer("relay-c")).await;
    assert_forgotten(&b.state, "relay-c");

    // Real upstream proof, in this test's isolated endpoint namespace.
    let address = format!("ipc://force_leave_{}_{}", std::process::id(), c_has_seed);
    let server = Arc::new(
        c2_server::Server::new_with_identity_and_endpoint(
            context.endpoint(&address).unwrap(),
            c2_server::ServerIpcConfig::default(),
            c2_server::ServerIdentity {
                server_id: "replacement".into(),
                server_instance_id: "replacement-instance".into(),
            },
        )
        .unwrap(),
    );
    super::test_support::register_echo_route(&server, "grid").await;
    let running = server.clone();
    let server_task = tokio::spawn(async move { running.run().await });
    server
        .wait_until_responsive(Duration::from_secs(5))
        .await
        .unwrap();
    let response = http.post(format!("{}/_register", b.url))
        .header(crate::client::LOCAL_ENDPOINT_NAMESPACE_HEADER, context.namespace_id()).json(&serde_json::json!({
        "name":"grid","server_id":"replacement","server_instance_id":"replacement-instance","address":address,
            "max_payload_size":c2_server::ServerIpcConfig::default().max_payload_size,
            "crm_ns":"test.echo","crm_name":"Echo","crm_ver":"0.1.0",
            "abi_hash":TEST_ABI_HASH,"signature_hash":TEST_SIGNATURE_HASH
    })).send().await.unwrap();
    let status = response.status();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        response.text().await.unwrap()
    );
    assert!(b.state.local_route("grid").is_some());

    // The same relay resumes at its original URL with its existing state.
    // Only production probes, seed retries and anti-entropy may restore it.
    let restored_at = Instant::now();
    c.restore_network().await;
    eventually(|| {
        [&a, &b].iter().all(|node| {
            ["grid", "still"].iter().all(|name| {
                node.state
                    .list_routes()
                    .iter()
                    .any(|entry| entry.name == *name && entry.relay_id == "relay-c")
            })
        })
    })
    .await;
    assert!(b.state.local_route("grid").is_some());
    assert_eq!(
        a.state
            .list_peers()
            .iter()
            .find(|peer| peer.relay_id == "relay-c")
            .unwrap()
            .url,
        c.url
    );
    assert!(
        [&a, &b]
            .iter()
            .all(|node| node.state.peer_is_alive("relay-c"))
    );
    eprintln!(
        "force-leave rejoin c_has_seed={c_has_seed}: {:?}",
        restored_at.elapsed()
    );
    a.finish().await;
    b.finish().await;
    c.finish().await;
    server
        .shutdown_and_wait(Duration::from_secs(5))
        .await
        .unwrap();
    server_task.await.unwrap().unwrap();
    drop(root);
}

#[tokio::test]
async fn force_leave_dead_peer_probe_rejoins_before_digest_without_merging_snapshot() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    let order = Arc::new(Mutex::new(Vec::new()));
    let digest_received = Arc::new(tokio::sync::Semaphore::new(0));
    let snapshot = FullSyncEnvelope {
        protocol_version: PROTOCOL_VERSION,
        snapshot: FullSyncSnapshot {
            routes: vec![],
            tombstones: vec![],
            peers: vec![PeerSnapshot {
                relay_id: "relay-foreign".into(),
                url: "http://relay-foreign:8080".into(),
                route_count: 0,
                status: PeerStatus::Alive,
            }],
        },
    };
    // The response really is a valid bootstrap snapshot; failure to learn this
    // third party must come from not merging it, not from invalid wire data.
    ValidatedFullSync::try_from(snapshot.clone()).unwrap();
    let health_order = order.clone();
    let join_order = order.clone();
    let digest_order = order.clone();
    let received = digest_received.clone();
    let app = Router::new()
        .route(
            "/health",
            get(move || {
                let order = health_order.clone();
                async move {
                    order.lock().unwrap().push("health");
                    StatusCode::OK
                }
            }),
        )
        .route(
            "/_peer/join",
            post(move |Json(envelope): Json<PeerEnvelope>| {
                let order = join_order.clone();
                let snapshot = snapshot.clone();
                async move {
                    assert_eq!(envelope.sender_relay_id, "relay-c");
                    assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
                    assert!(
                        matches!(envelope.message, PeerMessage::RelayJoin { relay_id, url }
                    if relay_id == "relay-c" && url == "http://relay-c:8080")
                    );
                    order.lock().unwrap().push("join");
                    Json(snapshot)
                }
            }),
        )
        .route(
            "/_peer/digest",
            post(move |Json(envelope): Json<PeerEnvelope>| {
                let order = digest_order.clone();
                let received = received.clone();
                async move {
                    assert_eq!(envelope.sender_relay_id, "relay-c");
                    assert!(matches!(
                        envelope.message,
                        PeerMessage::DigestExchange { .. }
                    ));
                    order.lock().unwrap().push("digest");
                    received.add_permits(1);
                    Json(PeerEnvelope::new(
                        "relay-b",
                        PeerMessage::DigestDiff { entries: vec![] },
                    ))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = Arc::new(RelayState::new(
        Arc::new(RelayConfig {
            relay_id: "relay-c".into(),
            advertise_url: "http://relay-c:8080".into(),
            heartbeat_interval: Duration::ZERO,
            anti_entropy_interval: Duration::ZERO,
            dead_peer_probe_interval: Duration::from_millis(10),
            ..Default::default()
        }),
        Arc::new(super::test_support::NoopDisseminator),
    ));
    state.register_peer(PeerInfo {
        relay_id: "relay-b".into(),
        url,
        route_count: 0,
        last_heartbeat: Instant::now(),
        status: PeerStatus::Dead,
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let tasks = super::background::spawn_background_tasks(state.clone(), cancel.clone());
    tokio::time::timeout(Duration::from_secs(5), digest_received.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    cancel.cancel();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), ["health", "join", "digest"]);
    assert!(state.peer_is_alive("relay-b"));
    assert!(!state.has_peer("relay-foreign"));
    http.abort();
    assert!(http.await.unwrap_err().is_cancelled());
}
