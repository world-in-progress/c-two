use parking_lot::RwLock;
use std::error::Error as _;
use std::fmt;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use c2_config::{CallOptions, ConnectDeadline, ConnectOptions};
use c2_http::client::{HttpCallControl, HttpResponseStream, HttpStreamCancellation};
use c2_ipc::IpcCallControl;

use crate::call_execution::{PreparingCall, guard_error, scope_error};
use crate::client_stream::{CoreResponseStream, StreamTransport};
use crate::{CallScope, CallScopeError, CallState};

use c2_contract::{ExpectedRouteContract, validate_expected_route_contract};
use c2_http::client::{HttpCallPhase, RelayAwareHttpClient, RelayLocalIpcCandidate};
use c2_ipc::{IpcCallError, RouteBinding};
use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget};

use crate::session::RelayResolvedConnection;
use crate::{
    Error, LifecycleError, Runtime, TransportPhase, normalize_http_error, normalize_ipc_error,
};

const STALE_POOL_RECONNECT_ATTEMPTS: usize = 2;

/// Closed connection-mode selection for a route-bound Core client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connect {
    DirectIpc { address: String },
    ExplicitRelay { relay_url: String },
    RelayAware,
}

/// The transport path selected and verified before a [`Client`] is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservedPath {
    DirectIpc,
    ExplicitRelay,
    RelayAwareLocalIpc,
    RelayAwareRelay,
}

/// Shared identity of the physical IPC connection selected for one call.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcCallTarget {
    pub server_identity: Arc<c2_wire::handshake::ServerIdentity>,
    pub connection_id: u64,
}

/// The transport selected at the original call's dispatch handoff.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallDispatchTarget {
    pub path: ObservedPath,
    pub ipc: Option<IpcCallTarget>,
}

/// Original outcome and its immutable dispatch target, including failures.
#[doc(hidden)]
pub struct CallRecordOutcome<R> {
    pub result: Result<R, Error>,
    pub dispatch_target: Option<CallDispatchTarget>,
}

/// Monotonic process-runtime observations of successfully selected paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathCounters {
    direct_ipc: u64,
    explicit_relay: u64,
    relay_aware_local_ipc: u64,
    relay_aware_relay: u64,
}

impl PathCounters {
    pub const fn direct_ipc(self) -> u64 {
        self.direct_ipc
    }

    pub const fn explicit_relay(self) -> u64 {
        self.explicit_relay
    }

    pub const fn relay_aware_local_ipc(self) -> u64 {
        self.relay_aware_local_ipc
    }

    pub const fn relay_aware_relay(self) -> u64 {
        self.relay_aware_relay
    }

    pub(crate) fn record(&mut self, path: ObservedPath) {
        let counter = match path {
            ObservedPath::DirectIpc => &mut self.direct_ipc,
            ObservedPath::ExplicitRelay => &mut self.explicit_relay,
            ObservedPath::RelayAwareLocalIpc => &mut self.relay_aware_local_ipc,
            ObservedPath::RelayAwareRelay => &mut self.relay_aware_relay,
        };
        *counter = counter.saturating_add(1);
    }
}

/// One concrete, route-bound client shared by direct, explicit-relay, and
/// relay-aware connection modes.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientConnection>,
    options: CallOptions,
}

struct ClientConnection {
    expected: ExpectedRouteContract,
    runtime: Runtime,
    acquire: ClientAcquire,
    current: RwLock<Arc<ClientTarget>>,
    refresh: Arc<tokio::sync::Mutex<()>>,
}

enum ClientAcquire {
    Direct {
        address: String,
    },
    Relay {
        anchor: String,
        settings: c2_config::ResolvedRelayClientConfig,
        http_only: bool,
    },
    Endpoint(Arc<EndpointAcquisition>),
}

pub(crate) struct ClientTarget {
    observed_path: ObservedPath,
    path_default: Option<Duration>,
    transport: ClientInner,
    // Invalidations belong to this exact target, never a later publication.
    facts: AtomicU8,
}

const NEEDS_ACQUIRE: u8 = 1;
const TERMINAL_REFUSAL: u8 = 2;

enum ClientInner {
    Ipc(PooledIpcClient),
    Http(RelayAwareHttpClient, Option<Duration>),
}

struct PooledIpcClient {
    client: c2_ipc::pool::ClientLease,
    binding: RouteBinding,
    server_identity: Arc<c2_wire::handshake::ServerIdentity>,
}

impl PooledIpcClient {
    fn new(client: c2_ipc::pool::ClientLease, binding: RouteBinding) -> Self {
        let server_identity = Arc::new(
            client
                .server_identity()
                .expect("route-bound IPC client has handshake identity")
                .clone(),
        );
        Self {
            client,
            binding,
            server_identity,
        }
    }
}

/// One SDK call's original budget, including acquisition before serialization.
#[doc(hidden)]
pub struct CallBudget {
    scope: CallScope,
    deadline: ConnectDeadline,
}

impl CallBudget {
    pub fn new(options: CallOptions, path_default: Option<Duration>) -> Result<Self, Error> {
        let options = match options.effective_timeout(path_default) {
            Some(timeout) => ConnectOptions::new().with_timeout(timeout),
            None => ConnectOptions::new(),
        };
        let deadline = ConnectDeadline::start(options)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        Ok(Self {
            scope: CallScope::from_deadline(deadline.instant()),
            deadline,
        })
    }

    pub fn scope(&self) -> CallScope {
        self.scope.clone()
    }

    pub fn connect_attempt(&self) -> crate::ConnectAttempt {
        crate::ConnectAttempt::from_deadline(self.deadline)
    }
}

impl ClientTarget {
    fn new(observed_path: ObservedPath, transport: ClientInner) -> Self {
        let path_default = match &transport {
            ClientInner::Ipc(_) => None,
            ClientInner::Http(_, default) => *default,
        };
        Self {
            observed_path,
            path_default,
            transport,
            facts: AtomicU8::new(0),
        }
    }

