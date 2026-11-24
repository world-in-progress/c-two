use super::*;
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, Host, HostOptions,
    MethodDefinition, Registration, ServiceConcurrencyMode, ServiceDefinition,
};
use std::sync::mpsc;

#[test]
fn error_value_keeps_outer_transport_phase_when_semantic_details_disagree() {
    let semantic =
        C2Error::new(ErrorCode::CallCapacityExceeded, "receive budget exhausted").with_details(
            std::collections::BTreeMap::from([("transport_phase".into(), "pre_dispatch".into())]),
        );
    let error = c2_core::normalize_ipc_error(
        c2_ipc::IpcError::ResponseFailure(Box::new(c2_ipc::IpcError::LocalCallRejected(semantic))),
        TransportPhase::DispatchUncertain,
    );

    let value = error_value(&error);
    assert_eq!(value["transportPhase"], "dispatch_uncertain");
    assert_eq!(value["details"]["transport_phase"], "pre_dispatch");
    assert_eq!(value["code"], ErrorCode::CallCapacityExceeded as u32);
}

#[test]
fn call_error_value_uses_scope_phase_only_for_lifecycle_errors() {
    let fixture = Fixture::new(64);
    let call = Call {
        endpoint: unsafe { &*fixture.endpoint }.0.clone(),
        state: Mutex::new(CallData {
            prepared: None,
            charged: 0,
            busy: false,
            dispatched: false,
            ready: false,
            stream: None,
            stream_operation: None,
            unary: None,
            dispatch_target: None,
            error: None,
            consumer_done: false,
        }),
        scope: CallScope::new(CallOptions::with_timeout(CallTimeout::Unlimited), None).unwrap(),
        stream_mode: false,
        call_id: None,
        cancellation: IpcStreamCancellation::new(),
        cancelled: AtomicBool::new(false),
        next_active: AtomicBool::new(false),
        changed: Notify::new(),
    };
    let configuration = invalid("invalid call configuration");
    let resource_failure =
        C2Error::new(ErrorCode::ResourceFunctionExecuting, "resource failed").into();
    assert!(error_value(&configuration).get("transportPhase").is_none());
    assert_eq!(
        call_error_value(&call, &configuration)["transportPhase"],
        "pre_dispatch"
    );
    assert!(
        call_error_value(&call, &resource_failure)
            .get("transportPhase")
            .is_none()
    );

    call.scope.try_begin_dispatch().unwrap();
    assert_eq!(
        call_error_value(&call, &configuration)["transportPhase"],
        "dispatch_uncertain"
    );
    assert!(
        call_error_value(&call, &resource_failure)
            .get("transportPhase")
            .is_none()
    );
}

#[test]
fn raw_call_begin_rejects_keyed_stream_and_invalid_length_delimited_keys() {
    let fixture = Fixture::new(64);
    let mut call = ptr::null_mut();
    let mut error = ptr::null_mut();
    for (mode, id, len) in [
        (1, b"valid".as_ptr(), 5),
        (0, b"".as_ptr(), 0),
        (0, b"a\0b".as_ptr(), 3),
        (0, ptr::null(), 1),
        (0, b"X".as_ptr(), usize::MAX),
    ] {
        let status = unsafe {
            c2_mem_ffi_call_begin(
                fixture.endpoint,
                c"items".as_ptr(),
                c"echo".as_ptr(),
                mode,
                ptr::null(),
                id,
                len,
                ptr::null(),
                &mut call,
                &mut error,
            )
        };
        assert_eq!(status, C2MemFfiStatus::InvalidArgument);
        assert!(call.is_null());
        let parts: Value = serde_json::from_str(
            unsafe { CStr::from_ptr(c2_mem_ffi_error_parts(error)) }
                .to_str()
                .unwrap(),
        )
        .unwrap();
        if mode == 1 {
            assert_eq!(parts["code"], ErrorCode::UnsupportedCallMode as u32);
            assert_eq!(parts["transportPhase"], "pre_dispatch");
        }
        assert!(parts.get("callId").is_none());
        unsafe { c2_mem_ffi_error_free(error) };
    }
}

const DESCRIPTOR: &[u8] =
    include_bytes!("../../c2-contract/tests/fixtures/server-stream.contract.json");
