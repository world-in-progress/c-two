//! Strict JSON codec for [`EndpointCredential`].
//!
//! This is the only Rust parser for the on-disk credential form. It is a
//! transfer format, not a secret and not an authorization capability: a
//! credential never proves liveness by itself, and every actual reap still
//! runs the native gate, lease, and identity checks. Decoding constructs a
//! value in memory only. It never touches the filesystem, never creates a
//! lock, and never claims that the described endpoint exists.
//!
//! The logical [`LocalEndpoint`] is rebuilt from the recorded address and
//! context through the platform's sole `c2-config` derivation. Default Unix
//! credentials use schema 2 and /tmp; non-default roots use schema 3 with
//! `unixRoot` and `namespaceId`. Windows uses kernel-managed schema 1.
//! Recorded protocol metadata must match that backend; a root in a document
//! describes a namespace and is never accepted as deletion authority.
//! A record that cannot be parsed strictly is
//! rejected rather than upgraded: an unknown record never becomes a UUID
//! incarnation credential.

use c2_config::{LocalEndpoint, LocalEndpointContext, LocalEndpointNamespace};
use serde::{Deserialize, Serialize};

// `EndpointCredential` is the codec's return type on every platform, including
// Windows, where the credential is kernel-managed metadata with no Unix
// identity fields. Only the socket identity is Unix-specific.
use crate::EndpointCredential;
#[cfg(unix)]
use crate::UnixSocketIdentity;

/// Newest supported credential schema. Default Unix endpoints still encode
/// schema 2; Windows kernel-managed endpoints still encode schema 1.
pub const ENDPOINT_CREDENTIAL_SCHEMA_VERSION: u32 = 3;

/// Maximum accepted JSON document size, in bytes. The rule is enforced on
/// both encode and decode so a caller can bound the file it will read.
pub const ENDPOINT_CREDENTIAL_MAX_BYTES: usize = 4096;

const SCHEMA_VERSION_V1: u32 = 1;
const SCHEMA_VERSION_V2: u32 = 2;
const SCHEMA_VERSION_V3: u32 = 3;
#[cfg(windows)]
const PROTOCOL_WINDOWS: &str = "named-pipe";
const PLATFORM_UNIX: &str = "unix";
const PLATFORM_WINDOWS: &str = "windows";

/// Why a credential document could not be accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointCredentialErrorKind {
    /// The document is larger than [`ENDPOINT_CREDENTIAL_MAX_BYTES`].
    TooLarge,
    /// The bytes are not valid JSON, or not a JSON object with the exact
    /// expected field set.
    MalformedJson,
    /// A required field is missing.
    MissingField,
    /// `schemaVersion` names a schema this build does not implement.
    UnsupportedSchemaVersion,
    /// A field carries a value outside its permitted set.
    InvalidValue,
    /// A record describes a platform or protocol this build cannot verify.
    UnsupportedPlatform,
    /// A Unix schema-v2/v3 record must name its listener incarnation.
    IncarnationRequired,
}

impl EndpointCredentialErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too-large",
            Self::MalformedJson => "malformed-json",
            Self::MissingField => "missing-field",
            Self::UnsupportedSchemaVersion => "unsupported-schema-version",
            Self::InvalidValue => "invalid-value",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::IncarnationRequired => "incarnation-required",
        }
    }
}

/// A rejected credential document. The message never echoes record contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointCredentialError {
    kind: EndpointCredentialErrorKind,
    field: Option<&'static str>,
}

impl EndpointCredentialError {
    const fn new(kind: EndpointCredentialErrorKind) -> Self {
        Self { kind, field: None }
    }

    const fn at(kind: EndpointCredentialErrorKind, field: &'static str) -> Self {
        Self {
            kind,
            field: Some(field),
        }
    }

    pub const fn kind(&self) -> EndpointCredentialErrorKind {
        self.kind
    }

    pub const fn field(&self) -> Option<&'static str> {
        self.field
    }
}

impl std::fmt::Display for EndpointCredentialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.field {
            Some(field) => write!(
                formatter,
                "invalid endpoint credential: {} ({field})",
                self.kind.as_str()
            ),
            None => write!(
                formatter,
                "invalid endpoint credential: {}",
                self.kind.as_str()
            ),
        }
    }
}

impl std::error::Error for EndpointCredentialError {}

/// The exact JSON document form. Field values stay as raw JSON so a wrong
/// type or an out-of-range integer is a strict error and never a silent
/// coercion.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(non_snake_case)]
struct CredentialWire {
    #[serde(rename = "schemaVersion")]
    schema_version: serde_json::Value,
    address: serde_json::Value,
    protocol: serde_json::Value,
    platform: serde_json::Value,
    #[serde(default, deserialize_with = "present_value")]
    incarnation: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    device: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    inode: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    changedSecs: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    changedNanos: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    unixRoot: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present_value")]
    namespaceId: Option<serde_json::Value>,
}

