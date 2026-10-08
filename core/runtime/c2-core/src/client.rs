use std::fmt;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use c2_config::CallOptions;
use c2_http::client::HttpCallControl;
use c2_ipc::IpcCallControl;

use crate::call_execution::{PreparingCall, RetainedCallInput, guard_error, scope_error};
use crate::{CallScope, CallScopeError, CallState};

use c2_contract::{ExpectedRouteContract, validate_expected_route_contract};
use c2_http::client::{HttpCallPhase, RelayAwareHttpClient, RelayLocalIpcCandidate};
use c2_ipc::{IpcCallError, RouteBinding, SyncClient};

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

/// Immutable route identity observed when one Core client is acquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedRoute {
    pub route_uid: String,
    pub route_revision: u64,
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
    observed_path: ObservedPath,
    observed_route: ObservedRoute,
    runtime: Runtime,
    path_default: Option<Duration>,
    transport: ClientInner,
}

enum ClientInner {
    Ipc(PooledIpcClient),
    Http(RelayAwareHttpClient, Option<Duration>),
}

struct PooledIpcClient {
    runtime: Runtime,
    address: String,
    client: Arc<SyncClient>,
    binding: RouteBinding,
}

impl Drop for PooledIpcClient {
    fn drop(&mut self) {
        self.runtime.release_ipc_client(&self.address, &self.client);
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("expected", &self.inner.expected)
            .field("observed_path", &self.inner.observed_path)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn expected_route(&self) -> &ExpectedRouteContract {
        &self.inner.expected
    }

    pub fn observed_path(&self) -> ObservedPath {
        self.inner.observed_path
    }

    pub fn observed_route(&self) -> &ObservedRoute {
        &self.inner.observed_route
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

    /// Release this view. Other views, executing calls and held responses retain
    /// their references to the same connection; no transport is cancelled here.
    pub fn close(self) {}

    /// Start the logical call BEFORE SDK serialization (including non-Send
    /// Python serializers, which remain on the caller). This reserves a finite
    /// slot and establishes the sole absolute deadline for the complete call.
    /// Initial connect remains a separate operation with its existing policy.
    #[doc(hidden)]
    pub fn begin_call(&self, method: &str) -> Result<PreparedCall, Error> {
        // Configuration/admission lock waits are part of this same logical
        // deadline. Never create a fresh D after first-domain resolution.
        let scope = CallScope::new(self.options, self.inner.path_default).map_err(scope_error)?;
        let preparing = self
            .inner
            .runtime
            .call_execution_context()?
            .prepare_scope(scope)?;
        Ok(PreparedCall {
            preparing,
            inner: self.inner.clone(),
            method: method.to_string(),
        })
    }
}

/// Opaque, single-use SDK preparation state. No cloning, scope mutation or
/// transport access is exposed. Drop abandons preparation without dispatch.
#[doc(hidden)]
pub struct PreparedCall {
    preparing: PreparingCall,
    inner: Arc<ClientConnection>,
    method: String,
}

impl PreparedCall {
    /// Reserve known prepared input bytes before native allocation/copy. For
    /// unknown pickle length, serialize on the caller after begin_call, then
    /// charge the resulting length; this checks the original deadline. It is
    /// not a hard-real-time guarantee for synchronous user serialization.
    pub fn charge_input(&mut self, nbytes: u64) -> Result<(), Error> {
        self.preparing.charge_input(nbytes)
    }

    /// Hand off ALREADY PREPARED native-owned data, not a user serializer.
    /// The factory must produce exactly nbytes bytes. Finite execution owns
    /// materialization, input and transport until real completion; Unlimited
    /// drives the same async transport inline without a task/budget charge.
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

    /// Borrowed compatibility entry: charge before making the owned native copy.
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
    pub fn call_owned(self) -> Result<Vec<u8>, Error> {
        let PreparedCall {
            preparing,
            inner,
            method,
        } = self.prepared;
        preparing
            .execute(
                self.nbytes,
                self.materialize,
                move |input, scope| async move {
                    match &inner.transport {
                        ClientInner::Ipc(connection) => {
                            let lease = call_ipc(connection, &method, input, scope).await?;
                            lease
                                .into_owned_bytes()
                                .map_err(|error| LifecycleError::ResponseCopy(error).into())
                        }
                        ClientInner::Http(client, _) => {
                            call_http(client, &method, input, scope).await
                        }
                    }
                },
            )?
            .wait()
    }

    pub fn call_held(self) -> Result<HeldResponse, Error> {
        let PreparedCall {
            preparing,
            inner,
            method,
        } = self.prepared;
        preparing
            .execute(
                self.nbytes,
                self.materialize,
                move |input, scope| async move {
                    let response = match &inner.transport {
                        ClientInner::Ipc(connection) => HeldResponse::from_response_lease(
                            call_ipc(connection, &method, input, scope).await?,
                        )?,
                        ClientInner::Http(client, _) => HeldResponse::from_owned_bytes(
                            call_http(client, &method, input, scope).await?,
                        ),
                    };
                    Ok(response.with_connection(inner))
                },
            )?
            .wait()
    }
}

async fn call_ipc(
    connection: &PooledIpcClient,
    method: &str,
    input: Arc<RetainedCallInput>,
    scope: CallScope,
) -> Result<c2_ipc::ResponseLease, Error> {
    let preparation = scope.clone();
    let control = IpcCallControl::new(move || scope.try_begin_dispatch().map_err(guard_error))
        .with_before_prepare(move || preparation.check_pre_dispatch().map_err(guard_error));
    let response = connection
        .client
        .call_bound_controlled_async_phased(
            &connection.binding,
            method,
            input.as_ref().as_ref(),
            &control,
        )
        .await
        .map_err(normalize_ipc_call_error)?;
    Ok(connection.client.lease_response(response))
}

async fn call_http(
    client: &RelayAwareHttpClient,
    method: &str,
    input: Arc<RetainedCallInput>,
    scope: CallScope,
) -> Result<Vec<u8>, Error> {
    let active = scope.clone();
    let control = HttpCallControl::new(
        // A preceding business POST can be authoritatively refused as stale.
        // Resolution/probe waits keep Dispatched sticky and the original D;
        // they grant no dispatch/retry permission themselves.
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
        move |previous| {
            match previous {
                None => scope.try_begin_dispatch(),
                Some(HttpCallPhase::PreDispatch) => {
                    scope.try_begin_retry_dispatch(TransportPhase::PreDispatch)
                }
                Some(HttpCallPhase::DispatchUncertain) => {
                    scope.try_begin_retry_dispatch(TransportPhase::DispatchUncertain)
                }
            }
            .map_err(guard_error)
        },
    );
    // The controlled channel has no reqwest total/upload timer for ALL policy
    // modes, including inherited defaults and explicit Unlimited/600 seconds.
    client
        .call_controlled_async(method, input, &control)
        .await
        .map_err(|error| {
            let phase = match error.phase() {
                HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
            };
            normalize_http_error(error.into_source(), phase)
        })
}

/// Public held facade also pins the exact acquired client while its underlying
/// response lease lives. The existing private carrier remains the sole release
/// and SDK invalidation authority; connection lifetime adds no free mechanism.
pub struct HeldResponse {
    response: crate::lifetime::HeldResponse,
    connection: Option<Arc<ClientConnection>>,
}

impl HeldResponse {
    pub fn from_owned_bytes(bytes: Vec<u8>) -> Self {
        Self {
            response: crate::lifetime::HeldResponse::from_owned_bytes(bytes),
            connection: None,
        }
    }

    pub fn from_response_lease(lease: c2_ipc::ResponseLease) -> Result<Self, LifecycleError> {
        Ok(Self {
            response: crate::lifetime::HeldResponse::from_response_lease(lease)?,
            connection: None,
        })
    }

    fn with_connection(mut self, connection: Arc<ClientConnection>) -> Self {
        self.connection = Some(connection);
        self
    }

    pub fn bytes(&self) -> &[u8] {
        self.response.bytes()
    }
    pub fn is_released(&self) -> bool {
        self.response.is_released()
    }

    pub fn invalidate_then_release<F>(&mut self, invalidate: F) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let result = self.response.invalidate_then_release(invalidate);
        self.connection.take();
        result
    }
}

mod private {
    pub trait Sealed {}
}

impl private::Sealed for Client {}

/// Stable encoded-call boundary consumed by generated clients.
///
/// The private supertrait prevents downstream transport implementations from
/// bypassing Core route, identity, and lease validation.
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

impl Runtime {
    pub fn connect(&self, expected: ExpectedRouteContract, mode: Connect) -> Result<Client, Error> {
        validate_expected_route_contract(&expected)?;
        match mode {
            Connect::DirectIpc { address } => {
                let connection = self.acquire_direct_ipc(&address, &expected)?;
                let observed_route = observed_ipc_route(&connection);
                self.finish_client(
                    expected,
                    ObservedPath::DirectIpc,
                    observed_route,
                    ClientInner::Ipc(connection),
                )
            }
            Connect::ExplicitRelay { relay_url } => {
                let settings = self.relay_client_settings()?;
                let (client, route_uid, route_revision) = self
                    .connect_explicit_relay_http_client(
                        &relay_url,
                        expected.clone(),
                        settings.use_proxy,
                        settings.max_attempts,
                        settings.call_timeout_secs,
                        settings.remote_payload_chunk_size,
                    )
                    .map_err(normalize_resolution_error)?;
                self.finish_client(
                    expected,
                    ObservedPath::ExplicitRelay,
                    ObservedRoute {
                        route_uid,
                        route_revision,
                    },
                    ClientInner::Http(client, relay_default(settings.call_timeout_secs)?),
                )
            }
            Connect::RelayAware => self.connect_relay_aware(expected),
        }
    }