const WAIT: Duration = Duration::from_secs(10);
struct Service;
impl EncodedService for Service {
    fn invoke(&self, _: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(b"pong".to_vec())
    }
}
impl EncodedStreamService for Service {
    fn open_stream(&self, _: u16, input: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
        Ok(Box::new(Items {
            count: input[0],
            cursor: 0,
            fail: input.get(1) == Some(&1),
        }))
    }
}
struct Items {
    count: u8,
    cursor: u8,
    fail: bool,
}
struct Item(u8);
static STREAM_GATE: (std::sync::Mutex<u8>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
impl EncodedStreamItem for Item {
    fn len(&self) -> usize {
        4
    }
    fn write_range(&self, _: usize, dst: &mut [u8]) -> Result<(), C2Error> {
        dst.fill(self.0);
        Ok(())
    }
}
impl EncodedStream for Items {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        if self.count == u8::MAX && self.cursor == 0 {
            let (lock, changed) = &STREAM_GATE;
            let mut state = lock.lock().unwrap();
            *state = 1;
            changed.notify_all();
            while *state != 2 {
                state = changed.wait(state).unwrap();
            }
        }
        if self.cursor == self.count {
            return if self.fail {
                Err(C2Error::new(
                    ErrorCode::ResourceFunctionExecuting,
                    "after items",
                ))
            } else {
                Ok(None)
            };
        }
        self.cursor += 1;
        Ok(Some(Box::new(Item(self.cursor))))
    }
    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Ok(())
    }
}
unsafe extern "C" fn complete(user: *mut std::ffi::c_void, value: *mut C2MemFfiResult) {
    let sender = unsafe { Box::from_raw(user as *mut mpsc::Sender<usize>) };
    sender.send(value as usize).unwrap();
}
fn async_result(
    start: impl FnOnce(C2MemFfiComplete, *mut std::ffi::c_void) -> C2MemFfiStatus,
) -> *mut C2MemFfiResult {
    let (tx, rx) = mpsc::channel::<usize>();
    let status = start(Some(complete), Box::into_raw(Box::new(tx)).cast());
    assert_eq!(status, C2MemFfiStatus::Ok);
    rx.recv_timeout(WAIT).unwrap() as *mut C2MemFfiResult
}
unsafe fn success(value: *mut C2MemFfiResult) {
    assert!(
        unsafe { (*value).error.is_null() },
        "{}",
        unsafe { CStr::from_ptr(c2_mem_ffi_error_parts((*value).error)) }.to_string_lossy()
    );
}
unsafe fn metadata(value: *mut C2MemFfiResult) -> Value {
    unsafe {
        success(value);
        serde_json::from_str(CStr::from_ptr((*value).json).to_str().unwrap()).unwrap()
    }
}
unsafe fn error_details(result: *mut C2MemFfiResult) -> Value {
    unsafe {
        let error = c2_mem_ffi_error_parts((*result).error);
        assert!(!error.is_null());
        let details = serde_json::from_slice(CStr::from_ptr(error).to_bytes()).unwrap();
        c2_mem_ffi_result_free(result);
        details
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    _runtime: Runtime,
    host: Host,
    registration: Option<Registration>,
    domain: *mut C2MemFfiClientDomain,
    endpoint: *mut C2MemFfiEndpointContext,
    _process_state: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    fn new(budget: u64) -> Self {
        Self::with_limits(json!({"memoryBudgetBytes": budget}))
    }
    fn with_limits(limits: Value) -> Self {
        Self::with_host_options(limits, HostOptions::default().without_relay())
    }
    fn with_host_options(limits: Value, host_options: HostOptions) -> Self {
        let process_state = crate::tests::PROCESS_STATE_LOCK.lock().unwrap();
        assert_eq!(c2_mem_ffi_abi_version(), 10);
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions {
                    #[cfg(unix)]
                    unix_root: Some(root.path().to_path_buf()),
                    ..Default::default()
                },
                ConfigSources::empty(),
            )
            .unwrap();
        let host = runtime.host(host_options).unwrap();
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        let registration = host
            .register(
                ServiceDefinition::new_stream(
                    &release,
                    release.reference(),
                    "items",
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
                    Arc::new(Service),
                )
                .unwrap()
                .with_concurrency(ServiceConcurrencyMode::ReadParallel, Some(32), Some(2))
                .unwrap(),
            )
            .unwrap();
        let context = C2MemFfiLocalEndpointContext {
            inner: runtime.local_endpoint_context().unwrap(),
        };
        let mut domain = ptr::null_mut();
        let mut error = ptr::null_mut();
        let options = CString::new(limits.to_string()).unwrap();
        assert_eq!(
            unsafe {
                c2_mem_ffi_client_domain_new(&context, options.as_ptr(), &mut domain, &mut error)
            },
            C2MemFfiStatus::Ok
        );
        let mut endpoint = ptr::null_mut();
        let address = CString::new(host.local_endpoint().address()).unwrap();
        let options = CString::new("{}").unwrap();
        assert_eq!(
            unsafe {
                c2_mem_ffi_endpoint_context_new(
                    domain,
                    address.as_ptr(),
                    options.as_ptr(),
                    &mut endpoint,
                    &mut error,
                )
            },
            C2MemFfiStatus::Ok
        );
        let expected = release.expected_route("items").unwrap();
        let contract=CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
        let route = CString::new("items").unwrap();
        let result = async_result(|callback, user| unsafe {
            c2_mem_ffi_endpoint_context_prepare(
                endpoint,
                route.as_ptr(),
                contract.as_ptr(),
                ptr::null(),
                callback,
                user,
            )
        });
        unsafe {
            success(result);
            c2_mem_ffi_result_free(result);
        }
        Self {
            _root: root,
            _runtime: runtime,
            host,
            registration: Some(registration),
            domain,
            endpoint,
            _process_state: process_state,
        }
    }
    fn call(&self, method: &str, input: &[u8], stream: bool) -> *mut C2MemFfiCall {
        let route = CString::new("items").unwrap();
        let method = CString::new(method).unwrap();
        let mut call = ptr::null_mut();
        let mut error = ptr::null_mut();
        assert_eq!(
            unsafe {
                c2_mem_ffi_call_begin(
                    self.endpoint,
                    route.as_ptr(),
                    method.as_ptr(),
                    u8::from(stream),
                    ptr::null(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    &mut call,
                    &mut error,
                )
            },
            C2MemFfiStatus::Ok
        );
        let result = async_result(|callback, user| unsafe {
            c2_mem_ffi_call_charge_input(call, input.len() as u64, callback, user)
        });
        unsafe {
            success(result);
            c2_mem_ffi_result_free(result);
        }
        let result = async_result(|callback, user| unsafe {
            c2_mem_ffi_call_dispatch(call, input.as_ptr(), input.len(), callback, user)
        });
        unsafe {
            let target = metadata(result);
            assert_eq!(target["path"], "DirectIpc");
            let mut snapshot = ptr::null_mut();
            assert_eq!(
                c2_mem_ffi_endpoint_context_connection_snapshot(
                    self.endpoint,
                    route.as_ptr(),
                    &mut snapshot,
                ),
                C2MemFfiStatus::Ok
            );
            let prepared = metadata(snapshot);
            for key in ["serverId", "serverInstanceId", "connectionId"] {
                assert_eq!(target[key], prepared[key], "dispatch target field {key}");
            }
            c2_mem_ffi_result_free(snapshot);
            c2_mem_ffi_result_free(result);
        }
        call
    }
}

#[test]
fn connection_snapshot_reports_the_prepared_physical_connection_and_its_end() {
    let fixture = Fixture::new(64);
    let route = CString::new("items").unwrap();
    let mut snapshot = ptr::null_mut();
    assert_eq!(
        unsafe {
            c2_mem_ffi_endpoint_context_connection_snapshot(
                fixture.endpoint,
                route.as_ptr(),
                &mut snapshot,
            )
        },
        C2MemFfiStatus::Ok
    );
    let value = unsafe { metadata(snapshot) };
    assert!(value["connectionId"].as_str().is_some());
    assert!(value["serverId"].as_str().is_some());
    assert!(value["serverInstanceId"].as_str().is_some());
    assert!(value["endReason"].is_null());
    unsafe {
        c2_mem_ffi_result_free(snapshot);
    }
    let missing = CString::new("missing").unwrap();
    assert_eq!(
        unsafe {
            c2_mem_ffi_endpoint_context_connection_snapshot(
                fixture.endpoint,
                missing.as_ptr(),
                &mut snapshot,
            )
        },
        C2MemFfiStatus::Ok
    );
    assert!(unsafe { (*snapshot).json.is_null() });
    unsafe {
        c2_mem_ffi_result_free(snapshot);
    }
    let closed =
        async_result(|c, u| unsafe { c2_mem_ffi_endpoint_context_close(fixture.endpoint, c, u) });
    unsafe {
        success(closed);
        c2_mem_ffi_result_free(closed);
    }
    assert_eq!(
        unsafe {
            c2_mem_ffi_endpoint_context_connection_snapshot(
                fixture.endpoint,
                route.as_ptr(),
                &mut snapshot,
            )
        },
        C2MemFfiStatus::Ok
    );
    let after = unsafe { metadata(snapshot) };
    assert_eq!(after["connectionId"], value["connectionId"]);
    assert_eq!(after["serverId"], value["serverId"]);
    assert_eq!(after["serverInstanceId"], value["serverInstanceId"]);
    assert_eq!(after["endReason"], "local_close");
    unsafe {
        c2_mem_ffi_result_free(snapshot);
    }
}

#[test]
fn prepare_zero_budget_expires_before_acquiring_an_existing_connection() {
    let fixture = Fixture::new(64);
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let expected = release.expected_route("items").unwrap();
    let contract = CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let zero = 0u64;
    let result = async_result(|c, u| unsafe {
        c2_mem_ffi_endpoint_context_prepare(
            fixture.endpoint,
            route.as_ptr(),
            contract.as_ptr(),
            &zero,
            c,
            u,
        )
    });
    let error: Value = unsafe {
        serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*result).error))
                .to_str()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(error["code"], ErrorCode::CallDeadlineExceeded as u32);
    assert_eq!(error["transportPhase"], "pre_dispatch");
    unsafe {
        c2_mem_ffi_result_free(result);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            let result = async_result(|callback, user| {
                c2_mem_ffi_endpoint_context_close(self.endpoint, callback, user)
            });
            success(result);
            c2_mem_ffi_result_free(result);
            c2_mem_ffi_endpoint_context_free(self.endpoint);
            c2_mem_ffi_client_domain_free(self.domain);
        }
        self.registration.take().unwrap().close().unwrap();
        assert!(
            self.host
                .shutdown_with_timeout(WAIT)
                .runtime_barrier_error
                .is_none()
        );
    }
}
#[test]
fn native_calls_share_the_driver_and_report_real_complete_item_counts() {
    let fixture = Fixture::new(64);
    for count in [0, 1, 5] {
        let call = fixture.call("echo", &[count, 0], true);
        let unary = fixture.call("ping", &[], false);
        let result = async_result(|c, u| unsafe { c2_mem_ffi_call_next(unary, c, u) });
        unsafe {
            success(result);
            assert_eq!((*(*result).buffer).0.bytes(), b"pong");
            c2_mem_ffi_result_free(result);
            c2_mem_ffi_call_free(unary);
        }
        for index in 1..=count {
            let result = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
            unsafe {
                success(result);
                assert_eq!((*(*result).buffer).0.bytes(), &[index; 4]);
                c2_mem_ffi_result_free(result);
            }
        }
        let end = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
        unsafe {
            success(end);
            assert!((*end).buffer.is_null());
            c2_mem_ffi_result_free(end);
        }
        let zero = 0;
        let observed = async_result(|c, u| unsafe { c2_mem_ffi_call_observe(call, &zero, c, u) });
        let observed_json = unsafe { metadata(observed) };
        assert_eq!(
            observed_json,
            json!({"completed":true,"error":null,"completeItems":count.to_string()})
        );
        unsafe {
            c2_mem_ffi_result_free(observed);
            c2_mem_ffi_call_free(call);
        }
    }
}

