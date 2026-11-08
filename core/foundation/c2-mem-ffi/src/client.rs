//! ABI 8: native client calls, with Rust owning the only physical IPC reader.
use super::*;
use c2_contract::ExpectedRouteContract;
use c2_core::{
    CallOptions, CallScope, CallState, CallTimeout, Client, CoreResponseStream, Error,
    HeldResponse, LifecycleError, PreparedCall, Runtime, RuntimeOptions, TransportPhase,
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
    address: String,
    options: Value,
    closed: AtomicBool,
    preparing: AtomicUsize,
    changed: Notify,
    bindings: Mutex<Bindings>,
    connection: Mutex<Option<Arc<c2_core::NativeEndpointContext>>>,
}
#[derive(Default)]
struct Bindings {
    server: Option<(String, String)>,
    clients: HashMap<String, Client>,
}
pub struct C2MemFfiCall(Arc<Call>);
struct Call {
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
    unary: Option<HeldResponse>,
    error: Option<Value>,
    consumer_done: bool,
}
pub struct C2MemFfiBuffer(HeldResponse);
pub struct C2MemFfiError(CString);
#[repr(C)]
pub struct C2MemFfiResult {
    pub buffer: *mut C2MemFfiBuffer,
    pub error: *mut C2MemFfiError,
    pub json: *const c_char,
}
pub type C2MemFfiComplete =
    Option<unsafe extern "C" fn(*mut std::ffi::c_void, *mut C2MemFfiResult)>;