    fn dispatch_target(&self) -> CallDispatchTarget {
        CallDispatchTarget {
            path: self.observed_path,
            ipc: match &self.transport {
                ClientInner::Ipc(connection) => Some(IpcCallTarget {
                    server_identity: Arc::clone(&connection.server_identity),
                    connection_id: connection
                        .client
                        .connection_observation()
                        .expect("route-bound IPC client has physical connection")
                        .connection_id(),
                }),
                ClientInner::Http(..) => None,
            },
        }
    }

    fn needs_acquire(&self) -> bool {
        let facts = self.facts.load(Ordering::Acquire);
        if facts & TERMINAL_REFUSAL != 0 {
            return false;
        }
        if facts & NEEDS_ACQUIRE != 0 {
            return true;
        }
        match &self.transport {
            ClientInner::Ipc(connection) => connection
                .client
                .connection_observation()
                .and_then(|observation| observation.snapshot())
                .is_some_and(|closed| {
                    matches!(
                        closed.reason(),
                        c2_local::ConnectionEndReason::PeerEof
                            | c2_local::ConnectionEndReason::PeerCloseMessage
                            | c2_local::ConnectionEndReason::ConnectionReset
                    )
                }),
            ClientInner::Http(..) => false,
        }
    }

    pub(crate) fn observe_error(&self, error: &Error) {
        match error {
            Error::Semantic(error)
                if error
                    .details
                    .get("transport_phase")
                    .or_else(|| error.details.get("dispatch_phase"))
                    .is_some_and(|phase| phase == "pre_dispatch") =>
            {
                use c2_error::ErrorCode::*;
                let fact = match error.code {
                    ResourceNotFound | ResourceRemoved | ResourceClosed | ContractMismatch => {
                        NEEDS_ACQUIRE
                    }
                    IdentityMismatch | ProtocolViolation => TERMINAL_REFUSAL,
                    _ => 0,
                };
                self.facts.fetch_or(fact, Ordering::Release);
            }
            Error::ProtocolVersionMismatch { .. } => {
                self.facts.fetch_or(TERMINAL_REFUSAL, Ordering::Release);
            }
            Error::Transport(error) => {
                let source = error.source();
                // IPC call failures do not identify why the connection ended.
                // In particular, framing_failed records ProtocolError but
                // fails dispatched calls with Closed. needs_acquire reads the
                // connection's terminal reason instead.
                let http =
                    source.and_then(|source| source.downcast_ref::<c2_http::client::HttpError>());
                let cannot_connect = matches!(http, Some(c2_http::client::HttpError::Request(error)) if error.is_connect());
                if cannot_connect {
                    self.facts.fetch_or(NEEDS_ACQUIRE, Ordering::Release);
                }
            }
            _ => {}
        }
    }
}

impl ClientConnection {
    async fn target_for_call(
        self: &Arc<Self>,
        target: Arc<ClientTarget>,
        scope: &CallScope,
        deadline: ConnectDeadline,
    ) -> Result<Arc<ClientTarget>, Error> {
        scope.check_pre_dispatch().map_err(scope_error)?;
        if !target.needs_acquire() {
            return Ok(target);
        }
        let owner = self.clone();
        let refresh = self.refresh.clone();
        let acquire = async move {
            let guard = refresh.lock_owned().await;
            let target = owner.current.read().clone();
            if !target.needs_acquire() {
                return Ok(target);
            }
            tokio::task::spawn_blocking(move || {
                // The worker owns the gate until the original bounded attempt ends.
                let _guard = guard;
                let target =
                    owner
                        .runtime
                        .acquire_target(&owner.expected, &owner.acquire, deadline)?;
                let target = owner.runtime.publish_client_with_deadline(
                    target.observed_path,
                    deadline,
                    || Arc::new(target),
                )?;
                let previous = std::mem::replace(&mut *owner.current.write(), target.clone());
                drop(previous);
                Ok(target)
            })
            .await
            .map_err(|error| {
                LifecycleError::Configuration(format!("client acquisition task failed: {error}"))
            })?
        };
        let result = match scope.deadline() {
            Some(deadline) => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), acquire)
                    .await
                    .map_err(|_| {
                        scope_error(CallScopeError::DeadlineExceeded {
                            phase: TransportPhase::PreDispatch,
                        })
                    })?
            }
            None => acquire.await,
        };
        result.map_err(|error| match error {
            Error::Semantic(error) if error.code == c2_error::ErrorCode::CallDeadlineExceeded => {
                scope_error(CallScopeError::DeadlineExceeded {
                    phase: TransportPhase::PreDispatch,
                })
            }
            other => other,
        })
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("expected", &self.inner.expected)
            .field("observed_path", &self.observed_path())
            .finish_non_exhaustive()
    }
}

impl Client {
    #[doc(hidden)]
    pub fn ipc_server_identity(&self) -> Option<c2_wire::handshake::ServerIdentity> {
        match &self.inner.current.read().transport {
            ClientInner::Ipc(client) => client.client.server_identity().cloned(),
            ClientInner::Http(..) => None,
        }
    }

    /// Observe this physical IPC connection without connecting or reconnecting.
    /// Other logical views may share it; closing one view need not end it.
    pub fn connection_observation(&self) -> Result<c2_local::ConnectionObservation, Error> {
        match &self.inner.current.read().transport {
            ClientInner::Ipc(client) => client.client.connection_observation().ok_or_else(|| {
                c2_error::C2Error::new(
                    c2_error::ErrorCode::ResourceClosed,
                    "IPC client has no physical connection",
                )
                .into()
            }),
            ClientInner::Http(..) => Err(c2_error::C2Error::new(
                c2_error::ErrorCode::UnsupportedCallMode,
                "connection observation requires a physical IPC connection",
            )
            .into()),
        }
    }

    pub fn expected_route(&self) -> &ExpectedRouteContract {
        &self.inner.expected
    }

    pub fn observed_path(&self) -> ObservedPath {
        self.inner.current.read().observed_path
    }

    /// Immutable policy view sharing the exact route and connection acquisition.
    pub fn with_call_options(&self, options: CallOptions) -> Self {
        Self {
            inner: self.inner.clone(),
            options,
        }
    }

    pub const fn call_options(&self) -> CallOptions {
        self.options
    }

    /// Release this view. Other views and executing calls retain their
    /// references to the same connection; no transport is cancelled here.
    pub fn close(self) {}