#[test]
fn dispatch_error_keeps_typed_failure_and_original_target() {
    let fixture = Fixture::new(64);
    let mut call = ptr::null_mut();
    let mut error = ptr::null_mut();
    assert_eq!(
        unsafe {
            c2_mem_ffi_call_begin(
                fixture.endpoint,
                c"items".as_ptr(),
                c"missing".as_ptr(),
                0,
                ptr::null(),
                ptr::null(),
                0,
                ptr::null(),
                &mut call,
                &mut error,
            )
        },
        C2MemFfiStatus::Ok
    );
    let charged = async_result(|c, u| unsafe { c2_mem_ffi_call_charge_input(call, 0, c, u) });
    unsafe {
        success(charged);
        c2_mem_ffi_result_free(charged);
    }
    let dispatched =
        async_result(|c, u| unsafe { c2_mem_ffi_call_dispatch(call, ptr::null(), 0, c, u) });
    unsafe {
        assert!(!(*dispatched).error.is_null());
        let failure: Value = serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*dispatched).error))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(failure["code"], ErrorCode::ProtocolViolation as u32);
        assert_eq!(failure["transportPhase"], "pre_dispatch");
        let target = (&*call).0.state.lock().dispatch_target.clone().unwrap();
        assert_eq!(target.path, c2_core::ObservedPath::DirectIpc);
        let ipc = target.ipc.unwrap();
        let (identity, observation) = (&*fixture.endpoint)
            .0
            .bindings
            .lock()
            .observations
            .get("items")
            .cloned()
            .unwrap();
        assert_eq!(ipc.server_identity.server_id, identity.server_id);
        assert_eq!(
            ipc.server_identity.server_instance_id,
            identity.server_instance_id
        );
        assert_eq!(ipc.connection_id, observation.connection_id());
        c2_mem_ffi_result_free(dispatched);
        c2_mem_ffi_call_free(call);
    }
}
#[test]
fn stream_error_is_after_complete_items_and_keeps_count() {
    let fixture = Fixture::new(64);
    let call = fixture.call("echo", &[2, 1], true);
    for _ in 0..2 {
        let value = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
        unsafe {
            success(value);
            c2_mem_ffi_result_free(value);
        }
    }
    let value = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
    unsafe {
        assert!(!(*value).error.is_null());
        let error: Value = serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*value).error))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error["code"], ErrorCode::ResourceFunctionExecuting as u32);
        c2_mem_ffi_result_free(value);
    }
    let observed = async_result(|c, u| unsafe { c2_mem_ffi_call_observe(call, ptr::null(), c, u) });
    let value = unsafe { metadata(observed) };
    assert_eq!(value["completed"], true);
    assert_eq!(value["completeItems"], "2");
    assert_eq!(value["error"]["canonicalName"], "ResourceFunctionExecuting");
    unsafe {
        c2_mem_ffi_result_free(observed);
        c2_mem_ffi_call_free(call);
    }
}

