use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallOutcomeStatus, CallTimeout, Client, ConfigField, Connect, DomainLimits,
    DomainLimitsOverrides, EncodedClient, EncodedService, Error, Host, HostOptions,
    MethodDefinition, ResultUnavailableReason, Runtime, RuntimeOptions, ServerConfigPatch,
    ServiceDefinition, TransportPhase, UnaryCallOptions,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::relay::{RelayConfig, RelayServer};

const WAIT: Duration = Duration::from_secs(5);
const TIMEOUT: CallTimeout = CallTimeout::After(WAIT);
const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

struct Resource<F>(F);

impl<F> EncodedService for Resource<F>
where
    F: Fn(u16, Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
{
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        (self.0)(index, request)
    }
}

fn runtime(client_slots: u64) -> Runtime {
    Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        client_limits: DomainLimitsOverrides {
            max_outstanding_calls: Some(client_slots),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap()
}

fn relay(runtime: &Runtime, slots: u64) -> (RelayServer, String) {
    let relay = RelayServer::start_resolved(
        RelayConfig {
            bind: "127.0.0.1:0".into(),
            domain_limits: DomainLimits {
                max_outstanding_calls: slots,
                ..Default::default()
            },
            ..Default::default()
        },
        runtime.local_endpoint_context().unwrap(),
    )
    .unwrap();
    let url = format!("http://{}", relay.local_addr());
    (relay, url)
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap()
}

fn register(
    host: &Host,
    route: &str,
    f: impl Fn(u16, Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
) -> c2_core::Registration {
    let release = release();
    host.register(
        ServiceDefinition::new(
            &release,
            release.reference(),
            route,
            [
                MethodDefinition {
                    index: 0,
                    name: "ping".into(),
                    access: MethodAccess::Read,
                },
                MethodDefinition {
                    index: 1,
                    name: "echo".into(),
                    access: MethodAccess::Write,
                },
            ],
            Arc::new(Resource(f)),
        )
        .unwrap(),
    )
    .unwrap()
}

fn connect(runtime: &Runtime, url: &str, route: &str) -> Client {
    runtime
        .connect(
            release().expected_route(route).unwrap(),
            Connect::ExplicitRelay {
                relay_url: url.into(),
            },
        )
        .unwrap()
}

fn connect_ipc(runtime: &Runtime, host: &Host, route: &str) -> Client {
    runtime
        .connect(
            release().expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_owned(),
            },
        )
        .unwrap()
}

fn keyed(id: &str, timeout: CallTimeout) -> UnaryCallOptions {
    UnaryCallOptions::new()
        .with_timeout(timeout)
        .with_call_id(id)
        .unwrap()
}

fn assert_semantic(error: Error, code: ErrorCode, phase: Option<TransportPhase>) -> C2Error {
    assert_eq!(error.transport_phase(), phase, "{error:?}");
    let Error::Semantic(error) = error else {
        panic!("expected {code:?}, got {error:?}");
    };
    assert_eq!(error.code, code);
    error
}

#[test]
fn relay_outcomes_round_trip_route_names_that_cannot_be_url_segments() {
    let runtime = runtime(16);
    let (mut relay, url) = relay(&runtime, 16);
    let host = runtime
        .host(HostOptions::default().with_relay_anchor_address(&url))
        .unwrap();
    for route in [".", "..", "_resolve", "_probe", "a/b", "测试/网格"] {
        let prefix = route.as_bytes().to_vec();
        let mut registration = register(&host, route, move |_, input| {
            Ok([prefix.as_slice(), &input].concat())
        });
        let client = connect(&runtime, &url, route);
        let expected = [route.as_bytes(), b"payload"].concat();
        assert_eq!(
            client
                .call_unary_with_options("echo", b"payload", keyed("same-key", TIMEOUT))
                .unwrap(),
            expected
        );
        let outcome = client.query_call("same-key", TIMEOUT).unwrap();
        assert_eq!(outcome.status, CallOutcomeStatus::Succeeded, "{route}");
        assert_eq!(outcome.method_name.as_deref(), Some("echo"));
        assert_eq!(
            client
                .recover_unary_call_held("echo", "same-key", TIMEOUT)
                .unwrap()
                .bytes(),
            expected
        );
        registration.close().unwrap();
    }
    host.shutdown();
    relay.stop().unwrap();
}

#[test]
fn outcome_query_bypasses_all_three_busy_unary_slots() {
    let runtime = runtime(1);
    let (mut relay, url) = relay(&runtime, 1);
    let host = runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(&url)
                .with_max_outstanding_calls(1),
        )
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let mut registration = register(&host, "busy-write", move |_, input| {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        Ok(input)
    });
    let client = connect(&runtime, &url, "busy-write");
    let payload: Vec<u8> = (0..300_000).map(|offset| (offset % 251) as u8).collect();
    let original = std::thread::spawn({
        let client = client.clone();
        let payload = payload.clone();
        move || {
            client.call_unary_with_options("echo", &payload, keyed("busy", CallTimeout::Unlimited))
        }
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    let client_slots = runtime.call_execution_snapshot().client;
    let server_slots = host.call_execution_snapshot().server;
    assert_eq!(
        (client_slots.used_operations, client_slots.max_operations),
        (Some(1), Some(1))
    );
    assert_eq!(
        (server_slots.used_operations, server_slots.max_operations),
        (Some(1), Some(1))
    );
    let relay_slots = relay.domain_snapshot().operations;
    assert_eq!(
        (relay_slots.used_operations, relay_slots.max_operations),
        (1, 1)
    );

    let inflight = client.query_call("busy", TIMEOUT);
    release_tx.send(()).unwrap();
    assert_eq!(inflight.unwrap().status, CallOutcomeStatus::InFlight);
    assert_eq!(original.join().unwrap().unwrap(), payload);
    let outcome = client.query_call("busy", TIMEOUT).unwrap();
    assert_eq!(outcome.status, CallOutcomeStatus::Succeeded);
    assert!(outcome.result_available);
    assert_eq!(
        client.recover_unary_call("echo", "busy", TIMEOUT).unwrap(),
        payload
    );
    registration.close().unwrap();
    host.shutdown();
    relay.stop().unwrap();
}

#[test]
fn nested_result_unavailable_is_recorded_as_outer_resource_failure() {
    verify_nested_result_unavailable(true);
}

#[test]
fn ipc_nested_result_unavailable_is_recorded_as_outer_resource_failure() {
    verify_nested_result_unavailable(false);
}

fn verify_nested_result_unavailable(over_http: bool) {
    let runtime_b = runtime(16);
    let mut relay = over_http.then(|| relay(&runtime_b, 16));
    let host_options = || match &relay {
        Some((_, url)) => HostOptions::default().with_relay_anchor_address(url),
        None => HostOptions::default().without_relay(),
    };
    let connect_route = |runtime: &Runtime, host: &Host, route: &str| match &relay {
        Some((_, url)) => connect(runtime, url, route),
        None => connect_ipc(runtime, host, route),
    };
    runtime_b
        .set_server(ServerConfigPatch {
            outcome_max_entries: ConfigField::Set(3),
            ..Default::default()
        })
        .unwrap();
    let host_b = runtime_b
        .host(host_options().with_outcome_max_retained_bytes(0))
        .unwrap();
    let config = runtime_b.server_config().unwrap().ipc;
    assert_eq!(config.outcome_max_entries, 3);
    assert_eq!(config.outcome_max_retained_bytes, Some(0));
    let mut registration_b = register(&host_b, "nested-b", |_, input| Ok(input));
    let client_b = connect_route(&runtime_b, &host_b, "nested-b");
    assert_eq!(
        client_b
            .call_unary_with_options("echo", b"done", keyed("b-key", TIMEOUT))
            .unwrap(),
        b"done"
    );
    let b_outcome = client_b.query_call("b-key", TIMEOUT).unwrap();
    assert_eq!(b_outcome.status, CallOutcomeStatus::Succeeded);
    assert_eq!(
        b_outcome.result_unavailable_reason,
        Some(ResultUnavailableReason::NotRetained)
    );

    let runtime_a = runtime(16);
    let host_a = runtime_a.host(host_options()).unwrap();
    let mut registration_a = register(&host_a, "nested-a", move |_, _| {
        match client_b.recover_unary_call("echo", "b-key", TIMEOUT) {
            Err(Error::Semantic(error)) => Err(error),
            other => panic!("expected B to report 722, got {other:?}"),
        }
    });
    let client_a = connect_route(&runtime_a, &host_a, "nested-a");
    let error = client_a
        .call_unary_with_options("echo", b"a", keyed("a-key", TIMEOUT))
        .unwrap_err();
    let error = assert_semantic(error, ErrorCode::ResourceFunctionExecuting, None);
    assert_eq!(error.details["cause_code"], "722");
    assert!(!error.details.contains_key("outcome"));
    assert!(!error.details.contains_key("reason"));
    let outcome = client_a.query_call("a-key", TIMEOUT).unwrap();
    assert_eq!(outcome.status, CallOutcomeStatus::Failed);
    assert_eq!(
        outcome.error_code,
        Some(ErrorCode::ResourceFunctionExecuting)
    );
    assert!(outcome.error_details_available);
    assert_eq!(
        assert_semantic(
            client_a
                .recover_unary_call("echo", "a-key", TIMEOUT)
                .unwrap_err(),
            ErrorCode::ResourceFunctionExecuting,
            None
        ),
        error,
    );

    registration_a.close().unwrap();
    host_a.shutdown();
    registration_b.close().unwrap();
    host_b.shutdown();
    if let Some((relay, _)) = &mut relay {
        relay.stop().unwrap();
    }
}

#[test]
fn relay_outcome_errors_do_not_reacquire_and_uncertain_waits_report_the_key() {
    let runtime = runtime(16);
    let (mut relay, url) = relay(&runtime, 16);
    let host = runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(&url)
                .with_outcome_max_retained_bytes(0),
        )
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let mut registration = register(&host, "healthy", move |_, input| {
        if input == b"stall" {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        Ok(input)
    });
    let client = connect(&runtime, &url, "healthy");
    let lookups = relay.route_lookup_count_for_test();
    let counters = runtime.path_counters();
    assert_eq!(
        client
            .call_unary_with_options("echo", b"first", keyed("existing", TIMEOUT))
            .unwrap(),
        b"first"
    );
    let pre_dispatch = Some(TransportPhase::PreDispatch);
    assert_semantic(
        client
            .call_unary_with_options("echo", b"different", keyed("existing", TIMEOUT))
            .unwrap_err(),
        ErrorCode::CallIdConflict,
        pre_dispatch,
    );
    assert_semantic(
        client
            .recover_unary_call("echo", "existing", TIMEOUT)
            .unwrap_err(),
        ErrorCode::CallResultUnavailable,
        None,
    );
    assert_semantic(
        client
            .recover_unary_call("echo", "missing", TIMEOUT)
            .unwrap_err(),
        ErrorCode::CallOutcomeUnknown,
        pre_dispatch,
    );
    assert_eq!(client.call_owned("echo", b"healthy").unwrap(), b"healthy");
    assert_eq!(relay.route_lookup_count_for_test(), lookups);
    assert_eq!(runtime.path_counters(), counters);

    let original = std::thread::spawn({
        let client = client.clone();
        move || {
            client.call_unary_with_options(
                "echo",
                b"stall",
                keyed("timed-out", CallTimeout::After(Duration::from_secs(1))),
            )
        }
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    let error = original.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    assert_eq!(
        error.transport_phase(),
        Some(TransportPhase::DispatchUncertain)
    );
    assert_eq!(error.call_id(), Some("timed-out"));

    registration.close().unwrap();
    host.shutdown();
    relay.stop().unwrap();
}

#[test]
fn direct_ipc_outcomes_share_the_existing_connection_and_control_capacity() {
    verify_ipc_outcomes(false);
}

#[test]
fn same_runtime_ipc_outcomes_share_the_existing_connection_and_control_capacity() {
    verify_ipc_outcomes(true);
}

fn verify_ipc_outcomes(same_runtime: bool) {
    let host_runtime = runtime(1);
    let mut relay = same_runtime.then(|| relay(&host_runtime, 1));
    let host_options = match &relay {
        Some((_, url)) => HostOptions::default().with_relay_anchor_address(url),
        None => HostOptions::default().without_relay(),
    };
    let host = host_runtime
        .host(
            host_options
                .with_outcome_max_retained_bytes(32 * 1024)
                .with_max_outstanding_calls(1),
        )
        .unwrap();
    let caller = if same_runtime {
        host_runtime.clone()
    } else {
        let caller = runtime(1);
        caller
            .inherit_local_endpoint_selection(&host_runtime)
            .unwrap();
        caller
    };
    let invocations = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let mut registration = register(&host, "ipc-outcomes", {
        let invocations = invocations.clone();
        move |_, input| {
            invocations.fetch_add(1, Ordering::SeqCst);
            if input == b"busy" {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv_timeout(WAIT * 2).unwrap();
            }
            Ok(input)
        }
    });
    let client = match &relay {
        Some((_, url)) => caller
            .connect(
                release().expected_route("ipc-outcomes").unwrap(),
                Connect::RelayAwareAt {
                    anchor_url: url.clone(),
                },
            )
            .unwrap(),
        None => connect_ipc(&caller, &host, "ipc-outcomes"),
    };
    assert_eq!(
        client.observed_path(),
        if same_runtime {
            c2_core::ObservedPath::RelayAwareLocalIpc
        } else {
            c2_core::ObservedPath::DirectIpc
        }
    );
    let observation = client.connection_observation().unwrap();
    let counters = caller.path_counters();
    let keyed_reply =
        |id, payload: &[u8]| client.call_unary_with_options("echo", payload, keyed(id, TIMEOUT));
    assert_eq!(keyed_reply("stable", b"one").unwrap(), b"one");
    assert_eq!(keyed_reply("stable", b"one").unwrap(), b"one");
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let pre_dispatch = Some(TransportPhase::PreDispatch);
    assert_semantic(
        keyed_reply("stable", b"changed").unwrap_err(),
        ErrorCode::CallIdConflict,
        pre_dispatch,
    );
    assert_eq!(
        client
            .recover_unary_call_held("echo", "stable", TIMEOUT)
            .unwrap()
            .bytes(),
        b"one"
    );
    assert_semantic(
        client
            .recover_unary_call("echo", "absent", TIMEOUT)
            .unwrap_err(),
        ErrorCode::CallOutcomeUnknown,
        pre_dispatch,
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 1);

    // Build directly into the negotiated slab and publish that same owner as KeyedUnary.
    let mut prepared = client
        .begin_unary_with_options("echo", keyed("large", TIMEOUT))
        .unwrap();
    let mut slab = prepared
        .try_alloc_request_slab(80 * 1024)
        .unwrap()
        .expect("negotiated IPC slab");
    slab.as_mut_slice().fill(83);
    let held = prepared.encode_slab(slab).unwrap().call_held().unwrap();
    assert_eq!(held.bytes(), vec![83; 80 * 1024]);
    let error = client
        .recover_unary_call("echo", "large", TIMEOUT)
        .unwrap_err();
    assert_eq!(
        assert_semantic(error, ErrorCode::CallResultUnavailable, None).details["outcome"],
        "succeeded"
    );
    assert_eq!(
        client
            .query_call("large", CallTimeout::Unlimited)
            .unwrap()
            .result_unavailable_reason,
        Some(ResultUnavailableReason::NotRetained)
    );
    assert!(observation.snapshot().is_none());
    assert_eq!(caller.path_counters(), counters);
    assert_eq!(
        client.connection_observation().unwrap().connection_id(),
        observation.connection_id()
    );

    let pending = std::thread::spawn({
        let client = client.clone();
        move || {
            client.call_unary_with_options(
                "echo",
                b"busy",
                keyed("busy", CallTimeout::After(Duration::from_secs(2))),
            )
        }
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(
        caller.call_execution_snapshot().client.used_operations,
        Some(1)
    );
    assert_eq!(
        host.call_execution_snapshot().server.used_operations,
        Some(1)
    );
    assert_eq!(
        client.query_call("busy", TIMEOUT).unwrap().status,
        CallOutcomeStatus::InFlight
    );
    let error = pending.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    assert_eq!(
        error.transport_phase(),
        Some(TransportPhase::DispatchUncertain)
    );
    assert_eq!(error.call_id(), Some("busy"));
    let deadline = Instant::now() + WAIT;
    while caller.call_execution_snapshot().client.used_operations != Some(0) {
        assert!(
            Instant::now() < deadline,
            "original native reply never settled"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        client.query_call("busy", TIMEOUT).unwrap().status,
        CallOutcomeStatus::Succeeded
    );
    assert_eq!(
        client.recover_unary_call("echo", "busy", TIMEOUT).unwrap(),
        b"busy"
    );
    assert!(observation.snapshot().is_none());
    assert_eq!(caller.path_counters(), counters);
    assert_eq!(
        client.connection_observation().unwrap().connection_id(),
        observation.connection_id()
    );

    registration.close().unwrap();
    host.shutdown();
    if let Some((relay, _)) = &mut relay {
        relay.stop().unwrap();
    }
}
