//! Idle-eviction audit gates through the public Rust SDK, using real IPC/HTTP.
#[path = "fixtures/persistent_client_lifecycle.rs"]
mod fixture;

use c_two::generated::EncodedClient;
use fixture::{Fixture, PathKind, observe_until};
use std::sync::{Arc, Barrier};

// Gates 1 and 6: a persistent manager connection sees a builder registered
// later on the same endpoint. Local paths keep the original physical carrier;
// HTTP uses the actual relay data plane, without a local SDK connection.
#[test]
fn persistent_direct_sees_later_builder_without_reconnecting() {
    late_builder(PathKind::Direct);
}

#[test]
fn persistent_relay_local_sees_later_builder_without_http_fallback() {
    late_builder(PathKind::Local);
}

#[test]
fn persistent_relay_http_sees_later_builder() {
    late_builder(PathKind::Http);
}

fn late_builder(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let manager = fixture.register("manager");
    let manager_client = fixture.connect(&manager);
    fixture.call(&manager_client, &manager, "manager-before-builder");
    let physical = fixture.outgoing_ids();
    let builder = fixture.register("builder");
    let builder_client = fixture.connect(&builder);
    fixture.call(&builder_client, &builder, "builder");
    fixture.call(&manager_client, &manager, "manager-after-builder");
    assert_eq!(fixture.outgoing_ids(), physical);
    if path != PathKind::Http {
        assert_eq!(physical.len(), 1);
        assert_eq!(
            manager_client
                .connection_observation()
                .unwrap()
                .connection_id(),
            builder_client
                .connection_observation()
                .unwrap()
                .connection_id(),
        );
    } else {
        assert!(physical.is_empty());
    }
    assert_eq!(manager.calls(), 2);
    assert_eq!(builder.calls(), 1);
    fixture.assert_paths(2);
    // No resource callback is entered by a refused acquisition.
    let absent = fixture.connect_name("never-registered").unwrap_err();
    fixture::assert_semantic(absent, fixture::ErrorCode::ResourceNotFound);
    assert_eq!(manager.calls(), 2);
    assert_eq!(builder.calls(), 1);
    drop((manager_client, builder_client, manager, builder));
    fixture.finish();
}

// Gates 3 and 6: actual background SDK eviction, then simultaneous public
// acquisitions. Route/instance identity survives; one physical reconnect is
// shared by all eight callers. Waiting observes terminal state, not elapsed sleep.
#[test]
fn direct_idle_eviction_retains_route_and_coalesces_concurrent_reconnects() {
    sdk_idle(PathKind::Direct);
}

#[test]
fn relay_local_idle_eviction_retains_route_and_coalesces_concurrent_reconnects() {
    sdk_idle(PathKind::Local);
}

fn sdk_idle(path: PathKind) {
    let fixture = Fixture::new(path, 0.05, 0);
    let route = fixture.register("idle");
    let client = fixture.connect(&route);
    fixture.call(&client, &route, "before-eviction");
    let old = client.connection_observation().unwrap();
    assert!(old.snapshot().is_none());
    drop(client);
    observe_until(|| old.snapshot().is_some());
    assert!(!route.registration.is_closed());
    assert_eq!(route.calls(), 1);
    let barrier = Arc::new(Barrier::new(8));
    let clients = std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for _ in 0..8 {
            let runtime = fixture.client_runtime.clone();
            let expected = route.registration.outcome().route_name.clone();
            let mode = fixture.connect_mode();
            let barrier = barrier.clone();
            threads.push(scope.spawn(move || {
                barrier.wait();
                // Use the same public contract held by the initial client.
                let expected = fixture::expected(&expected);
                runtime.connect(expected, mode).unwrap()
            }));
        }
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    let new = clients[0].connection_observation().unwrap();
    assert_ne!(new.connection_id(), old.connection_id());
    for client in &clients {
        assert_eq!(
            client.connection_observation().unwrap().connection_id(),
            new.connection_id()
        );
        fixture.call(client, &route, "after-eviction");
    }
    assert_eq!(route.calls(), 9);
    assert_eq!(fixture.outgoing_ids().len(), 2);
    fixture.assert_paths(9);
    drop((clients, route));
    fixture.finish();
}

// Gate 2: an entered call exposes the deterministic closed interval.
// Acquisition and old-proxy calls consult current authority without entering
// the resource; after removal and re-registration the old UID is stale.
#[test]
fn direct_closed_removed_and_reregistered_routes_use_current_authority() {
    route_authority(PathKind::Direct);
}