#[test]
fn cancelling_input_budget_wait_does_not_wait_for_a_held_item_to_release() {
    let fixture = Fixture::new(4);
    let first = fixture.call("echo", &[1, 0], true);
    let held = async_result(|c, u| unsafe { c2_mem_ffi_call_next(first, c, u) });
    unsafe {
        success(held);
    }
    let end = async_result(|c, u| unsafe { c2_mem_ffi_call_next(first, c, u) });
    unsafe {
        success(end);
        c2_mem_ffi_result_free(end);
    }
    let mut second = ptr::null_mut();
    let mut error = ptr::null_mut();
    let route = CString::new("items").unwrap();
    let method = CString::new("echo").unwrap();
    assert_eq!(
        unsafe {
            c2_mem_ffi_call_begin(
                fixture.endpoint,
                route.as_ptr(),
                method.as_ptr(),
                1,
                ptr::null(),
                ptr::null(),
                0,
                ptr::null(),
                &mut second,
                &mut error,
            )
        },
        C2MemFfiStatus::Ok
    );
    let (tx, rx) = mpsc::channel::<usize>();
    assert_eq!(
        unsafe {
            c2_mem_ffi_call_charge_input(
                second,
                2,
                Some(complete),
                Box::into_raw(Box::new(tx)).cast(),
            )
        },
        C2MemFfiStatus::Ok
    );
    assert!(unsafe { &*second }.0.state.lock().busy);
    let duplicate = async_result(|c, u| unsafe { c2_mem_ffi_call_charge_input(second, 2, c, u) });
    unsafe {
        assert!(!(*duplicate).error.is_null());
        let failure: Value = serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*duplicate).error))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(failure["transportPhase"], "pre_dispatch");
        c2_mem_ffi_result_free(duplicate);
    }
    unsafe {
        c2_mem_ffi_call_cancel(second);
    }
    let cancelled = rx.recv_timeout(Duration::from_secs(1));
    // Always release owned data before a regression assertion fails.
    unsafe {
        c2_mem_ffi_result_free(held);
    }
    let timely = cancelled.is_ok();
    let cancelled =
        cancelled.unwrap_or_else(|_| rx.recv_timeout(WAIT).unwrap()) as *mut C2MemFfiResult;
    unsafe {
        c2_mem_ffi_result_free(cancelled);
        c2_mem_ffi_call_free(first);
        c2_mem_ffi_call_free(second);
    }
    assert!(
        timely,
        "cancellation must wake input-budget admission without another hold release"
    );
}

#[test]
fn concurrent_next_reports_dispatch_uncertainty_through_abi_callback() {
    struct ReleaseGate;
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let (lock, changed) = &STREAM_GATE;
            *lock.lock().unwrap() = 2;
            changed.notify_all();
        }
    }
    let fixture = Fixture::new(64);
    *STREAM_GATE.0.lock().unwrap() = 0;
    let gate = ReleaseGate;
    let call = fixture.call("echo", &[u8::MAX, 0], true);
    let (tx, rx) = mpsc::channel::<usize>();
    assert_eq!(
        unsafe { c2_mem_ffi_call_next(call, Some(complete), Box::into_raw(Box::new(tx)).cast()) },
        C2MemFfiStatus::Ok
    );
    {
        let state = STREAM_GATE.0.lock().unwrap();
        let (state, _) = STREAM_GATE
            .1
            .wait_timeout_while(state, WAIT, |state| *state != 1)
            .unwrap();
        let started = *state == 1;
        drop(state);
        assert!(started, "resource step did not start");
    }
    let duplicate = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
    unsafe {
        assert!(!(*duplicate).error.is_null());
        let failure: Value = serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*duplicate).error))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(failure["transportPhase"], "dispatch_uncertain");
        assert!(
            failure["message"]
                .as_str()
                .unwrap()
                .contains("concurrent next")
        );
        c2_mem_ffi_result_free(duplicate);
    }
    drop(gate);
    let first = rx.recv_timeout(WAIT).unwrap() as *mut C2MemFfiResult;
    unsafe {
        success(first);
        c2_mem_ffi_result_free(first);
        c2_mem_ffi_call_free(call);
    }
}

fn domain_stats(domain: *const C2MemFfiClientDomain) -> Value {
    let mut result = ptr::null_mut();
    unsafe {
        assert_eq!(
            c2_mem_ffi_client_domain_stats(domain, &mut result),
            C2MemFfiStatus::Ok
        );
        let stats = metadata(result);
        c2_mem_ffi_result_free(result);
        stats
    }
}

#[test]
fn client_domain_creation_validates_only_current_limits() {
    for (options, message) in [
        (
            cr#"{"requestShmThreshold":1}"#,
            "unknown option requestShmThreshold",
        ),
        (
            cr#"{"requestChunkSize":64}"#,
            "unknown option requestChunkSize",
        ),
        (
            cr#"{"fileSpillBudgetBytes":"18446744073709551616"}"#,
            "invalid fileSpillBudgetBytes",
        ),
        (
            cr#"{"memoryBudgetBytes":0,"maxOutstandingCalls":1}"#,
            "must be zero",
        ),
    ] {
        let mut domain = ptr::null_mut();
        let mut error = ptr::null_mut();
        unsafe {
            assert_eq!(
                c2_mem_ffi_client_domain_new(
                    ptr::null(),
                    options.as_ptr(),
                    &mut domain,
                    &mut error
                ),
                C2MemFfiStatus::InvalidArgument,
            );
            assert!(domain.is_null());
            let details = CStr::from_ptr(c2_mem_ffi_error_parts(error))
                .to_string_lossy()
                .into_owned();
            c2_mem_ffi_error_free(error);
            assert!(details.contains(message), "{details}");
        }
    }
}

#[test]
fn client_domain_observation_is_read_only_before_connection() {
    let mut domain = ptr::null_mut();
    let mut error = ptr::null_mut();
    assert_eq!(
        unsafe {
            c2_mem_ffi_client_domain_new(
                ptr::null(),
                cr#"{"memoryBudgetBytes":17}"#.as_ptr(),
                &mut domain,
                &mut error,
            )
        },
        C2MemFfiStatus::Ok,
    );
    assert_eq!(
        domain_stats(domain),
        json!({
            "domain_id": null, "generation": "0", "frozen": false, "closed": false,
            "memory": null, "file_spill": null, "call_execution": {
                "max_operations": null, "used_operations": null, "peak_operations": null,
                "rejected_reservations": null, "closed": false,
            },
        })
    );
    assert!(!unsafe { &*domain }.0.runtime.client_config_frozen());
    unsafe {
        c2_mem_ffi_client_domain_free(domain);
    }
}