/// The encoded document form. Schema 3 adds the context root/id to the logical
/// address, protocol, platform, and native object identity. The socket name is
/// never serialized.
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
#[allow(non_snake_case)]
struct CredentialDocument<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    address: &'a str,
    protocol: &'a str,
    platform: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    incarnation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changedSecs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changedNanos: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unixRoot: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespaceId: Option<&'a str>,
}

impl EndpointCredential {
    /// Encodes this credential as strict JSON.
    ///
    /// The result is a description only. Encoding never reads the
    /// filesystem, never creates a lock, and never asserts that the endpoint
    /// exists or is alive.
    pub fn to_json(&self) -> Result<String, EndpointCredentialError> {
        let endpoint = self.endpoint();
        // Keep the default public path and schema unchanged. A lexical root
        // alias is a distinct context even when native descriptors name /tmp.
        #[cfg(unix)]
        let schema_version = if endpoint.context().unix_root() == Some(std::path::Path::new("/tmp"))
        {
            SCHEMA_VERSION_V2
        } else {
            SCHEMA_VERSION_V3
        };
        #[cfg(not(unix))]
        let schema_version = SCHEMA_VERSION_V1;
        let document = CredentialDocument {
            schema_version,
            address: endpoint.address(),
            protocol: endpoint.protocol(),
            platform: platform_name(),
            unixRoot: (schema_version == SCHEMA_VERSION_V3)
                .then(|| {
                    endpoint
                        .context()
                        .unix_root()
                        .and_then(|root| root.to_str())
                })
                .flatten(),
            namespaceId: (schema_version == SCHEMA_VERSION_V3)
                .then(|| endpoint.context().namespace_id()),
            #[cfg(unix)]
            incarnation: Some(encode_incarnation(self.incarnation())),
            #[cfg(not(unix))]
            incarnation: None,
            #[cfg(unix)]
            device: Some(self.identity().device),
            #[cfg(not(unix))]
            device: None,
            #[cfg(unix)]
            inode: Some(self.identity().inode),
            #[cfg(not(unix))]
            inode: None,
            #[cfg(unix)]
            changedSecs: Some(self.identity().changed_secs),
            #[cfg(not(unix))]
            changedSecs: None,
            #[cfg(unix)]
            changedNanos: Some(self.identity().changed_nanos),
            #[cfg(not(unix))]
            changedNanos: None,
        };
        let encoded = serde_json::to_string(&document)
            .map_err(|_| EndpointCredentialError::new(EndpointCredentialErrorKind::InvalidValue))?;
        if encoded.len() > ENDPOINT_CREDENTIAL_MAX_BYTES {
            return Err(EndpointCredentialError::new(
                EndpointCredentialErrorKind::TooLarge,
            ));
        }
        Ok(encoded)
    }

    /// Decodes a credential document.
    ///
    /// Decoding is a pure parse: it re-derives the OS endpoint name from the
    /// recorded address and context instead of trusting a socket path, rejects
    /// unknown fields and unsupported schema versions, and checks each
    /// integer against its wire range. A successful decode is not evidence
    /// that the endpoint exists; callers must still pass the value through
    /// [`crate::reap_endpoint`] for the native identity check.
    pub fn from_json(json: &str) -> Result<Self, EndpointCredentialError> {
        if json.len() > ENDPOINT_CREDENTIAL_MAX_BYTES {
            return Err(EndpointCredentialError::new(
                EndpointCredentialErrorKind::TooLarge,
            ));
        }
        let wire: CredentialWire = serde_json::from_str(json).map_err(|_| {
            EndpointCredentialError::new(EndpointCredentialErrorKind::MalformedJson)
        })?;
        let schema_version = schema_version(&wire)?;
        let address = required_string(&wire.address, "address")?;
        let recorded_protocol = required_string(&wire.protocol, "protocol")?;
        let platform = platform(&wire.platform)?;
        if platform != effective_namespace() {
            return Err(EndpointCredentialError::at(
                EndpointCredentialErrorKind::UnsupportedPlatform,
                "platform",
            ));
        }
        let context = recorded_context(&wire, schema_version, platform)?;
        let endpoint = context.endpoint(address).map_err(|_| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, "address")
        })?;
        if recorded_protocol != endpoint.protocol() {
            return Err(EndpointCredentialError::at(
                EndpointCredentialErrorKind::InvalidValue,
                "protocol",
            ));
        }
        match platform {
            Platform::Unix => unix_credential(&wire, schema_version, endpoint),
            Platform::Windows => windows_credential(&wire, schema_version, endpoint),
        }
    }
}

