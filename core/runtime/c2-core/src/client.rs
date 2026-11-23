use parking_lot::RwLock;
use std::error::Error as _;
use std::fmt;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use c2_config::{CallOptions, CallTimeout, ConnectDeadline, ConnectOptions, UnaryCallOptions};
use c2_http::client::{HttpCallControl, HttpResponseStream, HttpStreamCancellation};
use c2_ipc::IpcCallControl;

use crate::call_execution::{PreparingCall, guard_error, scope_error};
use crate::client_stream::{CoreResponseStream, StreamTransport};
use crate::{CallScope, CallScopeError, CallState};

use c2_contract::{ExpectedRouteContract, validate_expected_route_contract};
use c2_http::client::{HttpCallPhase, RelayAwareHttpClient, RelayLocalIpcCandidate};
use c2_ipc::{IpcCallError, RouteBinding};
use c2_mem::{BudgetedBuffer, DomainBudget, WritableSlab};

use crate::session::RelayResolvedConnection;
use crate::{
    CallOutcome, Error, LifecycleError, Runtime, TransportPhase, normalize_http_error,
    normalize_ipc_error,
};

const STALE_POOL_RECONNECT_ATTEMPTS: usize = 2;

/// Closed connection-mode selection for a route-bound Core client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connect {
    DirectIpc { address: String },
    ExplicitRelay { relay_url: String },
    RelayAware,
    /// Relay-aware acquisition against an explicit per-endpoint anchor URL
    /// inside a shared Runtime budget domain (preserves local IPC upgrade and
    /// HTTP fallback without mutating the Runtime's configured anchor).
    RelayAwareAt { anchor_url: String },
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
    /// The HTTP data client carries no total call timeout; only the caller's
    /// original scope deadline bounds a call.
    Http(RelayAwareHttpClient),
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
            ClientInner::Http(_) => None,
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
            Error::Semantic(semantic)
                if error.transport_phase() == Some(TransportPhase::PreDispatch) =>
            {
                use c2_error::ErrorCode::*;
                let fact = match semantic.code {
                    ResourceNotFound | ResourceRemoved | ResourceClosed | ContractMismatch
                    | RouteConflict => NEEDS_ACQUIRE,
                    ProtocolViolation
                        if semantic.details.get("reason").map(String::as_str)
                            == Some("unknown_topic") =>
                    {
                        0
                    }
                    IdentityMismatch | ProtocolViolation => TERMINAL_REFUSAL,
                    _ => 0,
                };
                self.facts.fetch_or(fact, Ordering::Release);
            }
            Error::Semantic(semantic)
                if semantic.code == c2_error::ErrorCode::CallCancelled
                    && matches!(
                        semantic.details.get("cause").map(String::as_str),
                        Some("route_unregistered" | "host_shutdown")
                    ) =>
            {
                self.facts.fetch_or(NEEDS_ACQUIRE, Ordering::Release);
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

    /// Submit an encoded unary request with optional per-operation deduplication.
    pub fn call_unary_with_options(
        &self,
        method: &str,
        request: &[u8],
        options: UnaryCallOptions,
    ) -> Result<Vec<u8>, Error> {
        self.begin_unary_with_options(method, options)?
            .encode_slice(request)?
            .call_owned()
    }

    /// Submit a unary request and return an independently owned response hold.
    pub fn call_unary_held_with_options(
        &self,
        method: &str,
        request: &[u8],
        options: UnaryCallOptions,
    ) -> Result<HeldResponse, Error> {
        self.begin_unary_with_options(method, options)?
            .encode_slice(request)?
            .call_held()
    }

    #[doc(hidden)]
    pub fn begin_unary_with_options(
        &self,
        method: &str,
        options: UnaryCallOptions,
    ) -> Result<PreparedCall, Error> {
        let target = self.inner.current.read().clone();
        let budget = CallBudget::new(options.call_options(), target.path_default)?;
        self.prepare_unary(method, budget, target, options.call_id())
    }

    #[doc(hidden)]
    pub fn begin_unary_with_budget(
        &self,
        method: &str,
        budget: CallBudget,
        call_id: Option<&str>,
    ) -> Result<PreparedCall, Error> {
        let target = self.inner.current.read().clone();
        self.prepare_unary(method, budget, target, call_id)
    }

    fn prepare_unary(
        &self,
        method: &str,
        budget: CallBudget,
        target: Arc<ClientTarget>,
        call_id: Option<&str>,
    ) -> Result<PreparedCall, Error> {
        let mode = if let Some(call_id) = call_id {
            validate_call_id(call_id)?;
            UnaryMode::Keyed(Arc::from(call_id))
        } else {
            UnaryMode::Plain
        };
        let mut prepared = self.prepare_with_budget(method, budget, target, false)?;
        prepared.unary_mode = mode;
        Ok(prepared)
    }

    /// Attach to or replay a keyed unary result without providing a request payload.
    pub fn recover_unary_call(
        &self,
        method: &str,
        call_id: &str,
        timeout: CallTimeout,
    ) -> Result<Vec<u8>, Error> {
        self.begin_recover(method, call_id, timeout)?.call_owned()
    }

    /// Attach to or replay a keyed unary result into an independently owned hold.
    pub fn recover_unary_call_held(
        &self,
        method: &str,
        call_id: &str,
        timeout: CallTimeout,
    ) -> Result<HeldResponse, Error> {
        self.begin_recover(method, call_id, timeout)?.call_held()
    }

    #[doc(hidden)]
    pub fn begin_recover(
        &self,
        method: &str,
        call_id: &str,
        timeout: CallTimeout,
    ) -> Result<EncodedCall, Error> {
        validate_call_id(call_id)?;
        let target = self.inner.current.read().clone();
        let budget = CallBudget::new(CallOptions::with_timeout(timeout), target.path_default)?;
        let mut prepared = self.prepare_with_budget(method, budget, target, false)?;
        prepared.unary_mode = UnaryMode::Recover(Arc::from(call_id));
        prepared.encode_vec(Vec::new())
    }

    /// Read the bound registration's current snapshot without taking a business slot.
    pub fn query_call(&self, call_id: &str, timeout: CallTimeout) -> Result<CallOutcome, Error> {
        validate_call_id(call_id)?;
        let target = self.inner.current.read().clone();
        let budget = CallBudget::new(CallOptions::with_timeout(timeout), target.path_default)?;
        budget.scope.check_pre_dispatch().map_err(scope_error)?;
        let query = async {
            let target = self
                .inner
                .target_for_call(target, &budget.scope, budget.deadline)
                .await?;
            match &target.transport {
                ClientInner::Ipc(connection) => connection
                    .client
                    .query_call_async(&connection.binding, call_id, budget.scope.deadline())
                    .await
                    .map_err(|error| match error {
                        c2_ipc::IpcError::Io(ref source)
                            if source.kind() == ErrorKind::TimedOut =>
                        {
                            scope_error(CallScopeError::DeadlineExceeded {
                                phase: TransportPhase::PreDispatch,
                            })
                        }
                        error => normalize_ipc_error(error, TransportPhase::PreDispatch),
                    }),
                ClientInner::Http(client) => client
                    .query_call_async(call_id)
                    .await
                    .map_err(normalize_http_call_error),
            }
        };
        let result = crate::call_execution::shared_executor()?.block_on(async {
            match budget.scope.deadline() {
                Some(deadline) => {
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), query)
                        .await
                        .map_err(|_| {
                            scope_error(CallScopeError::DeadlineExceeded {
                                phase: TransportPhase::PreDispatch,
                            })
                        })?
                }
                None => query.await,
            }
        });
        result.map(CallOutcome::from)
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

    /// Subscribe to a topic declared on this client's bound route contract.
    ///
    /// Waits for server `READY(start_seq)` before returning so empty topics
    /// confirm attachment immediately. Occupies one client-domain subscription
    /// slot under the internal `N-1` guard until the real terminal outcome.
    pub fn subscribe(
        &self,
        topic: &str,
        options: crate::SubscribeOptions,
    ) -> Result<crate::CoreTopicSubscription, Error> {
        self.subscribe_with_options(topic, options, self.options)
    }

    /// Subscribe with explicit per-call deadline options.
    pub fn subscribe_with_options(
        &self,
        topic: &str,
        options: crate::SubscribeOptions,
        call_options: CallOptions,
    ) -> Result<crate::CoreTopicSubscription, Error> {
        let runtime = crate::call_execution::shared_executor()?;
        runtime.block_on(self.subscribe_async(topic, options, call_options))
    }

    /// Async counterpart of [`Self::subscribe_with_options`].
    pub async fn subscribe_async(
        &self,
        topic: &str,
        options: crate::SubscribeOptions,
        call_options: CallOptions,
    ) -> Result<crate::CoreTopicSubscription, Error> {
        c2_contract::validate_contract_text_field("topic_name", topic).map_err(|error| {
            Error::Semantic(c2_error::C2Error::new(
                c2_error::ErrorCode::ProtocolViolation,
                error.to_string(),
            ))
        })?;
        let budget = CallBudget::new(call_options, None)?;
        let context = self.inner.runtime.call_execution_context()?;
        let permit = context.reserve_subscription_scope(&budget.scope)?;
        let payload_budget = context.payload_budget();
        let initial_target = self.inner.current.read().clone();
        let target = self
            .inner
            .target_for_call(initial_target, &budget.scope, budget.deadline)
            .await?;

        let cancellation = c2_ipc::IpcSubscribeCancellation::new();
        let result = match &target.transport {
            ClientInner::Ipc(connection) => {
                let scope = budget.scope.clone();
                let preparation = scope.clone();
                let control =
                    IpcCallControl::new(move || scope.try_begin_dispatch().map_err(guard_error))
                        .with_before_prepare(move || {
                            preparation.check_pre_dispatch().map_err(guard_error)
                        });
                let open_fut = connection.client.subscribe_bound_controlled_async_phased(
                    &connection.binding,
                    topic,
                    options.into(),
                    payload_budget,
                    Box::new(permit),
                    cancellation.clone(),
                    &control,
                );
                let opened = match budget.scope.deadline() {
                    Some(deadline) => match tokio::time::timeout_at(
                        tokio::time::Instant::from_std(deadline),
                        open_fut,
                    )
                    .await
                    {
                        Ok(result) => result.map_err(normalize_ipc_call_error),
                        Err(_) => {
                            cancellation.cancel();
                            let phase = match budget.scope.observe_deadline() {
                                CallState::ExpiredPreDispatch => TransportPhase::PreDispatch,
                                _ => TransportPhase::DispatchUncertain,
                            };
                            Err(scope_error(CallScopeError::DeadlineExceeded { phase }))
                        }
                    },
                    None => open_fut.await.map_err(normalize_ipc_call_error),
                };
                opened.map(|sub| {
                    crate::CoreTopicSubscription::from_ipc(
                        sub,
                        Arc::clone(&target),
                        budget.scope.clone(),
                        cancellation,
                    )
                })
            }
            ClientInner::Http(client) => {
                let http_cancellation = HttpStreamCancellation::new();
                let active = budget.scope.clone();
                let dispatch = budget.scope.clone();
                let control = HttpCallControl::new(
                    move || active.check_pre_dispatch().map_err(guard_error),
                    move || dispatch.try_begin_dispatch().map_err(guard_error),
                )
                .with_payload_budget(payload_budget, budget.scope.deadline());
                let open = client.subscribe_controlled_async(
                    topic,
                    options.into(),
                    Box::new(permit),
                    http_cancellation.clone(),
                    &control,
                );
                let normalize_open = |error: c2_http::client::HttpCallError| {
                    let phase = match error.phase() {
                        HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                        HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
                    };
                    normalize_http_error(error.into_source(), phase)
                };
                let opened = match budget.scope.deadline() {
                    Some(deadline) => match tokio::time::timeout_at(
                        tokio::time::Instant::from_std(deadline),
                        open,
                    )
                    .await
                    {
                        Ok(result) => result.map_err(normalize_open),
                        Err(_) => {
                            http_cancellation.cancel();
                            let phase = match budget.scope.observe_deadline() {
                                CallState::ExpiredPreDispatch => TransportPhase::PreDispatch,
                                _ => TransportPhase::DispatchUncertain,
                            };
                            Err(scope_error(CallScopeError::DeadlineExceeded { phase }))
                        }
                    },
                    None => open.await.map_err(normalize_open),
                };
                opened.map(|sub| {
                    crate::CoreTopicSubscription::from_http(
                        sub,
                        topic.to_string(),
                        Arc::clone(&target),
                        budget.scope.clone(),
                        http_cancellation,
                    )
                })
            }
        };
        if let Err(error) = &result {
            target.observe_error(error);
            let _ = budget.scope.complete_failure();
        }
        result
    }

    fn prepare_with_budget(
        &self,
        method: &str,
        budget: CallBudget,
        target: Arc<ClientTarget>,
        stream: bool,
    ) -> Result<PreparedCall, Error> {
        let context = self.inner.runtime.call_execution_context()?;
        let mut preparing = context.prepare_scope(budget.scope)?;
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
            unary_mode: UnaryMode::Plain,
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
    unary_mode: UnaryMode,
}