#[test]
fn factory_capacity_and_domain_identity_survive_reconnect() {
    let mut fixture = Fixture::with_limits(json!({
        "memoryBudgetBytes": 128,
        "fileSpillBudgetBytes": "18446744073709551615",
        "maxOutstandingCalls": "18446744073709551615",
    }));
    let initial = domain_stats(fixture.domain);
    let cell = |limit: &str| {
        json!({
            "limit_bytes": limit, "used_bytes": "0", "peak_bytes": "0",
            "rejected_allocations": "0", "rejected_bytes": "0",
        })
    };
    assert!(
        initial["domain_id"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    assert_eq!(
        initial,
        json!({
            "domain_id": initial["domain_id"], "generation": "0", "frozen": true, "closed": false,
            "memory": cell("128"), "file_spill": cell("18446744073709551615"), "call_execution": {
                "max_operations": "18446744073709551615", "used_operations": "0",
                "peak_operations": "0", "rejected_reservations": "0", "closed": false,
            },
        })
    );
    let address = CString::new(fixture.host.local_endpoint().address()).unwrap();
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let expected = release.expected_route("items").unwrap();
    let contract = CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let mut connection_ids = Vec::new();
    for _ in 0..2 {
        assert_eq!(domain_stats(fixture.domain), initial);
        let prepared = async_result(|c, u| unsafe {
            c2_mem_ffi_endpoint_context_prepare(
                fixture.endpoint,
                route.as_ptr(),
                contract.as_ptr(),
                ptr::null(),
                c,
                u,
            )
        });
        connection_ids.push(unsafe { metadata(prepared) }["connectionId"].clone());
        unsafe {
            c2_mem_ffi_result_free(prepared);
        }
        let closed = async_result(|c, u| unsafe {
            c2_mem_ffi_endpoint_context_close(fixture.endpoint, c, u)
        });
        unsafe {
            success(closed);
            c2_mem_ffi_result_free(closed);
            c2_mem_ffi_endpoint_context_free(fixture.endpoint);
        }
        fixture.endpoint = ptr::null_mut();
        let mut error = ptr::null_mut();
        assert_eq!(
            unsafe {
                c2_mem_ffi_endpoint_context_new(
                    fixture.domain,
                    address.as_ptr(),
                    c"{}".as_ptr(),
                    &mut fixture.endpoint,
                    &mut error,
                )
            },
            C2MemFfiStatus::Ok
        );
        let prepared = async_result(|c, u| unsafe {
            c2_mem_ffi_endpoint_context_prepare(
                fixture.endpoint,
                route.as_ptr(),
                contract.as_ptr(),
                ptr::null(),
                c,
                u,
            )
        });
        unsafe {
            success(prepared);
            c2_mem_ffi_result_free(prepared);
        }
    }
    assert_ne!(connection_ids[0], connection_ids[1]);
}

#[test]
fn closing_one_endpoint_does_not_close_another_endpoint_at_the_same_address() {
    let mut fixture = Fixture::new(64);
    let address = CString::new(fixture.host.local_endpoint().address()).unwrap();
    let options = CString::new("{}").unwrap();
    let mut second = ptr::null_mut();
    let mut error = ptr::null_mut();
    assert_eq!(
        unsafe {
            c2_mem_ffi_endpoint_context_new(
                fixture.domain,
                address.as_ptr(),
                options.as_ptr(),
                &mut second,
                &mut error,
            )
        },
        C2MemFfiStatus::Ok
    );
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let expected = release.expected_route("items").unwrap();
    let contract=CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let prepared = async_result(|c, u| unsafe {
        c2_mem_ffi_endpoint_context_prepare(
            second,
            route.as_ptr(),
            contract.as_ptr(),
            ptr::null(),
            c,
            u,
        )
    });
    unsafe {
        success(prepared);
        c2_mem_ffi_result_free(prepared);
    }
    let closed =
        async_result(|c, u| unsafe { c2_mem_ffi_endpoint_context_close(fixture.endpoint, c, u) });
    unsafe {
        success(closed);
        c2_mem_ffi_result_free(closed);
        c2_mem_ffi_endpoint_context_free(fixture.endpoint);
    }
    fixture.endpoint = second;
    let call = fixture.call("ping", &[], false);
    let response = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
    unsafe {
        success(response);
        assert_eq!((*(*response).buffer).0.bytes(), b"pong");
        c2_mem_ffi_result_free(response);
        c2_mem_ffi_call_free(call);
    }
}

#[test]
fn close_waits_for_a_connection_whose_route_preparation_was_refused() {
    let fixture = Fixture::new(64);
    let domain = unsafe { &*fixture.domain }.0.runtime.clone();
    let events = domain.connection_events();
    let mut endpoint = ptr::null_mut();
    let mut error = ptr::null_mut();
    let address = CString::new(fixture.host.local_endpoint().address()).unwrap();
    let options = CString::new("{}").unwrap();
    assert_eq!(
        unsafe {
            c2_mem_ffi_endpoint_context_new(
                fixture.domain,
                address.as_ptr(),
                options.as_ptr(),
                &mut endpoint,
                &mut error,
            )
        },
        C2MemFfiStatus::Ok
    );
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let mut expected = release.expected_route("items").unwrap();
    expected.signature_hash = "0".repeat(64);
    let contract=CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let rejected = async_result(|c, u| unsafe {
        c2_mem_ffi_endpoint_context_prepare(
            endpoint,
            route.as_ptr(),
            contract.as_ptr(),
            ptr::null(),
            c,
            u,
        )
    });
    unsafe {
        assert!(!(*rejected).error.is_null());
        c2_mem_ffi_result_free(rejected);
    }
    let Some(c2_core::ConnectionEventWait::Event(c2_core::ConnectionEvent::Opened(observation))) =
        c2_core::recv_connection_event(&events, Some(WAIT)).unwrap()
    else {
        panic!("refused route still connected first")
    };
    let closed = async_result(|c, u| unsafe { c2_mem_ffi_endpoint_context_close(endpoint, c, u) });
    unsafe {
        success(closed);
        c2_mem_ffi_result_free(closed);
    }
    let actually_closed = observation.snapshot().is_some();
    unsafe {
        c2_mem_ffi_endpoint_context_free(endpoint);
    }
    assert!(
        actually_closed,
        "close must await the original connection even without a published route binding"
    );
}

// ADR 0004: same-endpoint re-registration is admitted by name and contract.
#[test]
fn route_refusal_and_replacement_preserve_other_route_on_same_endpoint() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Tagged {
        label: &'static [u8],
        calls: Arc<AtomicUsize>,
    }
    impl EncodedService for Tagged {
        fn invoke(&self, _: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.label.to_vec())
        }
    }
    impl EncodedStreamService for Tagged {
        fn open_stream(
            &self,
            index: u16,
            input: Vec<u8>,
        ) -> Result<Box<dyn EncodedStream>, C2Error> {
            Service.open_stream(index, input)
        }
    }
    fn ping(endpoint: *mut C2MemFfiEndpointContext, route: &CString) -> *mut C2MemFfiResult {
        let mut call = ptr::null_mut();
        let mut error = ptr::null_mut();
        assert_eq!(
            unsafe {
                c2_mem_ffi_call_begin(
                    endpoint,
                    route.as_ptr(),
                    c"ping".as_ptr(),
                    0,
                    ptr::null(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    &mut call,
                    &mut error,
                )
            },
            C2MemFfiStatus::Ok
        );
        let charged = async_result(|c, u| unsafe { c2_mem_ffi_call_charge_input(call, 0, c, u) });
        unsafe {
            success(charged);
            c2_mem_ffi_result_free(charged);
        }
        let sent =
            async_result(|c, u| unsafe { c2_mem_ffi_call_dispatch(call, ptr::null(), 0, c, u) });
        if unsafe { !(*sent).error.is_null() } {
            unsafe {
                c2_mem_ffi_call_free(call);
            }
            return sent;
        }
        unsafe {
            success(sent);
            c2_mem_ffi_result_free(sent);
        }
        let result = async_result(|c, u| unsafe { c2_mem_ffi_call_next(call, c, u) });
        unsafe {
            c2_mem_ffi_call_free(call);
        }
        result
    }
    let fixture = Fixture::new(64);
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let definition = |label, calls| {
        ServiceDefinition::new_stream(
            &release,
            release.reference(),
            "other",
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
            Arc::new(Tagged { label, calls }),
        )
        .unwrap()
    };
    let mut old = fixture
        .host
        .register(definition(b"old", old_calls.clone()))
        .unwrap();
    let expected = release.expected_route("other").unwrap();
    let contract = CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let other = CString::new("other").unwrap();
    let prepare = || {
        async_result(|c, u| unsafe {
            c2_mem_ffi_endpoint_context_prepare(
                fixture.endpoint,
                other.as_ptr(),
                contract.as_ptr(),
                ptr::null(),
                c,
                u,
            )
        })
    };
    let first = prepare();
    let first_metadata = unsafe { metadata(first) };
    assert_eq!(
        first_metadata
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["connectionId", "serverId", "serverInstanceId"])
    );
    unsafe {
        c2_mem_ffi_result_free(first);
    }
    let result = ping(fixture.endpoint, &other);
    unsafe {
        success(result);
        assert_eq!((*(*result).buffer).0.bytes(), b"old");
        c2_mem_ffi_result_free(result);
    }
    old.close().unwrap();
    let result = ping(fixture.endpoint, &other);
    unsafe {
        assert!(!(*result).error.is_null());
        let error: Value = serde_json::from_str(
            CStr::from_ptr(c2_mem_ffi_error_parts((*result).error))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error["code"], 708);
        assert_eq!(error["transportPhase"], "pre_dispatch");
        assert!(error["details"].get("route_uid").is_none());
        c2_mem_ffi_result_free(result);
    }
    let unary = fixture.call("ping", &[], false);
    let result = async_result(|c, u| unsafe { c2_mem_ffi_call_next(unary, c, u) });
    unsafe {
        success(result);
        assert_eq!((*(*result).buffer).0.bytes(), b"pong");
        c2_mem_ffi_result_free(result);
        c2_mem_ffi_call_free(unary);
    }
    let _replacement = fixture
        .host
        .register(definition(b"new", new_calls.clone()))
        .unwrap();
    // The endpoint's previously prepared client calls the replacement without prepare.
    let result = ping(fixture.endpoint, &other);
    unsafe {
        success(result);
        assert_eq!((*(*result).buffer).0.bytes(), b"new");
        c2_mem_ffi_result_free(result);
    }
    let again = prepare();
    assert_eq!(unsafe { metadata(again) }, first_metadata);
    unsafe {
        c2_mem_ffi_result_free(again);
    }
    let result = ping(fixture.endpoint, &other);
    unsafe {
        success(result);
        assert_eq!((*(*result).buffer).0.bytes(), b"new");
        c2_mem_ffi_result_free(result);
    }
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_calls.load(Ordering::SeqCst), 2);
}