enum Output {
    Empty,
    Json(Value),
    Buffer(HeldResponse),
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
    let semantic = match error {
        Error::Semantic(error) => Some(error),
        Error::Transport(error) => {
            value["transportPhase"] = json!(match error.phase() {
                TransportPhase::PreDispatch => "pre_dispatch",
                TransportPhase::DispatchUncertain => "dispatch_uncertain",
            });
            value["category"] = json!(match error.kind() {
                c2_core::TransportKind::Ipc => "ipc",
                c2_core::TransportKind::Http => "http",
            });
            error.semantic_error()
        }
        Error::ProtocolVersionMismatch { local, peer } => {
            value["category"] = json!("protocol_version_mismatch");
            value["transportPhase"] = json!("pre_dispatch");
            value["localProtocolVersion"] = json!(local);
            value["peerProtocolVersion"] = json!(peer);
            None
        }
        Error::Lifecycle(LifecycleError::RuntimeRetired) => {
            value["transportPhase"] = json!("pre_dispatch");
            value["category"] = json!("runtime_retired");
            None
        }
        Error::Lifecycle(LifecycleError::ResponseCopy(_)) => {
            value["transportPhase"] = json!("dispatch_uncertain");
            None
        }
        Error::Contract(_) | Error::Admission(_) | Error::Lifecycle(_) => {
            value["transportPhase"] = json!("pre_dispatch");
            None
        }
    };
    if let Some(error) = semantic {
        value["message"] = json!(error.message);
        value["code"] = json!(error.code as u32);
        value["canonicalName"] = json!(error.code.name());
        value["details"] = json!(error.details);
        if let Some(phase) = error.details.get("transport_phase") {
            value["transportPhase"] = json!(phase);
        }
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
    if matches!(error, Error::Lifecycle(_))
        && matches!(
            call.scope.state(),
            CallState::Dispatched
                | CallState::FailedDispatchUncertain
                | CallState::ExpiredDispatchUncertain
        )
    {
        value["transportPhase"] = json!("dispatch_uncertain");
    }
    if value.get("transportPhase").is_none()
        && matches!(
            call.scope.state(),
            CallState::PreDispatch | CallState::FailedPreDispatch | CallState::ExpiredPreDispatch
        )
    {
        value["transportPhase"] = json!("pre_dispatch");
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
    };
    match output {
        Ok(Output::Empty) => {}
        Ok(Output::Json(value)) => {
            result.json = CString::new(value.to_string()).expect("JSON").into_raw()
        }
        Ok(Output::Buffer(buffer)) => {
            result.buffer = Box::into_raw(Box::new(C2MemFfiBuffer(buffer)))
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
    let Some(complete) = complete else {
        return C2MemFfiStatus::NullPointer;
    };
    let user = user as usize;
    let handle = match c2_core::native_client_executor() {
        Ok(handle) => handle,
        Err(error) => {
            unsafe {
                complete(user as *mut _, result(Err(error)));
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
            complete(user as *mut _, result(output));
        }
    });
    C2MemFfiStatus::Ok
}
unsafe fn native<T>(
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
                *error = owned_error(error_value(&cause));
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

struct Preparing(Arc<Endpoint>);
impl Drop for Preparing {
    fn drop(&mut self) {
        self.0.preparing.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
impl Endpoint {
    fn deactivate(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(connection) = self
            .connection
            .try_lock()
            .and_then(|connection| connection.clone())
        {
            connection.deactivate();
        }
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
                "IPC transport is deactivated",
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
        let connection = {
            let mut connection = self.connection.lock();
            if connection.is_none() {
                *connection = Some(Arc::new(
                    runtime.native_endpoint_context(&self.address, &attempt)?,
                ));
            }
            connection.as_ref().unwrap().clone()
        };
        if self.closed.load(Ordering::Acquire) {
            connection.deactivate();
            return Err(refused(
                ErrorCode::ResourceClosed,
                "IPC transport is deactivated",
            ));
        }
        let client = connection.prepare(expected, &attempt)?;
        let identity = client
            .ipc_server_identity()
            .ok_or_else(|| invalid("IPC handshake identity missing"))?;
        let pair = (
            identity.server_id.clone(),
            identity.server_instance_id.clone(),
        );
        let observation = client.connection_observation()?;
        let mut bindings = self.bindings.lock();
        let supplied = self
            .options
            .get("expectedServerIdentity")
            .map(|v| Ok::<_, Error>((field(v, "serverId")?, field(v, "serverInstanceId")?)))
            .transpose()?;
        if supplied
            .as_ref()
            .or(bindings.server.as_ref())
            .is_some_and(|expected| expected != &pair)
        {
            return Err(refused(
                ErrorCode::IdentityMismatch,
                "IPC server identity mismatch",
            ));
        }
        if self.closed.load(Ordering::Acquire) {
            connection.deactivate();
            return Err(refused(
                ErrorCode::ResourceClosed,
                "IPC transport is deactivated",
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
        bindings.server.get_or_insert(pair);
        bindings.clients.insert(route, client);
        Ok(
            json!({"serverId":identity.server_id,"serverInstanceId":identity.server_instance_id,"connectionId":observation.connection_id().to_string()}),
        )
    }
    async fn close(&self) -> Result<(), Error> {
        self.deactivate();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.preparing.load(Ordering::Acquire) == 0 {
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
            state.error = Some(error_value(&refused(
                ErrorCode::CallCancelled,
                "stream cancelled before dispatch",
            )));
            state.ready = true;
            let _ = self.scope.complete_failure();
        }
        self.changed.notify_waiters();
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
            let error = receipt
                .terminal
                .and_then(Result::err)
                .as_ref()
                .map(error_value);
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
/// Inputs obey the ownership and callback rules in the ABI 8 C header.
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
            let options = parse_options(options, &["requestShmThreshold", "requestChunkSize"])?;
            let threshold = unsigned_option(&options, "requestShmThreshold")?;
            let chunk = unsigned_option(&options, "requestChunkSize")?;
            let endpoint = if context.is_null() {
                capture_local_endpoint_context(None)
                    .map_err(|_| invalid("endpoint configuration failed"))?
            } else {
                (*context).inner.clone()
            };
            let runtime = Runtime::new(RuntimeOptions {
                use_process_relay_anchor: false,
                shm_threshold: threshold,
                client_ipc_overrides: chunk.map(|chunk_size| c2_config::ClientIpcConfigOverrides {
                    chunk_size: Some(chunk_size),
                    ..Default::default()
                }),
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
        let cell=runtime.payload_buffer_stats().map(|s|json!({"limit_bytes":s.limit_bytes.to_string(),"used_bytes":s.used_bytes.to_string(),"peak_bytes":s.peak_bytes.to_string(),"rejected_allocations":s.rejected_allocations.to_string(),"rejected_bytes":s.rejected_bytes.to_string()}));
        let slots = runtime
            .call_execution_snapshot()
            .map_err(|_| C2MemFfiStatus::PoolError)?;
        unsafe {
            *out = result(Ok(Output::Json(
                json!({"payload_buffer":cell,"used_operations":slots.used_operations.to_string()}),
            )));
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
            Ok(C2MemFfiEndpointContext(Arc::new(Endpoint {
                domain: (*domain).0.clone(),
                address: text(address)?,
                options: parse_options(options, &["connectTimeoutMs", "expectedServerIdentity"])?,
                closed: AtomicBool::new(false),
                preparing: AtomicUsize::new(0),
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
        if let Some(value) = endpoint.options.get("connectTimeoutMs") {
            options = options.with_timeout(Duration::from_millis(
                value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid connectTimeoutMs"))?,
            ));
        }
        Ok((route, contract, c2_core::ConnectAttempt::start(options)?))
    });
    endpoint.preparing.fetch_add(1, Ordering::AcqRel);
    let preparing = Preparing(endpoint.clone());
    launch(complete, user, async move {
        let (route, contract, attempt) = args?;
        tokio::task::spawn_blocking(move || {
            let _preparing = preparing;
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
            if endpoint.closed.load(Ordering::Acquire) {
                return Err(refused(
                    ErrorCode::ResourceClosed,
                    "IPC transport is deactivated",
                ));
            }
            if stream > 1 {
                return Err(invalid("invalid call mode"));
            }
            let route = text(route)?;
            let method = text(method)?;
            let client = endpoint
                .bindings
                .lock()
                .clients
                .get(&route)
                .cloned()
                .ok_or_else(|| invalid("route must be prepared before call_begin"))?;
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
                state: Mutex::new(CallData {
                    prepared: Some(prepared),
                    charged: 0,
                    busy: false,
                    dispatched: false,
                    ready: false,
                    stream: None,
                    unary: None,
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
    let prepared = {
        let mut state = call.state.lock();
        let prepared = state
            .prepared
            .take()
            .ok_or_else(|| invalid("call is not awaiting input"));
        if prepared.is_ok() {
            state.busy = true;
        }
        prepared
    };
    launch(complete, user, async move {
        let mut prepared = prepared?;
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
            state.error = Some(error_value(&refused(
                ErrorCode::CallCancelled,
                "stream cancelled before dispatch",
            )));
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
                state.error = Some(error_value(&error));
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
    launch(complete, user, async move {
        let original = call.clone();
        // The record owns the actual opening/outcome after the caller's wait
        // expires. No task or input is aborted and expiry never sends CANCEL.
        tokio::spawn(async move {
            let outcome = AssertUnwindSafe(async {
                let encoded = prepared.encode_vec(bytes)?;
                if original.stream_mode {
                    encoded
                        .stream_record_async(original.cancellation.clone())
                        .await
                        .map(|s| (Some(Arc::new(s)), None))
                } else {
                    encoded
                        .call_held_record_async()
                        .await
                        .map(|b| (None, Some(b)))
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(invalid("native dispatch panicked")));
            let mut state = original.state.lock();
            match outcome {
                Ok((stream, unary)) => {
                    state.stream = stream;
                    state.unary = unary;
                }
                Err(error) => {
                    state.error = Some(call_error_value(&original, &error));
                }
            }
            state.ready = true;
            original.changed.notify_waiters();
        });
        call.ready_for_caller().await?;
        let state = call.state.lock();
        Ok(state.error.clone().map_or(Output::Empty, Output::Error))
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
        return launch(complete, user, async {
            Err(invalid("concurrent next on one stream"))
        });
    }
    launch(complete, user, async move {
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
                call.state.lock().consumer_done = true;
                Ok(Output::Empty)
            }
            Err(error) => {
                call.state.lock().consumer_done = true;
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
    unsafe { &*call }.0.cancel();
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
    launch(complete, user, async move { call.observe(limit).await })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_check_deadline(
    call: *const C2MemFfiCall,
    error: *mut *mut C2MemFfiError,
) -> C2MemFfiStatus {
    if call.is_null() {
        return C2MemFfiStatus::NullPointer;
    }
    match unsafe { native(error, || (*call).0.check_deadline()) } {
        Ok(()) => C2MemFfiStatus::Ok,
        Err(status) => status,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_call_free(call: *mut C2MemFfiCall) {
    if !call.is_null() {
        let call = unsafe { Box::from_raw(call) };
        call.0.cancel();
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