    /// Start the logical call BEFORE SDK serialization (including non-Send
    /// Python serializers, which remain on the caller). This reserves a finite
    /// slot and establishes the sole absolute deadline for the complete call.
    /// Initial connect remains a separate operation with its existing policy.
    #[doc(hidden)]
    pub fn begin_call(&self, method: &str) -> Result<PreparedCall, Error> {
        let target = self.inner.current.read().clone();
        let budget = CallBudget::new(self.options, target.path_default)?;
        self.prepare_with_budget(method, budget, target, false)
    }

    #[doc(hidden)]
    pub fn begin_call_with_budget(
        &self,
        method: &str,
        budget: CallBudget,
    ) -> Result<PreparedCall, Error> {
        self.prepare_with_budget(method, budget, self.inner.current.read().clone(), false)
    }

    /// Streams have no inherited unary timeout and reserve a call slot.
    #[doc(hidden)]
    pub fn begin_stream(&self, method: &str) -> Result<PreparedCall, Error> {
        self.begin_stream_with_budget(method, CallBudget::new(self.options, None)?)
    }

    #[doc(hidden)]
    pub fn begin_stream_with_budget(
        &self,
        method: &str,
        budget: CallBudget,
    ) -> Result<PreparedCall, Error> {
        self.prepare_with_budget(method, budget, self.inner.current.read().clone(), true)
    }

    fn prepare_with_budget(
        &self,
        method: &str,
        budget: CallBudget,
        target: Arc<ClientTarget>,
        stream: bool,
    ) -> Result<PreparedCall, Error> {
        let context = self.inner.runtime.call_execution_context()?;
        let mut preparing = if stream {
            context.prepare_stream_scope(budget.scope)?
        } else {
            context.prepare_scope(budget.scope)?
        };
        if stream && matches!(&target.transport, ClientInner::Http(..)) {
            preparing = preparing.with_stream_http_headroom();
        }
        Ok(PreparedCall {
            preparing,
            inner: self.inner.clone(),
            target,
            deadline: budget.deadline,
            method: method.into(),
            stream,
        })
    }
}

/// Opaque, single-use SDK preparation state. No cloning, scope mutation or
/// transport access is exposed. Drop abandons preparation without dispatch.
#[doc(hidden)]
pub struct PreparedCall {
    preparing: PreparingCall,
    inner: Arc<ClientConnection>,
    target: Arc<ClientTarget>,
    deadline: ConnectDeadline,
    method: String,
    stream: bool,
}

impl PreparedCall {
    #[doc(hidden)]
    pub fn scope(&self) -> CallScope {
        self.preparing.scope_clone()
    }

    #[doc(hidden)]
    pub async fn charge_input_async(&mut self, nbytes: u64) -> Result<(), Error> {
        self.preparing.charge_input_async(nbytes).await
    }

    /// Reserve known prepared input bytes before native allocation/copy. For
    /// unknown pickle length, serialize on the caller after begin_call, then
    /// charge the resulting length; this checks the original deadline. It is
    /// not a hard-real-time guarantee for synchronous user serialization.
    pub fn charge_input(&mut self, nbytes: u64) -> Result<(), Error> {
        self.preparing.charge_input(nbytes)
    }

    /// Hand off ALREADY PREPARED native-owned data, not a user serializer.
    /// The factory must produce exactly nbytes bytes. Finite execution keeps
    /// its native task through real completion; Unlimited drives the same
    /// transport inline. Both retain B for the actual input owner.
    pub fn encode<M>(mut self, nbytes: usize, materialize: M) -> Result<EncodedCall, Error>
    where
        M: FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
    {
        self.charge_input(
            u64::try_from(nbytes)
                .map_err(|_| LifecycleError::Configuration("input length exceeds u64".into()))?,
        )?;
        Ok(EncodedCall {
            prepared: self,
            nbytes,
            materialize: Box::new(materialize),
        })
    }

    /// Transfer the original Vec into native execution without a second copy.
    pub fn encode_vec(self, request: Vec<u8>) -> Result<EncodedCall, Error> {
        self.encode(request.len(), move || Ok(request))
    }

    /// Borrowed entry: charge before making the owned native copy.
    /// Rust cannot hand a borrowed slice to a detached continuation.
    pub fn encode_slice(mut self, request: &[u8]) -> Result<EncodedCall, Error> {
        self.charge_input(
            u64::try_from(request.len())
                .map_err(|_| LifecycleError::Configuration("input length exceeds u64".into()))?,
        )?;
        self.encode_vec(request.to_vec())
    }
}

/// Closed execution handoff: one consumed preparation, one native materializer,
/// and the same route acquisition. It cannot be reused or replaced after charge.
#[doc(hidden)]
pub struct EncodedCall {
    prepared: PreparedCall,
    nbytes: usize,
    materialize: Box<dyn FnOnce() -> Result<Vec<u8>, Error> + Send>,
}

impl EncodedCall {
    pub fn stream(self) -> Result<CoreResponseStream, Error> {
        let (started, scope, cancellation, http_cancellation) =
            self.start_stream(false, c2_ipc::IpcStreamCancellation::new())?;
        Ok(CoreResponseStream::new(
            started.wait()?,
            scope,
            cancellation,
            http_cancellation,
        ))
    }

    /// Keep the original opening record available to a native facade whose
    /// caller may stop waiting independently of the transport.
    #[doc(hidden)]
    pub async fn stream_record_async(
        self,
        cancellation: c2_ipc::IpcStreamCancellation,
    ) -> CallRecordOutcome<CoreResponseStream> {
        let (started, scope, cancellation, http_cancellation) =
            match self.start_stream(true, cancellation) {
                Ok(started) => started,
                Err(error) => {
                    return CallRecordOutcome {
                        result: Err(error),
                        dispatch_target: None,
                    };
                }
            };
        let CallRecordOutcome {
            result,
            dispatch_target,
        } = started.wait_original().await;
        let result = result.map(|stream| {
            if cancellation.is_requested() {
                http_cancellation.cancel();
            }
            CoreResponseStream::new(stream, scope, cancellation, http_cancellation)
        });
        CallRecordOutcome {
            result,
            dispatch_target,
        }
    }