fn topic_release() -> ContractRelease {
    let mut descriptor: Value = serde_json::from_slice(DESCRIPTOR).unwrap();
    descriptor["topics"] = json!([{
        "name": "state", "output": {"kind": "payload"},
        "binding": descriptor["methods"][1]["bindings"]["output"],
    }]);
    descriptor["methods"] = json!([]);
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(descriptor.to_string().as_bytes()).unwrap();
    descriptor["fingerprints"] = json!({
        "abi_hash": fingerprints.abi_hash(),
        "signature_hash": fingerprints.signature_hash(),
    });
    ContractRelease::from_descriptor_json(descriptor.to_string().as_bytes()).unwrap()
}

fn topic_definition(release: &ContractRelease, route: &str) -> ServiceDefinition {
    ServiceDefinition::new(release, release.reference(), route, [], Arc::new(Service))
        .unwrap()
        .with_initial_topic_bytes("state", b"boot".to_vec())
        .unwrap()
}

fn topic_contract(release: &ContractRelease, route: &str) -> CString {
    let expected = release.expected_route(route).unwrap();
    CString::new(
        json!({
            "namespace": expected.crm_ns, "name": expected.crm_name, "version": expected.crm_ver,
            "abiHash": expected.abi_hash, "signatureHash": expected.signature_hash,
        })
        .to_string(),
    )
    .unwrap()
}