#[derive(Clone)]
enum UnaryMode {
    Plain,
    Keyed(Arc<str>),
    Recover(Arc<str>),
}

impl UnaryMode {
    fn call_id(&self) -> Option<&Arc<str>> {
        match self {
            Self::Plain => None,
            Self::Keyed(id) | Self::Recover(id) => Some(id),
        }
    }
}

fn validate_call_id(id: &str) -> Result<(), Error> {
    c2_config::validate_call_id(id).map_err(|error| LifecycleError::Configuration(error).into())
}

fn unsupported_keyed_method() -> Error {
    Error::Semantic(
        c2_error::C2Error::new(
            c2_error::ErrorCode::UnsupportedCallMode,
            "outcome operations require a declared unary method",
        )
        .with_details(std::collections::BTreeMap::from([(
            "transport_phase".into(),
            "pre_dispatch".into(),
        )])),
    )
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

    /// Acquire the request backing for direct serialization on an IPC unary call.
    /// A slab attempt replaces any provisional byte reservation. If it falls
    /// back, the normal input charge must precede owned materialization.
    pub fn try_alloc_request_slab(&mut self, nbytes: usize) -> Result<Option<WritableSlab>, Error> {
        self.preparing
            .scope()
            .check_pre_dispatch()
            .map_err(scope_error)?;
        if self.stream
            || nbytes as u64 <= c2_config::SHM_THRESHOLD
            || u32::try_from(nbytes).is_err()
        {
            return Ok(None);
        }
        let ClientInner::Ipc(connection) = &self.target.transport else {
            return Ok(None);
        };
        if nbytes as u64 > connection.binding.max_payload_size() {
            return Ok(None);
        }
        self.preparing.release_input_charge();
        connection
            .client
            .try_alloc_request_slab(nbytes)
            .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))
    }

    /// Hand off an already encoded slab, whose SHM backing is already charged.
    pub fn encode_slab(mut self, slab: WritableSlab) -> Result<EncodedCall, Error> {
        self.preparing
            .scope()
            .check_pre_dispatch()
            .map_err(scope_error)?;
        if self.stream {
            return Err(
                LifecycleError::Configuration("request slabs require begin_call".into()).into(),
            );
        }
        self.preparing.release_input_charge();
        Ok(EncodedCall {
            prepared: self,
            input: EncodedInput::Slab(slab),
        })
    }

    /// Hand off ALREADY PREPARED native-owned data, not a user serializer.
    /// The factory must produce exactly nbytes bytes. Native execution keeps
    /// its operation permit through real completion and the memory charge
    /// follows the actual input owner.
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
            input: EncodedInput::Materialize {
                nbytes,
                materialize: Box::new(materialize),
            },
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
    input: EncodedInput,
}