    fn start_stream(
        self,
        original: bool,
        cancellation: c2_ipc::IpcStreamCancellation,
    ) -> Result<
        (
            crate::call_execution::ExecutingCall<(StreamTransport, Arc<ClientTarget>)>,
            CallScope,
            c2_ipc::IpcStreamCancellation,
            HttpStreamCancellation,
        ),
        Error,
    > {
        let PreparedCall {
            mut preparing,
            inner,
            target,
            deadline,
            method,
            stream,
        } = self.prepared;
        if !stream {
            return Err(LifecycleError::Configuration(
                "stream handoff requires begin_stream".into(),
            )
            .into());
        }
        let scope = preparing.scope_clone();
        let payload_budget = preparing.payload_budget();
        let permit = preparing.take_stream_permit();
        let transport_cancellation = cancellation.clone();
        let handle_cancellation = cancellation.clone();
        let http_cancellation = HttpStreamCancellation::new();
        let transport_http_cancellation = http_cancellation.clone();
        let abandon_http_cancellation = http_cancellation.clone();
        let http_headroom_reserved = matches!(&target.transport, ClientInner::Http(..));
        let transport =
            move |input, scope: CallScope, record: crate::call_execution::CallRecord<_>| async move {
                let target = inner.target_for_call(target, &scope, deadline).await?;
                if original {
                    record.record_target(target.dispatch_target());
                }
                let result = match &target.transport {
                    ClientInner::Ipc(connection) => call_ipc_stream(
                        connection,
                        &method,
                        input,
                        scope,
                        payload_budget,
                        Box::new(permit),
                        transport_cancellation,
                    )
                    .await
                    .map(StreamTransport::Ipc),
                    ClientInner::Http(client, _) => call_http_stream(
                        client,
                        &method,
                        input,
                        scope,
                        payload_budget,
                        Box::new(permit),
                        transport_cancellation,
                        transport_http_cancellation,
                        http_headroom_reserved,
                    )
                    .await
                    .map(StreamTransport::Http),
                };
                if let Err(error) = &result {
                    target.observe_error(error);
                }
                result.map(|stream| (stream, target))
            };
        let abandon = |(stream, _target): &(StreamTransport, Arc<ClientTarget>)| {
            stream.cancel();
        };
        let on_abandon = move || {
            cancellation.cancel();
            abandon_http_cancellation.cancel();
        };
        let started = if original {
            preparing.execute_stream_record(
                self.nbytes,
                self.materialize,
                transport,
                abandon,
                on_abandon,
            )?
        } else {
            preparing.execute_stream(
                self.nbytes,
                self.materialize,
                transport,
                abandon,
                on_abandon,
            )?
        };
        Ok((started, scope, handle_cancellation, http_cancellation))
    }

    pub fn call_owned(self) -> Result<Vec<u8>, Error> {
        let PreparedCall {
            preparing,
            inner,
            target,
            deadline,
            method,
            stream,
        } = self.prepared;
        if stream {
            return Err(
                LifecycleError::Configuration("unary handoff requires begin_call".into()).into(),
            );
        }
        let payload_budget = preparing.payload_budget();
        preparing
            .execute(
                self.nbytes,
                self.materialize,
                move |input, scope: CallScope| async move {
                    let target = inner.target_for_call(target, &scope, deadline).await?;
                    let result = async {
                        match &target.transport {
                            ClientInner::Ipc(connection) => {
                                let response = call_ipc(connection, &method, input, scope).await?;
                                response
                                    .into_owned_bytes()
                                    .map_err(|error| LifecycleError::ResponseCopy(error).into())
                            }
                            ClientInner::Http(client, _) => {
                                call_http(client, &method, input, scope, payload_budget)
                                    .await
                                    .map(BudgetedBuffer::into_vec)
                            }
                        }
                    }
                    .await;
                    if let Err(error) = &result {
                        target.observe_error(error);
                    }
                    result
                },
            )?
            .wait()
    }

    pub fn call_held(self) -> Result<HeldResponse, Error> {
        self.start_held(false)?.wait()
    }

    #[doc(hidden)]
    pub async fn call_held_record_async(self) -> CallRecordOutcome<HeldResponse> {
        match self.start_held(true) {
            Ok(started) => started.wait_original().await,
            Err(error) => CallRecordOutcome {
                result: Err(error),
                dispatch_target: None,
            },
        }
    }

    fn start_held(
        self,
        original: bool,
    ) -> Result<crate::call_execution::ExecutingCall<HeldResponse>, Error> {
        let PreparedCall {
            preparing,
            inner,
            target,
            deadline,
            method,
            stream,
        } = self.prepared;
        if stream {
            return Err(
                LifecycleError::Configuration("unary handoff requires begin_call".into()).into(),
            );
        }
        let payload_budget = preparing.payload_budget();
        let transport =
            move |input,
                  scope: CallScope,
                  record: Option<crate::call_execution::CallRecord<HeldResponse>>| async move {
                let target = inner.target_for_call(target, &scope, deadline).await?;
                if let Some(record) = record {
                    record.record_target(target.dispatch_target());
                }
                let result = async {
                    let response = match &target.transport {
                        ClientInner::Ipc(connection) => HeldResponse::from_response_data(
                            call_ipc(connection, &method, input, scope).await?,
                        )?,
                        ClientInner::Http(client, _) => HeldResponse::from_budgeted_bytes(
                            call_http(client, &method, input, scope, payload_budget).await?,
                        ),
                    };
                    Ok(response)
                }
                .await;
                if let Err(error) = &result {
                    target.observe_error(error);
                }
                result
            };
        if original {
            preparing.execute_whole_record(
                self.nbytes,
                self.materialize,
                move |input, scope, record| transport(input, scope, Some(record)),
            )
        } else {
            preparing.execute(self.nbytes, self.materialize, move |input, scope| {
                transport(input, scope, None)
            })
        }
    }
}

