use c_two::generated::{EncodedService, MethodAccess, MethodDefinition, ServiceDefinition};
use c_two::{
    AdmissionContext, AdmissionDecision, AdmissionHook, AdmissionOperation, AdmissionRejection,
    AdmissionTransport, C2Error, CallContext, CallMetadata, CallOptions, CallOutcomeStatus,
    CallTimeout, Client, Connect, ContractRelease, Error, ErrorCode, HostOptions, LifecycleError,
    ResultUnavailableReason, Runtime, RuntimeOptions, TransportPhase, current_call_context,
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
        .with_call_options(CallOptions::with_timeout(WAIT))
}

fn host_options(label: &str) -> HostOptions {
    HostOptions::default()
        .without_relay()
        .with_server_id(format!("call-outcome-{label}-{}", std::process::id()))
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
        .call_unary_held_with_call_id("echo", b"value", "operation")
        .unwrap();
    assert_eq!(
        other
            .call_unary_with_call_id("echo", b"value", "operation")
            .unwrap(),
        b"value"
    );
    let recovered = other.recover_unary_call_held("echo", "operation").unwrap();
    original.invalidate_then_release(|| Ok(())).unwrap();
    assert_eq!(recovered.bytes(), b"value");
    assert_eq!(
        other.recover_unary_call("echo", "operation").unwrap(),
        b"value"
    );
    for refusal in [
        other.call_unary_with_call_id("echo", b"changed", "operation"),
        other.recover_unary_call("ping", "operation"),
    ] {
        semantic(
            refusal,
            ErrorCode::CallIdConflict,
            Some(TransportPhase::PreDispatch),
        );
    }
    assert_eq!(
        other.query_call("operation").unwrap().status,
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
                    .call_unary_with_call_id("echo", &request, id)
                    .unwrap(),
                request
            );
        }
        let outcome = client.query_call("first").unwrap();
        assert_eq!(outcome.status, CallOutcomeStatus::Succeeded);
        assert_eq!(outcome.method_name.as_deref(), Some("echo"));
        assert!(!outcome.result_available);
        assert_eq!(outcome.result_unavailable_reason, Some(reason));
        let error = semantic(
            client.recover_unary_call("echo", "first"),
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
            .call_unary_with_call_id("ping", b"", "third")
            .unwrap();
        for id in ["first", "never-submitted"] {
            assert_eq!(
                client.query_call(id).unwrap().status,
                CallOutcomeStatus::Unknown
            );
            semantic(
                client.recover_unary_call("echo", id),
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
            client
                .with_call_options(CallOptions::with_timeout(CallTimeout::After(
                    Duration::from_secs(2),
                )))
                .call_unary_with_call_id("echo", b"value", "blocked")
        });
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            runtime.call_execution_snapshot().client.used_operations,
            Some(1)
        );
        assert_eq!(
            client.query_call("blocked").unwrap().status,
            CallOutcomeStatus::InFlight
        );
        let error = pending.join().unwrap().unwrap_err();
        assert_eq!(
            error.transport_phase(),
            Some(TransportPhase::DispatchUncertain)
        );
        assert_eq!(error.call_id(), Some("blocked"));
        assert_eq!(
            client.query_call("blocked").unwrap().status,
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
        client.query_call("blocked").unwrap().status,
        CallOutcomeStatus::Succeeded
    );
    assert_eq!(
        client.recover_unary_call("echo", "blocked").unwrap(),
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
        .call_unary_with_call_id("echo", b"value", "inner-id")
        .unwrap();
    let _outer = register(&host, "outer", move |_, _| {
        inner
            .recover_unary_call("echo", "inner-id")
            .map_err(|error| {
                c_two::generated::adapter_error(
                    c_two::generated::AdapterFailurePhase::ResourceFunctionExecuting,
                    error,
                )
            })
    });
    let outer = client(&runtime, &runtime, "outer");
    let error = semantic(
        outer.call_unary_with_call_id("echo", b"", "outer-id"),
        ErrorCode::ResourceFunctionExecuting,
        None,
    );
    assert_eq!(error.details["cause_code"], "722");
    let outcome = outer.query_call("outer-id").unwrap();
    assert_eq!(outcome.status, CallOutcomeStatus::Failed);
    assert_eq!(
        outcome.error_code,
        Some(ErrorCode::ResourceFunctionExecuting)
    );
}

