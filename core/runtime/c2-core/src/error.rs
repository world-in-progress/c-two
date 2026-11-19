use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;

use c2_contract::ContractError;
use c2_error::{C2Error, C2ErrorEnvelope, ErrorCode};
use c2_http::client::HttpError;
use c2_ipc::IpcError;
use c2_ipc::client::ChunkError;
use c2_local::{EndpointBindCause, EndpointIoError};
use c2_server::{ServerError, ServerFailure, ServerStartupTimeout};

use crate::RegisterFailureOutcome;

/// Failures in process runtime setup, route lifecycle, and relay projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    RuntimeRetired,
    ShutdownWaitTimeout { stage: &'static str },
    InvalidServerId(String),
    ConfigFrozen,
    DuplicateRoute(String),
    MissingRoute(String),
    MissingRelayAddress,
    RegisterFailure(Box<RegisterFailureOutcome>),
    HeldResponseRelease { invalidation_error: String },
    ResponseCopy(String),
    Configuration(String),
    Server(ServerFailure),
    StartupTimeout(ServerStartupTimeout),
    Relay(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeRetired => formatter.write_str("runtime is retired"),
            Self::ShutdownWaitTimeout { stage } => {
                write!(formatter, "shutdown wait deadline exceeded at {stage}")
            }
            Self::InvalidServerId(message) => formatter.write_str(message),
            Self::ConfigFrozen => formatter.write_str("runtime configuration is frozen"),
            Self::DuplicateRoute(name) => write!(formatter, "route already registered: {name}"),
            Self::MissingRoute(name) => write!(formatter, "route not registered: {name}"),
            Self::MissingRelayAddress => formatter.write_str("no relay address configured"),
            Self::RegisterFailure(outcome) => write!(
                formatter,
                "registration failed at {}: {}",
                outcome.failure_source, outcome.error_message
            ),
            Self::HeldResponseRelease { invalidation_error } => {
                write!(
                    formatter,
                    "held response invalidation failed: {invalidation_error}"
                )
            }
            Self::ResponseCopy(message) => write!(formatter, "response copy failed: {message}"),
            Self::Configuration(message) => {
                write!(formatter, "runtime configuration error: {message}")
            }
            Self::Server(message) => write!(formatter, "server error: {message}"),
            Self::StartupTimeout(timeout) => timeout.fmt(formatter),
            Self::Relay(message) => write!(formatter, "relay error: {message}"),
        }
    }
}

impl StdError for LifecycleError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Server(failure) => Some(failure),
            Self::StartupTimeout(timeout) => Some(timeout),
            _ => None,
        }
    }
}

impl From<ServerError> for LifecycleError {
    fn from(error: ServerError) -> Self {
        match error {
            ServerError::StartupTimeout(timeout)
            | ServerError::Failed(ServerFailure {
                startup_timeout: Some(timeout),
                ..
            }) => Self::StartupTimeout(timeout),
            other => Self::Server((&other).into()),
        }
    }
}

/// Facts from the actual local endpoint operation, not a retry or cleanup permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalIoCause {
    Bind(EndpointBindCause),
    Connect(EndpointIoError),
}

impl LocalIoCause {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Bind(_) => "bind",
            Self::Connect(_) => "connect",
        }
    }

    pub fn error(self) -> EndpointIoError {
        match self {
            Self::Bind(cause) => cause.error,
            Self::Connect(error) => error,
        }
    }
}

impl LifecycleError {
    pub fn local_io_cause(&self) -> Option<LocalIoCause> {
        match self {
            Self::Server(failure) => failure.bind_cause.map(LocalIoCause::Bind),
            _ => None,
        }
    }
}

/// Whether a transport failure is known to have happened before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportPhase {
    PreDispatch,
    DispatchUncertain,
}

/// Transport family retained for diagnostics without leaking transport errors
/// into the SDK surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Ipc,
    Http,
}

/// A local transport failure together with its replay-safety phase.
#[derive(Debug)]
pub struct TransportError {
    phase: TransportPhase,
    kind: TransportKind,
    source: Box<dyn StdError + Send + Sync>,
    semantic_error: Option<C2Error>,
}

