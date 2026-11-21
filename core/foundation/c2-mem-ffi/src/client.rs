//! ABI 9: native client calls and topic subscriptions.
use super::*;
use c2_contract::ExpectedRouteContract;
use c2_core::{
    CallDispatchTarget, CallOptions, CallRecordOutcome, CallScope, CallState, CallTimeout, Client,
    Connect, CoreResponseStream, CoreTopicSubscription, Error, HeldResponse, LagPolicy,
    LifecycleError, ObservedPath, PreparedCall, Runtime, RuntimeOptions, SubscribeOptions,
    TopicStart, TransportPhase,
};
use c2_error::{C2Error, ErrorCode};
use c2_ipc::IpcStreamCancellation;
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::CString;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub struct C2MemFfiClientDomain(Arc<Domain>);
struct Domain {
    runtime: Runtime,
}
pub struct C2MemFfiEndpointContext(Arc<Endpoint>);
struct Endpoint {
    domain: Arc<Domain>,
    mode: Connect,
    connect_timeout: Option<u64>,
    expected_identity: Option<c2_core::ServerIdentity>,
    closed: AtomicBool,
    active: AtomicUsize,
    changed: Notify,
    bindings: Mutex<Bindings>,
    connection: Mutex<Option<Arc<c2_core::NativeEndpointContext>>>,
}
#[derive(Default)]
struct Bindings {
    clients: HashMap<String, Client>,
    observations: HashMap<String, (c2_core::ServerIdentity, c2_core::ConnectionObservation)>,
}
pub struct C2MemFfiCall(Arc<Call>);
struct Call {
    endpoint: Arc<Endpoint>,
    state: Mutex<CallData>,
    scope: CallScope,
    stream_mode: bool,
    cancellation: IpcStreamCancellation,
    cancelled: AtomicBool,
    next_active: AtomicBool,
    changed: Notify,
}
struct CallData {
    prepared: Option<PreparedCall>,
    charged: u64,
    busy: bool,
    dispatched: bool,
    ready: bool,
    stream: Option<Arc<CoreResponseStream>>,
    stream_operation: Option<EndpointOperation>,
    unary: Option<HeldResponse>,
    dispatch_target: Option<CallDispatchTarget>,
    error: Option<Value>,
    consumer_done: bool,
}
pub struct C2MemFfiSubscription(Arc<Subscription>);
struct Subscription {
    core: CoreTopicSubscription,
    operation: Mutex<Option<EndpointOperation>>,
    next_active: AtomicBool,
    consumer_done: AtomicBool,
}

struct SubscriptionNext(Arc<Subscription>);
impl Drop for SubscriptionNext {
    fn drop(&mut self) {
        self.0.next_active.store(false, Ordering::Release);
    }
}

pub struct C2MemFfiBuffer(HeldResponse);
pub struct C2MemFfiError(CString);
#[repr(C)]
pub struct C2MemFfiResult {
    pub buffer: *mut C2MemFfiBuffer,
    pub error: *mut C2MemFfiError,
    pub json: *const c_char,
    pub subscription: *mut C2MemFfiSubscription,
}
pub type C2MemFfiComplete =
    Option<unsafe extern "C" fn(*mut std::ffi::c_void, *mut C2MemFfiResult)>;
enum Output {
    Empty,
    Json(Value),
    Buffer(HeldResponse),
    Subscription(Arc<Subscription>),
    Delivery(HeldResponse, Value),
    Error(Value),
}

