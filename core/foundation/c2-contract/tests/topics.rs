use c2_contract::{
    BindingDirection, ContractError, ContractFingerprintField, ContractLimitMetric,
    ContractRelease, MAX_CONTRACT_NESTED_FASTDB_BYTES, MAX_CONTRACT_TOPICS,
    ValidatedContractDescriptor, derive_contract_fingerprints_json,
};
use serde_json::{Value, json};

const UNARY: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const SERVER_STREAM: &str = include_str!("fixtures/server-stream.contract.json");

fn sample_fastdb_binding() -> Value {
    let value: Value = serde_json::from_str(UNARY).unwrap();
    value["methods"][1]["bindings"]["output"].clone()
}

fn with_derived_fingerprints(mut value: Value) -> Value {
    let derived = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(derived.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] = Value::String(derived.signature_hash().to_string());
    value
}

#[test]
fn omitted_topics_preserve_unary_and_stream_identities_byte_for_byte() {
    for source in [UNARY, SERVER_STREAM] {
        let descriptor = ValidatedContractDescriptor::from_json(source.as_bytes()).unwrap();
        assert!(descriptor.topics().is_empty());
        let parsed: Value = serde_json::from_str(descriptor.canonical_json()).unwrap();
        assert!(!parsed.as_object().unwrap().contains_key("topics"));
    }
}

#[test]
fn mixed_contract_with_sorted_topics_validates_and_extracts_nested_specs() {
    let mut candidate: Value = serde_json::from_str(UNARY).unwrap();
    candidate["topics"] = json!([
        {
            "name": "field",
            "output": {"kind": "payload"},
            "binding": sample_fastdb_binding(),
        },
        {
            "name": "telemetry",
            "output": {"kind": "payload"},
            "binding": sample_fastdb_binding(),
        }
    ]);
    let signed = with_derived_fingerprints(candidate);
    let release = ContractRelease::from_descriptor_json(signed.to_string().as_bytes()).unwrap();
    let descriptor = release.descriptor();

    assert_eq!(descriptor.methods().len(), 2);
    assert_eq!(descriptor.topics().len(), 2);
    assert_eq!(descriptor.topics()[0].name(), "field");
    assert_eq!(
        descriptor.topics()[0].output().direction(),
        BindingDirection::Output
    );
    assert_eq!(
        descriptor.topics()[0].output().outer_path(),
        "$.topics[0].binding.spec"
    );
    assert_eq!(descriptor.topics()[1].name(), "telemetry");
    assert_eq!(
        descriptor.topics()[1].output().outer_path(),
        "$.topics[1].binding.spec"
    );

    let unary = ContractRelease::from_descriptor_json(UNARY.as_bytes()).unwrap();
    assert_ne!(unary.descriptor().abi_hash(), descriptor.abi_hash());
    assert_ne!(
        unary.descriptor().signature_hash(),
        descriptor.signature_hash()
    );
    assert_ne!(unary.descriptor_sha256(), release.descriptor_sha256());

    let mut stripped = signed.clone();
    stripped.as_object_mut().unwrap().remove("topics");
    let restored = derive_contract_fingerprints_json(stripped.to_string().as_bytes()).unwrap();
    assert_eq!(restored.abi_hash(), unary.descriptor().abi_hash());
    assert_eq!(
        restored.signature_hash(),
        unary.descriptor().signature_hash()
    );
}

#[test]
fn topic_only_contract_is_accepted() {
    let candidate = json!({
        "schema": "c-two.contract.v2",
        "crm": {
            "namespace": "sim.grid",
            "name": "FieldFeed",
            "version": "0.1.0",
        },
        "fingerprints": {
            "abi_hash": "0".repeat(64),
            "signature_hash": "0".repeat(64),
        },
        "methods": [],
        "topics": [
            {
                "name": "field",
                "output": {"kind": "payload"},
                "binding": sample_fastdb_binding(),
            }
        ],
    });
    let signed = with_derived_fingerprints(candidate);
    let descriptor =
        ValidatedContractDescriptor::from_json(signed.to_string().as_bytes()).unwrap();
    assert!(descriptor.methods().is_empty());
    assert_eq!(descriptor.topics().len(), 1);
    assert_eq!(descriptor.topics()[0].name(), "field");
}