#[test]
fn ipc_subscription_owns_ready_delivery_and_terminal_receipt() {
    let fixture = Fixture::with_limits(json!({"maxOutstandingCalls": 2}));
    let route = c"telemetry";
    let release = topic_release();
    let mut registration = fixture
        .host
        .register(topic_definition(&release, "telemetry"))
        .unwrap();
    let contract = topic_contract(&release, "telemetry");
    unsafe {
        let prepared = async_result(|c, u| {
            c2_mem_ffi_endpoint_context_prepare(
                fixture.endpoint,
                route.as_ptr(),
                contract.as_ptr(),
                ptr::null(),
                c,
                u,
            )
        });
        success(prepared);
        c2_mem_ffi_result_free(prepared);
        let open = |topic: &CStr, options: &CStr| {
            async_result(|c, u| {
                c2_mem_ffi_subscription_open(
                    fixture.endpoint,
                    route.as_ptr(),
                    topic.as_ptr(),
                    options.as_ptr(),
                    &5000,
                    c,
                    u,
                )
            })
        };
        let invalid = error_details(open(c"state", c"{\"start\":null}"));
        assert_eq!(
            invalid["message"],
            "runtime configuration error: invalid subscription start"
        );

        let opened = open(c"state", c"{}");
        assert_eq!(metadata(opened), json!({"startSeq":"1", "topic":"state"}));
        let sub = std::mem::replace(&mut (*opened).subscription, ptr::null_mut());
        c2_mem_ffi_result_free(opened);
        assert_eq!(error_details(open(c"state", c"{}"))["code"], 717);
        let delivery = async_result(|c, u| c2_mem_ffi_subscription_next(sub, c, u));
        assert_eq!(metadata(delivery), json!({"seq":"1","dropped":"0"}));

        registration.close().unwrap();
        let details = error_details(async_result(|c, u| c2_mem_ffi_subscription_next(sub, c, u)));
        assert_eq!(details["code"], 718);
        assert_eq!(details["details"]["cause"], "route_unregistered");
        let receipt = async_result(|c, u| c2_mem_ffi_subscription_close(sub, &5000, c, u));
        let terminal = metadata(receipt);
        assert_eq!(terminal["completed"], true);
        assert_eq!(terminal["completeItems"], "1");
        assert_eq!(terminal["error"]["code"], 718);
        c2_mem_ffi_result_free(receipt);
        c2_mem_ffi_subscription_free(sub);

        let mut data = ptr::null();
        let mut len = 0;
        assert_eq!(
            c2_mem_ffi_buffer_data((*delivery).buffer, &mut data, &mut len),
            C2MemFfiStatus::Ok
        );
        assert_eq!(std::slice::from_raw_parts(data, len), b"boot");
        c2_mem_ffi_result_free(delivery);
    }
}

#[test]
fn relay_endpoints_prepare_concurrently_and_share_one_client_domain() {
    use c2_http::relay::{RelayConfig, RelayServer};

    fn start_relay(
        runtime: &Runtime,
        host: &Host,
        route: &str,
        server_id: &str,
    ) -> (RelayServer, CString) {
        let relay = RelayServer::start_resolved(
            RelayConfig {
                bind: "127.0.0.1:0".into(),
                ..Default::default()
            },
            runtime.local_endpoint_context().unwrap(),
        )
        .unwrap();
        relay
            .register_upstream(route, server_id, host.local_endpoint().address())
            .unwrap();
        let url = CString::new(format!("http://{}", relay.local_addr())).unwrap();
        (relay, url)
    }
    fn host(_root: &std::path::Path, id: &str) -> (Runtime, Host) {
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions {
                    #[cfg(unix)]
                    unix_root: Some(_root.to_owned()),
                    ..Default::default()
                },
                ConfigSources::empty(),
            )
            .unwrap();
        let host = runtime
            .host(HostOptions::default().with_server_id(id).without_relay())
            .unwrap();
        (runtime, host)
    }

    let _process_state = crate::tests::PROCESS_STATE_LOCK.lock().unwrap();
    let local_root = tempfile::tempdir().unwrap();
    let remote_root = tempfile::tempdir().unwrap();
    let (local_runtime, local_host) = host(local_root.path(), "ffi-local");
    let (remote_runtime, remote_host) = host(remote_root.path(), "ffi-remote");
    let release = topic_release();
    let _local = local_host
        .register(topic_definition(&release, "local"))
        .unwrap();
    let _remote = remote_host
        .register(topic_definition(&release, "remote"))
        .unwrap();
    let (_first, first_url) = start_relay(&local_runtime, &local_host, "local", "ffi-local");
    let (_second, second_url) = start_relay(&remote_runtime, &remote_host, "remote", "ffi-remote");

    let context = C2MemFfiLocalEndpointContext {
        inner: local_runtime.local_endpoint_context().unwrap(),
    };
    let mut domain = ptr::null_mut();
    let mut error = ptr::null_mut();
    unsafe {
        assert_eq!(
            c2_mem_ffi_client_domain_new(
                &context,
                c"{\"maxOutstandingCalls\":3}".as_ptr(),
                &mut domain,
                &mut error,
            ),
            C2MemFfiStatus::Ok
        );
        let endpoint = |address: &CStr, options: &CStr| {
            let mut endpoint = ptr::null_mut();
            let mut error = ptr::null_mut();
            assert_eq!(
                c2_mem_ffi_endpoint_context_new(
                    domain,
                    address.as_ptr(),
                    options.as_ptr(),
                    &mut endpoint,
                    &mut error,
                ),
                C2MemFfiStatus::Ok
            );
            endpoint
        };
        let local = endpoint(&first_url, c"{}") as usize;
        let remote = endpoint(&second_url, c"{\"mode\":\"relay_aware\"}") as usize;
        let explicit = endpoint(&first_url, c"{\"mode\":\"explicit_relay\"}") as usize;
        let acquire = |endpoint: usize, route: &CStr| {
            let contract = topic_contract(&release, route.to_str().unwrap());
            let value = async_result(|c, u| {
                c2_mem_ffi_endpoint_context_prepare(
                    endpoint as *const _,
                    route.as_ptr(),
                    contract.as_ptr(),
                    &5000,
                    c,
                    u,
                )
            });
            let prepared = metadata(value);
            c2_mem_ffi_result_free(value);
            prepared
        };
        std::thread::scope(|threads| {
            let first = threads.spawn(|| acquire(local, c"local"));
            let second = threads.spawn(|| acquire(remote, c"remote"));
            assert_eq!(first.join().unwrap()["path"], "RelayAwareLocalIpc");
            // Windows endpoints in this process share the same TokenUser namespace.
            let remote_path = if cfg!(windows) {
                "RelayAwareLocalIpc"
            } else {
                "RelayAwareHttp"
            };
            assert_eq!(second.join().unwrap()["path"], remote_path);
        });
        assert_eq!(acquire(explicit, c"local")["path"], "ExplicitRelay");

        let open = |endpoint: usize, route: &CStr| {
            async_result(|c, u| {
                c2_mem_ffi_subscription_open(
                    endpoint as *const _,
                    route.as_ptr(),
                    c"state".as_ptr(),
                    c"{}".as_ptr(),
                    &5000,
                    c,
                    u,
                )
            })
        };
        let first = open(local, c"local");
        let second = open(remote, c"remote");
        let get_slots = || {
            let mut stats = ptr::null_mut();
            assert_eq!(
                c2_mem_ffi_client_domain_stats(domain, &mut stats),
                C2MemFfiStatus::Ok
            );
            let used = metadata(stats)["call_execution"]["used_operations"].clone();
            c2_mem_ffi_result_free(stats);
            used
        };
        assert_eq!(get_slots(), "2");
        let over_capacity = open(explicit, c"local");
        assert_eq!(error_details(over_capacity)["code"], 717);

        let deliver_and_close = |endpoint, route: &CStr, opened: *mut C2MemFfiResult| {
            assert_eq!(metadata(opened)["startSeq"], "1");
            let sub = (*opened).subscription;
            let item = async_result(|c, u| c2_mem_ffi_subscription_next(sub, c, u));
            assert_eq!(metadata(item), json!({"seq":"1","dropped":"0"}));
            assert_eq!((*(*item).buffer).0.bytes(), b"boot");
            c2_mem_ffi_result_free(item);

            let (tx, closed) = mpsc::channel::<usize>();
            assert_eq!(
                c2_mem_ffi_endpoint_context_close(
                    endpoint,
                    Some(complete),
                    Box::into_raw(Box::new(tx)).cast()
                ),
                C2MemFfiStatus::Ok
            );
            assert_eq!(
                closed.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            let refused = error_details(open(endpoint as usize, route));
            assert_eq!(refused["code"], 707);
            assert_eq!(refused["transportPhase"], "pre_dispatch");

            let terminal = async_result(|c, u| c2_mem_ffi_subscription_close(sub, &5000, c, u));
            let receipt = metadata(terminal);
            assert_eq!(receipt["completed"], true);
            assert_eq!(receipt["completeItems"], "1");
            assert_eq!(receipt["error"]["code"], 718);
            c2_mem_ffi_result_free(terminal);
            let result = closed.recv_timeout(WAIT).unwrap() as *mut C2MemFfiResult;
            success(result);
            c2_mem_ffi_result_free(result);
            c2_mem_ffi_result_free(opened);
        };
        deliver_and_close(local as *const _, c"local", first);
        deliver_and_close(remote as *const _, c"remote", second);
        deliver_and_close(explicit as *const _, c"local", open(explicit, c"local"));
        assert_eq!(get_slots(), "0");
        for handle in [local, remote, explicit] {
            c2_mem_ffi_endpoint_context_free(handle as *mut _);
        }
        c2_mem_ffi_client_domain_free(domain);
    }
    let _ = local_host.shutdown();
    let _ = remote_host.shutdown();
}