impl TransportError {
    pub fn new<E>(phase: TransportPhase, kind: TransportKind, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            phase,
            kind,
            source: Box::new(source),
            semantic_error: None,
        }
    }

    pub const fn phase(&self) -> TransportPhase {
        self.phase
    }

    pub const fn kind(&self) -> TransportKind {
        self.kind
    }

    pub fn local_io_cause(&self) -> Option<LocalIoCause> {
        match self.source.downcast_ref::<IpcError>()? {
            IpcError::Connect(error) => Some(LocalIoCause::Connect(error.into())),
            _ => None,
        }
    }

    /// Canonical semantic information for a local transport admission failure.
    /// Replay safety still comes from `phase`, even when a semantic code exists.
    pub fn semantic_error(&self) -> Option<&C2Error> {
        self.semantic_error.as_ref()
    }

    /// Fallback is legal only when dispatch is known not to have happened.
    pub const fn is_fallback_eligible(&self) -> bool {
        matches!(self.phase, TransportPhase::PreDispatch)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} transport failure during {:?}: {}",
            self.kind, self.phase, self.source
        )
    }
}

impl StdError for TransportError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The language-neutral local C-Two error facade.
#[derive(Debug)]
pub enum Error {
    /// A complete fixed IPC preamble proved that the peer uses another version.
    ProtocolVersionMismatch {
        local: u32,
        peer: u32,
    },
    Semantic(C2Error),
    Contract(ContractError),
    Admission(ContractError),
    Transport(TransportError),
    Lifecycle(LifecycleError),
}

impl Error {
    /// The dispatch phase of this call, when established by Core.
    pub fn transport_phase(&self) -> Option<TransportPhase> {
        match self {
            Self::ProtocolVersionMismatch { .. }
            | Self::Contract(_)
            | Self::Admission(_)
            | Self::Lifecycle(LifecycleError::RuntimeRetired) => Some(TransportPhase::PreDispatch),
            Self::Transport(error) => Some(error.phase()),
            Self::Lifecycle(LifecycleError::ResponseCopy(_)) => {
                Some(TransportPhase::DispatchUncertain)
            }
            Self::Lifecycle(_) => None,
            Self::Semantic(error) => match error
                .details
                .get("transport_phase")
                .or_else(|| error.details.get("dispatch_phase"))
                .map(String::as_str)
            {
                Some("pre_dispatch") => Some(TransportPhase::PreDispatch),
                Some("dispatch_uncertain") => Some(TransportPhase::DispatchUncertain),
                _ => None,
            },
        }
    }