fn invalid(message: impl Into<String>) -> Error {
    LifecycleError::Configuration(message.into()).into()
}
fn refused(code: ErrorCode, message: impl Into<String>) -> Error {
    C2Error::new(code, message)
        .with_details(std::collections::BTreeMap::from([(
            "transport_phase".into(),
            "pre_dispatch".into(),
        )]))
        .into()
}
fn error_value(error: &Error) -> Value {
    let mut value = json!({"message": error.to_string()});
    if let Some(phase) = error.transport_phase() {
        value["transportPhase"] = json!(match phase {
            TransportPhase::PreDispatch => "pre_dispatch",
            TransportPhase::DispatchUncertain => "dispatch_uncertain",
        });
    }
    let semantic = match error {
        Error::Semantic(error) => Some(error),
        Error::Transport(error) => {
            value["category"] = json!(match error.kind() {
                c2_core::TransportKind::Ipc => "ipc",
                c2_core::TransportKind::Http => "http",
            });
            error.semantic_error()
        }
        Error::ProtocolVersionMismatch { local, peer } => {
            value["category"] = json!("protocol_version_mismatch");
            value["localProtocolVersion"] = json!(local);
            value["peerProtocolVersion"] = json!(peer);
            None
        }
        Error::Lifecycle(LifecycleError::RuntimeRetired) => {
            value["category"] = json!("runtime_retired");
            None
        }
        Error::Contract(_) | Error::Admission(_) | Error::Lifecycle(_) => None,
    };
    if let Some(error) = semantic {
        value["message"] = json!(error.message);
        value["code"] = json!(error.code as u32);
        value["canonicalName"] = json!(error.code.name());
        value["details"] = json!(error.details);
    }
    if let Some(cause) = error.local_io_cause() {
        value["ioOperation"] = json!(cause.operation());
        value["ioKind"] = json!(format!("{:?}", cause.error().kind));
        value["rawOsError"] = json!(cause.error().raw_os_error);
    }
    value
}
fn call_error_value(call: &Call, error: &Error) -> Value {
    let mut value = error_value(error);
    if matches!(error, Error::Lifecycle(_)) {
        let phase = match call.scope.state() {
            CallState::PreDispatch
            | CallState::FailedPreDispatch
            | CallState::ExpiredPreDispatch => "pre_dispatch",
            CallState::Dispatched
            | CallState::FailedDispatchUncertain
            | CallState::ExpiredDispatchUncertain => "dispatch_uncertain",
            CallState::Succeeded => return value,
        };
        value["transportPhase"] = json!(phase);
    }
    value
}
fn owned_error(value: Value) -> *mut C2MemFfiError {
    Box::into_raw(Box::new(C2MemFfiError(
        CString::new(value.to_string()).expect("JSON escapes NUL"),
    )))
}
fn result(output: Result<Output, Error>) -> *mut C2MemFfiResult {
    let mut result = C2MemFfiResult {
        buffer: ptr::null_mut(),
        error: ptr::null_mut(),
        json: ptr::null(),
        subscription: ptr::null_mut(),
    };
    match output {
        Ok(Output::Empty) => {}
        Ok(Output::Json(value)) => {
            result.json = CString::new(value.to_string()).expect("JSON").into_raw()
        }
        Ok(Output::Buffer(buffer)) => {
            result.buffer = Box::into_raw(Box::new(C2MemFfiBuffer(buffer)))
        }
        Ok(Output::Subscription(sub)) => {
            let core = &sub.core;
            let metadata =
                json!({"startSeq": core.start_seq().to_string(), "topic": core.topic_name()});
            result.json = CString::new(metadata.to_string()).expect("JSON").into_raw();
            result.subscription = Box::into_raw(Box::new(C2MemFfiSubscription(sub)));
        }
        Ok(Output::Delivery(buffer, metadata)) => {
            result.json = CString::new(metadata.to_string()).expect("JSON").into_raw();
            result.buffer = Box::into_raw(Box::new(C2MemFfiBuffer(buffer)));
        }
        Ok(Output::Error(error)) => result.error = owned_error(error),
        Err(error) => result.error = owned_error(error_value(&error)),
    }
    Box::into_raw(Box::new(result))
}
fn launch(
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
    future: impl Future<Output = Result<Output, Error>> + Send + 'static,
) -> C2MemFfiStatus {
    launch_scoped(None, complete, user, future)
}
fn launch_call(
    call: Arc<Call>,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
    future: impl Future<Output = Result<Output, Error>> + Send + 'static,
) -> C2MemFfiStatus {
    launch_scoped(Some(call), complete, user, future)
}
fn launch_scoped(
    call: Option<Arc<Call>>,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
    future: impl Future<Output = Result<Output, Error>> + Send + 'static,
) -> C2MemFfiStatus {
    let Some(complete) = complete else {
        return C2MemFfiStatus::NullPointer;
    };
    let user = user as usize;
    let handle = match c2_core::native_client_executor() {
        Ok(handle) => handle,
        Err(error) => {
            unsafe {
                complete(user as *mut _, scoped_result(Err(error), call.as_deref()));
            }
            return C2MemFfiStatus::Ok;
        }
    };
    handle.spawn(async move {
        let output = AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(invalid("native client operation panicked")));
        unsafe {
            complete(user as *mut _, scoped_result(output, call.as_deref()));
        }
    });
    C2MemFfiStatus::Ok
}
fn scoped_result(output: Result<Output, Error>, call: Option<&Call>) -> *mut C2MemFfiResult {
    match (output, call) {
        (Err(error), Some(call)) => result(Ok(Output::Error(call_error_value(call, &error)))),
        (output, _) => result(output),
    }
}
unsafe fn native<T>(
    error: *mut *mut C2MemFfiError,
    action: impl FnOnce() -> Result<T, Error>,
) -> Result<T, C2MemFfiStatus> {
    unsafe { native_with_call(None, error, action) }
}
unsafe fn native_with_call<T>(
    call: Option<&Call>,
    error: *mut *mut C2MemFfiError,
    action: impl FnOnce() -> Result<T, Error>,
) -> Result<T, C2MemFfiStatus> {
    if error.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    unsafe {
        *error = ptr::null_mut();
    }
    match catch_unwind(AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err(invalid("native client boundary panicked")))
    {
        Ok(value) => Ok(value),
        Err(cause) => {
            unsafe {
                *error = owned_error(match call {
                    Some(call) => call_error_value(call, &cause),
                    None => error_value(&cause),
                });
            }
            Err(C2MemFfiStatus::InvalidArgument)
        }
    }
}
fn text(value: *const c_char) -> Result<String, Error> {
    parse_utf8_c_string(value).map_err(|_| invalid("expected a non-null UTF-8 string"))
}
fn parse(value: *const c_char) -> Result<Value, Error> {
    serde_json::from_str(&text(value)?).map_err(|e| invalid(e.to_string()))
}
fn parse_options(value: *const c_char, allowed: &[&str]) -> Result<Value, Error> {
    let value = parse(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("options must be an object"))?;
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid(format!("unknown option {key}")));
        }
    }
    Ok(value)
}
fn unsigned_option(options: &Value, key: &str) -> Result<Option<u64>, Error> {
    options
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid(format!("invalid {key}")))
        })
        .transpose()
}
fn domain_limit_option(options: &Value, key: &str) -> Result<Option<u64>, Error> {
    let Some(text) = options.get(key).and_then(Value::as_str) else {
        return unsigned_option(options, key);
    };
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(value) = text.parse() {
            return Ok(Some(value));
        }
    }
    Err(invalid(format!("invalid {key}")))
}