#[test]
fn relay_local_closed_acquire_denies_same_endpoint_fallback_and_retains_ipc_cause() {
    route_authority(PathKind::Local);
}

#[test]
fn relay_http_closed_probe_rejects_acquire_and_preserves_entered_call() {
    route_authority(PathKind::Http);
}

fn route_authority(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let mut route = fixture.register("authority");
    let old = fixture.connect(&route);
    let entered_client = old.clone();
    let entered =
        std::thread::spawn(move || entered_client.call_owned("echo", &fixture::encode("blocked")));
    observe_until(|| route.calls() == 1);
    let concurrency = route.registration.route_concurrency();
    let evidence = route.evidence();
    // Registration cannot be moved out of the resource guard: it releases
    // the test barrier on unwind. The close thread borrows it in this scope.
    std::thread::scope(|scope| {
        // Release before scoped threads are joined on an assertion failure.
        let evidence = evidence;
        let closing = scope.spawn(|| route.registration.close().unwrap());
        observe_until(|| concurrency.snapshot().closed);
        let refusal = fixture.connect_name("authority").unwrap_err();
        if path == PathKind::Local {
            let c_two::Error::Semantic(error) = refusal else {
                panic!("{refusal:?}")
            };
            assert_eq!(error.code, fixture::ErrorCode::FallbackDenied);
            assert_eq!(
                error.details["direct_ipc_failure_kind"],
                "semantic:ResourceClosed"
            );
            assert_eq!(error.details["direct_ipc_failure_phase"], "pre_dispatch");
            assert_eq!(
                error.details["ipc_address"],
                fixture.host.local_endpoint().address()
            );
            let cause: serde_json::Value =
                serde_json::from_str(&error.details["direct_ipc_failure_envelope"]).unwrap();
            assert_eq!(cause["code"], 707);
            assert_eq!(cause["name"], "ResourceClosed");
            assert_eq!(
                cause["details"]["route_uid"],
                old.observed_route().route_uid
            );
            println!("C2_PERSISTENT_FALLBACK_DENIED {error:?}");
        } else {
            // A real relay probe withdraws a closed upstream route. Discovery
            // therefore reports ResourceNotFound after that withdrawal.
            fixture::assert_semantic(
                refusal,
                if path == PathKind::Http {
                    fixture::ErrorCode::ResourceNotFound
                } else {
                    fixture::ErrorCode::ResourceClosed
                },
            );
        }
        fixture::assert_semantic(
            old.call_owned("echo", &fixture::encode("closed"))
                .unwrap_err(),
            if path == PathKind::Http {
                fixture::ErrorCode::ResourceNotFound
            } else {
                fixture::ErrorCode::ResourceClosed
            },
        );
        assert_eq!(evidence.calls(), 1);
        evidence.release();
        assert!(
            entered.join().unwrap().is_ok(),
            "entered unary must complete"
        );
        let outcome = closing.join().unwrap();
        assert!(outcome.local_removed && outcome.close.active_drained);
        assert!(outcome.close.close_error.is_none());
    });
    if path != PathKind::Direct {
        assert_eq!(route.calls(), 1);
        fixture.assert_paths(1);
        assert_eq!(
            fixture.outgoing_ids().len(),
            usize::from(path == PathKind::Local)
        );
        drop((old, route));
        fixture.finish();
        return;
    }
    fixture::assert_semantic(
        fixture.connect_name("authority").unwrap_err(),
        fixture::ErrorCode::ResourceRemoved,
    );
    fixture::assert_semantic(
        old.call_owned("echo", &fixture::encode("removed"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceRemoved,
    );
    assert_eq!(route.calls(), 1);
    let replacement = fixture.register("authority");
    assert_ne!(
        route.registration.outcome().route_uid,
        replacement.registration.outcome().route_uid
    );
    let fresh = fixture.connect(&replacement);
    fixture::assert_semantic(
        old.call_owned("echo", &fixture::encode("stale"))
            .unwrap_err(),
        fixture::ErrorCode::RouteStale,
    );
    assert_eq!(replacement.calls(), 0);
    fixture.call(&fresh, &replacement, "fresh");
    assert_eq!(
        fixture.outgoing_ids().len(),
        usize::from(path != PathKind::Http)
    );
    drop((old, fresh, route, replacement));
    fixture.finish();
}