async fn call_ipc(
    connection: &PooledIpcClient,
    method: &str,
    input: BudgetedBuffer,
    scope: CallScope,
) -> Result<c2_ipc::ResponseData, Error> {
    let preparation = scope.clone();
    let control = IpcCallControl::new(move || scope.try_begin_dispatch().map_err(guard_error))
        .with_before_prepare(move || preparation.check_pre_dispatch().map_err(guard_error));
    connection
        .client
        .call_bound_owned_controlled_async_phased(&connection.binding, method, input, &control)
        .await
        .map_err(normalize_ipc_call_error)
}

async fn call_ipc_stream(
    connection: &PooledIpcClient,
    method: &str,
    input: BudgetedBuffer,
    scope: CallScope,
    payload_budget: PayloadBufferBudget,
    completion_owner: Box<dyn Send>,
    cancellation: c2_ipc::IpcStreamCancellation,
) -> Result<c2_ipc::IpcResponseStream, Error> {
    let preparation = scope.clone();
    let control = IpcCallControl::new(move || scope.try_begin_dispatch().map_err(guard_error))
        .with_before_prepare(move || preparation.check_pre_dispatch().map_err(guard_error));
    connection
        .client
        .stream_bound_owned_controlled_async_phased(
            &connection.binding,
            method,
            input,
            payload_budget,
            completion_owner,
            cancellation,
            &control,
        )
        .await
        .map_err(normalize_ipc_call_error)
}

async fn call_http_stream(
    client: &RelayAwareHttpClient,
    method: &str,
    input: BudgetedBuffer,
    scope: CallScope,
    payload_budget: PayloadBufferBudget,
    completion_owner: Box<dyn Send>,
    opening_cancellation: c2_ipc::IpcStreamCancellation,
    cancellation: HttpStreamCancellation,
    http_headroom_reserved: bool,
) -> Result<HttpResponseStream, Error> {
    let (bytes, mut reservation) = input.into_parts();
    if !http_headroom_reserved {
        // The final target is HTTP after an IPC preparation. Admission cannot
        // wait while holding the input's positive reservation.
        reservation
            .try_grow(1)
            .map_err(crate::call_execution::payload_admission_error)?;
    }
    let input_reservation = reservation
        .split_off(bytes.capacity() as u64)
        .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
    let input = BudgetedBuffer::new(bytes, input_reservation);
    let active = scope.clone();
    let deadline = scope.deadline();
    let before_attempt_cancellation = opening_cancellation.clone();
    let control = HttpCallControl::new(
        move || {
            if before_attempt_cancellation.is_requested() {
                return Err(pre_dispatch_stream_cancellation());
            }
            match active.observe_deadline() {
                CallState::PreDispatch | CallState::Dispatched => Ok(()),
                CallState::ExpiredPreDispatch => {
                    Err(guard_error(CallScopeError::DeadlineExceeded {
                        phase: TransportPhase::PreDispatch,
                    }))
                }
                CallState::ExpiredDispatchUncertain => {
                    Err(guard_error(CallScopeError::DeadlineExceeded {
                        phase: TransportPhase::DispatchUncertain,
                    }))
                }
                state => Err(guard_error(CallScopeError::InvalidState { state })),
            }
        },
        move || {
            if opening_cancellation.is_requested() {
                return Err(pre_dispatch_stream_cancellation());
            }
            scope.try_begin_dispatch().map_err(guard_error)
        },
    )
    .with_payload_budget(payload_budget, deadline);
    client
        .stream_controlled_async(
            method,
            Arc::new(input),
            reservation,
            completion_owner,
            cancellation,
            &control,
        )
        .await
        .map_err(|error| {
            let phase = match error.phase() {
                HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
            };
            normalize_http_error(error.into_source(), phase)
        })
}

fn pre_dispatch_stream_cancellation() -> c2_error::C2Error {
    c2_error::C2Error::new(
        c2_error::ErrorCode::CallCancelled,
        "consumer canceled the stream before dispatch",
    )
    .with_details(std::collections::BTreeMap::from([
        ("transport_phase".into(), "pre_dispatch".into()),
        ("stage".into(), "stream_open".into()),
    ]))
}

async fn call_http(
    client: &RelayAwareHttpClient,
    method: &str,
    input: BudgetedBuffer,
    scope: CallScope,
    payload_budget: PayloadBufferBudget,
) -> Result<BudgetedBuffer, Error> {
    let active = scope.clone();
    let deadline = scope.deadline();
    let control = HttpCallControl::new(
        // Resolution and probe waits share the original deadline and do not
        // grant business dispatch permission.
        move || match active.observe_deadline() {
            CallState::PreDispatch | CallState::Dispatched => Ok(()),
            CallState::ExpiredPreDispatch => Err(guard_error(CallScopeError::DeadlineExceeded {
                phase: TransportPhase::PreDispatch,
            })),
            CallState::ExpiredDispatchUncertain => {
                Err(guard_error(CallScopeError::DeadlineExceeded {
                    phase: TransportPhase::DispatchUncertain,
                }))
            }
            state => Err(guard_error(CallScopeError::InvalidState { state })),
        },
        move || scope.try_begin_dispatch().map_err(guard_error),
    )
    .with_payload_budget(payload_budget, deadline);
    // The controlled channel has no reqwest total/upload timer for ALL policy
    // modes, including inherited defaults and explicit Unlimited/600 seconds.
    client
        .call_controlled_async(method, Arc::new(input), &control)
        .await
        .map_err(|error| {
            let phase = match error.phase() {
                HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
            };
            normalize_http_error(error.into_source(), phase)
        })
}

pub use crate::lifetime::HeldResponse;

mod private {
    pub trait Sealed {}
}

impl private::Sealed for Client {}

/// Stable encoded-call boundary consumed by generated clients.
///
/// The private supertrait prevents downstream transport implementations from
/// bypassing Core route, identity, and transport admission.
#[doc(hidden)]
pub trait EncodedClient: private::Sealed {
    fn call_owned(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, Error>;

    fn call_held(&self, method: &str, request: &[u8]) -> Result<HeldResponse, Error>;
}

impl EncodedClient for Client {
    fn call_owned(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        self.begin_call(method)?.encode_slice(request)?.call_owned()
    }

