use c_two::generated::{EncodedService, MethodAccess, MethodDefinition, ServiceDefinition};
use c_two::{
    C2Error, CallOutcomeStatus, CallTimeout, Client, Connect, ContractRelease, Error, ErrorCode,
    HostOptions, ResultUnavailableReason, Runtime, RuntimeOptions, TransportPhase,
    UnaryCallOptions,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../tests/fixtures/contracts/portable-release.contract.json");
const WAIT: CallTimeout = CallTimeout::After(Duration::from_secs(5));

struct Service<F>(F);

impl<F> EncodedService for Service<F>
where
    F: Fn(u16, Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
{
    fn invoke(&self, method: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        self.0(method, request)
    }
}

fn runtime(slots: u64) -> Runtime {
    let mut options = RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    };
    options.client_limits.max_outstanding_calls = Some(slots);
    Runtime::new(options).unwrap()
}

fn register(
    host: &c_two::Host,
    route: &str,
    callback: impl Fn(u16, Vec<u8>) -> Result<Vec<u8>, C2Error> + Send + Sync + 'static,
) -> c_two::Registration {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    host.register(
        ServiceDefinition::new(
            &release,
            release.reference(),
            route,
            vec![
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
            Arc::new(Service(callback)),
        )
        .unwrap(),
    )
    .unwrap()
}

fn client(runtime: &Runtime, host_runtime: &Runtime, route: &str) -> Client {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    runtime
        .connect(
            release.expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: host_runtime.server_address().unwrap(),
            },
        )
        .unwrap()
}

fn host_options(label: &str) -> HostOptions {
    HostOptions::default()
        .without_relay()
        .with_server_id(format!("call-outcome-{label}-{}", std::process::id()))
}

fn keyed(id: &str) -> UnaryCallOptions {
    UnaryCallOptions::new()
        .with_timeout(WAIT)
        .with_call_id(id)
        .unwrap()
}

fn semantic<T>(
    result: Result<T, Error>,
    code: ErrorCode,
    phase: Option<TransportPhase>,
) -> C2Error {
    let Err(error) = result else {
        panic!("expected {code:?}");
    };
    assert_eq!(error.transport_phase(), phase);
    assert_eq!(error.call_id(), None);
    let Error::Semantic(error) = error else {
        panic!("expected semantic error, got {error}")
    };
    assert_eq!(error.code, code);
    error
}

#[test]
fn keyed_submit_replays_across_connections_and_rejects_conflicts() {
    let runtime = runtime(8);
    let host = runtime.host(host_options("dedup")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let _registration = register(&host, "dedup", move |_, request| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(request)
    });
    let other_runtime = self::runtime(8);
    let first = client(&runtime, &runtime, "dedup");
    let other = client(&other_runtime, &runtime, "dedup");
    assert_ne!(
        first.connection_observation().unwrap().connection_id(),
        other.connection_observation().unwrap().connection_id()
    );
    let mut original = first
        .call_unary_held_with_options("echo", b"value", keyed("operation"))
        .unwrap();
    assert_eq!(
        other
            .call_unary_with_options("echo", b"value", keyed("operation"))
            .unwrap(),
        b"value"
    );
    let recovered = other
        .recover_unary_call_held("echo", "operation", WAIT)
        .unwrap();
    original.invalidate_then_release(|| Ok(())).unwrap();
    assert_eq!(recovered.bytes(), b"value");
    assert_eq!(
        other.recover_unary_call("echo", "operation", WAIT).unwrap(),
        b"value"
    );
    for refusal in [
        other.call_unary_with_options("echo", b"changed", keyed("operation")),
        other.recover_unary_call("ping", "operation", WAIT),
    ] {
        semantic(
            refusal,
            ErrorCode::CallIdConflict,
            Some(TransportPhase::PreDispatch),
        );
    }
    assert_eq!(
        other.query_call("operation", WAIT).unwrap().status,
        CallOutcomeStatus::Succeeded
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn recovery_reports_unretained_evicted_and_unknown_without_execution() {
    for (label, retained, reason) in [
        ("unretained", 0, ResultUnavailableReason::NotRetained),
        ("evicted", 8 * 1024, ResultUnavailableReason::Evicted),
    ] {
        let runtime = runtime(8);
        let host = runtime
            .host(
                host_options(label)
                    .with_outcome_max_entries(2)
                    .with_outcome_max_retained_bytes(retained),
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let _registration = register(&host, label, move |_, request| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(request)
        });
        let client = client(&runtime, &runtime, label);
        let request = vec![42; 6 * 1024];
        for id in ["first", "second"] {
            assert_eq!(
                client
                    .call_unary_with_options("echo", &request, keyed(id))
                    .unwrap(),
                request
            );
        }
        let outcome = client.query_call("first", WAIT).unwrap();
        assert_eq!(outcome.status, CallOutcomeStatus::Succeeded);
        assert_eq!(outcome.method_name.as_deref(), Some("echo"));
        assert!(!outcome.result_available);
        assert_eq!(outcome.result_unavailable_reason, Some(reason));
        let error = semantic(
            client.recover_unary_call("echo", "first", WAIT),
            ErrorCode::CallResultUnavailable,
            None,
        );
        assert_eq!(error.details["outcome"], "succeeded");
        assert_eq!(
            error.details["reason"],
            if retained == 0 {
                "not_retained"
            } else {
                "evicted"
            }
        );

        client
            .call_unary_with_options("ping", b"", keyed("third"))
            .unwrap();
        for id in ["first", "never-submitted"] {
            assert_eq!(
                client.query_call(id, WAIT).unwrap().status,
                CallOutcomeStatus::Unknown
            );
            semantic(
                client.recover_unary_call("echo", id, WAIT),
                ErrorCode::CallOutcomeUnknown,
                Some(TransportPhase::PreDispatch),
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn query_bypasses_full_call_slots_and_uncertain_error_keeps_the_key() {
    let runtime = runtime(1);
    let host = runtime
        .host(host_options("full").with_max_outstanding_calls(1))
        .unwrap();
    let (entered, entry) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    let _registration = register(&host, "full", move |_, request| {
        entered.send(()).unwrap();
        wait.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        Ok(request)
    });
    let client = client(&runtime, &runtime, "full");
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            client.call_unary_with_options(
                "echo",
                b"value",
                keyed("blocked").with_timeout(CallTimeout::After(Duration::from_secs(2))),
            )
        });
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            runtime.call_execution_snapshot().client.used_operations,
            Some(1)
        );
        assert_eq!(
            client.query_call("blocked", WAIT).unwrap().status,
            CallOutcomeStatus::InFlight
        );
        let error = pending.join().unwrap().unwrap_err();
        assert_eq!(
            error.transport_phase(),
            Some(TransportPhase::DispatchUncertain)
        );
        assert_eq!(error.call_id(), Some("blocked"));
        assert_eq!(
            client.query_call("blocked", WAIT).unwrap().status,
            CallOutcomeStatus::InFlight
        );
        release.send(()).unwrap();
    });
    let until = std::time::Instant::now() + Duration::from_secs(5);
    // The Host can publish its snapshot before the original client
    // receives CALL_END and releases its one business slot.
    while runtime.call_execution_snapshot().client.used_operations != Some(0) {
        assert!(
            std::time::Instant::now() < until,
            "original call did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        client.query_call("blocked", WAIT).unwrap().status,
        CallOutcomeStatus::Succeeded
    );
    assert_eq!(
        client.recover_unary_call("echo", "blocked", WAIT).unwrap(),
        b"value"
    );
}

#[test]
fn inner_unavailable_result_becomes_an_outer_resource_failure() {
    let runtime = runtime(8);
    let host = runtime
        .host(host_options("nested").with_outcome_max_retained_bytes(0))
        .unwrap();
    let _inner = register(&host, "inner", |_, request| Ok(request));
    let inner = client(&runtime, &runtime, "inner");
    inner
        .call_unary_with_options("echo", b"value", keyed("inner-id"))
        .unwrap();
    let _outer = register(&host, "outer", move |_, _| {
        inner
            .recover_unary_call("echo", "inner-id", WAIT)
            .map_err(|error| {
                c_two::generated::adapter_error(
                    c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
                    error,
                )
            })
    });
    let outer = client(&runtime, &runtime, "outer");
    let error = semantic(
        outer.call_unary_with_options("echo", b"", keyed("outer-id")),
        ErrorCode::ResourceFunctionExecuting,
        None,
    );
    assert_eq!(error.details["cause_code"], "722");
    let outcome = outer.query_call("outer-id", WAIT).unwrap();
    assert_eq!(outcome.status, CallOutcomeStatus::Failed);
    assert_eq!(
        outcome.error_code,
        Some(ErrorCode::ResourceFunctionExecuting)
    );
}
