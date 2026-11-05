use super::*;
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, Host, HostOptions,
    MethodDefinition, Registration, ServiceConcurrencyMode, ServiceDefinition,
};
use std::sync::mpsc;

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
        Self::with_policy(budget, json!({}))
    }
    fn with_policy(budget: u64, policy: Value) -> Self {
        let process_state = crate::tests::PROCESS_STATE_LOCK.lock().unwrap();
        assert_eq!(c2_mem_ffi_abi_version(), 8);
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
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
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
        let policy = CString::new(policy.to_string()).unwrap();
        assert_eq!(
            unsafe {
                c2_mem_ffi_client_domain_new(&context, policy.as_ptr(), &mut domain, &mut error)
            },
            C2MemFfiStatus::Ok
        );
        // Set the existing domain's native policy before its first call, without process env mutation.
        let domain_runtime = unsafe { &*domain }.0.runtime.clone();
        domain_runtime
            .set_call_execution_limits_with_sources(
                c2_core::CallExecutionLimitsOverrides {
                    payload_buffer_budget_bytes: Some(budget),
                    ..Default::default()
                },
                ConfigSources::empty(),
            )
            .unwrap();
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
            success(result);
            c2_mem_ffi_result_free(result);
        }
        call
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
fn factory_transport_policy_is_fixed_before_endpoints_and_reused_on_reconnect() {
    let mut fixture = Fixture::with_policy(
        128,
        json!({"requestShmThreshold": 123, "requestChunkSize": 64}),
    );
    let runtime = &unsafe { &*fixture.domain }.0.runtime;
    assert_eq!(runtime.shm_threshold_override(), Some(123));
    assert_eq!(runtime.client_ipc_overrides().unwrap().chunk_size, Some(64));
    let address = CString::new(fixture.host.local_endpoint().address()).unwrap();
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let expected = release.expected_route("items").unwrap();
    let contract = CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let mut connection_ids = Vec::new();
    for _ in 0..2 {
        let endpoint = unsafe { &*fixture.endpoint };
        let connection = endpoint.0.connection.lock();
        let config = connection.as_ref().unwrap().config();
        assert_eq!(config.shm_threshold, 123);
        assert_eq!(config.chunk_size, 64);
        drop(connection);
        let prepared = async_result(|c, u| unsafe {
            c2_mem_ffi_endpoint_context_prepare(
                fixture.endpoint,
                route.as_ptr(),
                contract.as_ptr(),
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
        c2_mem_ffi_endpoint_context_prepare(second, route.as_ptr(), contract.as_ptr(), c, u)
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
    let options = CString::new(r#"{"expectedRouteToken":{"routeUid":"wrong-uid"}}"#).unwrap();
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
    let expected = release.expected_route("items").unwrap();
    let contract=CString::new(json!({"namespace":expected.crm_ns,"name":expected.crm_name,"version":expected.crm_ver,"abiHash":expected.abi_hash,"signatureHash":expected.signature_hash}).to_string()).unwrap();
    let route = CString::new("items").unwrap();
    let rejected = async_result(|c, u| unsafe {
        c2_mem_ffi_endpoint_context_prepare(endpoint, route.as_ptr(), contract.as_ptr(), c, u)
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
