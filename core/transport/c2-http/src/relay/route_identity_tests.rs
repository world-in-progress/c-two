//! ADR 0004: "新的按名字获取在派发前失败，报路由冲突错误" and
//! "不参与解析，也不能在别处注册，直到那个 relay 回来后自己撤下".

use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

use super::{authority::RouteAuthority, router::build_router, test_support::*, types::*};

fn entry(owner: &str, locality: Locality) -> RouteEntry {
    RouteEntry {
        name: "grid".into(),
        relay_id: owner.into(),
        relay_url: format!("http://{owner}:8080"),
        server_id: (locality == Locality::Local).then(|| "server-local".into()),
        server_instance_id: (locality == Locality::Local).then(|| "instance-local".into()),
        ipc_address: (locality == Locality::Local).then(|| "ipc://local".into()),
        crm_ns: "test.echo".into(),
        crm_name: "Echo".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: TEST_ABI_HASH.into(),
        signature_hash: TEST_SIGNATURE_HASH.into(),
        max_payload_size: 1024,
        route_uid: (locality == Locality::Local).then(|| "grid-uid".into()),
        locality,
        registered_at: 1000.0,
    }
}

fn add_peer(state: &super::state::RelayState, status: PeerStatus) {
    state.register_peer(PeerInfo {
        relay_id: "relay-peer".into(),
        url: "http://relay-peer:8080".into(),
        route_count: 1,
        last_heartbeat: Instant::now(),
        status,
    });
    state.with_route_table_mut(|table| {
        assert!(table.register_route(entry("relay-peer", Locality::Peer)))
    });
}

#[test]
fn relay_leave_and_delayed_announcement_never_leave_an_orphan_registration() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::Duration;

    use super::peer::{PeerEnvelope, PeerMessage};

    // ADR 0004: a relay's departure frees its names. Deliver its earlier
    // announcement at the former two-lock boundary, without timing sleeps.
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    let (entered, resume) = state.pause_before_peer_removal();
    let leaving_state = state.clone();
    let leave = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let envelope = PeerEnvelope::new(
                    "relay-peer",
                    PeerMessage::RelayLeave {
                        relay_id: "relay-peer".into(),
                    },
                );
                build_router(leaving_state)
                    .oneshot(
                        Request::post("/_peer/leave")
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .status()
            })
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let announce = PeerEnvelope::new(
        "relay-peer",
        PeerMessage::RouteAnnounce {
            name: "grid".into(),
            relay_id: "relay-peer".into(),
            relay_url: "http://relay-peer:8080".into(),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 1024,
            registered_at: 1000.0,
        },
    );
    let response = runtime.block_on(
        build_router(state.clone()).oneshot(
            Request::post("/_peer/announce")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&announce).unwrap()))
                .unwrap(),
        ),
    );
    resume.send(()).unwrap();
    assert_eq!(response.unwrap().status(), 200);
    assert_eq!(leave.join().unwrap(), 200);
    assert!(!state.has_peer("relay-peer"));

    let resolution = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(resolve(state.clone()))
    }));
    let registration = catch_unwind(AssertUnwindSafe(|| {
        RouteAuthority::new(&state).register_local_preflight(
            "grid",
            "candidate",
            "candidate-instance",
            "ipc://candidate",
        )
    }));
    let listing = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(
            build_router(state.clone())
                .oneshot(Request::get("/_routes").body(Body::empty()).unwrap()),
        )
    }));
    assert!(
        resolution.is_ok(),
        "resolution panicked on an orphan registration"
    );
    assert!(
        registration.is_ok(),
        "registration panicked and the departed name stayed blocked"
    );
    assert!(
        listing.is_ok(),
        "route listing panicked on an orphan registration"
    );
    assert_eq!(resolution.unwrap().0, 404);
    assert!(registration.unwrap().is_ok());
    assert_eq!(listing.unwrap().unwrap().status(), 200);
    assert!(state.list_routes().is_empty());

    // An announcement that reaches the owner check after departure is ignored.
    runtime.block_on(super::peer_handlers::handle_peer_announce(
        axum::extract::State(state.clone()),
        axum::Json(announce),
    ));
    assert!(state.list_routes().is_empty());
}

async fn resolve(state: Arc<super::state::RelayState>) -> (u16, serde_json::Value) {
    let uri = format!(
        "/_resolve/grid?crm_ns=test.echo&crm_name=Echo&crm_ver=0.1.0&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}"
    );
    let response = build_router(state)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn same_name_live_registrations_refuse_new_acquisition() {
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    state.with_route_table_mut(|table| {
        assert!(table.register_route(entry("test-relay", Locality::Local)))
    });
    let (status, body) = resolve(state).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], 719);
    assert_eq!(body["details"]["dispatch_phase"], "pre_dispatch");
    let registrations: Vec<serde_json::Value> =
        serde_json::from_str(body["details"]["registrations"].as_str().unwrap()).unwrap();
    assert_eq!(registrations.len(), 2);
}