    fn call_held(&self, method: &str, request: &[u8]) -> Result<HeldResponse, Error> {
        self.begin_call(method)?.encode_slice(request)?.call_held()
    }
}

/// Native endpoint connection ownership. Configuration and all accounting are
/// shared with the Runtime; only the physical client cache belongs to this endpoint.
#[doc(hidden)]
pub struct NativeEndpointContext {
    source: Arc<EndpointAcquisition>,
}

struct EndpointAcquisition {
    runtime: Runtime,
    address: String,
    config: c2_config::ClientIpcConfig,
    pool: c2_ipc::ClientPool,
    expected_identity: Option<c2_wire::handshake::ServerIdentity>,
}

impl EndpointAcquisition {
    fn acquire(
        &self,
        expected: &ExpectedRouteContract,
        deadline: ConnectDeadline,
    ) -> Result<PooledIpcClient, Error> {
        for index in 0..STALE_POOL_RECONNECT_ATTEMPTS {
            let client = self
                .pool
                .acquire_lease_with_deadline(&self.address, Some(&self.config), deadline)
                .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
            if let Some(expected_identity) = &self.expected_identity {
                let actual = client
                    .server_identity()
                    .expect("verified IPC client has identity");
                if actual != expected_identity {
                    return Err(normalize_ipc_error(
                        c2_ipc::IpcError::IdentityMismatch {
                            expected_server_id: expected_identity.server_id.clone(),
                            expected_server_instance_id: expected_identity
                                .server_instance_id
                                .clone(),
                            actual_server_id: actual.server_id.clone(),
                            actual_server_instance_id: actual.server_instance_id.clone(),
                        },
                        TransportPhase::PreDispatch,
                    ));
                }
            }
            match client.acquire_route_with_deadline(expected, deadline) {
                Ok(binding) => return Ok(PooledIpcClient::new(client, binding)),
                Err(error)
                    if index + 1 < STALE_POOL_RECONNECT_ATTEMPTS
                        && is_stale_pooled_session_error(&error) =>
                {
                    self.pool
                        .discard_if_same_with_deadline(&self.address, client.client(), deadline)
                        .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
                }
                Err(error) => return Err(normalize_ipc_error(error, TransportPhase::PreDispatch)),
            }
        }
        unreachable!("bounded acquire returns a route or its original error")
    }
}

impl NativeEndpointContext {
    pub fn deactivate(&self) {
        self.source.pool.deactivate_endpoint_scope();
    }
    pub async fn close(&self) {
        self.source.pool.close_endpoint_scope().await;
    }
    pub fn config(&self) -> &c2_config::ClientIpcConfig {
        &self.source.config
    }

    pub fn prepare(
        &self,
        expected: ExpectedRouteContract,
        attempt: &crate::ConnectAttempt,
    ) -> Result<Client, Error> {
        validate_expected_route_contract(&expected)?;
        self.source.runtime.ensure_active_with_attempt(attempt)?;
        let acquire = ClientAcquire::Endpoint(self.source.clone());
        let target = self
            .source
            .runtime
            .acquire_target(&expected, &acquire, attempt.deadline())?;
        self.source
            .runtime
            .finish_client(expected, acquire, target, attempt.deadline())
    }
}

impl Runtime {
    #[doc(hidden)]
    pub fn native_endpoint_context(
        &self,
        address: &str,
        expected_identity: Option<c2_wire::handshake::ServerIdentity>,
        attempt: &crate::ConnectAttempt,
    ) -> Result<NativeEndpointContext, Error> {
        let (config, domain) = self
            .ipc_client_domain(attempt.deadline())
            .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
        let pool = domain
            .fork_endpoint_scope(&config, attempt.deadline())
            .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
        Ok(NativeEndpointContext {
            source: Arc::new(EndpointAcquisition {
                runtime: self.clone(),
                address: address.into(),
                config,
                pool,
                expected_identity,
            }),
        })
    }

    pub fn connect(&self, expected: ExpectedRouteContract, mode: Connect) -> Result<Client, Error> {
        self.connect_with_options(expected, mode, ConnectOptions::new())
    }

    pub fn connect_with_options(
        &self,
        expected: ExpectedRouteContract,
        mode: Connect,
        options: ConnectOptions,
    ) -> Result<Client, Error> {
        let attempt = crate::ConnectAttempt::start(options)?;
        self.connect_with_attempt(expected, mode, &attempt)
    }

    /// Continue SDK acquisition without restarting its original budget.
    #[doc(hidden)]
    pub fn connect_with_attempt(
        &self,
        expected: ExpectedRouteContract,
        mode: Connect,
        attempt: &crate::ConnectAttempt,
    ) -> Result<Client, Error> {
        (|| {
            let deadline = attempt.deadline();
            crate::connect_deadline::check(deadline, "connect_start")?;
            validate_expected_route_contract(&expected)?;
            self.ensure_active_with_attempt(attempt)?;
            let acquire = match mode {
                Connect::DirectIpc { address } => ClientAcquire::Direct { address },
                Connect::ExplicitRelay { relay_url } => ClientAcquire::Relay {
                    anchor: relay_url,
                    settings: self.relay_client_settings_with_deadline(deadline)?,
                    http_only: true,
                },
                Connect::RelayAware => ClientAcquire::Relay {
                    anchor: self
                        .effective_relay_anchor_address_with_deadline(deadline)?
                        .ok_or(LifecycleError::MissingRelayAddress)?,
                    settings: self.relay_client_settings_with_deadline(deadline)?,
                    http_only: false,
                },
            };
            let target = self.acquire_target(&expected, &acquire, deadline)?;
            self.finish_client(expected, acquire, target, deadline)
        })()
        .map_err(|error| attempt.classify(error))
    }

    fn finish_client(
        &self,
        expected: ExpectedRouteContract,
        acquire: ClientAcquire,
        target: ClientTarget,
        deadline: ConnectDeadline,
    ) -> Result<Client, Error> {
        self.publish_client_with_deadline(target.observed_path, deadline, || Client {
            inner: Arc::new(ClientConnection {
                expected,
                runtime: self.clone(),
                acquire,
                current: RwLock::new(Arc::new(target)),
                refresh: Arc::new(tokio::sync::Mutex::new(())),
            }),
            options: CallOptions::new(),
        })
    }