    pub fn local_io_cause(&self) -> Option<LocalIoCause> {
        match self {
            Self::Transport(error) => error.local_io_cause(),
            Self::Lifecycle(error) => error.local_io_cause(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolVersionMismatch { local, peer } => write!(
                formatter,
                "IPC protocol version mismatch: local={local}, peer={peer}"
            ),
            Self::Semantic(error) => error.fmt(formatter),
            Self::Contract(error) => write!(formatter, "contract error: {error}"),
            Self::Admission(error) => write!(formatter, "contract admission error: {error}"),
            Self::Transport(error) => error.fmt(formatter),
            Self::Lifecycle(error) => error.fmt(formatter),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::ProtocolVersionMismatch { .. } => None,
            Self::Semantic(error) => Some(error),
            Self::Contract(error) | Self::Admission(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

impl From<C2Error> for Error {
    fn from(error: C2Error) -> Self {
        Self::Semantic(error)
    }
}

impl From<ContractError> for Error {
    fn from(error: ContractError) -> Self {
        if matches!(&error, ContractError::LimitExceeded { .. }) {
            Self::Admission(error)
        } else {
            Self::Contract(error)
        }
    }
}

impl From<LifecycleError> for Error {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

/// Normalize an IPC error without flattening its transport source.
pub fn normalize_ipc_error(error: IpcError, phase: TransportPhase) -> Error {
    match error {
        IpcError::ResponseFailure(source) => {
            // Materialization failed after delivery. Neither admission nor
            // lookup classifications may make this failure replayable.
            let semantic_error = match source.as_ref() {
                IpcError::Chunk(ChunkError::Capacity(message)) => {
                    Some(response_capacity_error(message))
                }
                IpcError::LocalCallRejected(error) => Some(error.clone()),
                _ => None,
            };
            let mut transport = TransportError::new(
                TransportPhase::DispatchUncertain,
                TransportKind::Ipc,
                IpcError::ResponseFailure(source),
            );
            transport.semantic_error = semantic_error;
            Error::Transport(transport)
        }
        IpcError::PoolRetired => LifecycleError::RuntimeRetired.into(),
        IpcError::ProtocolVersionMismatch { local, peer } => {
            Error::ProtocolVersionMismatch { local, peer }
        }
        IpcError::LocalCallRejected(error) => Error::Semantic(error),
        IpcError::CallRejected(mut error) => {
            error
                .details
                .insert("transport_phase".into(), "pre_dispatch".into());
            error
                .details
                .insert("dispatch_phase".into(), "pre_dispatch".into());
            Error::Semantic(error)
        }
        IpcError::Chunk(ChunkError::Capacity(message)) => {
            let semantic = response_capacity_error(&message);
            let mut transport = TransportError::new(
                TransportPhase::DispatchUncertain,
                TransportKind::Ipc,
                IpcError::Chunk(ChunkError::Capacity(message)),
            );
            transport.semantic_error = Some(semantic);
            Error::Transport(transport)
        }
        IpcError::CrmError(bytes) => normalize_ipc_semantic_bytes(&bytes),
        IpcError::RouteNotFound(route_name) => semantic_route_error(
            ErrorCode::ResourceNotFound,
            format!("route not found: {route_name}"),
            [("route", route_name)],
        ),
        IpcError::RouteRemoved { route_name } => semantic_route_error(
            ErrorCode::ResourceRemoved,
            format!("route {route_name} was removed"),
            [("route", route_name)],
        ),
        IpcError::RouteClosed { route_name, reason } => semantic_route_error(
            ErrorCode::ResourceClosed,
            format!("route closed: {route_name}"),
            [("route", route_name), ("reason", reason)],
        ),
        IpcError::ContractMismatch(message) => Error::Semantic(
            C2Error::new(ErrorCode::ContractMismatch, message).with_details(BTreeMap::from([
                ("transport".into(), "ipc".into()),
                ("transport_phase".into(), "pre_dispatch".into()),
                ("dispatch_phase".into(), "pre_dispatch".into()),
            ])),
        ),
        IpcError::IdentityMismatch {
            expected_server_id,
            expected_server_instance_id,
            actual_server_id,
            actual_server_instance_id,
        } => Error::Semantic(
            C2Error::new(ErrorCode::IdentityMismatch, "IPC server identity mismatch").with_details(
                BTreeMap::from([
                    ("transport_phase".to_string(), "pre_dispatch".to_string()),
                    ("actual_server_id".to_string(), actual_server_id),
                    (
                        "actual_server_instance_id".to_string(),
                        actual_server_instance_id,
                    ),
                    ("expected_server_id".to_string(), expected_server_id),
                    (
                        "expected_server_instance_id".to_string(),
                        expected_server_instance_id,
                    ),
                ]),
            ),
        ),
        IpcError::Protocol(message) => {
            let mut error = protocol_violation("IPC protocol validation failed", "ipc", &message);
            error.details.insert(
                "transport_phase".into(),
                match phase {
                    TransportPhase::PreDispatch => "pre_dispatch",
                    TransportPhase::DispatchUncertain => "dispatch_uncertain",
                }
                .into(),
            );
            Error::Semantic(error)
        }
        IpcError::MethodNotFound {
            route_name,
            method_name,
        } => semantic_route_error(
            ErrorCode::ProtocolViolation,
            format!("method is not present in the acquired route: {method_name}"),
            [("route", route_name), ("method", method_name)],
        ),
        transport => Error::Transport(TransportError::new(phase, TransportKind::Ipc, transport)),
    }
}

fn response_capacity_error(message: &str) -> C2Error {
    C2Error::new(ErrorCode::ResourceUnavailable, message).with_details(BTreeMap::from([
        ("transport".into(), "ipc".into()),
        ("stage".into(), "response_reassembly_admission".into()),
        ("transport_phase".into(), "dispatch_uncertain".into()),
        ("fallback_eligible".into(), "false".into()),
    ]))
}

fn semantic_route_error<const N: usize>(
    code: ErrorCode,
    message: String,
    details: [(&str, String); N],
) -> Error {
    Error::Semantic(
        C2Error::new(code, message).with_details(
            details
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .chain([
                    ("transport_phase".into(), "pre_dispatch".into()),
                    ("dispatch_phase".into(), "pre_dispatch".into()),
                ])
                .collect(),
        ),
    )
}

/// Normalize an HTTP error without flattening its transport source.
pub fn normalize_http_error(error: HttpError, phase: TransportPhase) -> Error {
    match error {
        HttpError::LocalCallRejected(error) => Error::Semantic(error),
        HttpError::CrmError(bytes) => normalize_ipc_semantic_bytes(bytes.as_ref()),
        HttpError::ResponseError(status, bytes) => match std::str::from_utf8(bytes.as_ref()) {
            Ok(body) => normalize_http_call_semantic_body(body, status, phase),
            Err(error) => Error::Semantic(protocol_violation(
                "HTTP semantic error envelope is malformed",
                "http",
                &error.to_string(),
            )),
        },
        HttpError::ServerError(status, body) => {
            normalize_http_call_semantic_body(&body, status, phase)
        }
        transport => Error::Transport(TransportError::new(phase, TransportKind::Http, transport)),
    }
}

fn normalize_http_call_semantic_body(body: &str, status: u16, phase: TransportPhase) -> Error {
    let mut error = normalize_http_semantic_body(body);
    if let Error::Semantic(semantic) = &mut error {
        if status == 500 {
            clear_resource_failure_phase(semantic);
        } else if phase == TransportPhase::PreDispatch
            && semantic
                .details
                .get("dispatch_phase")
                .is_some_and(|phase| phase == "pre_dispatch")
        {
            semantic
                .details
                .insert("transport_phase".into(), "pre_dispatch".into());
        }
    }
    error
}

/// Decode a resource's Failed C2E1 reply without attributing an inner call's
/// admission phase to this already-executed resource call.
pub fn normalize_ipc_semantic_bytes(bytes: &[u8]) -> Error {
    let mut error = semantic_error_from_ipc_bytes(bytes);
    clear_resource_failure_phase(&mut error);
    Error::Semantic(error)
}

fn clear_resource_failure_phase(error: &mut C2Error) {
    strip_callback_error_phases(error);
}

/// Strip `transport_phase` and `dispatch_phase` from an error that escaped a
/// local resource method.
///
/// A same-process direct call enters the resource before its own transport
/// accounting applies: an exception the resource raised may already describe
/// state the resource modified, and inner transport admission details (for
/// example a downstream call's `pre_dispatch`) never describe this call's
/// dispatch state. Without this normalization an outer caller could misread a
/// state-modifying failure as retry-safe `pre_dispatch`.
pub fn strip_callback_error_phases(error: &mut C2Error) {
    error.details.remove("transport_phase");
    error.details.remove("dispatch_phase");
}

/// Decode canonical C2E1 bytes, mapping malformed envelopes to the canonical
/// `ProtocolViolation` semantic error.
pub fn semantic_error_from_ipc_bytes(bytes: &[u8]) -> C2Error {
    match C2Error::from_wire_bytes(bytes) {
        Ok(Some(error)) => error,
        Ok(None) => protocol_violation(
            "IPC semantic error payload is empty",
            "ipc",
            "empty C2E1 payload",
        ),
        Err(error) => protocol_violation(
            "IPC semantic error payload is malformed",
            "ipc",
            &error.to_string(),
        ),
    }
}

/// Decode an HTTP JSON semantic envelope into the same C2 error authority.
pub fn normalize_http_semantic_body(body: &str) -> Error {
    Error::Semantic(semantic_error_from_http_body(body))
}

/// Decode an HTTP JSON semantic envelope, mapping malformed envelopes to the
/// same canonical `ProtocolViolation` semantic error used by IPC.
pub fn semantic_error_from_http_body(body: &str) -> C2Error {
    let envelope = match serde_json::from_str::<C2ErrorEnvelope>(body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return protocol_violation(
                "HTTP semantic error envelope is malformed",
                "http",
                &error.to_string(),
            );
        }
    };
    semantic_error_from_http_envelope(envelope)
}

pub(crate) fn semantic_error_from_http_envelope(envelope: C2ErrorEnvelope) -> C2Error {
    match C2Error::from_envelope(envelope) {
        Ok(error) => error,
        Err(error) => protocol_violation(
            "HTTP semantic error envelope is invalid",
            "http",
            &error.to_string(),
        ),
    }
}

fn protocol_violation(message: &str, transport: &str, decode_error: &str) -> C2Error {
    C2Error::new(ErrorCode::ProtocolViolation, message).with_details(BTreeMap::from([
        ("decode_error".to_string(), decode_error.to_string()),
        ("protocol".to_string(), "c-two.error.v1".to_string()),
        ("transport".to_string(), transport.to_string()),
    ]))
}

/// Already-extracted fields from an external owner error.
///
/// The Core never imports or inspects the owner's error object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalCause<'a> {
    pub owner: &'a str,
    pub code: &'a str,
    pub symbol: &'a str,
    pub path: &'a str,
    pub message: &'a str,
    pub details_json: &'a str,
}

/// Project an external cause into the stable C-Two semantic detail convention.
pub fn external_cause_details(cause: ExternalCause<'_>) -> BTreeMap<String, String> {
    let prefix = cause.owner;
    BTreeMap::from([
        ("cause_owner".to_string(), cause.owner.to_string()),
        (format!("{prefix}_code"), cause.code.to_string()),
        (
            format!("{prefix}_details_json"),
            cause.details_json.to_string(),
        ),
        (format!("{prefix}_message"), cause.message.to_string()),
        (format!("{prefix}_path"), cause.path.to_string()),
        (format!("{prefix}_symbol"), cause.symbol.to_string()),
    ])
}

/// Portable generated-adapter failure phases frozen to existing C2 codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterFailurePhase {
    ClientInputSerializing,
    ClientOutputFromBuffer,
    ClientOutputDeserializing,
    ResourceInputFromBuffer,
    ResourceInputDeserializing,
    ResourceFunctionExecuting,
    ResourceOutputSerializing,
}

impl AdapterFailurePhase {
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::ClientInputSerializing => ErrorCode::ClientInputSerializing,
            Self::ClientOutputFromBuffer => ErrorCode::ClientOutputFromBuffer,
            Self::ClientOutputDeserializing => ErrorCode::ClientOutputDeserializing,
            Self::ResourceInputFromBuffer => ErrorCode::ResourceInputFromBuffer,
            Self::ResourceInputDeserializing => ErrorCode::ResourceInputDeserializing,
            Self::ResourceFunctionExecuting => ErrorCode::ResourceFunctionExecuting,
            Self::ResourceOutputSerializing => ErrorCode::ResourceOutputSerializing,
        }
    }
}

/// A generated adapter either preserves an existing semantic error or supplies
/// one local phase failure for canonical mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterFailure {
    Semantic(C2Error),
    Local {
        message: String,
        details: BTreeMap<String, String>,
    },
}