#[tokio::test]
async fn unreachable_peer_keeps_name_unavailable() {
    for status in [PeerStatus::Suspect, PeerStatus::Dead] {
        let state = test_state_for_client();
        add_peer(&state, status);
        let (code, body) = resolve(state.clone()).await;
        assert_eq!(code, 503, "{status:?}: {body}");
        assert_eq!(body["code"], 702);
        assert_eq!(state.list_routes().len(), 1);
    }
}

#[tokio::test]
async fn alive_heartbeat_and_metadata_writes_do_not_compare_conflicts() {
    use super::peer::{PeerEnvelope, PeerMessage};
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    state.with_route_table(|table| table.take_conflict_comparisons());
    super::peer_handlers::handle_peer_heartbeat(
        axum::extract::State(state.clone()),
        axum::Json(PeerEnvelope::new(
            "relay-peer",
            PeerMessage::Heartbeat {
                relay_id: "relay-peer".into(),
                route_count: 23,
            },
        )),
    )
    .await;
    state.with_route_table_mut(|table| {
        table.next_local_timestamp();
    });
    state.gc_tombstones(std::time::Duration::ZERO);
    state.with_route_table(|table| {
        assert_eq!(table.get_peer("relay-peer").unwrap().route_count, 23);
        assert!(
            table.take_conflict_comparisons().is_empty(),
            "ordinary heartbeat scanned routes"
        );
    });
}

#[test]
fn route_changes_compare_only_affected_names() {
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    state.with_route_table_mut(|table| {
        let mut unrelated = entry("test-relay", Locality::Local);
        unrelated.name = "unrelated".into();
        assert!(table.register_route(unrelated));
    });
    state.with_route_table(|table| table.take_conflict_comparisons());
    state.with_route_table_mut(|table| {
        assert!(table.register_route(entry("test-relay", Locality::Local)));
    });
    state.with_route_table(|table| {
        assert_eq!(table.take_conflict_comparisons(), ["grid", "grid"]);
    });
    state.with_route_table_mut(|table| {
        table.unregister_route_with_tombstone("grid", "relay-peer", 1001.0);
    });
    state.with_route_table(|table| {
        assert_eq!(table.take_conflict_comparisons(), ["grid", "grid"]);
    });
}

#[test]
fn only_online_status_transitions_compare_the_owners_names() {
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    state.with_route_table_mut(|table| {
        assert!(table.register_route(entry("test-relay", Locality::Local)));
        let mut unrelated = entry("test-relay", Locality::Local);
        unrelated.name = "unrelated".into();
        assert!(table.register_route(unrelated));
    });
    state.with_route_table(|table| table.take_conflict_comparisons());
    state.with_route_table_mut(|table| table.set_peer_status("relay-peer", PeerStatus::Suspect));
    state.with_route_table(|table| {
        assert_eq!(table.take_conflict_comparisons(), ["grid", "grid"]);
    });
    state.with_route_table_mut(|table| table.set_peer_status("relay-peer", PeerStatus::Dead));
    state.with_route_table(|table| assert!(table.take_conflict_comparisons().is_empty()));
    state.with_route_table_mut(|table| table.record_peer_heartbeat("relay-peer", 1));
    state.with_route_table(|table| {
        assert_eq!(table.take_conflict_comparisons(), ["grid", "grid"]);
    });
    state.with_route_table_mut(|table| {
        assert!(table.register_route(entry("relay-peer", Locality::Peer)));
    });
    state.with_route_table(|table| assert!(table.take_conflict_comparisons().is_empty()));
}

#[test]
fn conflict_comparison_records_one_before_image_per_transaction() {
    use super::route_table::RouteTable;
    let mut table = RouteTable::new("test-relay".into());
    table.register_peer(PeerInfo {
        relay_id: "relay-peer".into(),
        url: "http://relay-peer:8080".into(),
        route_count: 1,
        last_heartbeat: Instant::now(),
        status: PeerStatus::Alive,
    });
    assert!(table.register_route(entry("test-relay", Locality::Local)));
    assert!(table.register_route(entry("relay-peer", Locality::Peer)));
    table.begin_conflict_changes();
    table.unregister_route_with_tombstone("grid", "relay-peer", 1001.0);
    let mut replacement = entry("relay-peer", Locality::Peer);
    replacement.registered_at = 1002.0;
    assert!(table.register_route(replacement));
    let (before, after) = table.finish_conflict_changes();
    assert_eq!(before["grid"].len(), 2);
    assert_eq!(after["grid"].len(), 2);
    assert_eq!(table.take_conflict_comparisons(), ["grid", "grid"]);
    // The before-images are drained at commit, rather than cached across writes.
    table.begin_conflict_changes();
    assert!(table.finish_conflict_changes().0.is_empty());
    assert!(table.take_conflict_comparisons().is_empty());
}