#[test]
fn abi_metadata_uses_strict_json_and_rejects_duplicate_decoded_keys() {
    for invalid_json in [
        c"{\"a\":\"1\",\"a\":\"2\"}",
        cr#"{"a":"1","\u0061":"2"}"#,
        c"null",
        c"[]",
        c"{\"a\":123}",
        c"{\"UPPER\":\"token\"}",
        c"{\"a\":\" trailing\"}",
    ] {
        assert!(
            parse_metadata(invalid_json.as_ptr()).is_err(),
            "{invalid_json:?}"
        );
    }
    assert!(parse_metadata(ptr::null()).unwrap().is_empty());
    assert_eq!(
        parse_metadata(cr#"{"a":"first","b":"second"}"#.as_ptr())
            .unwrap()
            .get("b"),
        Some("second")
    );
    assert!(
        serde_json::from_str::<SubscriptionPolicy>(r#"{"metadata":{"a":"1","a":"2"}}"#).is_err()
    );
    assert!(serde_json::from_str::<SubscriptionPolicy>(r#"{"metadata":null}"#).is_err());
}

#[test]
fn metadata_is_owned_at_the_abi_and_host_denial_is_projected() {
    use c2_core::{AdmissionContext, AdmissionDecision, AdmissionOperation};
    let (send, observations) = mpsc::channel();
    let fixture = Fixture::with_host_options(
        json!({}),
        HostOptions::default()
            .without_relay()
            .with_admission_hook(Arc::new(move |context: &AdmissionContext<'_>| {
                let token = context.metadata().get("token").map(str::to_owned);
                send.send((context.operation(), token.clone())).unwrap();
                if token.as_deref() == Some("allowed") {
                    AdmissionDecision::allow()
                } else {
                    AdmissionDecision::deny("missing token")
                }
            })),
    );
    let route = c"items".as_ptr();
    let id = b"probe";
    let query = |metadata_json, timeout| {
        async_result(|callback, user| unsafe {
            c2_mem_ffi_query_call(
                fixture.endpoint,
                route,
                id.as_ptr(),
                id.len(),
                timeout,
                metadata_json,
                callback,
                user,
            )
        })
    };
    let result = query(cr#"{"token":"allowed"}"#.as_ptr(), ptr::null());
    assert_eq!(unsafe { metadata(result) }["status"], "unknown");
    unsafe { c2_mem_ffi_result_free(result) };
    let refusal = unsafe { error_details(query(ptr::null(), ptr::null())) };
    assert_eq!(refusal["code"], 724);
    assert_eq!(refusal["canonicalName"], "AdmissionDenied");
    assert_eq!(refusal["transportPhase"], "pre_dispatch");
    let expired = unsafe { error_details(query(cr#"{"token":"allowed"}"#.as_ptr(), &0)) };
    assert_eq!(expired["code"], 715);
    assert_eq!(expired["transportPhase"], "pre_dispatch");
    assert_eq!(
        observations.try_iter().collect::<Vec<_>>(),
        [
            (
                AdmissionOperation::QueryOutcome,
                Some("allowed".to_string())
            ),
            (AdmissionOperation::QueryOutcome, None),
        ]
    );
}

#[test]
fn query_deadline_includes_time_queued_behind_a_blocking_worker() {
    let fixture = Fixture::new(64);
    let endpoint = &unsafe { &*fixture.endpoint }.0;
    let client = endpoint.bound_client("items").unwrap();
    let operation = endpoint.begin_operation().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocker = executor.spawn_blocking(move || {
        entered.send(()).unwrap();
        gate.recv().unwrap();
    });
    ready.recv_timeout(WAIT).unwrap();
    let limit = Duration::from_millis(50);
    executor.block_on(async {
        use std::task::Poll;
        let started = Instant::now();
        let mut query = Box::pin(query_bound_client(
            operation,
            client,
            "queued".into(),
            Some(limit),
            CallMetadata::empty(),
            started,
        ));
        std::future::poll_fn(|task| {
            assert!(query.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::sleep_until((started + limit).into()).await;
        release.send(()).unwrap();
        blocker.await.unwrap();
        let error = query.await.unwrap_err();
        assert_eq!(error_value(&error)["code"], 715);
        assert_eq!(error.transport_phase(), Some(TransportPhase::PreDispatch));
    });
}
