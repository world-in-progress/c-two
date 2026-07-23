use crate::{
    ArtifactComposer, ArtifactKind, ArtifactLimits, ArtifactProvenance, CodegenError,
    ContractArtifact, ContractArtifactSet, fastdb_error, lower_hex,
    targets::{TargetMethod, render_target_artifacts},
};
use c2_contract::{
    BindingDirection, ContractRelease, MethodAccess, NestedFastDbSpec, ValidatedContractDescriptor,
};
use fastdb::{
    ArtifactKind as FastDbArtifactKind, Capabilities, CodegenOptions as FastDbCodegenOptions,
    CodegenTarget as FastDbCodegenTarget, CompiledSpec,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractCodegenTarget {
    Rust,
    Python,
    TypeScript,
}

impl ContractCodegenTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }

    fn fastdb_target(self) -> FastDbCodegenTarget {
        match self {
            Self::Rust => FastDbCodegenTarget::Rust,
            Self::Python => FastDbCodegenTarget::Python,
            Self::TypeScript => FastDbCodegenTarget::TypeScript,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractCodegenOptions {
    pub artifact_limits: ArtifactLimits,
    pub fastdb_codegen: FastDbCodegenOptions,
}

struct BindingFact {
    outer_path: String,
    direction: BindingDirection,
    fastdb_sha256: String,
}

struct PayloadArtifactFact {
    final_path: String,
    core_path: String,
    kind: ArtifactKind,
    sha256: String,
}

struct PayloadFact {
    first_binding_path: String,
    canonical_json: Vec<u8>,
    manifest_json: Vec<u8>,
    capabilities: Capabilities,
    artifacts: Vec<PayloadArtifactFact>,
}

struct MethodFact {
    index: usize,
    name: String,
    access: MethodAccess,
    input_sha256: Option<String>,
    output_sha256: Option<String>,
}

pub fn compile_contract_artifacts(
    descriptor_json: &[u8],
    target: ContractCodegenTarget,
    options: &ContractCodegenOptions,
) -> Result<ContractArtifactSet, CodegenError> {
    let descriptor = ValidatedContractDescriptor::from_json(descriptor_json)?;
    let release = ContractRelease::from_descriptor_json(descriptor_json)?;
    let release_ref_json = release.reference().to_canonical_json()?;
    let mut bindings = Vec::new();
    let mut methods = Vec::new();
    let mut payloads = BTreeMap::<String, PayloadFact>::new();
    let mut payload_artifacts = Vec::new();

    for (index, method) in descriptor.methods().iter().enumerate() {
        let input_sha256 = method
            .input()
            .map(|nested| {
                compile_binding(
                    nested,
                    target,
                    options,
                    &mut payloads,
                    &mut payload_artifacts,
                )
            })
            .transpose()?;
        if let (Some(nested), Some(digest)) = (method.input(), input_sha256.as_ref()) {
            bindings.push(BindingFact {
                outer_path: nested.outer_path().to_string(),
                direction: nested.direction(),
                fastdb_sha256: digest.clone(),
            });
        }

        let output_sha256 = method
            .output()
            .map(|nested| {
                compile_binding(
                    nested,
                    target,
                    options,
                    &mut payloads,
                    &mut payload_artifacts,
                )
            })
            .transpose()?;
        if let (Some(nested), Some(digest)) = (method.output(), output_sha256.as_ref()) {
            bindings.push(BindingFact {
                outer_path: nested.outer_path().to_string(),
                direction: nested.direction(),
                fastdb_sha256: digest.clone(),
            });
        }
        methods.push(MethodFact {
            index,
            name: method.name().to_string(),
            access: method.access(),
            input_sha256,
            output_sha256,
        });
    }

    let target_methods = methods
        .iter()
        .map(|method| TargetMethod {
            index: method.index,
            name: &method.name,
            access: method.access,
            input_sha256: method.input_sha256.as_deref(),
            output_sha256: method.output_sha256.as_deref(),
        })
        .collect::<Vec<_>>();
    let target_artifacts = render_target_artifacts(target, &descriptor, &target_methods)?;
    let contract_artifact = ContractArtifact::new(
        "metadata/contract.json",
        ArtifactKind::Metadata,
        descriptor.canonical_json().as_bytes().to_vec(),
        ArtifactProvenance::new("c-two", "contract-release")?,
    )?;
    let release_ref_artifact = ContractArtifact::new(
        "metadata/contract-release-ref.json",
        ArtifactKind::Metadata,
        release_ref_json.as_bytes().to_vec(),
        ArtifactProvenance::new("c-two", "contract-release-ref")?,
    )?;
    let manifest_bytes = composition_manifest(target, &release_ref_json, &bindings, &payloads)?;
    let manifest_artifact = ContractArtifact::new(
        "metadata/composition-manifest.json",
        ArtifactKind::Metadata,
        manifest_bytes,
        ArtifactProvenance::new("c-two", "artifact-composition")?,
    )?;

    let mut composer = ArtifactComposer::new(options.artifact_limits);
    composer.push(contract_artifact);
    composer.push(release_ref_artifact);
    composer.extend(target_artifacts);
    composer.extend(payload_artifacts);
    composer.push(manifest_artifact);
    composer.finish()
}

fn compile_binding(
    nested: &NestedFastDbSpec,
    target: ContractCodegenTarget,
    options: &ContractCodegenOptions,
    payloads: &mut BTreeMap<String, PayloadFact>,
    payload_artifacts: &mut Vec<ContractArtifact>,
) -> Result<String, CodegenError> {
    let compiled = compile_nested(nested)?;
    let digest = compiled
        .sha256()
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;
    let digest_hex = lower_hex(&digest);
    let canonical_json = compiled
        .canonical_json()
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;
    let calculated: [u8; 32] = Sha256::digest(&canonical_json).into();
    if calculated != digest {
        return Err(CodegenError::FastDbCanonicalDigestMismatch {
            binding_path: nested.outer_path().to_string(),
            expected: digest_hex,
            actual: lower_hex(&calculated),
        });
    }
    let manifest_json = compiled
        .manifest_json()
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;
    let capabilities = compiled
        .capabilities()
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;

    if let Some(existing) = payloads.get(&digest_hex) {
        if existing.canonical_json != canonical_json
            || existing.manifest_json != manifest_json
            || existing.capabilities != capabilities
        {
            return Err(CodegenError::FastDbIdentityConflict {
                sha256: digest_hex,
                first_binding_path: existing.first_binding_path.clone(),
                second_binding_path: nested.outer_path().to_string(),
            });
        }
        return Ok(digest_hex);
    }

    let generated = compiled
        .generate(target.fastdb_target(), &options.fastdb_codegen)
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;
    let count = generated
        .len()
        .map_err(|error| fastdb_error(nested.outer_path(), error))?;
    let mut artifact_facts = Vec::new();
    for index in 0..count {
        let artifact = generated
            .artifact(index)
            .map_err(|error| fastdb_error(nested.outer_path(), error))?;
        let kind = match artifact.kind {
            FastDbArtifactKind::Source => ArtifactKind::Source,
        };
        let final_path = format!(
            "{}/payloads/{}/{}",
            target.as_str(),
            digest_hex,
            artifact.relative_path
        );
        let provenance = ArtifactProvenance::new(
            "fastdb-core",
            format!("{}#{digest_hex}", nested.outer_path()),
        )?;
        let composed = ContractArtifact::from_claimed_parts(
            final_path.clone(),
            kind,
            artifact.bytes,
            artifact.sha256,
            provenance,
        )?;
        artifact_facts.push(PayloadArtifactFact {
            final_path,
            core_path: artifact.relative_path,
            kind,
            sha256: composed.sha256_hex(),
        });
        payload_artifacts.push(composed);
    }
    artifact_facts.sort_unstable_by(|left, right| left.final_path.cmp(&right.final_path));
    payloads.insert(
        digest_hex.clone(),
        PayloadFact {
            first_binding_path: nested.outer_path().to_string(),
            canonical_json,
            manifest_json,
            capabilities,
            artifacts: artifact_facts,
        },
    );
    Ok(digest_hex)
}

fn compile_nested(nested: &NestedFastDbSpec) -> Result<CompiledSpec, CodegenError> {
    CompiledSpec::compile(nested.canonical_json().as_bytes())
        .map_err(|error| fastdb_error(nested.outer_path(), error))
}

fn composition_manifest(
    target: ContractCodegenTarget,
    release_ref_json: &str,
    bindings: &[BindingFact],
    payloads: &BTreeMap<String, PayloadFact>,
) -> Result<Vec<u8>, CodegenError> {
    let release_ref: Value = serde_json::from_str(release_ref_json)
        .map_err(|error| CodegenError::MetadataSerialization(error.to_string()))?;
    let binding_values = bindings
        .iter()
        .map(|binding| {
            serde_json::json!({
                "outer_path": binding.outer_path,
                "direction": direction_name(binding.direction),
                "fastdb_sha256": binding.fastdb_sha256,
            })
        })
        .collect::<Vec<_>>();
    let payload_values =
        payloads
            .iter()
            .map(|(sha256, payload)| {
                let canonical_spec = serde_json::from_slice::<Value>(&payload.canonical_json)
                    .map_err(|error| CodegenError::FastDbInvalidJson {
                        binding_path: payload.first_binding_path.clone(),
                        fact: "canonical specification",
                        message: error.to_string(),
                    })?;
                let manifest =
                    serde_json::from_slice::<Value>(&payload.manifest_json).map_err(|error| {
                        CodegenError::FastDbInvalidJson {
                            binding_path: payload.first_binding_path.clone(),
                            fact: "manifest",
                            message: error.to_string(),
                        }
                    })?;
                let artifact_values = payload
                    .artifacts
                    .iter()
                    .map(|artifact| {
                        serde_json::json!({
                            "path": artifact.final_path,
                            "core_path": artifact.core_path,
                            "kind": artifact.kind.as_str(),
                            "sha256": artifact.sha256,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "fastdb_sha256": sha256,
                    "canonical_spec": canonical_spec,
                    "manifest": manifest,
                    "capabilities": {
                        "semantic_flags": payload.capabilities.semantic_flags,
                        "operation_flags": payload.capabilities.operation_flags,
                        "codegen_target_flags": payload.capabilities.codegen_target_flags,
                        "direct_build_status": payload.capabilities.direct_build_status,
                    },
                    "artifacts": artifact_values,
                }))
            })
            .collect::<Result<Vec<_>, CodegenError>>()?;
    let manifest = serde_json::json!({
        "schema": "c-two.artifact-composition.v1",
        "target": target.as_str(),
        "contract_release": release_ref,
        "bindings": binding_values,
        "payloads": payload_values,
    });
    Ok(canonical_json(&manifest).into_bytes())
}

fn direction_name(direction: BindingDirection) -> &'static str {
    match direction {
        BindingDirection::Input => "input",
        BindingDirection::Output => "output",
    }
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
        Value::Array(values) => {
            let body = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            let body = entries
                .into_iter()
                .map(|(key, value)| {
                    let key = Value::String(key.clone()).to_string();
                    format!("{key}:{}", canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
    }
}