    fn acquire_target(
        &self,
        expected: &ExpectedRouteContract,
        acquire: &ClientAcquire,
        deadline: ConnectDeadline,
    ) -> Result<ClientTarget, Error> {
        self.ensure_active_with_deadline(deadline)?;
        match acquire {
            ClientAcquire::Direct { address } => Ok(ClientTarget::new(
                ObservedPath::DirectIpc,
                ClientInner::Ipc(self.acquire_direct_ipc(address, expected, deadline)?),
            )),
            ClientAcquire::Endpoint(endpoint) => Ok(ClientTarget::new(
                ObservedPath::DirectIpc,
                ClientInner::Ipc(endpoint.acquire(expected, deadline)?),
            )),
            ClientAcquire::Relay {
                anchor,
                settings,
                http_only,
            } => self.acquire_relay_target(expected, anchor, settings, *http_only, deadline),
        }
    }

    fn acquire_direct_ipc(
        &self,
        address: &str,
        expected: &ExpectedRouteContract,
        deadline: ConnectDeadline,
    ) -> Result<PooledIpcClient, Error> {
        for attempt in 0..STALE_POOL_RECONNECT_ATTEMPTS {
            let client = self
                .acquire_ipc_client_with_deadline(address, deadline)
                .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
            match client.acquire_route_with_deadline(expected, deadline) {
                Ok(binding) => {
                    return Ok(PooledIpcClient::new(client, binding));
                }
                Err(error)
                    if attempt + 1 < STALE_POOL_RECONNECT_ATTEMPTS
                        && is_stale_pooled_session_error(&error) =>
                {
                    self.discard_ipc_client_with_deadline(address, client.client(), deadline)?;
                }
                Err(error) => {
                    return Err(normalize_ipc_error(error, TransportPhase::PreDispatch));
                }
            }
        }
        unreachable!("stale pooled direct IPC retry loop must return")
    }

    fn acquire_relay_ipc(
        &self,
        candidate: &RelayLocalIpcCandidate,
        expected: &ExpectedRouteContract,
        deadline: ConnectDeadline,
    ) -> Result<PooledIpcClient, Error> {
        for attempt in 0..STALE_POOL_RECONNECT_ATTEMPTS {
            let client = self
                .acquire_ipc_client_with_deadline(&candidate.address, deadline)
                .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;

            let actual = client.server_identity();
            if !actual.as_ref().is_some_and(|identity| {
                identity.server_id == candidate.server_id
                    && identity.server_instance_id == candidate.server_instance_id
            }) {
                let (actual_server_id, actual_server_instance_id) = actual
                    .map(|identity| {
                        (
                            identity.server_id.clone(),
                            identity.server_instance_id.clone(),
                        )
                    })
                    .unwrap_or_else(|| ("<missing>".to_string(), "<missing>".to_string()));
                self.discard_ipc_client_with_deadline(
                    &candidate.address,
                    client.client(),
                    deadline,
                )?;
                if attempt + 1 < STALE_POOL_RECONNECT_ATTEMPTS {
                    continue;
                }
                return Err(normalize_ipc_error(
                    c2_ipc::IpcError::IdentityMismatch {
                        expected_server_id: candidate.server_id.clone(),
                        expected_server_instance_id: candidate.server_instance_id.clone(),
                        actual_server_id,
                        actual_server_instance_id,
                    },
                    TransportPhase::PreDispatch,
                ));
            }

            match client.acquire_route_with_deadline(expected, deadline) {
                Ok(binding) => {
                    return Ok(PooledIpcClient::new(client, binding));
                }
                Err(error)
                    if attempt + 1 < STALE_POOL_RECONNECT_ATTEMPTS
                        && is_stale_pooled_session_error(&error) =>
                {
                    self.discard_ipc_client_with_deadline(
                        &candidate.address,
                        client.client(),
                        deadline,
                    )?;
                }
                Err(error) => {
                    return Err(normalize_ipc_error(error, TransportPhase::PreDispatch));
                }
            }
        }
        unreachable!("stale pooled relay IPC retry loop must return")
    }