#[test]
fn topic_spec_change_changes_abi_only_while_name_change_changes_both() {
    let mut base: Value = serde_json::from_str(UNARY).unwrap();
    base["topics"] = json!([
        {
            "name": "field",
            "output": {"kind": "payload"},
            "binding": sample_fastdb_binding(),
        }
    ]);
    let base_fp = derive_contract_fingerprints_json(base.to_string().as_bytes()).unwrap();

    let mut spec_changed = base.clone();
    let spec_text = spec_changed["topics"][0]["binding"]["spec"]
        .to_string()
        .replace("\"kind\":\"str\"", "\"kind\":\"wstr\"");
    spec_changed["topics"][0]["binding"]["spec"] = serde_json::from_str(&spec_text).unwrap();
    let spec_fp = derive_contract_fingerprints_json(spec_changed.to_string().as_bytes()).unwrap();
    assert_ne!(base_fp.abi_hash(), spec_fp.abi_hash());
    assert_eq!(base_fp.signature_hash(), spec_fp.signature_hash());

    let mut renamed = base.clone();
    renamed["topics"][0]["name"] = json!("grid_field");
    let renamed_fp = derive_contract_fingerprints_json(renamed.to_string().as_bytes()).unwrap();
    assert_ne!(base_fp.abi_hash(), renamed_fp.abi_hash());
    assert_ne!(base_fp.signature_hash(), renamed_fp.signature_hash());

    // Tampered fingerprints must be rejected explicitly.
    let mut forged = base;
    forged["fingerprints"]["abi_hash"] = Value::String(renamed_fp.abi_hash().to_string());
    forged["fingerprints"]["signature_hash"] =
        Value::String(renamed_fp.signature_hash().to_string());
    assert!(matches!(
        ValidatedContractDescriptor::from_json(forged.to_string().as_bytes()),
        Err(ContractError::FingerprintMismatch {
            field: ContractFingerprintField::AbiHash,
            ..
        })
    ));
}

#[test]
fn invalid_topic_declarations_are_rejected_at_the_boundary() {
    // 1. Explicit empty topics array is forbidden.
    let mut empty_topics: Value = serde_json::from_str(UNARY).unwrap();
    empty_topics["topics"] = json!([]);
    assert!(matches!(
        derive_contract_fingerprints_json(empty_topics.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. }) if path == "$.topics"
    ));

    // 2. Unsorted topics are rejected.
    let mut unsorted: Value = serde_json::from_str(UNARY).unwrap();
    unsorted["topics"] = json!([
        {"name": "zeta", "output": {"kind": "payload"}, "binding": sample_fastdb_binding()},
        {"name": "alpha", "output": {"kind": "payload"}, "binding": sample_fastdb_binding()},
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(unsorted.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, message })
            if path == "$.topics[1].name" && message.contains("sorted")
    ));

    // 3. Duplicate topic names are rejected.
    let mut duplicate: Value = serde_json::from_str(UNARY).unwrap();
    duplicate["topics"] = json!([
        {"name": "field", "output": {"kind": "payload"}, "binding": sample_fastdb_binding()},
        {"name": "field", "output": {"kind": "payload"}, "binding": sample_fastdb_binding()},
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(duplicate.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, message })
            if path == "$.topics[1].name" && message.contains("duplicate")
    ));

    // 4. Collision with an existing method name is rejected.
    let mut collides: Value = serde_json::from_str(UNARY).unwrap();
    collides["topics"] = json!([
        {"name": "echo", "output": {"kind": "payload"}, "binding": sample_fastdb_binding()},
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(collides.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, message })
            if path == "$.topics[0].name" && message.contains("collides")
    ));

    // 5. Null binding is rejected for portable topics.
    let mut null_binding: Value = serde_json::from_str(UNARY).unwrap();
    null_binding["topics"] = json!([
        {"name": "field", "output": {"kind": "payload"}, "binding": Value::Null},
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(null_binding.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. }) if path == "$.topics[0].binding"
    ));

    // 6. Non-payload output kind is rejected.
    let mut none_output: Value = serde_json::from_str(UNARY).unwrap();
    none_output["topics"] = json!([
        {"name": "field", "output": {"kind": "none"}, "binding": sample_fastdb_binding()},
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(none_output.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. }) if path == "$.topics[0].output.kind"
    ));

    // 7. Unknown field on a topic object is rejected.
    let mut extra_field: Value = serde_json::from_str(UNARY).unwrap();
    extra_field["topics"] = json!([
        {
            "name": "field",
            "output": {"kind": "payload"},
            "binding": sample_fastdb_binding(),
            "access": "read",
        },
    ]);
    assert!(matches!(
        derive_contract_fingerprints_json(extra_field.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. }) if path == "$.topics[0].access"
    ));
}

