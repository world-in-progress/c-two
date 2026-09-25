use c2_contract::{
    BindingDirection, ContractError, ContractFingerprintField, ContractRelease, MethodAccess,
    ValidatedContractDescriptor, derive_contract_fingerprints_json,
};

const SOURCE: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

fn source() -> String {
    let fingerprints = derive_contract_fingerprints_json(SOURCE.as_bytes()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    value["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
    value.to_string()
}

#[test]
fn validated_v2_descriptor_extracts_identity_methods_and_opaque_nested_specs() {
    let source = source();
    let descriptor = ValidatedContractDescriptor::from_json(source.as_bytes()).unwrap();

    assert_eq!(descriptor.contract_schema(), "c-two.contract.v2");
    assert_eq!(descriptor.crm_namespace(), "test.contract-release");
    assert_eq!(descriptor.crm_name(), "Portable");
    assert_eq!(descriptor.crm_version(), "0.1.0");
    assert_eq!(descriptor.methods().len(), 2);

    let ping = &descriptor.methods()[0];
    assert_eq!(ping.name(), "ping");
    assert_eq!(ping.access(), MethodAccess::Read);
    assert!(ping.input().is_none());
    assert!(ping.output().is_none());

    let echo = &descriptor.methods()[1];
    assert_eq!(echo.name(), "echo");
    assert_eq!(echo.access(), MethodAccess::Write);
    let input = echo.input().unwrap();
    assert_eq!(input.direction(), BindingDirection::Input);
    assert_eq!(input.outer_path(), "$.methods[1].bindings.input.spec");
    let output = echo.output().unwrap();
    assert_eq!(output.direction(), BindingDirection::Output);
    assert_eq!(output.outer_path(), "$.methods[1].bindings.output.spec");
    assert_eq!(input.canonical_json(), output.canonical_json());
    assert!(input.canonical_json().contains("\"fastdb.payload.v1\""));
    assert!(input.canonical_json().contains("\"nullable\":true"));
}

#[test]
fn formatting_and_key_order_do_not_change_descriptor_identity_or_nested_bytes() {
    let source = source();
    let original = ValidatedContractDescriptor::from_json(source.as_bytes()).unwrap();
    let original_release = ContractRelease::from_descriptor_json(source.as_bytes()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&source).unwrap();
    let reordered_json = format!(
        "{{\"methods\":{},\"fingerprints\":{},\"crm\":{},\"schema\":\"c-two.contract.v2\"}}",
        value["methods"], value["fingerprints"], value["crm"],
    );
    let reordered = ValidatedContractDescriptor::from_json(reordered_json.as_bytes()).unwrap();
    let reordered_release =
        ContractRelease::from_descriptor_json(reordered_json.as_bytes()).unwrap();

    assert_eq!(original.canonical_json(), reordered.canonical_json());
    assert_eq!(original.descriptor_sha256(), reordered.descriptor_sha256());
    assert_eq!(original_release.reference(), reordered_release.reference());
    assert_eq!(
        original.methods()[1].input().unwrap().canonical_json(),
        reordered.methods()[1].input().unwrap().canonical_json(),
    );
}

#[test]
fn nested_spec_change_changes_abi_fingerprint_and_release_digest() {
    let source = source();
    let original_fingerprints = derive_contract_fingerprints_json(source.as_bytes()).unwrap();
    let original = ValidatedContractDescriptor::from_json(source.as_bytes()).unwrap();
    let changed_placeholders = source.replace("\"kind\":\"str\"", "\"kind\":\"wstr\"");
    let changed_fingerprints =
        derive_contract_fingerprints_json(changed_placeholders.as_bytes()).unwrap();
    assert_ne!(
        original_fingerprints.abi_hash(),
        changed_fingerprints.abi_hash()
    );
    assert_eq!(
        original_fingerprints.signature_hash(),
        changed_fingerprints.signature_hash()
    );

    let mut changed: serde_json::Value = serde_json::from_str(&changed_placeholders).unwrap();
    changed["fingerprints"]["abi_hash"] =
        serde_json::Value::String(changed_fingerprints.abi_hash().to_string());
    changed["fingerprints"]["signature_hash"] =
        serde_json::Value::String(changed_fingerprints.signature_hash().to_string());
    let changed = ValidatedContractDescriptor::from_json(changed.to_string().as_bytes()).unwrap();
    assert_ne!(original.descriptor_sha256(), changed.descriptor_sha256());
}

#[test]
fn supplied_fingerprint_mismatch_is_typed() {
    let mut value: serde_json::Value = serde_json::from_str(&source()).unwrap();
    value["fingerprints"]["abi_hash"] = serde_json::Value::String("f".repeat(64));

    assert!(matches!(
        ValidatedContractDescriptor::from_json(value.to_string().as_bytes()),
        Err(ContractError::FingerprintMismatch {
            field: ContractFingerprintField::AbiHash,
            ..
        })
    ));
}

#[test]
fn malformed_nested_values_survive_outer_validation_for_fastdb_to_reject() {
    for nested in [
        serde_json::Value::Null,
        serde_json::json!("not a FastDB object"),
        serde_json::json!([1, 2, 3]),
        serde_json::json!({"schema": "wrong"}),
    ] {
        let mut candidate: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
        candidate["methods"][1]["bindings"]["input"]["spec"] = nested;
        let fingerprints =
            derive_contract_fingerprints_json(candidate.to_string().as_bytes()).unwrap();
        candidate["fingerprints"]["abi_hash"] =
            serde_json::Value::String(fingerprints.abi_hash().to_string());
        candidate["fingerprints"]["signature_hash"] =
            serde_json::Value::String(fingerprints.signature_hash().to_string());

        let descriptor =
            ValidatedContractDescriptor::from_json(candidate.to_string().as_bytes()).unwrap();
        assert!(descriptor.methods()[1].input().is_some());
    }
}

#[test]
fn input_output_shape_relationship_is_outer_owned() {
    let mut missing_parameter: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    missing_parameter["methods"][1]["parameters"] = serde_json::json!([]);
    let error =
        derive_contract_fingerprints_json(missing_parameter.to_string().as_bytes()).unwrap_err();
    assert!(matches!(
        error,
        ContractError::InvalidDescriptor { path, .. }
            if path == "$.methods[1].parameters"
    ));

    let mut wrong_return: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    wrong_return["methods"][1]["return"] = serde_json::json!({"kind": "none"});
    let error = derive_contract_fingerprints_json(wrong_return.to_string().as_bytes()).unwrap_err();
    assert!(matches!(
        error,
        ContractError::InvalidDescriptor { path, .. }
            if path == "$.methods[1].return"
    ));
}

#[test]
fn legacy_and_unknown_outer_method_fields_are_rejected() {
    for field in ["wire", "buffer", "unexpected"] {
        let mut candidate: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
        candidate["methods"][0][field] = serde_json::Value::Null;
        let error =
            derive_contract_fingerprints_json(candidate.to_string().as_bytes()).unwrap_err();
        assert!(
            matches!(
                error,
                ContractError::InvalidDescriptor { ref path, .. }
                    if path == &format!("$.methods[0].{field}")
            ),
            "unexpected error for {field}: {error}",
        );
    }
}

#[test]
fn binding_envelope_is_outer_validated_but_spec_content_is_not() {
    let mut wrong_kind: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    wrong_kind["methods"][1]["bindings"]["input"]["kind"] = serde_json::json!("other");
    assert!(matches!(
        derive_contract_fingerprints_json(wrong_kind.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. })
            if path == "$.methods[1].bindings.input.kind"
    ));

    let mut missing_spec: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    missing_spec["methods"][1]["bindings"]["input"]
        .as_object_mut()
        .unwrap()
        .remove("spec");
    assert!(matches!(
        derive_contract_fingerprints_json(missing_spec.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. })
            if path == "$.methods[1].bindings.input.spec"
    ));

    let mut extra_envelope_field: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    extra_envelope_field["methods"][1]["bindings"]["input"]["digest"] =
        serde_json::json!("not-owned-by-c-two");
    assert!(matches!(
        derive_contract_fingerprints_json(extra_envelope_field.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. })
            if path == "$.methods[1].bindings.input.digest"
    ));
}

