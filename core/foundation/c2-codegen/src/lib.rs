//! FastDB delegation and deterministic artifact composition for C-Two contracts.

mod artifact;
mod compile;
mod legacy_typescript;
mod publish;

pub use artifact::{
    ArtifactComposer, ArtifactKind, ArtifactLimits, ArtifactProvenance, ContractArtifact,
    ContractArtifactSet,
};
pub use compile::{ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts};
pub use legacy_typescript::{
    CodegenError as LegacyTypeScriptCodegenError, TypeScriptOptions, generate_typescript_client,
};

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodegenError {
    #[error(transparent)]
    Contract(#[from] c2_contract::ContractError),
    #[error(
        "FastDB failed for nested binding {binding_path}: {symbol} ({code}) at {path}: {message}"
    )]
    FastDb {
        binding_path: String,
        code: u32,
        symbol: String,
        path: String,
        message: String,
        details_json: String,
    },
    #[error("artifact provenance invalid at {field}: {message}")]
    InvalidArtifactProvenance {
        field: &'static str,
        message: String,
    },
    #[error("artifact path {relative_path:?} is not portable: {reason}")]
    InvalidArtifactPath {
        relative_path: String,
        reason: String,
    },
    #[error("artifact {relative_path:?} hash mismatch: expected {expected}, calculated {actual}")]
    ArtifactHashMismatch {
        relative_path: String,
        expected: String,
        actual: String,
    },
    #[error("duplicate artifact path {relative_path:?}")]
    DuplicateArtifactPath { relative_path: String },
    #[error("artifact paths {first:?} and {second:?} collide on case-insensitive filesystems")]
    ArtifactPathCaseCollision { first: String, second: String },
    #[error("artifact path {parent:?} conflicts with descendant path {child:?}")]
    ArtifactPathPrefixConflict { parent: String, child: String },
    #[error("artifact limit {limit} exceeded: {actual} > {maximum}")]
    ArtifactLimitExceeded {
        limit: &'static str,
        actual: u64,
        maximum: u64,
    },
    #[error(
        "FastDB Core returned inconsistent facts for digest {sha256} between {first_binding_path} and {second_binding_path}"
    )]
    FastDbIdentityConflict {
        sha256: String,
        first_binding_path: String,
        second_binding_path: String,
    },
    #[error(
        "FastDB Core canonical bytes for {binding_path} do not match its returned digest: expected {expected}, calculated {actual}"
    )]
    FastDbCanonicalDigestMismatch {
        binding_path: String,
        expected: String,
        actual: String,
    },
    #[error("FastDB Core returned invalid {fact} JSON for {binding_path}: {message}")]
    FastDbInvalidJson {
        binding_path: String,
        fact: &'static str,
        message: String,
    },
    #[error("composition metadata serialization failed: {0}")]
    MetadataSerialization(String),
    #[error("artifact destination already exists: {}", path.display())]
    DestinationExists { path: PathBuf },
    #[error("artifact destination has no usable parent: {}", path.display())]
    InvalidDestination { path: PathBuf },
    #[error("atomic no-replace directory publication is unsupported on this platform")]
    AtomicPublicationUnsupported,
    #[error("artifact I/O failed while {operation} {}: {source}", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) fn fastdb_error(binding_path: &str, error: fastdb::PayloadError) -> CodegenError {
    CodegenError::FastDb {
        binding_path: binding_path.to_string(),
        code: error.code(),
        symbol: error.symbol().to_string(),
        path: error.path().to_string(),
        message: error.message().to_string(),
        details_json: error.details_json().to_string(),
    }
}

pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
