use std::fmt;
use std::io::ErrorKind;
use std::sync::Arc;

use c2_contract::{ExpectedRouteContract, validate_expected_route_contract};
use c2_http::client::{HttpCallPhase, RelayAwareHttpClient, RelayLocalIpcCandidate};
use c2_ipc::{IpcCallError, RouteBinding, SyncClient};

use crate::session::RelayResolvedConnection;
use crate::{
    Error, HeldResponse, LifecycleError, Runtime, TransportPhase, normalize_http_error,
    normalize_ipc_error,
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
pub struct Client {
    expected: ExpectedRouteContract,
    observed_path: ObservedPath,
    inner: ClientInner,
}

enum ClientInner {
    Ipc(PooledIpcClient),
    Http(RelayAwareHttpClient),
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
            .field("expected", &self.expected)
            .field("observed_path", &self.observed_path)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn expected_route(&self) -> &ExpectedRouteContract {
        &self.expected
    }

    pub const fn observed_path(&self) -> ObservedPath {
        self.observed_path
    }

    fn call_ipc(
        connection: &PooledIpcClient,
        method: &str,
        request: &[u8],
    ) -> Result<c2_ipc::ResponseLease, Error> {
        let response = connection
            .client
            .call_bound_phased(&connection.binding, method, request)
            .map_err(normalize_ipc_call_error)?;
        Ok(connection.client.lease_response(response))
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
        match &self.inner {
            ClientInner::Ipc(connection) => {
                let lease = Self::call_ipc(connection, method, request)?;
                lease
                    .into_owned_bytes()
                    .map_err(|error| LifecycleError::ResponseCopy(error).into())
            }
            ClientInner::Http(client) => client.call_phased(method, request).map_err(|error| {
                let phase = match error.phase() {
                    HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                    HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
                };
                normalize_http_error(error.into_source(), phase)
            }),
        }
    }

    fn call_held(&self, method: &str, request: &[u8]) -> Result<HeldResponse, Error> {
        match &self.inner {
            ClientInner::Ipc(connection) => {
                let lease = Self::call_ipc(connection, method, request)?;
                HeldResponse::from_response_lease(lease).map_err(Error::from)
            }
            ClientInner::Http(client) => {
                let bytes = client.call_phased(method, request).map_err(|error| {
                    let phase = match error.phase() {
                        HttpCallPhase::PreDispatch => TransportPhase::PreDispatch,
                        HttpCallPhase::DispatchUncertain => TransportPhase::DispatchUncertain,
                    };
                    normalize_http_error(error.into_source(), phase)
                })?;
                Ok(HeldResponse::from_owned_bytes(bytes))
            }
        }
    }
}

impl Runtime {
    pub fn connect(&self, expected: ExpectedRouteContract, mode: Connect) -> Result<Client, Error> {
        validate_expected_route_contract(&expected)?;
        match mode {
            Connect::DirectIpc { address } => {
                let connection = self.acquire_direct_ipc(&address, &expected)?;
                self.finish_client(
                    expected,
                    ObservedPath::DirectIpc,
                    ClientInner::Ipc(connection),
                )
            }
            Connect::ExplicitRelay { relay_url } => {
                let settings = self.relay_client_settings()?;
                let client = self
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
                    ClientInner::Http(client),
                )
            }
            Connect::RelayAware => self.connect_relay_aware(expected),
        }
    }

    fn finish_client(
        &self,
        expected: ExpectedRouteContract,
        observed_path: ObservedPath,
        inner: ClientInner,
    ) -> Result<Client, Error> {
        self.record_path(observed_path);
        Ok(Client {
            expected,
            observed_path,
            inner,
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
            RelayResolvedConnection::Http { client } => self.finish_client(
                expected,
                ObservedPath::RelayAwareRelay,
                ClientInner::Http(client),
            ),
            RelayResolvedConnection::Ipc { client, candidate } => {
                match self.acquire_relay_ipc(&candidate, &expected) {
                    Ok(connection) => self.finish_client(
                        expected,
                        ObservedPath::RelayAwareLocalIpc,
                        ClientInner::Ipc(connection),
                    ),
                    Err(error) if local_candidate_failure_is_terminal(&error) => Err(error),
                    Err(_) => {
                        let resolved = Runtime::resolve_relay_connection_after_local_ipc_failures(
                            client,
                            std::slice::from_ref(&candidate),
                        )
                        .map_err(normalize_resolution_error)?;
                        match resolved {
                            RelayResolvedConnection::Http { client } => self.finish_client(
                                expected,
                                ObservedPath::RelayAwareRelay,
                                ClientInner::Http(client),
                            ),
                            RelayResolvedConnection::Ipc { candidate, .. } => {
                                let connection = self.acquire_relay_ipc(&candidate, &expected)?;
                                self.finish_client(
                                    expected,
                                    ObservedPath::RelayAwareLocalIpc,
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