#[test]
fn payload_parameter_shape_is_exact_and_non_variadic() {
    for (field, value, expected_path) in [
        (
            "type",
            serde_json::json!({"kind": "primitive", "name": "bytes"}),
            "$.methods[1].parameters[0].type.name",
        ),
        (
            "default",
            serde_json::json!({"kind": "json_scalar", "value": null}),
            "$.methods[1].parameters[0].default.value",
        ),
        (
            "kind",
            serde_json::json!("VAR_POSITIONAL"),
            "$.methods[1].parameters[0].kind",
        ),
    ] {
        let mut candidate: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
        candidate["methods"][1]["parameters"][0][field] = value;
        let error =
            derive_contract_fingerprints_json(candidate.to_string().as_bytes()).unwrap_err();
        assert!(
            matches!(
                error,
                ContractError::InvalidDescriptor { ref path, .. } if path == expected_path
            ),
            "unexpected error for {field}: {error}",
        );
    }
}

#[test]
fn signature_and_abi_projections_cover_separate_owned_facts() {
    let original = derive_contract_fingerprints_json(SOURCE.as_bytes()).unwrap();

    let mut access_change: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    access_change["methods"][1]["access"] = serde_json::json!("read");
    let access_change =
        derive_contract_fingerprints_json(access_change.to_string().as_bytes()).unwrap();
    assert_eq!(original.abi_hash(), access_change.abi_hash());
    assert_ne!(original.signature_hash(), access_change.signature_hash());

    let mut method_order_change: serde_json::Value = serde_json::from_str(SOURCE).unwrap();
    method_order_change["methods"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let method_order_change =
        derive_contract_fingerprints_json(method_order_change.to_string().as_bytes()).unwrap();
    assert_ne!(original.abi_hash(), method_order_change.abi_hash());
    assert_ne!(
        original.signature_hash(),
        method_order_change.signature_hash()
    );
}

#[test]
fn signature_fingerprint_mismatch_is_typed() {
    let mut value: serde_json::Value = serde_json::from_str(&source()).unwrap();
    value["fingerprints"]["signature_hash"] = serde_json::Value::String("f".repeat(64));

    assert!(matches!(
        ValidatedContractDescriptor::from_json(value.to_string().as_bytes()),
        Err(ContractError::FingerprintMismatch {
            field: ContractFingerprintField::SignatureHash,
            ..
        })
    ));
}
