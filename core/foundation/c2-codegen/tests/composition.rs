use c2_codegen::{
    ArtifactComposer, ArtifactKind, ArtifactLimits, ArtifactProvenance, CodegenError,
    ContractArtifact, ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::sync::{Arc, Barrier};

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

fn provenance() -> ArtifactProvenance {
    ArtifactProvenance::new("test", "fixture").unwrap()
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn descriptor_with_nested_spec(spec: serde_json::Value) -> String {
    let mut value: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    value["methods"][1]["bindings"]["input"]["spec"] = spec.clone();
    value["methods"][1]["bindings"]["output"]["spec"] = spec;
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
    value.to_string()
}

#[test]
fn delegates_nested_specs_to_core_and_deduplicates_by_core_identity() {
    let set = compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::Rust,
        &ContractCodegenOptions::default(),
    )
    .unwrap();

    assert!(set.get("metadata/contract.json").is_some());
    assert!(set.get("metadata/contract-release-ref.json").is_some());
    let manifest = set.get("metadata/composition-manifest.json").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(manifest.bytes()).unwrap();
    assert_eq!(manifest["schema"], "c-two.artifact-composition.v1");
    assert_eq!(manifest["target"], "rust");
    assert_eq!(manifest["bindings"].as_array().unwrap().len(), 2);
    assert_eq!(
        manifest["bindings"][0]["fastdb_sha256"],
        manifest["bindings"][1]["fastdb_sha256"],
    );
    assert_eq!(manifest["payloads"].as_array().unwrap().len(), 1);
    assert!(manifest["payloads"][0]["canonical_spec"].is_object());
    assert!(manifest["payloads"][0]["manifest"].is_object());

    let descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    let nested =
        serde_json::to_vec(&descriptor["methods"][1]["bindings"]["input"]["spec"]).unwrap();
    let core_digest = fastdb::CompiledSpec::compile(&nested)
        .unwrap()
        .sha256()
        .unwrap();
    assert_eq!(manifest["bindings"][0]["fastdb_sha256"], hex(&core_digest),);

    let payload_paths = set
        .artifacts()
        .iter()
        .filter(|artifact| artifact.relative_path().starts_with("rust/payloads/"))
        .map(|artifact| artifact.relative_path())
        .collect::<Vec<_>>();
    assert_eq!(payload_paths.len(), 1);
    assert_eq!(set.artifacts().len(), 4);
    for artifact in set.artifacts() {
        assert_eq!(artifact.sha256(), &digest(artifact.bytes()));
    }
}

#[test]
fn all_supported_targets_use_their_own_deterministic_payload_subtree() {
    for (target, directory) in [
        (ContractCodegenTarget::Rust, "rust"),
        (ContractCodegenTarget::Python, "python"),
        (ContractCodegenTarget::TypeScript, "typescript"),
    ] {
        let set = compile_contract_artifacts(
            DESCRIPTOR.as_bytes(),
            target,
            &ContractCodegenOptions::default(),
        )
        .unwrap();
        assert!(set.artifacts().iter().any(|artifact| {
            artifact
                .relative_path()
                .starts_with(&format!("{directory}/payloads/"))
        }),);
        let manifest: serde_json::Value = serde_json::from_slice(
            set.get("metadata/composition-manifest.json")
                .unwrap()
                .bytes(),
        )
        .unwrap();
        assert_eq!(manifest["target"], directory);
    }
}

#[test]
fn no_payload_contract_produces_only_owner_metadata() {
    let mut value: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    value["methods"].as_array_mut().unwrap().truncate(1);
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());

    let set = compile_contract_artifacts(
        value.to_string().as_bytes(),
        ContractCodegenTarget::Rust,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    assert_eq!(set.artifacts().len(), 3);
    let manifest: serde_json::Value = serde_json::from_slice(
        set.get("metadata/composition-manifest.json")
            .unwrap()
            .bytes(),
    )
    .unwrap();
    assert_eq!(manifest["bindings"], serde_json::json!([]));
    assert_eq!(manifest["payloads"], serde_json::json!([]));
}

#[test]
fn object_graph_spec_is_delegated_without_a_c_two_profile_parser() {
    let descriptor = descriptor_with_nested_spec(serde_json::json!({
        "schema": "fastdb.payload.v1",
        "profile": "object_graph.v1",
        "entries": [
            {
                "id": "root",
                "cardinality": "many",
                "type": {"kind": "component", "id": "Node"}
            }
        ],
        "components": [
            {
                "id": "Node",
                "kind": "record",
                "fields": [
                    {
                        "id": "next",
                        "type": {
                            "kind": "ref",
                            "target": "Node",
                            "nullable": true
                        }
                    }
                ]
            }
        ]
    }));
    let set = compile_contract_artifacts(
        descriptor.as_bytes(),
        ContractCodegenTarget::Python,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        set.get("metadata/composition-manifest.json")
            .unwrap()
            .bytes(),
    )
    .unwrap();
    assert_eq!(
        manifest["payloads"][0]["canonical_spec"]["profile"],
        "object_graph.v1",
    );
    assert!(manifest["payloads"][0].get("profile").is_none());
}

#[test]
fn preserves_fastdb_structured_error_and_outer_binding_path() {
    let descriptor = descriptor_with_nested_spec(serde_json::json!({"schema": "wrong"}));
    let error = compile_contract_artifacts(
        descriptor.as_bytes(),
        ContractCodegenTarget::Python,
        &ContractCodegenOptions::default(),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CodegenError::FastDb {
            binding_path,
            code,
            symbol,
            path,
            message,
            details_json,
        } if binding_path == "$.methods[1].bindings.input.spec"
            && code != 0
            && !symbol.is_empty()
            && !path.is_empty()
            && !message.is_empty()
            && serde_json::from_str::<serde_json::Value>(&details_json).is_ok()
    ));
}