#[test]
fn admission_metadata_and_context_follow_the_client_view() {
    let runtime = runtime(8);
    let hook: Arc<dyn AdmissionHook> = Arc::new(|context: &AdmissionContext<'_>| {
        assert_eq!(context.transport(), AdmissionTransport::Ipc);
        assert_eq!(
            context.target(),
            if context.operation() == AdmissionOperation::QueryOutcome {
                None
            } else {
                Some("echo")
            }
        );
        match context.metadata().get("token") {
            Some(token) => AdmissionDecision::allow_with_extension(Arc::new(token.to_owned())),
            None => AdmissionDecision::Deny(AdmissionRejection::new("missing token")),
        }
    });
    let options = host_options("admission").with_admission_hook(hook.clone());
    let host = runtime.host(options.clone()).unwrap();
    runtime
        .host(options.clone().with_admission_hook(hook))
        .unwrap();
    let other = Arc::new(|_: &AdmissionContext<'_>| AdmissionDecision::allow());
    for changed in [
        options.clone().with_admission_hook(other),
        options.without_admission_hook(),
    ] {
        assert!(matches!(
            runtime.host(changed),
            Err(Error::Lifecycle(LifecycleError::ConfigFrozen))
        ));
    }

    let _inner = register(&host, "admitted", |_, request| {
        let context: CallContext = current_call_context().unwrap();
        assert_eq!(context.operation(), AdmissionOperation::Unary);
        assert_eq!(context.route_name(), "admitted");
        assert_eq!(context.target(), "echo");
        assert_eq!(context.metadata().get("token"), Some("allowed"));
        assert_eq!(context.extension::<String>().unwrap().as_str(), "allowed");
        assert!(context.extension::<u64>().is_none());
        assert_eq!(
            context.call_id(),
            (request == b"keyed").then_some("tracked")
        );
        Ok(request)
    });
    let inner = client(&runtime, &runtime, "admitted");
    let denied = inner.clone();
    let _outer = register(&host, "nested-admission", move |_, _| {
        let Error::Semantic(denial) = denied
            .call_unary_with_call_id("echo", b"nested", "child")
            .unwrap_err()
        else {
            panic!("expected an admission refusal");
        };
        assert_eq!(denial.code, ErrorCode::AdmissionDenied);
        Err(denial)
    });
    let metadata = CallMetadata::from_entries([("token", "allowed")]).unwrap();
    let policy = CallOptions::with_timeout(WAIT).with_metadata(metadata);
    let allowed = inner.with_call_options(policy.clone());
    assert_eq!(
        allowed
            .call_unary_with_call_id("echo", b"keyed", "tracked")
            .unwrap(),
        b"keyed"
    );
    assert_eq!(
        allowed.recover_unary_call("echo", "tracked").unwrap(),
        b"keyed"
    );
    assert_eq!(
        allowed.query_call("tracked").unwrap().status,
        CallOutcomeStatus::Succeeded
    );
    let pre_dispatch = Some(TransportPhase::PreDispatch);
    semantic(
        inner.query_call("tracked"),
        ErrorCode::AdmissionDenied,
        pre_dispatch,
    );
    semantic(
        inner.recover_unary_call("echo", "tracked"),
        ErrorCode::AdmissionDenied,
        pre_dispatch,
    );
    let outer = client(&runtime, &runtime, "nested-admission").with_call_options(policy);
    let error = semantic(
        outer.call_unary_with_call_id("echo", b"", "parent"),
        ErrorCode::ResourceFunctionExecuting,
        None,
    );
    assert_eq!(error.details["cause_code"], "724");
    assert!(!error.details.contains_key("transport_phase"));
    assert!(!error.details.contains_key("dispatch_phase"));
    assert!(current_call_context().is_none());
}
