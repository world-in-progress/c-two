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
// the resource; after removal and re-registration the old proxy reaches the
// current resource on the same endpoint.
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
            assert_eq!(cause["details"]["route"], "authority");
            assert!(cause["details"].get("route_uid").is_none());
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
    if path == PathKind::Http {
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
    // This new acquisition resolves the now absent route without first trying
    // a cached IPC candidate; the old proxy below still retains its carrier.
    fixture.client_runtime.clear_relay_projection_cache();
    fixture::assert_semantic(
        fixture.connect_name("authority").unwrap_err(),
        if path == PathKind::Direct {
            fixture::ErrorCode::ResourceRemoved
        } else {
            fixture::ErrorCode::ResourceNotFound
        },
    );
    fixture::assert_semantic(
        old.call_owned("echo", &fixture::encode("removed"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceRemoved,
    );
    assert_eq!(route.calls(), 1);
    let replacement = fixture.register("authority");
    assert_ne!(route.label(), replacement.label());
    let fresh = fixture.connect(&replacement);
    fixture.call(&old, &replacement, "old-proxy-current");
    fixture.call(&fresh, &replacement, "fresh");
    assert_eq!(replacement.calls(), 2);
    assert_eq!(
        fixture.outgoing_ids().len(),
        usize::from(path != PathKind::Http)
    );
    fixture.assert_paths(2);
    drop((old, fresh, route, replacement));
    fixture.finish();
}

// Gates 2 and 6, relay-local: after withdrawal, name acquisition finds no
// resource while the bound proxy reports removal. Re-registration changes
// the registration, and the old proxy reaches the current resource.
#[test]
fn relay_local_removed_and_reregistered_routes_preserve_old_binding() {
    let fixture = Fixture::new(PathKind::Local, 0.0, 0);
    let mut route = fixture.register("replacement");
    let old = fixture.connect(&route);
    fixture.call(&old, &route, "before-removal");
    let physical = fixture.outgoing_ids();
    let outcome = route.registration.close().unwrap();
    assert!(outcome.local_removed && outcome.close.active_drained);
    assert!(outcome.close.close_error.is_none());
    fixture.client_runtime.clear_relay_projection_cache();
    fixture::assert_semantic(
        fixture.connect_name("replacement").unwrap_err(),
        fixture::ErrorCode::ResourceNotFound,
    );
    fixture::assert_semantic(
        old.call_owned("echo", &fixture::encode("removed"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceRemoved,
    );
    assert_eq!(route.calls(), 1);
    let replacement = fixture.register("replacement");
    assert_ne!(route.label(), replacement.label());
    fixture.call(&old, &replacement, "old-proxy-current");
    let fresh = fixture.connect(&replacement);
    fixture.call(&fresh, &replacement, "fresh");
    assert_eq!(replacement.calls(), 2);
    assert_eq!(fixture.outgoing_ids(), physical);
    fixture.assert_paths(2);
    drop((old, fresh, route, replacement));
    fixture.finish();
}

#[test]
fn relay_local_first_acquire_uses_current_registration_with_cached_resolution() {
    let fixture = Fixture::new(PathKind::Local, 0.0, 0);
    let mut route = fixture.register("cached-resolution");
    let old = fixture.connect(&route);
    fixture.call(&old, &route, "cache-resolution");
    let physical = fixture.outgoing_ids();
    assert!(route.registration.close().unwrap().local_removed);
    let replacement = fixture.register("cached-resolution");
    assert_ne!(route.label(), replacement.label());
    let fresh = fixture.connect(&replacement);
    assert_eq!(fresh.observed_path(), fixture::PathKind::Local.observed());
    fixture.call(&fresh, &replacement, "new-acquisition");
    fixture.call(&old, &replacement, "old-acquisition");
    assert_eq!((route.calls(), replacement.calls()), (1, 2));
    assert_eq!(fixture.outgoing_ids(), physical);
    fixture.assert_paths(2);
    drop((old, fresh, route, replacement));
    fixture.finish();
}

#[test]
fn direct_real_process_same_endpoint_reregistration_uses_current_resource() {
    real_process_same_endpoint_reregistration(PathKind::Direct);
}

#[test]
fn relay_local_real_process_same_endpoint_reregistration_uses_current_resource() {
    real_process_same_endpoint_reregistration(PathKind::Local);
}

fn real_process_same_endpoint_reregistration(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let mut peer = fixture::ResourceProcess::start(&fixture, &fixture::unique("same-endpoint"));
    let old = peer.connect(&fixture);
    let old_connection = old.connection_observation().unwrap();
    let mut original = fixture::open_stream(&old, "complete");
    let first = original.next().unwrap().unwrap();
    peer.assert_item(&first, 1);
    assert!(original.next().is_none());
    let terminal = original.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    let old_label = peer.ready["resource"].clone();
    let before = peer.snapshot();
    let removed = peer.command_barrier("unregister", "unregistered");
    assert_eq!(removed["resource"], old_label);
    let refusal = old.call_owned("ping", &[]).unwrap_err();
    fixture::assert_semantic(refusal, fixture::ErrorCode::ResourceRemoved);
    let acquire = fixture
        .client_runtime
        .connect(
            fixture::stream_expected("process-route"),
            if path == PathKind::Direct {
                c_two::Connect::DirectIpc {
                    address: peer.ready["address"].as_str().unwrap().into(),
                }
            } else {
                c_two::Connect::RelayAware
            },
        )
        .unwrap_err();
    fixture::assert_semantic(
        acquire,
        if path == PathKind::Direct {
            fixture::ErrorCode::ResourceRemoved
        } else {
            fixture::ErrorCode::ResourceNotFound
        },
    );
    assert_eq!(peer.snapshot()["opens"], before["opens"]);
    let registered = peer.command_barrier("register", "registered");
    assert_ne!(registered["resource"], old_label);
    peer.ready["resource"] = registered["resource"].clone();
    let fresh = peer.connect(&fixture);
    assert_eq!(
        fresh.connection_observation().unwrap().connection_id(),
        old_connection.connection_id()
    );
    let mut current = fixture::open_stream(&old, "complete");
    let item = current.next().unwrap().unwrap();
    peer.assert_item(&item, 1);
    assert!(current.next().is_none());
    let terminal = current.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    let mut acquired = fixture::open_stream(&fresh, "complete");
    let item = acquired.next().unwrap().unwrap();
    peer.assert_item(&item, 1);
    assert!(acquired.next().is_none());
    let terminal = acquired.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    assert_eq!(peer.snapshot()["opens"], 3);
    fixture.assert_paths(2);
    drop((original, current, acquired, first, item, old, fresh));
    peer.stop();
    fixture.finish();
}

// Gate 4, direct IPC and relay-local: A and B share the physical connection.
// Closing and removing B refuses only B; replacement reaches the current B.
// A's dispatched unary, held response, and carrier survive these changes.
#[test]
fn direct_route_errors_preserve_other_route_inflight_call_and_carrier() {
    route_errors_are_local(PathKind::Direct);
}

#[test]
fn relay_local_route_errors_preserve_other_route_inflight_call_and_carrier() {
    route_errors_are_local(PathKind::Local);
}

fn route_errors_are_local(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let a = fixture.register("a");
    let mut b = fixture.register("b");
    let a_client = fixture.connect(&a);
    let b_client = fixture.connect(&b);
    let mut held = a_client
        .call_held("echo", &fixture::encode("held-before-b"))
        .unwrap();
    let a_entered = a_client.clone();
    let a_call =
        std::thread::spawn(move || a_entered.call_owned("echo", &fixture::encode("blocked")));
    let b_entered = b_client.clone();
    let b_call =
        std::thread::spawn(move || b_entered.call_owned("echo", &fixture::encode("blocked")));
    observe_until(|| a.calls() == 2 && b.calls() == 1);
    let physical = fixture.outgoing_ids();
    assert_eq!(physical.len(), 1);
    let carrier = a_client.connection_observation().unwrap();
    assert_eq!(
        carrier.connection_id(),
        b_client.connection_observation().unwrap().connection_id()
    );
    let concurrency = b.registration.route_concurrency();
    let evidence = b.evidence();
    let b_response = std::thread::scope(|scope| {
        let evidence = evidence;
        let closing = scope.spawn(|| b.registration.close().unwrap());
        observe_until(|| concurrency.snapshot().closed);
        fixture::assert_semantic(
            b_client
                .call_owned("echo", &fixture::encode("closed-b"))
                .unwrap_err(),
            fixture::ErrorCode::ResourceClosed,
        );
        assert_eq!(evidence.calls(), 1);
        assert!(!a_call.is_finished());
        assert!(carrier.snapshot().is_none());
        evidence.release();
        let response = b_call.join().unwrap().unwrap();
        let outcome = closing.join().unwrap();
        assert!(outcome.local_removed && outcome.close.active_drained);
        assert!(outcome.close.close_error.is_none());
        response
    });
    fixture.assert_response(&b_client, &b, "blocked", &b_response);
    fixture::assert_semantic(
        b_client
            .call_owned("echo", &fixture::encode("removed-b"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceRemoved,
    );
    let replacement = fixture.register("b");
    fixture.call(&b_client, &replacement, "current-b");
    assert_eq!(replacement.calls(), 1);
    fixture.assert_response(&a_client, &a, "held-before-b", held.bytes());
    held.invalidate_then_release(|| Ok(())).unwrap();
    assert!(!a_call.is_finished());
    assert!(carrier.snapshot().is_none());
    a.evidence().release();
    fixture.assert_response(&a_client, &a, "blocked", &a_call.join().unwrap().unwrap());
    fixture.call(&a_client, &a, "a-after-b-errors");
    assert_eq!((a.calls(), b.calls(), replacement.calls()), (3, 1, 1));
    assert_eq!(fixture.outgoing_ids(), physical);
    fixture.assert_paths(2);
    drop((a_client, b_client, a, b, replacement));
    fixture.finish();
}

// Gate 2, HTTP removal: both a name acquire and the cached proxy are refused;
// no callback runs after relay withdrawal.
#[test]
fn relay_http_removed_route_refuses_acquire_and_old_call() {
    let fixture = Fixture::new(PathKind::Http, 0.0, 0);
    let mut route = fixture.register("removed-http");
    let old = fixture.connect(&route);
    fixture.call(&old, &route, "before-removal");
    let outcome = route.registration.close().unwrap();
    assert!(outcome.local_removed && outcome.close.active_drained);
    assert!(outcome.close.close_error.is_none());
    fixture::assert_semantic(
        old.call_owned("echo", &fixture::encode("removed"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceNotFound,
    );
    fixture::assert_semantic(
        fixture.connect_name("removed-http").unwrap_err(),
        fixture::ErrorCode::ResourceNotFound,
    );
    assert_eq!(route.calls(), 1);
    assert!(fixture.outgoing_ids().is_empty());
    fixture.assert_paths(1);
    drop((old, route));
    fixture.finish();
}

// Gates 2 and 6, HTTP replacement: an old proxy reaches the current registration
// even when its shared discovery cache still holds the previous route metadata.
#[test]
fn relay_http_reregistered_route_accepts_old_cached_proxy() {
    let fixture = Fixture::with_c3();
    let mut route = fixture.register("stale-http");
    let old = fixture.connect(&route);
    fixture.call(&old, &route, "before-replacement");
    assert!(route.registration.close().unwrap().local_removed);
    let replacement = fixture.register("stale-http");
    fixture.call(&old, &replacement, "old-cached-proxy");
    assert_eq!((route.calls(), replacement.calls()), (1, 1));
    let fresh = fixture.connect(&replacement);
    fixture.call(&fresh, &replacement, "new-acquire");
    assert_eq!((route.calls(), replacement.calls()), (1, 2));
    assert!(fixture.outgoing_ids().is_empty());
    fixture.assert_paths(2);
    drop((old, fresh, route, replacement));
    fixture.finish();
}

#[test]
fn relay_http_other_acquire_refreshes_shared_cache_before_old_proxy_call() {
    let fixture = Fixture::with_c3();
    let mut route = fixture.register("shared-cache-http");
    let old = fixture.connect(&route);
    fixture.call(&old, &route, "before-replacement");
    assert!(route.registration.close().unwrap().local_removed);
    fixture::assert_semantic(
        fixture.connect_name("shared-cache-http").unwrap_err(),
        fixture::ErrorCode::ResourceNotFound,
    );
    let replacement = fixture.register("shared-cache-http");
    let other = fixture.connect(&replacement);
    fixture.call(&old, &replacement, "old-after-other-acquire");
    fixture.call(&other, &replacement, "other-after-refresh");
    assert_eq!((route.calls(), replacement.calls()), (1, 2));
    assert!(fixture.outgoing_ids().is_empty());
    fixture.assert_paths(2);
    drop((old, other, route, replacement));
    fixture.finish();
}

// Gate 3, HTTP: observe the relay's actual background idle eviction, then
// eight simultaneous callers. The upstream control remains live, the route
// UID and instance are unchanged, and only one new data carrier is opened.
#[test]
fn relay_http_idle_eviction_retains_route_and_coalesces_concurrent_reconnects() {
    use c_two::observe::ConnectionEvent;
    let fixture = Fixture::new(PathKind::Http, 0.0, 1);
    let route = fixture.register("idle-http");
    let client = fixture.connect(&route);
    fixture.call(&client, &route, "before-eviction");
    observe_until(|| fixture.live_accepted().len() == 2);
    let original = fixture.live_accepted();
    observe_until(|| {
        original
            .iter()
            .filter(|connection| connection.snapshot().is_none())
            .count()
            == 1
    });
    let evicted = original
        .iter()
        .find(|connection| connection.snapshot().is_some())
        .unwrap();
    let control = original
        .iter()
        .find(|connection| connection.snapshot().is_none())
        .unwrap();
    assert!(!route.registration.is_closed());
    assert_eq!(route.calls(), 1);
    let prior = fixture.accepted_events().len();
    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        let mut calls = Vec::new();
        for _ in 0..8 {
            let barrier = barrier.clone();
            let client = client.clone();
            let fixture = &fixture;
            let route = &route;
            calls.push(scope.spawn(move || {
                barrier.wait();
                fixture.call(&client, route, "after-eviction");
            }));
        }
        for call in calls {
            call.join().unwrap();
        }
    });
    let events = fixture.accepted_events();
    let new_connections = events[prior..]
        .iter()
        .filter_map(|event| match event {
            ConnectionEvent::Opened(observation) => Some(observation.connection_id()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(new_connections.len(), 1);
    assert!(control.snapshot().is_none());
    assert_eq!(route.calls(), 9);
    assert!(fixture.outgoing_ids().is_empty());
    fixture.assert_paths(1);
    println!(
        "C2_PERSISTENT_RELAY_EVICTION old={} control={} new={new_connections:?}",
        evicted.connection_id(),
        control.connection_id()
    );
    drop((client, route));
    fixture.finish();
}

// Exact subprocess entry; an ordinary suite invocation returns before
// opening any endpoint because no child configuration is present.
#[test]
fn persistent_resource_process() {
    fixture::run_resource_process();
}

// Gates 3, 4, 5 and 6: true peer end before CALL_END is dispatch-uncertain.
// The delivered item stays valid, no second open is made, and the broken
// carrier closes. A fresh acquire after restart selects the current instance
// before business dispatch. The relay restores authority after its old control
// connection ends; concurrent retired registration/withdrawal evidence cannot
// remove or deactivate its new owner. Rust streams over HTTP as well.
#[test]
fn direct_peer_end_preserves_delivered_item_and_recovers_only_new_acquire() {
    peer_end_and_recovery(PathKind::Direct);
}
#[test]
fn relay_local_peer_end_preserves_delivered_item_and_recovers_only_new_acquire() {
    peer_end_and_recovery(PathKind::Local);
}
#[test]
fn relay_http_peer_end_preserves_delivered_item_and_recovers_only_new_acquire() {
    peer_end_and_recovery(PathKind::Http);
}
fn peer_end_and_recovery(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let id = fixture::unique("peer-end");
    let mut peer = fixture::ResourceProcess::start(&fixture, &id);
    let old = peer.connect(&fixture);
    let mut stream = fixture::open_stream(&old, "blocked");
    let first = stream.next().unwrap().unwrap();
    peer.assert_item(&first, 1);
    observe_until(|| peer.records("step").iter().any(|event| event["index"] == 1));
    let before = peer.snapshot();
    assert_eq!(before["opens"], 1);
    assert_eq!(before["finishes"], 0);
    let physical = fixture.outgoing_ids();
    let old_connection = (path != PathKind::Http).then(|| old.connection_observation().unwrap());
    peer.kill();
    fixture::assert_uncertain(
        stream
            .next()
            .expect("missing terminal is an error")
            .unwrap_err(),
    );
    assert!(stream.next().is_none());
    let receipt = stream.close_with_timeout(Some(fixture::WAIT));
    assert!(receipt.completed);
    fixture::assert_uncertain(receipt.terminal.unwrap().unwrap_err());
    peer.assert_item(&first, 1);
    assert_eq!(peer.records("open").len(), 1);
    assert_eq!(
        fixture.outgoing_ids(),
        physical,
        "dispatched stream must not reconnect or replay"
    );
    if let Some(connection) = &old_connection {
        observe_until(|| connection.snapshot().is_some());
        assert!(connection.snapshot().unwrap().is_peer_end());
    }
    let mut replacement = fixture::ResourceProcess::start(&fixture, &id);
    assert_eq!(
        peer.ready["identity"]["server_id"],
        replacement.ready["identity"]["server_id"]
    );
    assert_ne!(
        peer.ready["identity"]["server_instance_id"],
        replacement.ready["identity"]["server_instance_id"]
    );
    assert_ne!(peer.ready["resource"], replacement.ready["resource"]);
    assert_eq!(replacement.snapshot()["opens"], 0);
    if path == PathKind::Http {
        old.call_owned("ping", &[]).unwrap();
    } else {
        let refusal = old.call_owned("ping", &[]).unwrap_err();
        println!("C2_PERSISTENT_OLD_PROXY_REFUSAL {refusal:?}");
    }
    assert_eq!(
        replacement.snapshot()["unary"],
        if path == PathKind::Http { 1 } else { 0 }
    );
    let fresh = replacement.connect(&fixture);
    if let Some(identity) = old.ipc_server_identity() {
        assert_eq!(
            identity.server_instance_id,
            peer.ready["identity"]["server_instance_id"]
        );
    }
    assert_eq!(
        replacement.snapshot()["opens"],
        0,
        "acquisition cannot dispatch the stream"
    );
    let mut recovered = fixture::open_stream(&fresh, "blocked");
    let item = recovered.next().unwrap().unwrap();
    replacement.assert_item(&item, 1);
    observe_until(|| {
        replacement
            .records("step")
            .iter()
            .any(|event| event["index"] == 1)
    });
    let carrier = (path != PathKind::Http).then(|| fresh.connection_observation().unwrap());
    fixture::race_old_owner_evidence(&fixture, &peer);
    let racing = replacement.snapshot();
    assert_eq!(racing["opens"], 1);
    assert_eq!(racing["finishes"], 0);
    assert_eq!(racing["unary"], if path == PathKind::Http { 1 } else { 0 });
    if let Some(carrier) = &carrier {
        assert!(carrier.snapshot().is_none());
    }
    replacement.send("release");
    let second = recovered.next().unwrap().unwrap();
    replacement.assert_item(&second, 2);
    assert!(recovered.next().is_none());
    let terminal = recovered.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    assert_eq!(terminal.complete_items, Some(2));
    let after = replacement.snapshot();
    assert_eq!(after["opens"], 1);
    assert_eq!(after["finishes"], 1);
    fresh.call_owned("ping", &[]).unwrap();
    let healthy = replacement.snapshot();
    assert_eq!(healthy["unary"], if path == PathKind::Http { 2 } else { 1 });
    assert_eq!(
        healthy["physical_opened"], after["physical_opened"],
        "old evidence must not force a new data carrier"
    );
    if let Some(carrier) = &carrier {
        assert!(carrier.snapshot().is_none());
    }
    peer.assert_item(&first, 1);
    assert_eq!(
        fixture.outgoing_ids().len(),
        if path == PathKind::Http { 0 } else { 2 }
    );
    fixture.assert_paths(2);
    drop((stream, recovered, first, item, second, old, fresh));
    replacement.stop();
    fixture.finish();
}

// Gate 4, HTTP: one upstream carrier serves A and the other routes. Closed and
// removed refusals preserve A; replacement serves B without disturbing A.
#[test]
fn relay_http_route_errors_preserve_other_route_inflight_call_and_carrier() {
    use c_two::observe::ConnectionEvent;
    let fixture = Fixture::new(PathKind::Http, 0.0, 0);
    let a = fixture.register("a-http");
    let mut b = fixture.register("closed-b-http");
    let mut token = fixture.register("stale-b-http");
    let a_client = fixture.connect(&a);
    let b_client = fixture.connect(&b);
    let token_client = fixture.connect(&token);
    fixture.call(&token_client, &token, "cache-business-token");
    let entered = a_client.clone();
    let a_call =
        std::thread::spawn(move || entered.call_owned("echo", &fixture::encode("blocked")));
    let entered = b_client.clone();
    let b_call =
        std::thread::spawn(move || entered.call_owned("echo", &fixture::encode("blocked")));
    observe_until(|| a.calls() == 1 && b.calls() == 1);
    observe_until(|| fixture.live_accepted().len() == 2);
    let carriers = fixture.live_accepted();
    let concurrency = b.registration.route_concurrency();
    let evidence = b.evidence();
    let b_response = std::thread::scope(|scope| {
        let evidence = evidence;
        let closing = scope.spawn(|| b.registration.close().unwrap());
        observe_until(|| concurrency.snapshot().closed);
        fixture::assert_semantic(
            fixture.connect_name("closed-b-http").unwrap_err(),
            fixture::ErrorCode::ResourceNotFound,
        );
        fixture::assert_semantic(
            b_client
                .call_owned("echo", &fixture::encode("closed"))
                .unwrap_err(),
            fixture::ErrorCode::ResourceNotFound,
        );
        assert!(!a_call.is_finished());
        assert!(carriers.iter().all(|carrier| carrier.snapshot().is_none()));
        evidence.release();
        let response = b_call.join().unwrap().unwrap();
        let outcome = closing.join().unwrap();
        assert!(outcome.local_removed && outcome.close.active_drained);
        assert!(outcome.close.close_error.is_none());
        response
    });
    fixture.assert_response(&b_client, &b, "blocked", &b_response);
    fixture::assert_semantic(
        b_client
            .call_owned("echo", &fixture::encode("removed"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceNotFound,
    );
    assert!(token.registration.close().unwrap().local_removed);
    let replacement = fixture.register("stale-b-http");
    fixture.call(&token_client, &replacement, "replaced");
    assert_eq!(replacement.calls(), 1);
    assert!(!a_call.is_finished());
    assert!(carriers.iter().all(|carrier| carrier.snapshot().is_none()));
    a.evidence().release();
    fixture.assert_response(&a_client, &a, "blocked", &a_call.join().unwrap().unwrap());
    let before = fixture.accepted_events().len();
    fixture.call(&a_client, &a, "after-b-refusals");
    let fresh = fixture.connect(&replacement);
    fixture.call(&fresh, &replacement, "fresh-b");
    let events = fixture.accepted_events();
    assert!(
        !events[before..]
            .iter()
            .any(|event| matches!(event, ConnectionEvent::Opened(_)))
    );
    assert!(carriers.iter().all(|carrier| carrier.snapshot().is_none()));
    assert_eq!(
        (a.calls(), b.calls(), token.calls(), replacement.calls()),
        (2, 1, 1, 2)
    );
    assert!(fixture.outgoing_ids().is_empty());
    fixture.assert_paths(4);
    println!(
        "C2_PERSISTENT_HTTP_SHARED_CARRIERS {:?}",
        carriers
            .iter()
            .map(|carrier| carrier.connection_id())
            .collect::<Vec<_>>()
    );
    drop((
        a_client,
        b_client,
        token_client,
        fresh,
        a,
        b,
        token,
        replacement,
    ));
    fixture.finish();
}

// Gate 6: a resource error is a known terminal after dispatch. It is returned
// once, without replay or fallback; an explicit next call uses the same route.
#[test]
fn direct_resource_error_is_terminal_without_replay_or_reconnect() {
    resource_error(PathKind::Direct);
}
#[test]
fn relay_local_resource_error_is_terminal_without_replay_or_reconnect() {
    resource_error(PathKind::Local);
}
#[test]
fn relay_http_resource_error_is_terminal_without_replay_or_reconnect() {
    resource_error(PathKind::Http);
}
fn resource_error(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let route = fixture.register("service-error");
    let client = fixture.connect(&route);
    let physical = fixture.outgoing_ids();
    fixture::assert_semantic(
        client
            .call_owned("echo", &fixture::encode("fail"))
            .unwrap_err(),
        fixture::ErrorCode::ResourceFunctionExecuting,
    );
    assert_eq!(route.calls(), 1);
    fixture.call(&client, &route, "caller-next-call");
    assert_eq!(route.calls(), 2);
    assert_eq!(fixture.outgoing_ids(), physical);
    assert_eq!(physical.len(), usize::from(path != PathKind::Http));
    fixture.assert_paths(1);
    drop((client, route));
    fixture.finish();
}

// Gate 6 and streaming design section 8: once Success or a resource Failed
// terminal is known, a later real EOF cannot replace it with uncertainty.
// Both complete payloads remain readable after their resource process ends.
#[test]
fn direct_known_terminals_and_items_survive_later_peer_end() {
    known_terminal(PathKind::Direct);
}
#[test]
fn relay_local_known_terminals_and_items_survive_later_peer_end() {
    known_terminal(PathKind::Local);
}
#[test]
fn relay_http_known_terminals_and_items_survive_later_peer_end() {
    known_terminal(PathKind::Http);
}
fn known_terminal(path: PathKind) {
    let fixture = Fixture::new(path, 0.0, 0);
    let mut peer = fixture::ResourceProcess::start(&fixture, &fixture::unique("known-end"));
    let client = peer.connect(&fixture);
    let mut success = fixture::open_stream(&client, "complete");
    let item = success.next().unwrap().unwrap();
    peer.assert_item(&item, 1);
    assert!(success.next().is_none());
    let terminal = success.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    assert_eq!(terminal.complete_items, Some(1));
    let mut failed = fixture::open_stream(&client, "fail");
    let failed_item = failed.next().unwrap().unwrap();
    peer.assert_item(&failed_item, 1);
    fixture::assert_semantic(
        failed.next().unwrap().unwrap_err(),
        fixture::ErrorCode::ResourceFunctionExecuting,
    );
    assert!(failed.next().is_none());
    let terminal = failed.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed);
    fixture::assert_semantic(
        terminal.terminal.unwrap().unwrap_err(),
        fixture::ErrorCode::ResourceFunctionExecuting,
    );
    assert_eq!(terminal.complete_items, Some(1));
    let before = peer.snapshot();
    assert_eq!(before["opens"], 2);
    assert_eq!(before["finishes"], 2);
    let physical = fixture.outgoing_ids();
    let connection = (path != PathKind::Http).then(|| client.connection_observation().unwrap());
    peer.kill();
    if let Some(connection) = connection {
        observe_until(|| connection.snapshot().is_some());
        assert!(connection.snapshot().unwrap().is_peer_end());
    }
    let terminal = success.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed && terminal.terminal.unwrap().is_ok());
    assert_eq!(terminal.complete_items, Some(1));
    let terminal = failed.close_with_timeout(Some(fixture::WAIT));
    assert!(terminal.completed);
    fixture::assert_semantic(
        terminal.terminal.unwrap().unwrap_err(),
        fixture::ErrorCode::ResourceFunctionExecuting,
    );
    assert_eq!(terminal.complete_items, Some(1));
    assert!(success.next().is_none() && failed.next().is_none());
    peer.assert_item(&item, 1);
    peer.assert_item(&failed_item, 1);
    assert_eq!(peer.records("open").len(), 2);
    assert_eq!(fixture.outgoing_ids(), physical);
    assert_eq!(physical.len(), usize::from(path != PathKind::Http));
    fixture.assert_paths(1);
    drop((success, failed, item, failed_item, client));
    fixture.finish();
}