fn memory_cell_json(cell: c2_core::MemoryCellStats) -> Value {
    json!({
        "limit_bytes": cell.limit_bytes.to_string(),
        "used_bytes": cell.used_bytes.to_string(),
        "peak_bytes": cell.peak_bytes.to_string(),
        "rejected_allocations": cell.rejected_allocations.to_string(),
        "rejected_bytes": cell.rejected_bytes.to_string(),
    })
}

fn field(value: &Value, key: &str) -> Result<String, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("missing text field {key}")))
}
fn timeout(value: *const u64) -> Option<Duration> {
    if value.is_null() {
        None
    } else {
        Some(Duration::from_millis(unsafe { *value }))
    }
}

struct EndpointOperation(Arc<Endpoint>);
impl Drop for EndpointOperation {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
impl Endpoint {
    fn deactivate(&self) {
        {
            let _bindings = self.bindings.lock();
            self.closed.store(true, Ordering::Release);
        }
        if let Some(connection) = self
            .connection
            .try_lock()
            .and_then(|connection| connection.clone())
        {
            connection.deactivate();
        }
    }
    fn require_open(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(refused(
                ErrorCode::ResourceClosed,
                "endpoint is deactivated",
            ));
        }
        Ok(())
    }

    fn begin_operation(self: &Arc<Self>) -> Result<EndpointOperation, Error> {
        let _bindings = self.bindings.lock();
        self.require_open()?;
        self.active.fetch_add(1, Ordering::AcqRel);
        Ok(EndpointOperation(self.clone()))
    }

    fn prepare(
        &self,
        route: String,
        contract: Value,
        attempt: c2_core::ConnectAttempt,
    ) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(refused(
                ErrorCode::ResourceClosed,
                "endpoint is deactivated",
            ));
        }
        let runtime = &self.domain.runtime;
        let expected = ExpectedRouteContract {
            route_name: route.clone(),
            crm_ns: field(&contract, "namespace")?,
            crm_name: field(&contract, "name")?,
            crm_ver: field(&contract, "version")?,
            abi_hash: field(&contract, "abiHash")?,
            signature_hash: field(&contract, "signatureHash")?,
        };
        let client = if let Connect::DirectIpc { address } = &self.mode {
            let connection = {
                let mut connection = self.connection.lock();
                if connection.is_none() {
                    *connection = Some(Arc::new(runtime.native_endpoint_context(
                        address,
                        self.expected_identity.clone(),
                        &attempt,
                    )?));
                }
                connection.as_ref().unwrap().clone()
            };
            if self.closed.load(Ordering::Acquire) {
                connection.deactivate();
                return Err(refused(
                    ErrorCode::ResourceClosed,
                    "endpoint is deactivated",
                ));
            }
            connection.prepare(expected, &attempt)?
        } else {
            runtime.connect_with_attempt(expected, self.mode.clone(), &attempt)?
        };
        let ipc = client
            .ipc_server_identity()
            .map(|identity| Ok::<_, Error>((identity, client.connection_observation()?)))
            .transpose()?;
        let mut prepared = target_json(
            client.observed_path(),
            ipc.as_ref()
                .map(|(identity, observation)| (identity, observation.connection_id())),
        );
        if matches!(self.mode, Connect::DirectIpc { .. }) {
            prepared
                .as_object_mut()
                .expect("target JSON object")
                .remove("path");
        }
        let mut bindings = self.bindings.lock();
        if self.closed.load(Ordering::Acquire) {
            return Err(refused(
                ErrorCode::ResourceClosed,
                "endpoint is deactivated",
            ));
        }
        if let Some(old) = bindings.clients.get(&route) {
            if old.expected_route() != client.expected_route() {
                return Err(refused(
                    ErrorCode::ContractMismatch,
                    "route contract changed",
                ));
            }
        }
        bindings.observations.remove(&route);
        if let Some(ipc) = ipc {
            bindings.observations.insert(route.clone(), ipc);
        }
        bindings.clients.insert(route, client);
        Ok(prepared)
    }

    fn bound_client(&self, route: &str) -> Result<Client, Error> {
        let bindings = self.bindings.lock();
        self.require_open()?;
        bindings
            .clients
            .get(route)
            .cloned()
            .ok_or_else(|| invalid("route must be prepared before starting an operation"))
    }

    async fn close(&self) -> Result<(), Error> {
        self.deactivate();
        self.bindings.lock().clients.clear();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                break;
            }
            changed.await;
        }
        let connection = self.connection.lock().clone();
        if let Some(connection) = connection {
            connection.close().await;
        }
        Ok(())
    }
}
impl Call {
    fn cancel(&self) {
        if !self.stream_mode {
            return;
        }
        self.cancelled.store(true, Ordering::Release);
        self.cancellation.cancel();
        let mut state = self.state.lock();
        if !state.dispatched && !state.busy && !state.ready {
            state.prepared.take();
            state.error = Some(call_error_value(
                self,
                &refused(ErrorCode::CallCancelled, "stream cancelled before dispatch"),
            ));
            state.ready = true;
            let _ = self.scope.complete_failure();
        }
        self.changed.notify_waiters();
    }
    fn drain_cancelled_stream(&self) {
        let mut state = self.state.lock();
        if let Some(operation) = state.stream_operation.take() {
            let stream = state.stream.as_ref().expect("active stream").clone();
            let executor = c2_core::native_client_executor().expect("running stream executor");
            executor.spawn(async move {
                let _operation = operation;
                stream.close_with_timeout_async(None).await;
            });
        }
    }