enum EncodedInput {
    Materialize {
        nbytes: usize,
        materialize: Box<dyn FnOnce() -> Result<Vec<u8>, Error> + Send>,
    },
    Slab(WritableSlab),
}

enum UnaryInput {
    Owned(BudgetedBuffer),
    Slab(WritableSlab),
}

enum UnaryResponse {
    Ipc(c2_ipc::ResponseData),
    Http(BudgetedBuffer),
}

impl UnaryInput {
    fn into_http(self, budget: &DomainBudget, scope: &CallScope) -> Result<BudgetedBuffer, Error> {
        let slab = match self {
            Self::Owned(buffer) => return Ok(buffer),
            Self::Slab(slab) => slab,
        };
        scope.check_pre_dispatch().map_err(scope_error)?;
        // Both owners exist during the copy. Admit immediately while holding
        // the slab so a full domain cannot wait for its own backing to drop.
        let mut reservation = budget
            .try_reserve_memory(slab.len() as u64)
            .map_err(crate::call_execution::payload_admission_error)?;
        let mut bytes = reservation.allocate_vec(slab.len()).map_err(|error| {
            LifecycleError::Configuration(format!("HTTP input allocation failed: {error}"))
        })?;
        bytes.extend_from_slice(slab.as_slice());
        drop(slab);
        Ok(BudgetedBuffer::new(bytes, reservation))
    }
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
            unary_mode,
        } = self.prepared;
        if unary_mode.call_id().is_some() {
            return Err(unsupported_keyed_method());
        }
        if !stream {
            return Err(LifecycleError::Configuration(
                "stream handoff requires begin_stream".into(),
            )
            .into());
        }
        let EncodedInput::Materialize {
            nbytes,
            materialize,
        } = self.input
        else {
            unreachable!("stream preparation cannot encode a slab");
        };
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
                    ClientInner::Http(client) => call_http_stream(
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
            preparing.execute_stream_record(nbytes, materialize, transport, abandon, on_abandon)?
        } else {
            preparing.execute_stream(nbytes, materialize, transport, abandon, on_abandon)?
        };
        Ok((started, scope, handle_cancellation, http_cancellation))
    }

    pub fn call_owned(self) -> Result<Vec<u8>, Error> {
        let call_id = self.prepared.unary_mode.call_id().cloned();
        self.start_unary(false, |response, _| match response {
            UnaryResponse::Ipc(response) => response
                .into_owned_bytes()
                .map_err(|error| LifecycleError::ResponseCopy(error).into()),
            UnaryResponse::Http(buffer) => Ok(buffer.into_parts().0),
        })?
        .wait()
        .map_err(|error| error.with_uncertain_call_id(call_id.as_deref()))
    }

    pub fn call_held(self) -> Result<HeldResponse, Error> {
        let call_id = self.prepared.unary_mode.call_id().cloned();
        self.start_held(false)?
            .wait()
            .map_err(|error| error.with_uncertain_call_id(call_id.as_deref()))
    }

    #[doc(hidden)]
    pub async fn call_held_record_async(self) -> CallRecordOutcome<HeldResponse> {
        let call_id = self.prepared.unary_mode.call_id().cloned();
        match self.start_held(true) {
            Ok(started) => {
                let CallRecordOutcome {
                    result,
                    dispatch_target,
                } = started.wait_original().await;
                CallRecordOutcome {
                    result: result
                        .map_err(|error| error.with_uncertain_call_id(call_id.as_deref())),
                    dispatch_target,
                }
            }
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
        self.start_unary(original, |response, budget| match response {
            UnaryResponse::Ipc(response) => {
                HeldResponse::from_response_data_in(response, budget).map_err(Error::from)
            }
            UnaryResponse::Http(buffer) => Ok(HeldResponse::from_budgeted_bytes(buffer)),
        })
    }

    fn start_unary<R: Send + 'static>(
        self,
        original: bool,
        finish: fn(UnaryResponse, &DomainBudget) -> Result<R, Error>,
    ) -> Result<crate::call_execution::ExecutingCall<R>, Error> {
        let PreparedCall {
            preparing,
            inner,
            target,
            deadline,
            method,
            stream,
            unary_mode,
        } = self.prepared;
        if stream {
            return Err(
                LifecycleError::Configuration("unary handoff requires begin_call".into()).into(),
            );
        }
        let payload_budget = preparing.payload_budget();
        let transport =
            move |input, scope: CallScope, record: Option<crate::call_execution::CallRecord<R>>| async move {
                let target = inner.target_for_call(target, &scope, deadline).await?;
                if let Some(record) = record {
                    record.record_target(target.dispatch_target());
                }
                let result = async {
                    let response = match &target.transport {
                        ClientInner::Ipc(connection) => UnaryResponse::Ipc(
                            call_ipc(connection, &method, input, scope, &unary_mode).await?,
                        ),
                        ClientInner::Http(client) => {
                            let input = input.into_http(&payload_budget, &scope)?;
                            UnaryResponse::Http(
                                call_http(
                                    client,
                                    &method,
                                    input,
                                    scope,
                                    payload_budget.clone(),
                                    &unary_mode,
                                )
                                .await?,
                            )
                        }
                    };
                    finish(response, &payload_budget)
                }
                .await;
                if let Err(error) = &result {
                    target.observe_error(error);
                }
                result
            };
        match self.input {
            EncodedInput::Slab(slab) => {
                preparing.execute_slab(slab, original, move |input, scope, record| {
                    transport(UnaryInput::Slab(input), scope, record)
                })
            }
            EncodedInput::Materialize {
                nbytes,
                materialize,
            } if original => {
                preparing.execute_whole_record(nbytes, materialize, move |input, scope, record| {
                    transport(UnaryInput::Owned(input), scope, Some(record))
                })
            }
            EncodedInput::Materialize {
                nbytes,
                materialize,
            } => preparing.execute(nbytes, materialize, move |input, scope| {
                transport(UnaryInput::Owned(input), scope, None)
            }),
        }
    }
}