    fn acquire_relay_target(
        &self,
        expected: &ExpectedRouteContract,
        anchor: &str,
        settings: &c2_config::ResolvedRelayClientConfig,
        http_only: bool,
        deadline: ConnectDeadline,
    ) -> Result<ClientTarget, Error> {
        let http = |client| {
            ClientTarget::new(
                if http_only {
                    ObservedPath::ExplicitRelay
                } else {
                    ObservedPath::RelayAwareRelay
                },
                ClientInner::Http(
                    client,
                    relay_default(settings.call_timeout_secs).expect("resolved timeout is valid"),
                ),
            )
        };
        if http_only {
            return self
                .connect_explicit_relay_http_client_with_deadline(
                    anchor,
                    expected.clone(),
                    settings.use_proxy,
                    settings.call_timeout_secs,
                    settings.remote_payload_chunk_size,
                    deadline,
                )
                .map(http);
        }
        let resolved = self.resolve_relay_connection_with_deadline(
            anchor,
            expected.clone(),
            settings.use_proxy,
            settings.call_timeout_secs,
            settings.remote_payload_chunk_size,
            deadline,
        )?;
        self.ensure_active_with_deadline(deadline)?;
        let resolved = self.reconcile_relay_connection_with_deadline(resolved, deadline)?;
        match resolved {
            RelayResolvedConnection::Http { client } => Ok(http(client)),
            RelayResolvedConnection::Ipc {
                client, candidate, ..
            } => {
                match self.acquire_relay_ipc(&candidate, expected, deadline) {
                    Ok(connection) => Ok(ClientTarget::new(
                        ObservedPath::RelayAwareLocalIpc,
                        ClientInner::Ipc(connection),
                    )),
                    Err(error) if local_candidate_failure_is_terminal(&error) => Err(error),
                    Err(direct_error) => {
                        self.ensure_active_with_deadline(deadline)?;
                        let resolved = Runtime::resolve_relay_connection_after_local_ipc_failures_with_deadline(
                            client, std::slice::from_ref(&candidate), deadline,
                        ).map_err(|error| retain_local_ipc_cause(error, &direct_error))?;
                        match self.reconcile_relay_connection_with_deadline(resolved, deadline)? {
                            RelayResolvedConnection::Http { client } => Ok(http(client)),
                            RelayResolvedConnection::Ipc { candidate, .. } => {
                                Ok(ClientTarget::new(
                                    ObservedPath::RelayAwareLocalIpc,
                                    ClientInner::Ipc(
                                        self.acquire_relay_ipc(&candidate, expected, deadline)?,
                                    ),
                                ))
                            }
                        }
                    }
                }
            }
        }
    }
}

fn is_stale_pooled_session_error(error: &c2_ipc::IpcError) -> bool {
    match error {
        c2_ipc::IpcError::Closed => true,
        c2_ipc::IpcError::Io(io_error) | c2_ipc::IpcError::Connect(io_error) => matches!(
            io_error.kind(),
            ErrorKind::UnexpectedEof
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::NotConnected
        ),
        _ => false,
    }
}

pub(crate) fn normalize_ipc_call_error(error: IpcCallError) -> Error {
    let phase = match error.phase() {
        c2_ipc::TransportPhase::PreDispatch => TransportPhase::PreDispatch,
        c2_ipc::TransportPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
    };
    normalize_ipc_error(error.into_source(), phase)
}

// Candidate exclusion belongs to the relay resolver; its final denial is
// enriched here, where the original typed IPC acquisition error is still owned.
fn retain_local_ipc_cause(mut resolution: Error, direct: &Error) -> Error {
    let Error::Semantic(denial) = &mut resolution else {
        return resolution;
    };
    if denial.code != c2_error::ErrorCode::FallbackDenied {
        return resolution;
    }
    denial
        .details
        .insert("direct_ipc_failure".into(), direct.to_string());
    let (kind, phase) = match direct {
        Error::Semantic(error) => {
            denial.details.insert(
                "direct_ipc_failure_envelope".into(),
                serde_json::to_string(&error.envelope())
                    .expect("canonical IPC cause contains only serializable fields"),
            );
            (format!("semantic:{}", error.code.name()), "pre_dispatch")
        }
        Error::Transport(error) => {
            let kind = match error
                .source()
                .and_then(|source| source.downcast_ref::<c2_ipc::IpcError>())
            {
                Some(c2_ipc::IpcError::Io(error) | c2_ipc::IpcError::Connect(error)) => {
                    format!("io:{:?}", error.kind())
                }
                Some(c2_ipc::IpcError::Closed) => "ipc:closed".into(),
                _ => "transport:ipc".into(),
            };
            let phase = match error.phase() {
                TransportPhase::PreDispatch => "pre_dispatch",
                TransportPhase::DispatchUncertain => "dispatch_uncertain",
            };
            (kind, phase)
        }
        Error::ProtocolVersionMismatch { .. } => {
            ("protocol_version_mismatch".into(), "pre_dispatch")
        }
        Error::Contract(_) => ("contract".into(), "pre_dispatch"),
        Error::Admission(_) => ("admission".into(), "pre_dispatch"),
        Error::Lifecycle(_) => ("lifecycle".into(), "pre_dispatch"),
    };
    denial
        .details
        .insert("direct_ipc_failure_kind".into(), kind);
    denial
        .details
        .insert("direct_ipc_failure_phase".into(), phase.into());
    resolution
}

fn local_candidate_failure_is_terminal(error: &Error) -> bool {
    matches!(
        error,
        Error::ProtocolVersionMismatch { .. } | Error::Lifecycle(LifecycleError::RuntimeRetired)
    ) || matches!(
        error,
        Error::Semantic(error)
            if matches!(
                error.code,
                c2_error::ErrorCode::CallDeadlineExceeded
                    | c2_error::ErrorCode::ContractMismatch
                    | c2_error::ErrorCode::IdentityMismatch
                    | c2_error::ErrorCode::ProtocolViolation
            )
    )
}

fn relay_default(seconds: f64) -> Result<Option<Duration>, LifecycleError> {
    if seconds == 0.0 {
        return Ok(None);
    }
    c2_config::timeout_from_seconds(seconds)
        .map(Some)
        .map_err(|_| LifecycleError::Configuration("HTTP call timeout is not representable".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_pool_is_a_terminal_relay_candidate_failure() {
        let error = normalize_ipc_error(c2_ipc::IpcError::PoolRetired, TransportPhase::PreDispatch);
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::RuntimeRetired)
        ));
        assert!(local_candidate_failure_is_terminal(&error));
        assert!(error.local_io_cause().is_none());
    }

    #[test]
    fn protocol_version_mismatch_is_terminal_with_both_versions() {
        let error = normalize_ipc_error(
            c2_ipc::IpcError::ProtocolVersionMismatch {
                local: 14,
                peer: 15,
            },
            TransportPhase::PreDispatch,
        );
        assert!(matches!(
            error,
            Error::ProtocolVersionMismatch {
                local: 14,
                peer: 15
            }
        ));
        assert!(local_candidate_failure_is_terminal(&error));
        assert!(error.local_io_cause().is_none());
        assert_eq!(
            error.to_string(),
            "IPC protocol version mismatch: local=14, peer=15"
        );
        assert!(!is_stale_pooled_session_error(
            &c2_ipc::IpcError::ProtocolVersionMismatch {
                local: 14,
                peer: 15
            }
        ));
    }

    #[test]
    fn held_inline_response_preserves_bytes_and_release_lifetime() {
        for bytes in [Vec::new(), b"inline response".to_vec()] {
            let source_ptr = bytes.as_ptr();
            let expected = bytes.clone();
            let mut held =
                HeldResponse::from_response_data(c2_ipc::ResponseData::Inline(bytes)).unwrap();
            assert_eq!(held.bytes(), expected);
            if !expected.is_empty() {
                assert_eq!(held.bytes().as_ptr(), source_ptr, "inline bytes must move");
            }
            assert!(!held.is_released());
            let mut invalidations = 0;
            held.invalidate_then_release(|| {
                invalidations += 1;
                Ok(())
            })
            .unwrap();
            held.invalidate_then_release(|| panic!("invalidation must run once"))
                .unwrap();
            assert_eq!(invalidations, 1);
            assert!(held.is_released());
        }
    }
}
