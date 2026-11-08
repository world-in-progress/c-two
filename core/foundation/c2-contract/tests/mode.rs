use c2_contract::{
    ContractError, ContractFingerprintField, ContractRelease, MethodMode,
    ValidatedContractDescriptor, derive_contract_fingerprints_json,
};
use serde_json::{Value, json};

const UNARY: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const SERVER_STREAM: &str = include_str!("fixtures/server-stream.contract.json");

#[test]
fn omitted_mode_preserves_unary_descriptor_bytes() {
    let descriptor = ValidatedContractDescriptor::from_json(UNARY.as_bytes()).unwrap();
    assert!(
        descriptor
            .methods()
            .iter()
            .all(|method| method.mode() == MethodMode::Unary)
    );
    assert_eq!(
        descriptor.canonical_json(),
        include_str!("../../../../tests/fixtures/contracts/portable-release.canonical.json")
            .trim_end(),
    );
    let value: Value = serde_json::from_str(descriptor.canonical_json()).unwrap();
    assert!(
        value["methods"]
            .as_array()
            .unwrap()
            .iter()
            .all(|method| { !method.as_object().unwrap().contains_key("mode") })
    );
}

#[test]
fn server_stream_preserves_the_item_payload_binding_and_return() {
    let unary = ContractRelease::from_descriptor_json(UNARY.as_bytes()).unwrap();
    let stream = ContractRelease::from_descriptor_json(SERVER_STREAM.as_bytes()).unwrap();
    let unary_method = &unary.descriptor().methods()[1];
    let stream_method = &stream.descriptor().methods()[1];
    assert_eq!(stream_method.mode(), MethodMode::ServerStream);
    assert_eq!(stream.descriptor().methods()[0].mode(), MethodMode::Unary);
    assert_eq!(unary_method.input(), stream_method.input());
    assert_eq!(unary_method.output(), stream_method.output());
    let unary_json: Value = serde_json::from_str(UNARY).unwrap();
    let stream_json: Value = serde_json::from_str(stream.canonical_descriptor_json()).unwrap();
    assert_eq!(stream_json["schema"], "c-two.contract.v2");
    assert_eq!(stream_json["methods"][1]["mode"], "server_stream");
    assert_eq!(
        stream_json["methods"][1]["return"],
        json!({"kind": "payload"})
    );
    assert_eq!(
        stream_json["methods"][1]["bindings"],
        unary_json["methods"][1]["bindings"]
    );
    assert_ne!(unary.descriptor_sha256(), stream.descriptor_sha256());
    assert_ne!(unary.reference(), stream.reference());
}

#[test]
fn server_stream_mode_changes_both_fingerprint_projections() {
    let unary = derive_contract_fingerprints_json(UNARY.as_bytes()).unwrap();
    let stream = derive_contract_fingerprints_json(SERVER_STREAM.as_bytes()).unwrap();
    assert_ne!(unary.abi_hash(), stream.abi_hash());
    assert_ne!(unary.signature_hash(), stream.signature_hash());

    let mut restored: Value = serde_json::from_str(SERVER_STREAM).unwrap();
    restored["methods"][1]
        .as_object_mut()
        .unwrap()
        .remove("mode");
    assert_eq!(
        derive_contract_fingerprints_json(restored.to_string().as_bytes()).unwrap(),
        unary,
    );
}

#[test]
fn server_stream_cannot_be_ignored_and_validated_as_unary() {
    let mut candidate: Value = serde_json::from_str(UNARY).unwrap();
    candidate["methods"][1]["mode"] = json!("server_stream");
    assert!(matches!(
        ValidatedContractDescriptor::from_json(candidate.to_string().as_bytes()),
        Err(ContractError::FingerprintMismatch {
            field: ContractFingerprintField::AbiHash,
            ..
        })
    ));
    candidate["fingerprints"]["abi_hash"] =
        serde_json::from_str::<Value>(SERVER_STREAM).unwrap()["fingerprints"]["abi_hash"].clone();
    assert!(matches!(
        ValidatedContractDescriptor::from_json(candidate.to_string().as_bytes()),
        Err(ContractError::FingerprintMismatch {
            field: ContractFingerprintField::SignatureHash,
            ..
        })
    ));
}

#[test]
fn explicit_unary_unknown_and_non_string_modes_are_rejected_at_the_boundary() {
    for mode in [
        json!("unary"),
        json!("client_stream"),
        json!("bidi_stream"),
        json!("unknown"),
        json!(""),
        json!(" server_stream"),
        Value::Null,
        json!(true),
        json!(1),
        json!(1.5),
        json!([]),
        json!({}),
    ] {
        let mut candidate: Value = serde_json::from_str(UNARY).unwrap();
        candidate["methods"][1]["mode"] = mode.clone();
        let bytes = candidate.to_string();
        for error in [
            derive_contract_fingerprints_json(bytes.as_bytes()).unwrap_err(),
            ValidatedContractDescriptor::from_json(bytes.as_bytes()).unwrap_err(),
        ] {
            assert!(
                matches!(
                    error, ContractError::InvalidDescriptor { ref path, .. }
                        if path == "$.methods[1].mode"
                ),
                "mode {mode} produced {error}"
            );
        }
    }
}

#[test]
fn server_stream_requires_a_payload_item_shape_even_when_it_may_return_no_items() {
    let mut candidate: Value = serde_json::from_str(UNARY).unwrap();
    candidate["methods"][0]["mode"] = json!("server_stream");
    assert!(matches!(
        derive_contract_fingerprints_json(candidate.to_string().as_bytes()),
        Err(ContractError::InvalidDescriptor { path, .. })
            if path == "$.methods[0].return"
    ));
}