#[test]
fn composer_rejects_bad_hash_duplicate_and_prefix_conflicts() {
    let bytes = b"first".to_vec();
    let mut bad_hash = ArtifactComposer::new(ArtifactLimits::default());
    bad_hash.push(
        ContractArtifact::from_claimed_parts(
            "rust/client.rs",
            ArtifactKind::Source,
            bytes.clone(),
            [0; 32],
            provenance(),
        )
        .unwrap(),
    );
    assert!(matches!(
        bad_hash.finish(),
        Err(CodegenError::ArtifactHashMismatch { relative_path, .. })
            if relative_path == "rust/client.rs"
    ));

    let first = ContractArtifact::new(
        "rust/client.rs",
        ArtifactKind::Source,
        bytes.clone(),
        provenance(),
    )
    .unwrap();
    let duplicate =
        ContractArtifact::new("rust/client.rs", ArtifactKind::Source, bytes, provenance()).unwrap();
    let mut duplicates = ArtifactComposer::new(ArtifactLimits::default());
    duplicates.push(first);
    duplicates.push(duplicate);
    assert!(matches!(
        duplicates.finish(),
        Err(CodegenError::DuplicateArtifactPath { relative_path })
            if relative_path == "rust/client.rs"
    ));

    let mut prefix = ArtifactComposer::new(ArtifactLimits::default());
    prefix.push(
        ContractArtifact::new("rust/payloads", ArtifactKind::Source, vec![1], provenance())
            .unwrap(),
    );
    prefix.push(
        ContractArtifact::new(
            "rust/payloads/generated.rs",
            ArtifactKind::Source,
            vec![2],
            provenance(),
        )
        .unwrap(),
    );
    assert!(matches!(
        prefix.finish(),
        Err(CodegenError::ArtifactPathPrefixConflict { parent, child })
            if parent == "rust/payloads" && child == "rust/payloads/generated.rs"
    ));

    let mut non_adjacent_prefix = ArtifactComposer::new(ArtifactLimits::default());
    for path in ["a", "a-b", "a/b"] {
        non_adjacent_prefix.push(
            ContractArtifact::new(path, ArtifactKind::Source, vec![1], provenance()).unwrap(),
        );
    }
    assert!(matches!(
        non_adjacent_prefix.finish(),
        Err(CodegenError::ArtifactPathPrefixConflict { parent, child })
            if parent == "a" && child == "a/b"
    ));

    let mut case_collision = ArtifactComposer::new(ArtifactLimits::default());
    for path in ["Rust/Client.rs", "rust/client.rs"] {
        case_collision.push(
            ContractArtifact::new(path, ArtifactKind::Source, vec![1], provenance()).unwrap(),
        );
    }
    assert!(matches!(
        case_collision.finish(),
        Err(CodegenError::ArtifactPathCaseCollision { first, second })
            if first == "Rust/Client.rs" && second == "rust/client.rs"
    ));

    let mut folded_prefix = ArtifactComposer::new(ArtifactLimits::default());
    for path in ["Payloads", "payloads-extra", "payloads/generated.rs"] {
        folded_prefix.push(
            ContractArtifact::new(path, ArtifactKind::Source, vec![1], provenance()).unwrap(),
        );
    }
    assert!(matches!(
        folded_prefix.finish(),
        Err(CodegenError::ArtifactPathPrefixConflict { parent, child })
            if parent == "Payloads" && child == "payloads/generated.rs"
    ));
}

