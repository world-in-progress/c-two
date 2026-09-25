use crate::{CodegenError, lower_hex};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_ARTIFACT_PATH_BYTES: usize = 1024;
const MAX_ARTIFACT_SEGMENT_BYTES: usize = 255;
const MAX_PROVENANCE_TEXT_BYTES: usize = 1024;

/// Semantic content classification. Publication currently writes every kind as a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Source,
    Metadata,
    Binary,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Metadata => "metadata",
            Self::Binary => "binary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactProvenance {
    owner: String,
    source: String,
}

impl ArtifactProvenance {
    pub fn new(owner: impl Into<String>, source: impl Into<String>) -> Result<Self, CodegenError> {
        let owner = owner.into();
        let source = source.into();
        validate_provenance_text("owner", &owner)?;
        validate_provenance_text("source", &source)?;
        Ok(Self { owner, source })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractArtifact {
    relative_path: String,
    kind: ArtifactKind,
    bytes: Vec<u8>,
    sha256: [u8; 32],
    provenance: ArtifactProvenance,
}

impl ContractArtifact {
    pub fn new(
        relative_path: impl Into<String>,
        kind: ArtifactKind,
        bytes: Vec<u8>,
        provenance: ArtifactProvenance,
    ) -> Result<Self, CodegenError> {
        let sha256 = Sha256::digest(&bytes).into();
        Self::from_claimed_parts(relative_path, kind, bytes, sha256, provenance)
    }

    pub fn from_claimed_parts(
        relative_path: impl Into<String>,
        kind: ArtifactKind,
        bytes: Vec<u8>,
        sha256: [u8; 32],
        provenance: ArtifactProvenance,
    ) -> Result<Self, CodegenError> {
        let relative_path = relative_path.into();
        validate_artifact_path(&relative_path)?;
        Ok(Self {
            relative_path,
            kind,
            bytes,
            sha256,
            provenance,
        })
    }

    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub fn sha256_hex(&self) -> String {
        lower_hex(&self.sha256)
    }

    pub fn provenance(&self) -> &ArtifactProvenance {
        &self.provenance
    }
}

/// Admission limits for one complete composed artifact set.
///
/// The defaults admit at most 4,096 artifacts and 256 MiB of aggregate bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactLimits {
    pub max_artifacts: u64,
    pub max_total_bytes: u64,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_artifacts: 4096,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Collects independently owned regular-file artifacts into one verified portable set.
#[derive(Debug)]
pub struct ArtifactComposer {
    limits: ArtifactLimits,
    artifacts: Vec<ContractArtifact>,
}

impl ArtifactComposer {
    pub fn new(limits: ArtifactLimits) -> Self {
        Self {
            limits,
            artifacts: Vec::new(),
        }
    }

    pub fn push(&mut self, artifact: ContractArtifact) {
        self.artifacts.push(artifact);
    }

    pub fn extend(&mut self, artifacts: impl IntoIterator<Item = ContractArtifact>) {
        self.artifacts.extend(artifacts);
    }

    pub fn finish(mut self) -> Result<ContractArtifactSet, CodegenError> {
        let artifact_count = u64::try_from(self.artifacts.len()).map_err(|_| {
            CodegenError::ArtifactLimitExceeded {
                limit: "max_artifacts",
                actual: u64::MAX,
                maximum: self.limits.max_artifacts,
            }
        })?;
        if artifact_count > self.limits.max_artifacts {
            return Err(CodegenError::ArtifactLimitExceeded {
                limit: "max_artifacts",
                actual: artifact_count,
                maximum: self.limits.max_artifacts,
            });
        }

        let mut total_bytes = 0_u64;
        for artifact in &self.artifacts {
            let byte_count = u64::try_from(artifact.bytes.len()).map_err(|_| {
                CodegenError::ArtifactLimitExceeded {
                    limit: "max_total_bytes",
                    actual: u64::MAX,
                    maximum: self.limits.max_total_bytes,
                }
            })?;
            total_bytes =
                total_bytes
                    .checked_add(byte_count)
                    .ok_or(CodegenError::ArtifactLimitExceeded {
                        limit: "max_total_bytes",
                        actual: u64::MAX,
                        maximum: self.limits.max_total_bytes,
                    })?;
            if total_bytes > self.limits.max_total_bytes {
                return Err(CodegenError::ArtifactLimitExceeded {
                    limit: "max_total_bytes",
                    actual: total_bytes,
                    maximum: self.limits.max_total_bytes,
                });
            }

            let calculated: [u8; 32] = Sha256::digest(&artifact.bytes).into();
            if calculated != artifact.sha256 {
                return Err(CodegenError::ArtifactHashMismatch {
                    relative_path: artifact.relative_path.clone(),
                    expected: lower_hex(&artifact.sha256),
                    actual: lower_hex(&calculated),
                });
            }
        }

        self.artifacts
            .sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
        for pair in self.artifacts.windows(2) {
            if pair[0].relative_path == pair[1].relative_path {
                return Err(CodegenError::DuplicateArtifactPath {
                    relative_path: pair[0].relative_path.clone(),
                });
            }
        }

        let mut portable_paths = BTreeMap::<String, &str>::new();
        for artifact in &self.artifacts {
            let folded = artifact.relative_path.to_ascii_lowercase();
            if let Some(existing) = portable_paths.insert(folded, &artifact.relative_path) {
                return Err(CodegenError::ArtifactPathCaseCollision {
                    first: existing.to_string(),
                    second: artifact.relative_path.clone(),
                });
            }
        }
        for artifact in &self.artifacts {
            let child = artifact.relative_path.as_str();
            let folded_child = child.to_ascii_lowercase();
            for (separator, _) in folded_child.match_indices('/') {
                let folded_parent = &folded_child[..separator];
                if let Some(parent) = portable_paths.get(folded_parent) {
                    return Err(CodegenError::ArtifactPathPrefixConflict {
                        parent: (*parent).to_string(),
                        child: child.to_string(),
                    });
                }
            }
        }

        Ok(ContractArtifactSet {
            artifacts: self.artifacts,
            total_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractArtifactSet {
    pub(crate) artifacts: Vec<ContractArtifact>,
    pub(crate) total_bytes: u64,
}

impl ContractArtifactSet {
    pub fn artifacts(&self) -> &[ContractArtifact] {
        &self.artifacts
    }

    pub fn get(&self, relative_path: &str) -> Option<&ContractArtifact> {
        self.artifacts
            .binary_search_by_key(&relative_path, |artifact| artifact.relative_path())
            .ok()
            .map(|index| &self.artifacts[index])
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

fn validate_provenance_text(field: &'static str, value: &str) -> Result<(), CodegenError> {
    let invalid = |message: &str| CodegenError::InvalidArtifactProvenance {
        field,
        message: message.to_string(),
    };
    if value.is_empty() {
        return Err(invalid("cannot be empty"));
    }
    if value.len() > MAX_PROVENANCE_TEXT_BYTES {
        return Err(invalid("exceeds 1024 UTF-8 bytes"));
    }
    if value.trim() != value {
        return Err(invalid("cannot have leading or trailing whitespace"));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid("cannot contain control characters"));
    }
    Ok(())
}

fn validate_artifact_path(path: &str) -> Result<(), CodegenError> {
    let invalid = |reason: &str| CodegenError::InvalidArtifactPath {
        relative_path: path.to_string(),
        reason: reason.to_string(),
    };
    if path.is_empty() {
        return Err(invalid("path cannot be empty"));
    }
    if path.len() > MAX_ARTIFACT_PATH_BYTES {
        return Err(invalid("path exceeds 1024 bytes"));
    }
    if !path.is_ascii() {
        return Err(invalid("path must use portable ASCII characters"));
    }
    if path.starts_with('/') {
        return Err(invalid("path must be relative"));
    }
    if path.contains('\\') {
        return Err(invalid("backslashes are not portable separators"));
    }

    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(invalid("path cannot contain an empty segment"));
        }
        if matches!(segment, "." | "..") {
            return Err(invalid("path cannot contain . or .. segments"));
        }
        if segment.len() > MAX_ARTIFACT_SEGMENT_BYTES {
            return Err(invalid("path segment exceeds 255 bytes"));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(invalid("path segment cannot end in dot or space"));
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(invalid(
                "path segments may contain only ASCII letters, digits, dot, underscore, and hyphen",
            ));
        }
        if is_windows_reserved_name(segment) {
            return Err(invalid("path contains a Windows reserved device name"));
        }
    }
    Ok(())
}

fn is_windows_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}