#[test]
fn topic_count_and_nested_fastdb_limits_are_enforced() {
    let mut at_limit: Value = serde_json::from_str(UNARY).unwrap();
    let topics: Vec<Value> = (0..MAX_CONTRACT_TOPICS)
        .map(|i| {
            json!({
                "name": format!("topic_{i:04}"),
                "output": {"kind": "payload"},
                "binding": {"kind": "fastdb", "spec": {"schema": "fastdb.payload.v1"}},
            })
        })
        .collect();
    at_limit["topics"] = Value::Array(topics);
    derive_contract_fingerprints_json(at_limit.to_string().as_bytes()).unwrap();

    let mut over_limit = at_limit;
    over_limit["topics"].as_array_mut().unwrap().push(json!({
        "name": format!("topic_{MAX_CONTRACT_TOPICS:04}"),
        "output": {"kind": "payload"},
        "binding": {"kind": "fastdb", "spec": {"schema": "fastdb.payload.v1"}},
    }));
    assert_eq!(
        derive_contract_fingerprints_json(over_limit.to_string().as_bytes()).unwrap_err(),
        ContractError::LimitExceeded {
            metric: ContractLimitMetric::Topics,
            limit: MAX_CONTRACT_TOPICS as u64,
            observed: (MAX_CONTRACT_TOPICS + 1) as u64,
            path: "$.topics".to_string(),
        }
    );

    // Nested FastDB bytes accumulate across methods and topics under one shared limit.
    let nested_spec = format!(
        "{{\"padding\":\"{}\",\"numbers\":[{}]}}",
        "x".repeat(5_200_000),
        vec!["-0"; 100_000].join(",")
    );
    let canonical_spec_len = serde_json::from_str::<Value>(&nested_spec)
        .unwrap()
        .to_string()
        .len();
    let marker = "c2-topic-nested-spec-marker";
    let mut combined: Value = serde_json::from_str(UNARY).unwrap();
    combined["methods"][1]["bindings"]["input"]["spec"] = marker.into();
    combined["methods"][1]["bindings"]["output"]["spec"] = marker.into();
    combined["topics"] = json!([
        {
            "name": "field",
            "output": {"kind": "payload"},
            "binding": {"kind": "fastdb", "spec": marker},
        }
    ]);
    let source = combined
        .to_string()
        .replace(&serde_json::to_string(marker).unwrap(), &nested_spec);
    assert_eq!(
        ContractRelease::from_descriptor_json(source.as_bytes()).unwrap_err(),
        ContractError::LimitExceeded {
            metric: ContractLimitMetric::NestedFastDbBytes,
            limit: MAX_CONTRACT_NESTED_FASTDB_BYTES,
            observed: u64::try_from(3 * canonical_spec_len).unwrap(),
            path: "$.topics[0].binding.spec".to_string(),
        }
    );
}