#[test]
fn peer_held_name_refuses_registration_for_each_status() {
    for status in [PeerStatus::Alive, PeerStatus::Suspect, PeerStatus::Dead] {
        let state = test_state_for_client();
        add_peer(&state, status);
        assert!(
            RouteAuthority::new(&state)
                .register_local_preflight(
                    "grid",
                    "candidate",
                    "candidate-instance",
                    "ipc://candidate"
                )
                .is_err(),
            "{status:?}"
        );
    }
}

#[test]
fn full_sync_keeps_dead_peer_registration_without_recovering_it() {
    let state = test_state_for_client();
    let sync = FullSync {
        routes: vec![entry("relay-peer", Locality::Peer)],
        tombstones: vec![],
        peers: vec![PeerSnapshot {
            relay_id: "relay-peer".into(),
            url: "http://relay-peer:8080".into(),
            route_count: 1,
            status: PeerStatus::Dead,
        }],
    };
    state.merge_snapshot(
        ValidatedFullSync::try_from(FullSyncEnvelope {
            protocol_version: super::peer::PROTOCOL_VERSION,
            snapshot: FullSyncSnapshot::from_internal(sync),
        })
        .unwrap(),
    );
    assert_eq!(state.list_routes().len(), 1);
    assert!(!state.peer_is_alive("relay-peer"));
}

#[test]
fn registration_commit_rechecks_peer_announcement_after_preflight() {
    use super::authority::{
        AttestedRouteContract, ControlError, LocalRegistration, LocalRouteOwner,
    };
    let state = test_state_for_client();
    let before_announce = Arc::new(std::sync::Barrier::new(2));
    let after_announce = Arc::new(std::sync::Barrier::new(2));
    let peer_state = state.clone();
    let peer_before = before_announce.clone();
    let peer_after = after_announce.clone();
    let peer = std::thread::spawn(move || {
        peer_before.wait();
        add_peer(&peer_state, PeerStatus::Alive);
        peer_after.wait();
    });
    assert!(
        RouteAuthority::new(&state)
            .register_local_preflight("grid", "candidate", "candidate-instance", "ipc://candidate")
            .is_ok()
    );
    before_announce.wait();
    after_announce.wait();
    let result = RouteAuthority::new(&state).register_local(LocalRegistration {
        owner: LocalRouteOwner {
            name: "grid".into(),
            server_id: "candidate".into(),
            server_instance_id: "candidate-instance".into(),
            address: "ipc://candidate".into(),
        },
        contract: AttestedRouteContract {
            route_uid: "candidate-uid".into(),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 1024,
        },
        replacement: None,
    });
    peer.join().unwrap();
    assert!(matches!(result, Err(ControlError::PeerRouteHeld { .. })));
    assert!(state.local_route("grid").is_none());
}

#[tokio::test]
async fn conflict_precedes_contract_filter_and_only_marks_live_rows() {
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Alive);
    state.with_route_table_mut(|table| {
        let mut local = entry("test-relay", Locality::Local);
        local.abi_hash = "c".repeat(64);
        assert!(table.register_route(local));
    });
    let (status, body) = resolve(state.clone()).await;
    assert_eq!((status, body["code"].as_u64()), (409, Some(719)));
    let response = build_router(state.clone())
        .oneshot(Request::get("/_routes").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["conflict"] == true)
    );
    state.with_route_table_mut(|table| table.set_peer_status("relay-peer", PeerStatus::Dead));
    let response = build_router(state)
        .oneshot(Request::get("/_routes").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["conflict"] == false)
    );
    assert!(
        body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["relay_status"] == "Dead")
    );
}

#[test]
fn full_sync_absence_does_not_withdraw_known_unreachable_registration() {
    let state = test_state_for_client();
    add_peer(&state, PeerStatus::Suspect);
    let last_heartbeat =
        state.with_route_table(|table| table.get_peer("relay-peer").unwrap().last_heartbeat);
    let sync = FullSync {
        routes: vec![],
        tombstones: vec![],
        peers: vec![PeerSnapshot {
            relay_id: "relay-peer".into(),
            url: "http://relay-peer:8080".into(),
            route_count: 0,
            status: PeerStatus::Alive,
        }],
    };
    state.merge_snapshot(
        ValidatedFullSync::try_from(FullSyncEnvelope {
            protocol_version: super::peer::PROTOCOL_VERSION,
            snapshot: FullSyncSnapshot::from_internal(sync),
        })
        .unwrap(),
    );
    assert_eq!(state.list_routes().len(), 1);
    state.with_route_table(|table| {
        let peer = table.get_peer("relay-peer").unwrap();
        assert_eq!(peer.status, PeerStatus::Suspect);
        assert_eq!(peer.last_heartbeat, last_heartbeat);
    });
}