    async fn ready(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().ready {
                return;
            }
            changed.await;
        }
    }
    async fn ready_for_caller(&self) -> Result<(), Error> {
        self.check_deadline()?;
        match self.scope.deadline() {
            Some(deadline) => {
                if tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), self.ready())
                    .await
                    .is_err()
                {
                    self.check_deadline()?;
                }
            }
            None => self.ready().await,
        }
        self.check_deadline()
    }
    fn check_deadline(&self) -> Result<(), Error> {
        let phase = match self.scope.observe_deadline() {
            CallState::ExpiredPreDispatch => "pre_dispatch",
            CallState::ExpiredDispatchUncertain => "dispatch_uncertain",
            _ => return Ok(()),
        };
        Err(
            C2Error::new(ErrorCode::CallDeadlineExceeded, "call deadline exceeded")
                .with_details(std::collections::BTreeMap::from([(
                    "transport_phase".into(),
                    phase.into(),
                )]))
                .into(),
        )
    }
    async fn observe(&self, limit: Option<Duration>) -> Result<Output, Error> {
        self.cancel();
        let start = Instant::now();
        let waiting = match limit {
            Some(Duration::ZERO) => !self.state.lock().ready,
            Some(limit) => tokio::time::timeout(limit, self.ready()).await.is_err(),
            None => {
                self.ready().await;
                false
            }
        };
        if waiting {
            return Ok(Output::Json(
                json!({"completed":false,"error":null,"completeItems":null}),
            ));
        }
        let (stream, error) = {
            let state = self.state.lock();
            (state.stream.clone(), state.error.clone())
        };
        if let Some(stream) = stream {
            let receipt = stream
                .close_with_timeout_async(limit.map(|l| l.saturating_sub(start.elapsed())))
                .await;
            if receipt.completed {
                self.state.lock().stream_operation.take();
            } else {
                self.drain_cancelled_stream();
            }
            let error = receipt
                .terminal
                .and_then(Result::err)
                .as_ref()
                .map(|error| call_error_value(self, error));
            Ok(Output::Json(
                json!({"completed":receipt.completed,"error":error,"completeItems":receipt.complete_items.map(|v|v.to_string())}),
            ))
        } else {
            Ok(Output::Json(
                json!({"completed":true,"error":error,"completeItems":null}),
            ))
        }
    }
}