async fn call_ipc(
    connection: &PooledIpcClient,
    method: &str,
    input: UnaryInput,
    scope: CallScope,
    mode: &UnaryMode,
) -> Result<c2_ipc::ResponseData, Error> {
    let preparation = scope.clone();
    let control = IpcCallControl::new(move || scope.try_begin_dispatch().map_err(guard_error))
        .with_before_prepare(move || preparation.check_pre_dispatch().map_err(guard_error));
    let call_id = mode.call_id().map(AsRef::as_ref);
    let result = match (mode, input) {
        (UnaryMode::Recover(id), _) => {
            connection
                .client
                .recover_call_bound_controlled_async_phased(
                    &connection.binding,
                    method,
                    id,
                    &control,
                )
                .await
        }
        (_, UnaryInput::Owned(input)) => {
            connection
                .client
                .call_bound_owned_with_key_controlled_async_phased(
                    &connection.binding,
                    method,
                    input,
                    call_id,
                    &control,
                )
                .await
        }
        (_, UnaryInput::Slab(slab)) => {
            connection
                .client
                .call_bound_slab_with_key_controlled_async_phased(
                    &connection.binding,
                    method,
                    slab,
                    call_id,
                    &control,
                )
                .await
        }
    };
    result.map_err(|error| {
        if call_id.is_some()
            && matches!(
                error.source_error(),
                c2_ipc::IpcError::MethodNotFound { .. }
            )
        {
            return unsupported_keyed_method();
        }
        normalize_ipc_call_error(error)
    })
}