// Preserve field presence, including null, so a forbidden field cannot be
// smuggled into a platform/schema document by spelling its value as null.
fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn recorded_context(
    wire: &CredentialWire,
    schema_version: u32,
    platform: Platform,
) -> Result<LocalEndpointContext, EndpointCredentialError> {
    if schema_version != SCHEMA_VERSION_V3 {
        for (field, value) in [
            ("unixRoot", &wire.unixRoot),
            ("namespaceId", &wire.namespaceId),
        ] {
            if value.is_some() {
                return Err(EndpointCredentialError::at(
                    EndpointCredentialErrorKind::InvalidValue,
                    field,
                ));
            }
        }
        // Schema 2 is always /tmp, independent of C2_IPC_ROOT.
        return LocalEndpointContext::default_for_platform().map_err(|_| {
            EndpointCredentialError::new(EndpointCredentialErrorKind::UnsupportedPlatform)
        });
    }
    if platform != Platform::Unix {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::UnsupportedPlatform,
            "schemaVersion",
        ));
    }
    let root = required_string(
        wire.unixRoot.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "unixRoot")
        })?,
        "unixRoot",
    )?;
    let namespace_id = required_string(
        wire.namespaceId.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "namespaceId")
        })?,
        "namespaceId",
    )?;
    // c2-config alone validates/normalizes the root. No filesystem or env IO.
    let context =
        LocalEndpointContext::with_unix_root(std::path::Path::new(root)).map_err(|_| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, "unixRoot")
        })?;
    if context.unix_root() == Some(std::path::Path::new("/tmp")) {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "unixRoot",
        ));
    }
    if namespace_id != context.namespace_id() {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "namespaceId",
        ));
    }
    Ok(context)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Platform {
    Unix,
    Windows,
}

fn platform_name() -> &'static str {
    if cfg!(windows) {
        PLATFORM_WINDOWS
    } else {
        PLATFORM_UNIX
    }
}

fn effective_namespace() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

/// Rebinds the derived value to the namespace this build actually supports.
fn bind_namespace(endpoint: LocalEndpoint) -> Result<LocalEndpoint, EndpointCredentialError> {
    let supported = match effective_namespace() {
        Platform::Unix => LocalEndpointNamespace::UnixFilesystem,
        Platform::Windows => LocalEndpointNamespace::WindowsNamedPipe,
    };
    if endpoint.namespace() != supported {
        return Err(EndpointCredentialError::new(
            EndpointCredentialErrorKind::UnsupportedPlatform,
        ));
    }
    Ok(endpoint)
}

fn schema_version(wire: &CredentialWire) -> Result<u32, EndpointCredentialError> {
    let version = wire.schema_version.as_u64().ok_or_else(|| {
        EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, "schemaVersion")
    })?;
    let version = u32::try_from(version).map_err(|_| {
        EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, "schemaVersion")
    })?;
    match version {
        SCHEMA_VERSION_V1 | SCHEMA_VERSION_V2 | SCHEMA_VERSION_V3 => Ok(version),
        _ => Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::UnsupportedSchemaVersion,
            "schemaVersion",
        )),
    }
}

fn required_string<'a>(
    value: &'a serde_json::Value,
    field: &'static str,
) -> Result<&'a str, EndpointCredentialError> {
    value.as_str().ok_or_else(|| {
        EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, field)
    })
}

fn platform(value: &serde_json::Value) -> Result<Platform, EndpointCredentialError> {
    match required_string(value, "platform")? {
        PLATFORM_UNIX => Ok(Platform::Unix),
        PLATFORM_WINDOWS => Ok(Platform::Windows),
        _ => Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "platform",
        )),
    }
}

#[cfg(unix)]
fn encode_incarnation(incarnation: [u8; 16]) -> String {
    let mut hex = String::with_capacity(32);
    for byte in incarnation {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(unix)]
fn decode_incarnation(value: &serde_json::Value) -> Result<[u8; 16], EndpointCredentialError> {
    let text = required_string(value, "incarnation")?;
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "incarnation",
        ));
    }
    let mut incarnation = [0u8; 16];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(EndpointCredentialError::at(
                EndpointCredentialErrorKind::InvalidValue,
                "incarnation",
            ));
        };
        incarnation[index] = ((high << 4) | low) as u8;
    }
    Ok(incarnation)
}

#[cfg(unix)]
fn required_u64(
    value: &serde_json::Value,
    field: &'static str,
) -> Result<u64, EndpointCredentialError> {
    value.as_u64().ok_or_else(|| {
        EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, field)
    })
}