    fn finish_client(
        &self,
        expected: ExpectedRouteContract,
        observed_path: ObservedPath,
        observed_route: ObservedRoute,
        inner: ClientInner,
    ) -> Result<Client, Error> {
        let path_default = match &inner {
            ClientInner::Ipc(_) => None,
            ClientInner::Http(_, default) => *default,
        };
        self.record_path(observed_path);
        Ok(Client {
            inner: Arc::new(ClientConnection {
                expected,
                observed_path,
                observed_route,
                runtime: self.clone(),
                path_default,
                transport: inner,
            }),
            options: CallOptions::new(),
        })
    }

    fn acquire_direct_ipc(
        &self,
        address: &str,
        expected: &ExpectedRouteContract,
    ) -> Result<PooledIpcClient, Error> {
        for attempt in 0..STALE_POOL_RECONNECT_ATTEMPTS {
            let client = self
                .acquire_ipc_client(address)
                .map_err(|error| normalize_ipc_error(error, TransportPhase::PreDispatch))?;
            match client.acquire_route(expected) {
                Ok(binding) => {
                    return Ok(PooledIpcClient {
                        runtime: self.clone(),
                        address: address.to_string(),
                        client,
                        binding,
                    });
                }
                Err(error)
                    if attempt + 1 < STALE_POOL_RECONNECT_ATTEMPTS
                        && is_stale_pooled_session_error(&error) =>
                {
                    self.discard_ipc_client(address, &client);
                }
                Err(error) => {
                    self.release_ipc_client(address, &client);
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
    ) -> Result<PooledIpcClient, Error> {
        for attempt in 0..STALE_POOL_RECONNECT_ATTEMPTS {
            let client = self
                .acquire_ipc_client(&candidate.address)
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
                self.discard_ipc_client(&candidate.address, &client);
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

            match client.acquire_route_token(
                expected,
                &candidate.route_uid,
                candidate.route_revision,
            ) {
                Ok(binding) => {
                    return Ok(PooledIpcClient {
                        runtime: self.clone(),
                        address: candidate.address.clone(),
                        client,
                        binding,
                    });
                }
                Err(error)
                    if attempt + 1 < STALE_POOL_RECONNECT_ATTEMPTS
                        && is_stale_pooled_session_error(&error) =>
                {
                    self.discard_ipc_client(&candidate.address, &client);
                }
                Err(error) => {
                    self.release_ipc_client(&candidate.address, &client);
                    return Err(normalize_ipc_error(error, TransportPhase::PreDispatch));
                }
            }
        }
        unreachable!("stale pooled relay IPC retry loop must return")
    }

    fn connect_relay_aware(&self, expected: ExpectedRouteContract) -> Result<Client, Error> {
        let relay_anchor_address = self
            .effective_relay_anchor_address()?
            .ok_or(LifecycleError::MissingRelayAddress)?;
        let settings = self.relay_client_settings()?;
        let resolved = self
            .resolve_relay_connection(
                &relay_anchor_address,
                expected.clone(),
                settings.use_proxy,
                settings.max_attempts,
                settings.call_timeout_secs,
                settings.remote_payload_chunk_size,
            )
            .map_err(normalize_resolution_error)?;
        match resolved {
            RelayResolvedConnection::Http {
                client,
                route_uid,
                route_revision,
            } => self.finish_client(
                expected,
                ObservedPath::RelayAwareRelay,
                ObservedRoute {
                    route_uid,
                    route_revision,
                },
                ClientInner::Http(client, relay_default(settings.call_timeout_secs)?),
            ),
            RelayResolvedConnection::Ipc { client, candidate } => {
                match self.acquire_relay_ipc(&candidate, &expected) {
                    Ok(connection) => {
                        let observed_route = observed_ipc_route(&connection);
                        self.finish_client(
                            expected,
                            ObservedPath::RelayAwareLocalIpc,
                            observed_route,
                            ClientInner::Ipc(connection),
                        )
                    }
                    Err(error) if local_candidate_failure_is_terminal(&error) => Err(error),
                    Err(_) => {
                        let resolved = Runtime::resolve_relay_connection_after_local_ipc_failures(
                            client,
                            std::slice::from_ref(&candidate),
                        )
                        .map_err(normalize_resolution_error)?;
                        match resolved {
                            RelayResolvedConnection::Http {
                                client,
                                route_uid,
                                route_revision,
                            } => self.finish_client(
                                expected,
                                ObservedPath::RelayAwareRelay,
                                ObservedRoute {
                                    route_uid,
                                    route_revision,
                                },
                                ClientInner::Http(
                                    client,
                                    relay_default(settings.call_timeout_secs)?,
                                ),
                            ),
                            RelayResolvedConnection::Ipc { candidate, .. } => {
                                let connection = self.acquire_relay_ipc(&candidate, &expected)?;
                                let observed_route = observed_ipc_route(&connection);
                                self.finish_client(
                                    expected,
                                    ObservedPath::RelayAwareLocalIpc,
                                    observed_route,
                                    ClientInner::Ipc(connection),
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

fn observed_ipc_route(connection: &PooledIpcClient) -> ObservedRoute {
    ObservedRoute {
        route_uid: connection.binding.route_uid().to_string(),
        route_revision: connection.binding.route_revision(),
    }
}

fn is_stale_pooled_session_error(error: &c2_ipc::IpcError) -> bool {
    match error {
        c2_ipc::IpcError::Closed => true,
        c2_ipc::IpcError::Io(io_error) => matches!(
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

fn normalize_ipc_call_error(error: IpcCallError) -> Error {
    let phase = match error.phase() {
        c2_ipc::TransportPhase::PreDispatch => TransportPhase::PreDispatch,
        c2_ipc::TransportPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
    };
    normalize_ipc_error(error.into_source(), phase)
}

fn normalize_resolution_error(error: LifecycleError) -> Error {
    match error {
        LifecycleError::RelayHttp {
            status_code,
            message,
        } => normalize_http_error(
            c2_http::client::HttpError::ServerError(status_code, message),
            TransportPhase::PreDispatch,
        ),
        other => Error::Lifecycle(other),
    }
}

fn local_candidate_failure_is_terminal(error: &Error) -> bool {
    matches!(
        error,
        Error::Semantic(error)
            if matches!(
                error.code,
                c2_error::ErrorCode::ContractMismatch
                    | c2_error::ErrorCode::IdentityMismatch
                    | c2_error::ErrorCode::ProtocolViolation
            )
    )
}

fn relay_default(seconds: f64) -> Result<Option<Duration>, LifecycleError> {
    if seconds == 0.0 {
        return Ok(None);
    }
    Duration::try_from_secs_f64(seconds)
        .map(Some)
        .map_err(|_| LifecycleError::Configuration("HTTP call timeout is not representable".into()))
}