async fn call_ipc_stream(
    connection: &PooledIpcClient,
    method: &str,
    input: BudgetedBuffer,
    scope: CallScope,
    payload_budget: DomainBudget,
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
    payload_budget: DomainBudget,
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
    payload_budget: DomainBudget,
    mode: &UnaryMode,
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
    match mode {
        UnaryMode::Plain => {
            client
                .call_controlled_async(method, Arc::new(input), &control)
                .await
        }
        UnaryMode::Keyed(call_id) => {
            client
                .call_keyed_controlled_async(method, call_id, Arc::new(input), &control)
                .await
        }
        UnaryMode::Recover(call_id) => {
            client
                .recover_call_controlled_async(method, call_id, &control)
                .await
        }
    }
    .map_err(normalize_http_call_error)
}

fn normalize_http_call_error(error: c2_http::client::HttpCallError) -> Error {
    let phase = match error.phase() {
        HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
        HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
    };
    normalize_http_error(error.into_source(), phase)
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
                .acquire_lease_with_deadline(&self.address, deadline)
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
        c2_config::LocalEndpoint::validate_address(address)
            .map_err(|error| LifecycleError::Configuration(error.to_string()))?;
        let pool_domain = self
            .ipc_client_domain(attempt.deadline())
            .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
        let pool = pool_domain
            .fork_endpoint_scope(attempt.deadline())
            .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
        Ok(NativeEndpointContext {
            source: Arc::new(EndpointAcquisition {
                runtime: self.clone(),
                address: address.into(),
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
                Connect::ExplicitRelay { relay_url } => {
                    let (anchor, settings) =
                        self.relay_business_config_with_deadline(Some(relay_url), deadline)?;
                    ClientAcquire::Relay {
                        anchor,
                        settings,
                        http_only: true,
                    }
                }
                Connect::RelayAware => {
                    let (anchor, settings) =
                        self.relay_business_config_with_deadline(None, deadline)?;
                    ClientAcquire::Relay {
                        anchor,
                        settings,
                        http_only: false,
                    }
                }
                Connect::RelayAwareAt { anchor_url } => {
                    let (anchor, settings) =
                        self.relay_business_config_with_deadline(Some(anchor_url), deadline)?;
                    ClientAcquire::Relay {
                        anchor,
                        settings,
                        http_only: false,
                    }
                }
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
                ClientInner::Http(client),
            )
        };
        if http_only {
            return self
                .connect_explicit_relay_http_client_with_deadline(
                    anchor,
                    expected.clone(),
                    settings.use_proxy,
                    deadline,
                )
                .map(http);
        }
        let resolved = self.resolve_relay_connection_with_deadline(
            anchor,
            expected.clone(),
            settings.use_proxy,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_controls_do_not_mark_a_bound_target_for_reacquisition_or_refusal() {
        use c2_error::{C2Error, ErrorCode};
        let expected = ExpectedRouteContract {
            route_name: "outcome-target".into(),
            crm_ns: "test.outcome".into(),
            crm_name: "Outcome".into(),
            crm_ver: "1.0.0".into(),
            abi_hash: "a".repeat(64),
            signature_hash: "b".repeat(64),
        };
        let client = RelayAwareHttpClient::new("http://127.0.0.1:9", expected, false, Default::default())
            .unwrap();
        let target = ClientTarget::new(ObservedPath::ExplicitRelay, ClientInner::Http(client));
        for code in [
            ErrorCode::CallIdConflict,
            ErrorCode::CallResultUnavailable,
            ErrorCode::CallOutcomeUnknown,
        ] {
            let mut semantic = C2Error::new(code, code.name());
            let (ipc, status, phase) = if code == ErrorCode::CallResultUnavailable {
                (
                    c2_ipc::IpcError::CrmError(semantic.to_wire_bytes()),
                    500,
                    TransportPhase::DispatchUncertain,
                )
            } else {
                semantic.details.insert("dispatch_phase".into(), "pre_dispatch".into());
                (
                    c2_ipc::IpcError::CallRejected(semantic.clone()),
                    409,
                    TransportPhase::PreDispatch,
                )
            };
            let http = c2_http::client::HttpError::ServerError(
                status,
                serde_json::to_string(&semantic.envelope()).unwrap(),
            );
            for error in [normalize_ipc_error(ipc, phase), normalize_http_error(http, phase)] {
                assert_eq!(
                    error.transport_phase(),
                    (status == 409).then_some(TransportPhase::PreDispatch),
                );
                target.observe_error(&error);
                assert_eq!(target.facts.load(Ordering::Acquire), 0, "{code:?}");
            }
        }
    }

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