#[test]
fn composer_rejects_nonportable_paths_and_limits() {
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../escape",
        "a//b",
        "a\\b",
        "C:/drive",
        "file:uri",
        "rust/CON.rs",
        "rust/trailing.",
        "rust/雪.rs",
    ] {
        assert!(
            matches!(
                ContractArtifact::new(path, ArtifactKind::Source, vec![1], provenance(),),
                Err(CodegenError::InvalidArtifactPath { .. })
            ),
            "path unexpectedly accepted: {path:?}",
        );
    }

    let mut composer = ArtifactComposer::new(ArtifactLimits {
        max_artifacts: 1,
        max_total_bytes: 1,
    });
    composer.push(
        ContractArtifact::new("a.rs", ArtifactKind::Source, vec![1, 2], provenance()).unwrap(),
    );
    assert!(matches!(
        composer.finish(),
        Err(CodegenError::ArtifactLimitExceeded {
            limit: "max_total_bytes",
            actual: 2,
            maximum: 1,
        })
    ));
}

#[test]
fn composition_is_byte_deterministic_and_lexicographically_sorted() {
    let first = compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::TypeScript,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    let second = compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::TypeScript,
        &ContractCodegenOptions::default(),
    )
    .unwrap();

    assert_eq!(first, second);
    assert!(
        first
            .artifacts()
            .windows(2)
            .all(|pair| pair[0].relative_path() < pair[1].relative_path())
    );
}

#[test]
fn new_tree_publication_is_complete_and_refuses_existing_destination() {
    let set = compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::Python,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("generated");

    set.publish_new_tree(&destination).unwrap();
    for artifact in set.artifacts() {
        let written = fs::read(destination.join(artifact.relative_path())).unwrap();
        assert_eq!(written, artifact.bytes());
    }
    assert!(matches!(
        set.publish_new_tree(&destination),
        Err(CodegenError::DestinationExists { path }) if path == destination
    ));
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("stage")
    }));
}

#[test]
fn concurrent_publication_never_replaces_a_winning_complete_tree() {
    let set = Arc::new(
        compile_contract_artifacts(
            DESCRIPTOR.as_bytes(),
            ContractCodegenTarget::Rust,
            &ContractCodegenOptions::default(),
        )
        .unwrap(),
    );
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("generated");
    let barrier = Arc::new(Barrier::new(3));
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let set = Arc::clone(&set);
            let barrier = Arc::clone(&barrier);
            let destination = destination.clone();
            handles.push(scope.spawn(move || {
                barrier.wait();
                set.publish_new_tree(&destination)
            }));
        }
        barrier.wait();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CodegenError::DestinationExists { .. })))
            .count(),
        1,
    );
    for artifact in set.artifacts() {
        assert_eq!(
            fs::read(destination.join(artifact.relative_path())).unwrap(),
            artifact.bytes(),
        );
    }
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("stage")
    }));
}

#[cfg(unix)]
#[test]
fn publication_refuses_a_symlink_destination_without_following_it() {
    use std::os::unix::fs::symlink;

    let set = compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::Rust,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    let destination = temp.path().join("generated");
    symlink(&target, &destination).unwrap();

    assert!(matches!(
        set.publish_new_tree(&destination),
        Err(CodegenError::DestinationExists { path }) if path == destination
    ));
    assert!(fs::read_dir(&target).unwrap().next().is_none());
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