pub fn normalize_adapter_failure(phase: AdapterFailurePhase, failure: AdapterFailure) -> C2Error {
    match failure {
        AdapterFailure::Semantic(error) => error,
        AdapterFailure::Local { message, details } => {
            C2Error::new(phase.error_code(), message).with_details(details)
        }
    }
}

#[cfg(test)]
mod native_boundary_tests {
    use super::*;
    use c2_config::PoolConfig;
    use c2_mem::{DomainBudget, MemPool};
    use c2_wire::chunk::{config::ChunkConfig, registry::ChunkRegistry};
    use c2_wire::connection::calls::ValueReceive;
    use c2_wire::flags::{FLAG_CHUNKED, FLAG_FIRST_CHUNK, FLAG_RESPONSE, FLAG_RPC_RECORD};
    use c2_wire::frame::{decode_frame, encode_frame};
    use c2_wire::rpc::{CallMode, ChunkPosition, RpcRecord, decode_rpc_record};
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn wire_admission_error(
        total_size: u64,
        data_len: usize,
        file_limit: u64,
        live_limit: u64,
    ) -> (IpcError, DomainBudget) {
        let budget = DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: live_limit,
            file_spill_budget_bytes: file_limit,
            max_outstanding_calls: 0,
        });
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    pool_enabled: false,
                    spill_threshold: 0.0,
                    ..PoolConfig::default()
                },
                "error-boundary-pure".to_string(),
                budget.clone(),
            )
            .expect("test pool"),
        ));
        let registry = ChunkRegistry::new(pool, ChunkConfig::default());
        let mut payload = vec![6, 0]; // CALL_END, Success, unary: no item count.
        payload.extend_from_slice(&total_size.to_le_bytes());
        payload.resize(payload.len() + data_len, 0x42);
        let bytes = encode_frame(
            9,
            FLAG_RPC_RECORD | FLAG_RESPONSE | FLAG_CHUNKED | FLAG_FIRST_CHUNK,
            &payload,
        );
        let (header, payload) = decode_frame(&bytes).unwrap();
        let value = match decode_rpc_record(&header, payload, Some(CallMode::Unary)) {
            Ok(RpcRecord::CallEnd { value, .. }) => value,
            Err(error) => return (IpcError::Decode(error), budget),
            other => panic!("expected CALL_END: {other:?}"),
        };
        let ChunkPosition::First { total_bytes } = value.chunk else {
            panic!("first chunk");
        };
        let receive = ValueReceive::first(total_bytes, value.storage.len()).unwrap();
        let (chunks, part_bytes) = receive.geometry().unwrap();
        let failure = registry
            .insert_reply(1, 9, total_bytes, chunks, part_bytes, total_size)
            .unwrap_err();
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.total_bytes(), 0);
        (IpcError::Chunk(failure.into()), budget)
    }

    #[test]
    fn response_admission_keeps_capacity_code_and_dispatch_uncertainty() {
        // Exercise encoded reply metadata, actual registry/allocator admission,
        // IPC mapping, and Core normalization, rather than inventing a variant.
        let total_size = 128;
        for (file_limit, live_limit) in [(1024, 0), (0, 1024)] {
            for data_len in [64, 32] {
                for supplied_phase in [
                    TransportPhase::PreDispatch,
                    TransportPhase::DispatchUncertain,
                ] {
                    let (source, budget) =
                        wire_admission_error(total_size, data_len, file_limit, live_limit);
                    assert!(matches!(source, IpcError::Chunk(ChunkError::Capacity(_))));
                    let error = normalize_ipc_error(source, supplied_phase);
                    let Error::Transport(transport) = error else {
                        panic!("response admission must retain its transport safety phase");
                    };
                    assert_eq!(transport.phase(), TransportPhase::DispatchUncertain);
                    assert_eq!(transport.kind(), TransportKind::Ipc);
                    assert!(!transport.is_fallback_eligible());
                    assert!(transport.source().unwrap().is::<IpcError>());
                    let semantic = transport.semantic_error().unwrap();
                    assert_eq!(semantic.code, ErrorCode::ResourceUnavailable);
                    assert_eq!(semantic.details["stage"], "response_reassembly_admission");
                    assert_eq!(semantic.details["transport_phase"], "dispatch_uncertain");
                    assert_eq!(semantic.details["fallback_eligible"], "false");
                    let decoded = C2Error::from_wire_bytes(&semantic.to_wire_bytes())
                        .unwrap()
                        .unwrap();
                    assert_eq!(decoded, *semantic);
                    let snapshot = budget.snapshot();
                    assert_eq!(snapshot.memory.used_bytes, 0);
                    assert_eq!(snapshot.file_spill.used_bytes, 0);
                    if live_limit > 0 {
                        // The tracking metadata briefly charged the memory
                        // cell before the zero file-spill cell refused the
                        // file-backed capacity admission.
                        let tracking_bytes = total_size.div_ceil(data_len as u64);
                        assert_eq!(snapshot.memory.peak_bytes, tracking_bytes);
                        assert_eq!(snapshot.file_spill.rejected_allocations, 1);
                    } else {
                        // A zero memory cell refuses before any storage or
                        // file mapping could be created.
                        assert_eq!(snapshot.memory.rejected_allocations, 1);
                        assert_eq!(snapshot.memory.rejected_bytes, total_size);
                    }
                }
            }
        }
    }

    #[test]
    fn encoded_invalid_geometry_never_becomes_capacity_error() {
        for (size, data_len) in [(0, 1), (0, 64), (128, 0), (128, 128), (128, 129)] {
            // Zero budgets amplify an incorrect attempt to allocate invalid geometry.
            let (source, budget) = wire_admission_error(size, data_len, 0, 0);
            assert!(matches!(source, IpcError::Decode(_)));
            let error = normalize_ipc_error(source, TransportPhase::DispatchUncertain);
            let Error::Transport(transport) = error else {
                panic!("expected protocol transport error")
            };
            assert!(transport.semantic_error().is_none());
            assert!(!transport.is_fallback_eligible());
            assert_eq!(budget.snapshot().memory.rejected_allocations, 0);
        }
    }

    #[test]
    fn encoded_valid_geometry_above_512_chunks_reports_budget_capacity() {
        let (source, budget) = wire_admission_error(1024, 1, 0, 0);
        assert!(matches!(source, IpcError::Chunk(ChunkError::Capacity(_))));
        let error = normalize_ipc_error(source, TransportPhase::DispatchUncertain);
        let Error::Transport(transport) = error else {
            panic!("expected reassembly capacity transport error")
        };
        assert_eq!(transport.phase(), TransportPhase::DispatchUncertain);
        assert!(!transport.is_fallback_eligible());
        let semantic = transport.semantic_error().unwrap();
        assert_eq!(semantic.code, ErrorCode::ResourceUnavailable);
        assert_eq!(semantic.details["stage"], "response_reassembly_admission");
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.memory.rejected_allocations, 1);
        assert_eq!(snapshot.memory.used_bytes, 0);
        assert_eq!(snapshot.file_spill.used_bytes, 0);
    }

    #[test]
    fn response_failure_never_becomes_a_pre_dispatch_error() {
        for source in [
            IpcError::Protocol("malformed response".into()),
            IpcError::Shm("response admission failed".into()),
            IpcError::Chunk(ChunkError::Capacity("response budget full".into())),
        ] {
            let is_capacity = matches!(&source, IpcError::Chunk(ChunkError::Capacity(_)));
            let error = normalize_ipc_error(
                IpcError::ResponseFailure(Box::new(source)),
                TransportPhase::PreDispatch,
            );
            let Error::Transport(transport) = error else {
                panic!("response failure must retain its transport phase");
            };
            assert_eq!(transport.phase(), TransportPhase::DispatchUncertain);
            assert!(!transport.is_fallback_eligible());
            assert!(matches!(
                transport.source().unwrap().downcast_ref::<IpcError>(),
                Some(IpcError::ResponseFailure(_))
            ));
            if is_capacity {
                assert_eq!(
                    transport.semantic_error().unwrap().code,
                    ErrorCode::ResourceUnavailable
                );
            }
        }
    }

    #[test]
    fn transport_phase_classifies_core_error_families() {
        use TransportPhase::{DispatchUncertain, PreDispatch};

        let invalid_contract = ContractError::Empty { field: "route" };
        for (error, expected) in [
            (
                Error::ProtocolVersionMismatch {
                    local: 16,
                    peer: 15,
                },
                Some(PreDispatch),
            ),
            (Error::Contract(invalid_contract.clone()), Some(PreDispatch)),
            (Error::Admission(invalid_contract), Some(PreDispatch)),
            (LifecycleError::RuntimeRetired.into(), Some(PreDispatch)),
            (
                LifecycleError::ResponseCopy("copy failed".into()).into(),
                Some(DispatchUncertain),
            ),
            (
                LifecycleError::Configuration("invalid host option".into()).into(),
                None,
            ),
        ] {
            assert_eq!(error.transport_phase(), expected, "{error:?}");
        }
    }

    #[test]
    fn ipc_contract_mismatch_is_pre_dispatch() {
        let error = normalize_ipc_error(
            IpcError::ContractMismatch("wrong contract".into()),
            TransportPhase::PreDispatch,
        );
        assert_eq!(error.transport_phase(), Some(TransportPhase::PreDispatch));
        let Error::Semantic(error) = error else {
            panic!("semantic contract error")
        };
        assert_eq!(error.details["transport"], "ipc");
        assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
        assert_eq!(error.details["transport_phase"], "pre_dispatch");
    }

    #[test]
    fn semantic_transport_phase_uses_only_canonical_values() {
        let details = BTreeMap::from([
            ("transport_phase".into(), "dispatch_uncertain".into()),
            ("dispatch_phase".into(), "pre_dispatch".into()),
        ]);
        let mut error =
            C2Error::new(ErrorCode::ResourceUnavailable, "forwarding failed").with_details(details);
        assert_eq!(
            Error::Semantic(error.clone()).transport_phase(),
            Some(TransportPhase::DispatchUncertain)
        );

        error
            .details
            .insert("transport_phase".into(), "PRE_DISPATCH".into());
        assert_eq!(Error::Semantic(error).transport_phase(), None);
    }

    #[test]
    fn http_call_errors_project_the_phase_of_this_dispatch() {
        for (status, code, dispatch_phase, expected) in [
            (
                503,
                ErrorCode::RouteWatchUnavailable,
                "pre_dispatch",
                Some(TransportPhase::PreDispatch),
            ),
            (
                502,
                ErrorCode::ResourceUnavailable,
                "dispatch_uncertain",
                Some(TransportPhase::DispatchUncertain),
            ),
            (
                500,
                ErrorCode::ResourceFunctionExecuting,
                "pre_dispatch",
                None,
            ),
        ] {
            let mut semantic = C2Error::new(code, "relay call failed");
            semantic
                .details
                .insert("dispatch_phase".into(), dispatch_phase.into());
            if status == 500 {
                semantic
                    .details
                    .insert("transport_phase".into(), dispatch_phase.into());
            }
            let body = serde_json::to_string(&semantic.envelope()).unwrap();
            let error = normalize_http_error(
                HttpError::ServerError(status, body),
                TransportPhase::PreDispatch,
            );
            assert_eq!(error.transport_phase(), expected, "{error:?}");
            let Error::Semantic(actual) = error else {
                panic!("expected a semantic error");
            };
            assert_eq!(actual.code, code);
        }
    }

    #[test]
    fn local_call_rejection_keeps_semantic_code_and_no_fallback_details() {
        for code in [
            ErrorCode::CallDeadlineExceeded,
            ErrorCode::UnsupportedCallMode,
            ErrorCode::CallCapacityExceeded,
        ] {
            let source = C2Error::new(code, "local call stopped").with_details(BTreeMap::from([
                (
                    "transport_phase".to_string(),
                    "dispatch_uncertain".to_string(),
                ),
                ("fallback_eligible".to_string(), "false".to_string()),
            ]));
            match normalize_http_error(
                HttpError::LocalCallRejected(source.clone()),
                TransportPhase::PreDispatch,
            ) {
                Error::Semantic(actual) => assert_eq!(actual, source),
                other => panic!("local rejection became a transport failure: {other:?}"),
            }
            match normalize_ipc_error(
                IpcError::LocalCallRejected(source.clone()),
                TransportPhase::PreDispatch,
            ) {
                Error::Semantic(actual) => assert_eq!(actual, source),
                other => panic!("local IPC rejection became a transport failure: {other:?}"),
            }
        }
    }

    #[test]
    fn cancelled_crm_error_preserves_canonical_semantic_projection() {
        let source = C2Error::new(ErrorCode::CallCancelled, "stream closed")
            .with_details(BTreeMap::from([("reason".into(), "client_close".into())]));
        let Error::Semantic(actual) = normalize_ipc_error(
            IpcError::CrmError(source.to_wire_bytes()),
            TransportPhase::DispatchUncertain,
        ) else {
            panic!("cancelled CRM error must remain semantic");
        };
        assert_eq!(actual, source);
    }

    #[test]
    fn stream_receive_capacity_remains_uncertain_with_the_original_semantic_error() {
        let source = C2Error::new(ErrorCode::CallCapacityExceeded, "item exceeds B").with_details(
            BTreeMap::from([("stage".into(), "payload_buffer_admission".into())]),
        );
        let Error::Transport(actual) = normalize_ipc_error(
            IpcError::ResponseFailure(Box::new(IpcError::LocalCallRejected(source.clone()))),
            TransportPhase::PreDispatch,
        ) else {
            panic!("a produced item cannot be pre-dispatch");
        };
        assert_eq!(actual.phase, TransportPhase::DispatchUncertain);
        assert_eq!(actual.semantic_error, Some(source));
    }
}
