use c2_contract::{
    CONTRACT_RELEASE_REF_SCHEMA, ContractError, ContractRelease, ContractReleaseRef,
    ContractReleaseRefField, derive_contract_fingerprints_json,
};

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const REFERENCE: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.ref.json");
const DIGEST: &str = "d25a8e308c803acd08df897279827384fc0de97ea1cacd3323554b94c6b560a3";

fn with_derived_fingerprints(mut value: serde_json::Value) -> String {
    let fingerprints = derive_contract_fingerprints_json(value.to_string().as_bytes()).unwrap();
    value["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
    value.to_string()
}

#[test]
fn release_derives_the_golden_route_independent_reference() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let reference = release.reference();

    assert_eq!(reference.schema(), CONTRACT_RELEASE_REF_SCHEMA);
    assert_eq!(reference.contract_schema(), "c-two.contract.v2");
    assert_eq!(reference.crm_namespace(), "test.contract-release");
    assert_eq!(reference.crm_name(), "Portable");
    assert_eq!(reference.crm_version(), "0.1.0");
    assert_eq!(reference.descriptor_sha256().as_str(), DIGEST);
    assert_eq!(reference.to_canonical_json().unwrap(), REFERENCE.trim_end());

    let encoded = reference.to_canonical_json().unwrap();
    for forbidden in [
        "route_name",
        "route_token",
        "abi_hash",
        "signature_hash",
        "authority",
        "publisher",
        "storage_url",
        "resolver_url",
        "signature",
        "compatibility_range",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "unexpected field: {forbidden}"
        );
    }
}

#[test]
fn release_reference_round_trips_and_verifies() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let reference = ContractReleaseRef::from_json(REFERENCE.as_bytes()).unwrap();

    reference.verify_release(&release).unwrap();
    assert_eq!(reference, release.reference());
}

#[test]
fn reference_parser_rejects_unknown_root_field() {
    let unknown = REFERENCE.trim_end().replace(
        "\"schema\":\"c-two.contract-release-ref.v1\"",
        "\"extra\":true,\"schema\":\"c-two.contract-release-ref.v1\"",
    );

    assert!(matches!(
        ContractReleaseRef::from_json(unknown.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.extra"
    ));
}

#[test]
fn reference_parser_rejects_non_lowercase_digest() {
    let uppercase = REFERENCE.trim_end().replace(DIGEST, &DIGEST.to_uppercase());

    assert!(matches!(
        ContractReleaseRef::from_json(uppercase.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. })
            if path == "$.descriptor_sha256"
    ));
}

#[test]
fn reference_parser_rejects_wrong_digest_length() {
    let short = REFERENCE
        .trim_end()
        .replace(DIGEST, &DIGEST[..DIGEST.len() - 1]);

    assert!(matches!(
        ContractReleaseRef::from_json(short.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. })
            if path == "$.descriptor_sha256"
    ));
}

#[test]
fn verification_identifies_each_mismatch_field() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    for (actual, expected, field) in [
        (
            "c-two.contract.v2",
            "c-two.contract.v1",
            ContractReleaseRefField::ContractSchema,
        ),
        (
            "test.contract-release",
            "test.other",
            ContractReleaseRefField::CrmNamespace,
        ),
        ("Portable", "Other", ContractReleaseRefField::CrmName),
        ("0.1.0", "0.2.0", ContractReleaseRefField::CrmVersion),
        (
            DIGEST,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ContractReleaseRefField::DescriptorSha256,
        ),
    ] {
        let candidate = REFERENCE.trim_end().replacen(actual, expected, 1);
        let reference = ContractReleaseRef::from_json(candidate.as_bytes()).unwrap();

        assert_eq!(
            reference.verify_release(&release),
            Err(ContractError::ReleaseRefMismatch {
                field,
                expected: expected.to_string(),
                actual: actual.to_string(),
            })
        );
    }
}

#[test]
fn release_projects_runtime_contract_only_after_route_is_supplied() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let expected = release.expected_route("catalog/grid").unwrap();

    assert_eq!(expected.route_name, "catalog/grid");
    assert_eq!(expected.crm_ns, "test.contract-release");
    assert_eq!(expected.crm_name, "Portable");
    assert_eq!(expected.crm_ver, "0.1.0");
    assert_eq!(
        expected.abi_hash,
        "bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe"
    );
    assert_eq!(
        expected.signature_hash,
        "c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940"
    );
}

#[test]
fn reference_parser_rejects_malformed_json() {
    assert!(matches!(
        ContractReleaseRef::from_json(br#"{"#),
        Err(ContractError::InvalidReleaseRefJson(_))
    ));
}

#[test]
fn reference_parser_rejects_wrong_reference_schema() {
    let wrong_schema = REFERENCE.trim_end().replace(
        "c-two.contract-release-ref.v1",
        "c-two.contract-release-ref.v2",
    );

    assert!(matches!(
        ContractReleaseRef::from_json(wrong_schema.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.schema"
    ));
}

#[test]
fn reference_parser_rejects_unknown_nested_crm_field() {
    let unknown_nested = REFERENCE.trim_end().replace(
        "\"version\":\"0.1.0\"",
        "\"extra\":true,\"version\":\"0.1.0\"",
    );

    assert!(matches!(
        ContractReleaseRef::from_json(unknown_nested.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.crm.extra"
    ));
}

#[test]
fn reference_parser_rejects_malformed_nested_crm_shape() {
    let mut malformed: serde_json::Value = serde_json::from_str(REFERENCE).unwrap();
    malformed["crm"] = serde_json::json!([]);

    assert!(matches!(
        ContractReleaseRef::from_json(malformed.to_string().as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.crm"
    ));
}

#[test]
fn reference_parser_rejects_missing_required_field() {
    let missing = REFERENCE
        .trim_end()
        .replace(&format!(",\"descriptor_sha256\":\"{DIGEST}\""), "");

    assert!(matches!(
        ContractReleaseRef::from_json(missing.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. })
            if path == "$.descriptor_sha256"
    ));
}

#[test]
fn release_rejects_invalid_runtime_route_text() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();

    assert!(matches!(
        release.expected_route("bad\\route"),
        Err(ContractError::Separator {
            field: "route name"
        })
    ));
}

#[test]
fn descriptor_mutation_changes_the_derived_reference_digest() {
    let original = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    changed["methods"][0]["name"] = serde_json::json!("health");
    let changed = with_derived_fingerprints(changed);
    let changed = ContractRelease::from_descriptor_json(changed.as_bytes()).unwrap();

    assert_ne!(
        original.reference().descriptor_sha256(),
        changed.reference().descriptor_sha256(),
    );
}

#[test]
fn no_payload_descriptor_is_a_valid_release() {
    let mut no_payload: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    no_payload["methods"].as_array_mut().unwrap().truncate(1);
    let no_payload = with_derived_fingerprints(no_payload);

    ContractRelease::from_descriptor_json(no_payload.as_bytes()).unwrap();
}

#[test]
fn fastdb_nested_value_remains_opaque_but_digest_covered() {
    let original = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    changed["methods"][1]["bindings"]["input"]["spec"]["entries"][0]["type"]["kind"] =
        serde_json::json!("wstr");
    let changed = with_derived_fingerprints(changed);
    let changed = ContractRelease::from_descriptor_json(changed.as_bytes()).unwrap();

    assert_ne!(original.descriptor_sha256(), changed.descriptor_sha256());
}