#[cfg(unix)]
fn required_i64(
    value: &serde_json::Value,
    field: &'static str,
) -> Result<i64, EndpointCredentialError> {
    value.as_i64().ok_or_else(|| {
        EndpointCredentialError::at(EndpointCredentialErrorKind::InvalidValue, field)
    })
}

#[cfg(unix)]
fn unix_credential(
    wire: &CredentialWire,
    schema_version: u32,
    endpoint: LocalEndpoint,
) -> Result<EndpointCredential, EndpointCredentialError> {
    let endpoint = bind_namespace(endpoint)?;
    let device = required_u64(
        wire.device.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "device")
        })?,
        "device",
    )?;
    let inode = required_u64(
        wire.inode.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "inode")
        })?,
        "inode",
    )?;
    let changed_secs = required_i64(
        wire.changedSecs.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "changedSecs")
        })?,
        "changedSecs",
    )?;
    let changed_nanos = required_i64(
        wire.changedNanos.as_ref().ok_or_else(|| {
            EndpointCredentialError::at(EndpointCredentialErrorKind::MissingField, "changedNanos")
        })?,
        "changedNanos",
    )?;
    let identity = UnixSocketIdentity {
        device,
        inode,
        changed_secs,
        changed_nanos,
    };
    let incarnation = match wire.incarnation.as_ref() {
        Some(value) => Some(decode_incarnation(value)?),
        None => None,
    };
    build_unix(endpoint, identity, incarnation, schema_version)
}

#[cfg(unix)]
fn build_unix(
    endpoint: LocalEndpoint,
    identity: UnixSocketIdentity,
    incarnation: Option<[u8; 16]>,
    schema_version: u32,
) -> Result<EndpointCredential, EndpointCredentialError> {
    match (schema_version, incarnation) {
        // Only schema-v2/v3 records with a native incarnation describe Unix listeners.
        (SCHEMA_VERSION_V2 | SCHEMA_VERSION_V3, Some(incarnation)) => Ok(
            EndpointCredential::unix_managed(endpoint, identity, incarnation),
        ),
        (SCHEMA_VERSION_V2 | SCHEMA_VERSION_V3, None) => Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::IncarnationRequired,
            "incarnation",
        )),
        // A v1 record can never be promoted into a UUID incarnation, and a
        // managed-v2 endpoint is not representable without one.
        (SCHEMA_VERSION_V1, _) => Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "schemaVersion",
        )),
        _ => Err(EndpointCredentialError::new(
            EndpointCredentialErrorKind::UnsupportedSchemaVersion,
        )),
    }
}

#[cfg(not(unix))]
fn unix_credential(
    _wire: &CredentialWire,
    _schema_version: u32,
    _endpoint: LocalEndpoint,
) -> Result<EndpointCredential, EndpointCredentialError> {
    Err(EndpointCredentialError::at(
        EndpointCredentialErrorKind::UnsupportedPlatform,
        "platform",
    ))
}

#[cfg(windows)]
fn windows_credential(
    wire: &CredentialWire,
    schema_version: u32,
    endpoint: LocalEndpoint,
) -> Result<EndpointCredential, EndpointCredentialError> {
    if schema_version != SCHEMA_VERSION_V1 {
        // A kernel-managed pipe has no incarnation, so v2 cannot describe it.
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::IncarnationRequired,
            "schemaVersion",
        ));
    }
    if wire.incarnation.is_some() {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "incarnation",
        ));
    }
    // The socket identity fields must be absent: a named pipe is a kernel
    // object with no filesystem inode to record.
    if wire.device.is_some()
        || wire.inode.is_some()
        || wire.changedSecs.is_some()
        || wire.changedNanos.is_some()
    {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::InvalidValue,
            "device",
        ));
    }
    let endpoint = bind_namespace(endpoint)?;
    if endpoint.protocol() != PROTOCOL_WINDOWS {
        return Err(EndpointCredentialError::at(
            EndpointCredentialErrorKind::UnsupportedPlatform,
            "protocol",
        ));
    }
    // This records metadata only. It never claims that a pipe exists, and it
    // never removes a file.
    Ok(EndpointCredential::kernel_managed(endpoint))
}

#[cfg(not(windows))]
fn windows_credential(
    _wire: &CredentialWire,
    _schema_version: u32,
    _endpoint: LocalEndpoint,
) -> Result<EndpointCredential, EndpointCredentialError> {
    Err(EndpointCredentialError::at(
        EndpointCredentialErrorKind::UnsupportedPlatform,
        "platform",
    ))
}