/// # Safety
/// Inputs obey the ownership and callback rules in the ABI 9 C header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_client_domain_new(
    context: *const C2MemFfiLocalEndpointContext,
    options: *const c_char,
    out: *mut *mut C2MemFfiClientDomain,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if out.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    match unsafe {
        native(error, || {
            let options = parse_options(
                options,
                &[
                    "memoryBudgetBytes",
                    "fileSpillBudgetBytes",
                    "maxOutstandingCalls",
                ],
            )?;
            let client_limits = c2_config::DomainLimitsOverrides {
                memory_budget_bytes: domain_limit_option(&options, "memoryBudgetBytes")?,
                file_spill_budget_bytes: domain_limit_option(&options, "fileSpillBudgetBytes")?,
                max_outstanding_calls: domain_limit_option(&options, "maxOutstandingCalls")?,
            };
            let endpoint = if context.is_null() {
                capture_local_endpoint_context(None)
                    .map_err(|_| invalid("endpoint configuration failed"))?
            } else {
                (*context).inner.clone()
            };
            let runtime = Runtime::new(RuntimeOptions {
                use_process_relay_anchor: false,
                client_limits,
                ..Default::default()
            })?;
            runtime.set_captured_local_endpoint(endpoint)?;
            Ok(C2MemFfiClientDomain(Arc::new(Domain { runtime })))
        })
    } {
        Ok(domain) => {
            unsafe {
                *out = Box::into_raw(Box::new(domain));
            }
            C2MemFfiStatus::Ok
        }
        Err(status) => status,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_client_domain_free(domain: *mut C2MemFfiClientDomain) {
    if !domain.is_null() {
        drop(unsafe { Box::from_raw(domain) });
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_client_domain_stats(
    domain: *const C2MemFfiClientDomain,
    out: *mut *mut C2MemFfiResult,
) -> C2MemFfiStatus {
    guard_status(|| {
        if domain.is_null() || out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let runtime = &unsafe { &*domain }.0.runtime;
        let memory = runtime.memory_stats().client;
        // An activation racing this read must not populate the call
        // counters of the dormant role captured by the memory snapshot.
        let calls = if memory.frozen {
            runtime.call_execution_snapshot().client
        } else {
            Default::default()
        };
        let decimal = |value: u64| value.to_string();
        let stats = json!({
            "domain_id": memory.domain_id.map(decimal),
            "generation": memory.generation.to_string(),
            "frozen": memory.frozen,
            "closed": memory.closed,
            "memory": memory.memory.map(memory_cell_json),
            "file_spill": memory.file_spill.map(memory_cell_json),
            "call_execution": {
                "max_operations": calls.max_operations.map(decimal),
                "used_operations": calls.used_operations.map(decimal),
                "peak_operations": calls.peak_operations.map(decimal),
                "rejected_reservations": calls.rejected_reservations.map(decimal),
                "closed": calls.closed,
            },
        });
        unsafe {
            *out = result(Ok(Output::Json(stats)));
        }
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_new(
    domain: *const C2MemFfiClientDomain,
    address: *const c_char,
    options: *const c_char,
    out: *mut *mut C2MemFfiEndpointContext,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if domain.is_null() || out.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    match unsafe {
        native(error, || {
            let address = text(address)?;
            let options = parse_options(
                options,
                &["connectTimeoutMs", "expectedServerIdentity", "mode"],
            )?;
            let mode = match (address.split_once("://"), options.get("mode")) {
                (Some(("ipc", name)), None) if !name.is_empty() => Connect::DirectIpc { address },
                (Some(("http" | "https", authority)), mode) if !authority.is_empty() => {
                    match mode {
                        None => Connect::RelayAwareAt {
                            anchor_url: address,
                        },
                        Some(value) if value == "relay_aware" => Connect::RelayAwareAt {
                            anchor_url: address,
                        },
                        Some(value) if value == "explicit_relay" => {
                            Connect::ExplicitRelay { relay_url: address }
                        }
                        _ => return Err(invalid("invalid endpoint mode")),
                    }
                }
                _ => {
                    return Err(invalid(
                        "expected an ipc:// endpoint without mode or an http(s):// endpoint",
                    ));
                }
            };
            let expected_identity = options
                .get("expectedServerIdentity")
                .map(|value| {
                    if !matches!(mode, Connect::DirectIpc { .. }) {
                        return Err(invalid(
                            "expectedServerIdentity requires an ipc:// endpoint",
                        ));
                    }
                    if value.as_object().is_none_or(|fields| fields.len() != 2) {
                        return Err(invalid("invalid expectedServerIdentity"));
                    }
                    Ok(c2_core::ServerIdentity {
                        server_id: field(value, "serverId")?,
                        server_instance_id: field(value, "serverInstanceId")?,
                    })
                })
                .transpose()?;
            Ok(C2MemFfiEndpointContext(Arc::new(Endpoint {
                domain: (*domain).0.clone(),
                mode,
                connect_timeout: unsigned_option(&options, "connectTimeoutMs")?,
                expected_identity,
                closed: AtomicBool::new(false),
                active: AtomicUsize::new(0),
                changed: Notify::new(),
                bindings: Mutex::new(Bindings::default()),
                connection: Mutex::new(None),
            })))
        })
    } {
        Ok(endpoint) => {
            unsafe {
                *out = Box::into_raw(Box::new(endpoint));
            }
            C2MemFfiStatus::Ok
        }
        Err(status) => status,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_prepare(
    endpoint: *const C2MemFfiEndpointContext,
    route: *const c_char,
    contract: *const c_char,
    timeout_millis: *const u64,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if endpoint.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let endpoint = unsafe { &*endpoint }.0.clone();
    let args = text(route).and_then(|route| {
        let contract = parse(contract)?;
        let mut options = c2_core::ConnectOptions::new();
        let configured = endpoint.connect_timeout;
        let per_call = if timeout_millis.is_null() {
            None
        } else {
            Some(unsafe { *timeout_millis })
        };
        if let Some(millis) = configured.into_iter().chain(per_call).min() {
            options = options.with_timeout(Duration::from_millis(millis));
        }
        let attempt = c2_core::ConnectAttempt::start(options)?;
        Ok((route, contract, attempt, endpoint.begin_operation()?))
    });
    launch(complete, user, async move {
        let (route, contract, attempt, operation) = args?;
        tokio::task::spawn_blocking(move || {
            let _operation = operation;
            match endpoint.prepare(route, contract, attempt) {
                Ok(value) => Ok(Output::Json(value)),
                Err(error) => {
                    let mut value = error_value(&error);
                    value["transportPhase"] = json!("pre_dispatch");
                    Ok(Output::Error(value))
                }
            }
        })
        .await
        .map_err(|e| invalid(e.to_string()))?
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_connection_snapshot(
    endpoint: *const C2MemFfiEndpointContext,
    route: *const c_char,
    out: *mut *mut C2MemFfiResult,
) -> C2MemFfiStatus {
    guard_status(|| {
        if endpoint.is_null() || out.is_null() || route.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = ptr::null_mut();
        }
        let route = text(route).map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        let binding = unsafe { &*endpoint }
            .0
            .bindings
            .lock()
            .observations
            .get(&route)
            .cloned();
        let output = binding.map(|(identity, observation)| {
            let end = observation
                .snapshot()
                .map(|closed| closed.reason().as_str());
            Output::Json(json!({
                "connectionId": observation.connection_id().to_string(),
                "serverId": identity.server_id,
                "serverInstanceId": identity.server_instance_id,
                "endReason": end,
            }))
        });
        unsafe {
            *out = result(Ok(output.unwrap_or(Output::Empty)));
        }
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_deactivate(
    endpoint: *const C2MemFfiEndpointContext,
) -> C2MemFfiStatus {
    if endpoint.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    unsafe { &*endpoint }.0.deactivate();
    C2MemFfiStatus::Ok
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_close(
    endpoint: *const C2MemFfiEndpointContext,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if endpoint.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let endpoint = unsafe { &*endpoint }.0.clone();
    endpoint.deactivate();
    launch(complete, user, async move {
        endpoint.close().await?;
        Ok(Output::Empty)
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_endpoint_context_free(endpoint: *mut C2MemFfiEndpointContext) {
    if !endpoint.is_null() {
        let endpoint = unsafe { Box::from_raw(endpoint) };
        endpoint.0.deactivate();
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_begin(
    endpoint: *const C2MemFfiEndpointContext,
    route: *const c_char,
    method: *const c_char,
    stream: u8,
    limit: *const u64,
    out: *mut *mut C2MemFfiCall,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if endpoint.is_null() || out.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    match unsafe {
        native(error, || {
            let endpoint = &(*endpoint).0;
            if stream > 1 {
                return Err(invalid("invalid call mode"));
            }
            let route = text(route)?;
            let method = text(method)?;
            let _operation = endpoint.begin_operation()?;
            let client = endpoint.bound_client(&route)?;
            let client = client.with_call_options(CallOptions::with_timeout(
                timeout(limit).map_or(CallTimeout::Unlimited, CallTimeout::After),
            ));
            let prepared = if stream == 1 {
                client.begin_stream(&method)?
            } else {
                client.begin_call(&method)?
            };
            let scope = prepared.scope();
            Ok(C2MemFfiCall(Arc::new(Call {
                endpoint: endpoint.clone(),
                state: Mutex::new(CallData {
                    prepared: Some(prepared),
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
                scope,
                stream_mode: stream == 1,
                cancellation: IpcStreamCancellation::new(),
                cancelled: AtomicBool::new(false),
                next_active: AtomicBool::new(false),
                changed: Notify::new(),
            })))
        })
    } {
        Ok(call) => {
            unsafe {
                *out = Box::into_raw(Box::new(call));
            }
            C2MemFfiStatus::Ok
        }
        Err(status) => status,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_charge_input(
    call: *const C2MemFfiCall,
    nbytes: u64,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let call = unsafe { &*call }.0.clone();
    if complete.is_none() {
        return C2MemFfiStatus::NullPointer;
    }
    let operation = call.endpoint.begin_operation();
    let prepared = operation.and_then(|operation| {
        let mut state = call.state.lock();
        let prepared = state
            .prepared
            .take()
            .ok_or_else(|| invalid("call is not awaiting input"));
        if prepared.is_ok() {
            state.busy = true;
        }
        prepared.map(|prepared| (prepared, operation))
    });
    launch_call(call.clone(), complete, user, async move {
        let (mut prepared, _operation) = prepared?;
        let cancelled = call.changed.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        let charged = if call.cancelled.load(Ordering::Acquire) {
            Err(refused(
                ErrorCode::CallCancelled,
                "stream cancelled before dispatch",
            ))
        } else {
            tokio::select! {
                biased;
                _ = cancelled => Err(refused(ErrorCode::CallCancelled,"stream cancelled before dispatch")),
                result = prepared.charge_input_async(nbytes) => result,
            }
        };
        let mut state = call.state.lock();
        state.busy = false;
        if call.cancelled.load(Ordering::Acquire) {
            state.ready = true;
            state.error = Some(call_error_value(
                &call,
                &refused(ErrorCode::CallCancelled, "stream cancelled before dispatch"),
            ));
            let _ = call.scope.complete_failure();
            call.changed.notify_waiters();
            return charged.map(|_| Output::Empty);
        }
        match charged {
            Ok(()) => {
                state.charged = nbytes;
                state.prepared = Some(prepared);
                Ok(Output::Empty)
            }
            Err(error) => {
                state.ready = true;
                state.error = Some(call_error_value(&call, &error));
                call.changed.notify_waiters();
                Err(error)
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_dispatch(
    call: *const C2MemFfiCall,
    data: *const u8,
    len: usize,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if complete.is_none() || call.is_null() || (data.is_null() && len > 0) {
        return C2MemFfiStatus::NullPointer;
    }
    let call = unsafe { &*call }.0.clone();
    let operation = match call.endpoint.begin_operation() {
        Ok(operation) => operation,
        Err(error) => return launch_call(call, complete, user, async { Err(error) }),
    };
    let prepared = {
        let mut state = call.state.lock();
        if state.busy || state.charged < len as u64 || state.dispatched {
            return C2MemFfiStatus::InvalidArgument;
        }
        let Some(prepared) = state.prepared.take() else {
            return C2MemFfiStatus::InvalidArgument;
        };
        state.dispatched = true;
        prepared
    };
    let bytes = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
    };
    launch_call(call.clone(), complete, user, async move {
        let original = call.clone();
        // The record owns the actual opening/outcome after the caller's wait
        // expires. No task or input is aborted and expiry never sends CANCEL.
        tokio::spawn(async move {
            let outcome = AssertUnwindSafe(async {
                let encoded = match prepared.encode_vec(bytes) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        return CallRecordOutcome {
                            result: Err(error),
                            dispatch_target: None,
                        };
                    }
                };
                if original.stream_mode {
                    let record = encoded
                        .stream_record_async(original.cancellation.clone())
                        .await;
                    CallRecordOutcome {
                        result: record.result.map(|s| (Some(Arc::new(s)), None)),
                        dispatch_target: record.dispatch_target,
                    }
                } else {
                    let record = encoded.call_held_record_async().await;
                    CallRecordOutcome {
                        result: record.result.map(|b| (None, Some(b))),
                        dispatch_target: record.dispatch_target,
                    }
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| CallRecordOutcome {
                result: Err(invalid("native dispatch panicked")),
                dispatch_target: None,
            });
            let mut state = original.state.lock();
            state.dispatch_target = outcome.dispatch_target;
            match outcome.result {
                Ok((Some(stream), _)) => {
                    state.stream_operation = Some(operation);
                    state.stream = Some(stream);
                }
                Ok((None, unary)) => state.unary = unary,
                Err(error) => {
                    state.error = Some(call_error_value(&original, &error));
                }
            }
            state.ready = true;
            original.changed.notify_waiters();
            drop(state);
            if original.cancelled.load(Ordering::Acquire) {
                original.drain_cancelled_stream();
            }
        });
        call.ready_for_caller().await?;
        let state = call.state.lock();
        if let Some(error) = &state.error {
            return Ok(Output::Error(error.clone()));
        }
        let target = state
            .dispatch_target
            .as_ref()
            .ok_or_else(|| invalid("dispatch target missing"))?;
        Ok(Output::Json(target_json(
            target.path,
            target
                .ipc
                .as_ref()
                .map(|ipc| (ipc.server_identity.as_ref(), ipc.connection_id)),
        )))
    })
}
fn target_json(path: ObservedPath, ipc: Option<(&c2_core::ServerIdentity, u64)>) -> Value {
    let path = match path {
        ObservedPath::DirectIpc => "DirectIpc",
        ObservedPath::ExplicitRelay => "ExplicitRelay",
        ObservedPath::RelayAwareLocalIpc => "RelayAwareLocalIpc",
        ObservedPath::RelayAwareRelay => "RelayAwareHttp",
    };
    json!({
        "path": path,
        "serverId": ipc.map(|(identity, _)| &identity.server_id),
        "serverInstanceId": ipc.map(|(identity, _)| &identity.server_instance_id),
        "connectionId": ipc.map(|(_, id)| id.to_string()),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_next(
    call: *const C2MemFfiCall,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if complete.is_none() || call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let call = unsafe { &*call }.0.clone();
    if call.next_active.swap(true, Ordering::AcqRel) {
        return launch_call(call.clone(), complete, user, async {
            Err(invalid("concurrent next on one stream"))
        });
    }
    launch_call(call.clone(), complete, user, async move {
        struct Active(Arc<Call>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.next_active.store(false, Ordering::Release);
            }
        }
        let _active = Active(call.clone());
        call.ready_for_caller().await?;
        let stream = {
            let mut state = call.state.lock();
            if state.consumer_done {
                return Ok(Output::Empty);
            }
            if let Some(error) = state.error.clone() {
                state.consumer_done = true;
                return Ok(Output::Error(error));
            }
            if !call.stream_mode {
                state.consumer_done = true;
                return Ok(state.unary.take().map_or(Output::Empty, Output::Buffer));
            }
            state.stream.clone()
        };
        let outcome = stream
            .ok_or_else(|| invalid("stream open failed"))?
            .next_item_async()
            .await;
        match outcome {
            Ok(Some(buffer)) => Ok(Output::Buffer(buffer)),
            Ok(None) => {
                let mut state = call.state.lock();
                state.consumer_done = true;
                state.stream_operation.take();
                Ok(Output::Empty)
            }
            Err(error) => {
                let mut state = call.state.lock();
                state.consumer_done = true;
                if call.check_deadline().is_ok() {
                    state.stream_operation.take();
                }
                Err(error)
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_cancel(call: *const C2MemFfiCall) -> C2MemFfiStatus {
    if call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let call = &unsafe { &*call }.0;
    call.cancel();
    call.drain_cancelled_stream();
    C2MemFfiStatus::Ok
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_observe(
    call: *const C2MemFfiCall,
    limit: *const u64,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let call = unsafe { &*call }.0.clone();
    let limit = timeout(limit);
    call.cancel();
    launch_call(call.clone(), complete, user, async move {
        call.observe(limit).await
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_check_deadline(
    call: *const C2MemFfiCall,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let call = unsafe { &*call };
    match unsafe { native_with_call(Some(call.0.as_ref()), error, || call.0.check_deadline()) } {
        Ok(()) => C2MemFfiStatus::Ok,
        Err(status) => status,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_free(call: *mut C2MemFfiCall) {
    if !call.is_null() {
        let call = unsafe { Box::from_raw(call) };
        call.0.cancel();
        call.0.drain_cancelled_stream();
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_open(
    endpoint: *const C2MemFfiEndpointContext,
    route: *const c_char,
    topic: *const c_char,
    options: *const c_char,
    limit: *const u64,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if endpoint.is_null() || complete.is_none() {
        return C2MemFfiStatus::NullPointer;
    }
    let started = Instant::now();
    let endpoint = unsafe { &*endpoint }.0.clone();
    let args = (|| {
        let deadline = timeout(limit)
            .map(|duration| c2_config::deadline_after(started, duration))
            .transpose()
            .map_err(|error| invalid(error.to_string()))?;
        let route = text(route)?;
        let topic = text(topic)?;
        let options = parse_options(options, &["start", "onLag"])?;
        let start = match options.get("start") {
            None => TopicStart::Latest,
            Some(value) if value == "latest" => TopicStart::Latest,
            Some(value) if value == "next" => TopicStart::Next,
            _ => return Err(invalid("invalid subscription start")),
        };
        let on_lag = match options.get("onLag") {
            None => LagPolicy::Skip,
            Some(value) if value == "skip" => LagPolicy::Skip,
            Some(value) if value == "error" => LagPolicy::Error,
            _ => return Err(invalid("invalid subscription onLag")),
        };
        let operation = endpoint.begin_operation()?;
        Ok::<_, Error>((
            route,
            topic,
            SubscribeOptions { start, on_lag },
            deadline,
            operation,
        ))
    })();
    launch(complete, user, async move {
        let (route, topic, options, deadline, operation) = args?;
        let client = endpoint.bound_client(&route)?;
        let call_options =
            CallOptions::with_timeout(deadline.map_or(CallTimeout::Unlimited, |deadline| {
                CallTimeout::After(deadline.saturating_duration_since(Instant::now()))
            }));
        let core = client
            .subscribe_async(&topic, options, call_options)
            .await?;
        Ok(Output::Subscription(Arc::new(Subscription {
            core,
            operation: Mutex::new(Some(operation)),
            next_active: AtomicBool::new(false),
            consumer_done: AtomicBool::new(false),
        })))
    })
}

impl Subscription {
    fn drain_cancelled(self: &Arc<Self>) {
        let _ = self.core.cancel();
        if let Some(operation) = self.operation.lock().take() {
            let sub = self.clone();
            let executor =
                c2_core::native_client_executor().expect("running subscription executor");
            executor.spawn(async move {
                let _operation = operation;
                sub.core.close_with_timeout_async(None).await;
            });
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_next(
    sub: *const C2MemFfiSubscription,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if sub.is_null() || complete.is_none() {
        return C2MemFfiStatus::NullPointer;
    }
    let sub = unsafe { &*sub }.0.clone();
    if sub.next_active.swap(true, Ordering::AcqRel) {
        return launch(complete, user, async {
            Err(invalid("concurrent next on one subscription"))
        });
    }
    let active = SubscriptionNext(sub);
    launch(complete, user, async move {
        let sub = &active.0;
        if sub.consumer_done.load(Ordering::Acquire) {
            return Ok(Output::Empty);
        }
        match sub.core.next_delivery_async().await {
            Ok(delivery) => {
                let metadata = json!({"seq": delivery.seq().to_string(), "dropped": delivery.dropped().to_string()});
                Ok(Output::Delivery(delivery.into_held(), metadata))
            }
            Err(error) => {
                sub.consumer_done.store(true, Ordering::Release);
                if sub.core.check_deadline().is_ok() {
                    sub.operation.lock().take();
                }
                Err(error)
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_cancel(
    sub: *const C2MemFfiSubscription,
) -> C2MemFfiStatus {
    if sub.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    unsafe { &*sub }.0.drain_cancelled();
    C2MemFfiStatus::Ok
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_close(
    sub: *const C2MemFfiSubscription,
    limit: *const u64,
    complete: C2MemFfiComplete,
    user: *mut std::ffi::c_void,
) -> C2MemFfiStatus {
    if sub.is_null() || complete.is_none() {
        return C2MemFfiStatus::NullPointer;
    }
    let sub = unsafe { &*sub }.0.clone();
    let limit = timeout(limit);
    let _ = sub.core.cancel();
    launch(complete, user, async move {
        let receipt = sub.core.close_with_timeout_async(limit).await;
        if receipt.completed {
            sub.operation.lock().take();
        } else {
            sub.drain_cancelled();
        }
        let error = receipt
            .terminal
            .and_then(Result::err)
            .as_ref()
            .map(error_value);
        Ok(Output::Json(json!({
            "completed": receipt.completed,
            "error": error,
            "completeItems": receipt.complete_items.map(|count| count.to_string()),
        })))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_check_deadline(
    sub: *const C2MemFfiSubscription,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if sub.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    match unsafe { native(error, || (&*sub).0.core.check_deadline()) } {
        Ok(()) => C2MemFfiStatus::Ok,
        Err(status) => status,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_subscription_free(sub: *mut C2MemFfiSubscription) {
    if !sub.is_null() {
        let sub = unsafe { Box::from_raw(sub) };
        sub.0.drain_cancelled();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_buffer_data(
    buffer: *const C2MemFfiBuffer,
    data: *mut *const u8,
    len: *mut usize,
) -> C2MemFfiStatus {
    if buffer.is_null() || data.is_null() || len.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    let bytes = unsafe { &*buffer }.0.bytes();
    unsafe {
        *data = bytes.as_ptr();
        *len = bytes.len();
    }
    C2MemFfiStatus::Ok
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_buffer_release(buffer: *mut C2MemFfiBuffer) {
    if !buffer.is_null() {
        drop(unsafe { Box::from_raw(buffer) });
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_error_parts(error: *const C2MemFfiError) -> *const c_char {
    if error.is_null() {
        ptr::null()
    } else {
        unsafe { &*error }.0.as_ptr()
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_error_free(error: *mut C2MemFfiError) {
    if !error.is_null() {
        drop(unsafe { Box::from_raw(error) });
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_result_free(value: *mut C2MemFfiResult) {
    if !value.is_null() {
        let value = unsafe { Box::from_raw(value) };
        unsafe {
            c2_mem_ffi_buffer_release(value.buffer);
            c2_mem_ffi_subscription_free(value.subscription);
            c2_mem_ffi_error_free(value.error);
            if !value.json.is_null() {
                drop(CString::from_raw(value.json.cast_mut()));
            }
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
