use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;

use c2_contract::ContractError;
use c2_error::{C2Error, C2ErrorEnvelope, ErrorCode};
use c2_http::client::HttpError;
use c2_ipc::IpcError;

use crate::RegisterFailureOutcome;

/// Failures in process runtime setup, route lifecycle, and relay projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    InvalidServerId(String),
    ClientConfigFrozen,
    DuplicateRoute(String),
    MissingRoute(String),
    RelayDuplicateRoute(String),
    MissingRelayAddress,
    RelayHttp {
        status_code: u16,
        message: String,
    },
    RegisterFailure(Box<RegisterFailureOutcome>),
    HeldResponseCopy {
        copy_error: String,
        transport_error: Option<String>,
    },
    HeldResponseRelease {
        invalidation_error: Option<String>,
        transport_error: Option<String>,
    },
    Server(String),
    Relay(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidServerId(message) => formatter.write_str(message),
            Self::ClientConfigFrozen => formatter.write_str("client IPC configuration is frozen"),
            Self::DuplicateRoute(name) => write!(formatter, "route already registered: {name}"),
            Self::MissingRoute(name) => write!(formatter, "route not registered: {name}"),
            Self::RelayDuplicateRoute(name) => {
                write!(formatter, "relay route already registered: {name}")
            }
            Self::MissingRelayAddress => formatter.write_str("no relay address configured"),
            Self::RelayHttp {
                status_code,
                message,
            } => write!(formatter, "relay HTTP {status_code}: {message}"),
            Self::RegisterFailure(outcome) => write!(
                formatter,
                "registration failed at {}: {}",
                outcome.failure_source, outcome.error_message
            ),
            Self::HeldResponseCopy {
                copy_error,
                transport_error,
            } => {
                write!(formatter, "held response copy failed: {copy_error}")?;
                if let Some(transport_error) = transport_error {
                    write!(
                        formatter,
                        "; response transport cleanup also failed: {transport_error}"
                    )?;
                }
                Ok(())
            }
            Self::HeldResponseRelease {
                invalidation_error,
                transport_error,
            } => {
                formatter.write_str("held response release failed")?;
                if let Some(invalidation_error) = invalidation_error {
                    write!(formatter, "; invalidation: {invalidation_error}")?;
                }
                if let Some(transport_error) = transport_error {
                    write!(formatter, "; transport cleanup: {transport_error}")?;
                }
                Ok(())
            }
            Self::Server(message) => write!(formatter, "server error: {message}"),
            Self::Relay(message) => write!(formatter, "relay error: {message}"),
        }
    }
}

impl StdError for LifecycleError {}

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
        }
    }

    pub const fn phase(&self) -> TransportPhase {
        self.phase
    }

    pub const fn kind(&self) -> TransportKind {
        self.kind
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
    Semantic(C2Error),
    Contract(ContractError),
    Admission(ContractError),
    Transport(TransportError),
    Lifecycle(LifecycleError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
        IpcError::CrmError(bytes) => normalize_ipc_semantic_bytes(&bytes),
        transport => Error::Transport(TransportError::new(phase, TransportKind::Ipc, transport)),
    }
}

/// Normalize an HTTP error without flattening its transport source.
pub fn normalize_http_error(error: HttpError, phase: TransportPhase) -> Error {
    match error {
        HttpError::CrmError(bytes) => normalize_ipc_semantic_bytes(&bytes),
        HttpError::ServerError(_, body) => normalize_http_semantic_body(&body),
        transport => Error::Transport(TransportError::new(phase, TransportKind::Http, transport)),
    }
}

/// Decode canonical C2E1 bytes into the semantic error authority.
pub fn normalize_ipc_semantic_bytes(bytes: &[u8]) -> Error {
    Error::Semantic(semantic_error_from_ipc_bytes(bytes))
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
