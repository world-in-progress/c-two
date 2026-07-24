use c2_contract::{
    ContractError, ContractLimitMetric, ContractLimits, ContractLimitsProfile, ContractRelease,
    ContractReleaseRef, MAX_CONTRACT_METHODS, contract_descriptor_sha256_hex_with_limits,
    derive_contract_fingerprints_json_with_limits,
};
use std::sync::Mutex;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const REFERENCE: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.ref.json");

static LARGE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn assert_limit(
    error: ContractError,
    metric: ContractLimitMetric,
    limit: u64,
    observed: u64,
    path: &str,
) {
    assert_eq!(
        error,
        ContractError::LimitExceeded {
            profile: ContractLimitsProfile::V1,
            metric,
            limit,
            observed,
            path: path.to_string(),
        }
    );
}

#[test]
fn v1_defaults_are_stable_control_plane_policy() {
    let limits = ContractLimits::v1();

    assert_eq!(limits.profile(), ContractLimitsProfile::V1);
    assert_eq!(limits.max_source_bytes(), 16 * 1024 * 1024);
    assert_eq!(limits.max_json_values(), 1_000_000);
    assert_eq!(limits.max_nesting_depth(), 128);
    assert_eq!(limits.max_methods(), 256);
    assert_eq!(limits.max_nested_fastdb_bytes(), 16 * 1024 * 1024);
    assert_eq!(limits, ContractLimits::default());
    assert_eq!(
        ContractLimits::v1()
            .with_max_methods(u64::MAX)
            .max_methods(),
        u64::try_from(MAX_CONTRACT_METHODS).unwrap()
    );
}

#[test]
fn source_bytes_accept_the_v1_boundary_and_reject_one_over_before_parsing() {
    let _large_test = LARGE_TEST_LOCK.lock().unwrap();
    let limit = usize::try_from(ContractLimits::v1().max_source_bytes()).unwrap();
    let mut exact = Vec::with_capacity(limit);
    exact.push(b'"');
    exact.resize(limit - 1, b'x');
    exact.push(b'"');
    assert_eq!(exact.len(), limit);
    contract_descriptor_sha256_hex_with_limits(&exact, ContractLimits::v1()).unwrap();

    let one_over = vec![0; limit + 1];
    let error =
        contract_descriptor_sha256_hex_with_limits(&one_over, ContractLimits::v1()).unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::SourceBytes,
        u64::try_from(limit).unwrap(),
        u64::try_from(limit + 1).unwrap(),
        "$",
    );
}

#[test]
fn a_tighter_source_limit_is_enforced_at_its_exact_boundary() {
    let exact =
        ContractLimits::v1().with_max_source_bytes(u64::try_from(DESCRIPTOR.len()).unwrap());
    ContractRelease::from_descriptor_json_with_limits(DESCRIPTOR.as_bytes(), exact).unwrap();

    let limit = u64::try_from(DESCRIPTOR.len() - 1).unwrap();
    let error = ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1().with_max_source_bytes(limit),
    )
    .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::SourceBytes,
        limit,
        u64::try_from(DESCRIPTOR.len()).unwrap(),
        "$",
    );
}

#[test]
fn json_values_accept_the_v1_boundary_and_reject_one_over() {
    let _large_test = LARGE_TEST_LOCK.lock().unwrap();
    let limit = usize::try_from(ContractLimits::v1().max_json_values()).unwrap();
    let mut exact = Vec::with_capacity(limit * 2);
    exact.push(b'[');
    for index in 0..(limit - 1) {
        if index != 0 {
            exact.push(b',');
        }
        exact.push(b'0');
    }
    exact.push(b']');
    contract_descriptor_sha256_hex_with_limits(&exact, ContractLimits::v1()).unwrap();

    exact.pop();
    exact.extend_from_slice(b",0]");
    let error =
        contract_descriptor_sha256_hex_with_limits(&exact, ContractLimits::v1()).unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::JsonValues,
        u64::try_from(limit).unwrap(),
        u64::try_from(limit + 1).unwrap(),
        &format!("$[{}]", limit - 1),
    );
}

#[test]
fn release_reference_value_count_excludes_object_keys_and_irrelevant_metrics() {
    let exact = ContractLimits::v1()
        .with_max_json_values(8)
        .with_max_methods(0)
        .with_max_nested_fastdb_bytes(u64::MAX);
    ContractReleaseRef::from_json_with_limits(REFERENCE.as_bytes(), exact).unwrap();

    let error = ContractReleaseRef::from_json_with_limits(
        REFERENCE.as_bytes(),
        ContractLimits::v1().with_max_json_values(7),
    )
    .unwrap_err();
    assert_limit(error, ContractLimitMetric::JsonValues, 7, 8, "$.schema");
}

#[test]
fn depth_accepts_the_v1_boundary_and_rejects_one_over_inside_the_visitor() {
    let exact_depth = usize::try_from(ContractLimits::v1().max_nesting_depth()).unwrap();
    let exact = nested_array_json(exact_depth);
    contract_descriptor_sha256_hex_with_limits(exact.as_bytes(), ContractLimits::v1()).unwrap();

    let one_over = nested_array_json(exact_depth + 1);
    let error =
        contract_descriptor_sha256_hex_with_limits(one_over.as_bytes(), ContractLimits::v1())
            .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::JsonDepth,
        u64::try_from(exact_depth).unwrap(),
        u64::try_from(exact_depth + 1).unwrap(),
        &format!("${}", "[0]".repeat(exact_depth)),
    );
}

#[test]
fn deeply_nested_hostile_descriptor_is_rejected_during_bounded_admission() {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    let nested_spec: serde_json::Value = serde_json::from_str(&nested_array_json(124)).unwrap();
    descriptor["methods"][1]["bindings"]["input"]["spec"] = nested_spec;
    let source = descriptor.to_string();

    let error = ContractRelease::from_descriptor_json(source.as_bytes()).unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::JsonDepth,
        128,
        129,
        &format!("$.methods[1].bindings.input.spec{}", "[0]".repeat(123)),
    );
}

#[test]
fn depth_counts_root_as_one() {
    contract_descriptor_sha256_hex_with_limits(
        b"0",
        ContractLimits::v1().with_max_nesting_depth(1),
    )
    .unwrap();
    let error = contract_descriptor_sha256_hex_with_limits(
        b"[0]",
        ContractLimits::v1().with_max_nesting_depth(1),
    )
    .unwrap_err();
    assert_limit(error, ContractLimitMetric::JsonDepth, 1, 2, "$[0]");
}

#[test]
fn non_identifier_object_keys_use_unambiguous_bracket_paths() {
    let error = contract_descriptor_sha256_hex_with_limits(
        br#"{"bad.key":[0]}"#,
        ContractLimits::v1().with_max_nesting_depth(2),
    )
    .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::JsonDepth,
        2,
        3,
        r#"$["bad.key"][0]"#,
    );
}

#[test]
fn tighter_depth_limits_reject_children_inside_the_visitor() {
    let exact = ContractLimits::v1().with_max_nesting_depth(3);
    ContractReleaseRef::from_json_with_limits(REFERENCE.as_bytes(), exact).unwrap();

    let error = ContractReleaseRef::from_json_with_limits(
        REFERENCE.as_bytes(),
        ContractLimits::v1().with_max_nesting_depth(2),
    )
    .unwrap_err();
    assert_limit(error, ContractLimitMetric::JsonDepth, 2, 3, "$.crm.name");

    let hostile = br#"[[[0]]]"#;
    let error = ContractReleaseRef::from_json_with_limits(
        hostile,
        ContractLimits::v1().with_max_nesting_depth(3),
    )
    .unwrap_err();
    assert_limit(error, ContractLimitMetric::JsonDepth, 3, 4, "$[0][0][0]");
}

#[test]
fn method_limit_accepts_the_hard_boundary_and_rejects_one_over_before_iteration() {
    let exact = descriptor_with_methods(MAX_CONTRACT_METHODS, true);
    ContractRelease::from_descriptor_json(exact.as_bytes()).unwrap();

    let one_over = descriptor_with_methods(MAX_CONTRACT_METHODS + 1, false);
    let error = ContractRelease::from_descriptor_json(one_over.as_bytes()).unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::Methods,
        u64::try_from(MAX_CONTRACT_METHODS).unwrap(),
        u64::try_from(MAX_CONTRACT_METHODS + 1).unwrap(),
        "$.methods",
    );
}

#[test]
fn a_tighter_method_limit_is_checked_at_the_outer_methods_array() {
    ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1().with_max_methods(2),
    )
    .unwrap();

    let error = ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1().with_max_methods(1),
    )
    .unwrap_err();
    assert_limit(error, ContractLimitMetric::Methods, 1, 2, "$.methods");
}

#[test]
fn nested_fastdb_bytes_accept_the_v1_boundary_and_reject_one_under() {
    let _large_test = LARGE_TEST_LOCK.lock().unwrap();
    let nested_limit = ContractLimits::v1().max_nested_fastdb_bytes();
    let limits = ContractLimits::v1().with_max_source_bytes(32 * 1024 * 1024);
    let descriptor = descriptor_with_one_nested_spec(nested_limit, limits);

    ContractRelease::from_descriptor_json_with_limits(descriptor.as_bytes(), limits).unwrap();

    let error = ContractRelease::from_descriptor_json_with_limits(
        descriptor.as_bytes(),
        limits.with_max_nested_fastdb_bytes(nested_limit - 1),
    )
    .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::NestedFastDbBytes,
        nested_limit - 1,
        nested_limit,
        "$.methods[0].bindings.input.spec",
    );
}

#[test]
fn nested_fastdb_bytes_count_every_binding_occurrence() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let echo = &release.descriptor().methods()[1];
    let input_bytes = echo.input().unwrap().canonical_json().len();
    let output_bytes = echo.output().unwrap().canonical_json().len();
    let total = u64::try_from(input_bytes + output_bytes).unwrap();

    ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1().with_max_nested_fastdb_bytes(total),
    )
    .unwrap();

    let error = ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1().with_max_nested_fastdb_bytes(total - 1),
    )
    .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::NestedFastDbBytes,
        total - 1,
        total,
        "$.methods[1].bindings.output.spec",
    );
}

#[test]
fn allocation_unrepresentable_limit_configuration_is_rejected_before_json_parsing() {
    let error = contract_descriptor_sha256_hex_with_limits(
        b"{",
        ContractLimits::v1().with_max_source_bytes(u64::MAX),
    )
    .unwrap_err();
    assert_limit(
        error,
        ContractLimitMetric::SourceBytes,
        u64::try_from(isize::MAX).unwrap(),
        u64::MAX,
        "$limits.max_source_bytes",
    );
}

#[test]
fn admission_policy_does_not_change_release_identity() {
    let default_release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let relaxed_release = ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::v1()
            .with_max_source_bytes(32 * 1024 * 1024)
            .with_max_json_values(2_000_000)
            .with_max_nesting_depth(256)
            .with_max_methods(512)
            .with_max_nested_fastdb_bytes(32 * 1024 * 1024),
    )
    .unwrap();

    assert_eq!(default_release.reference(), relaxed_release.reference());
    assert_eq!(
        default_release.canonical_descriptor_json(),
        relaxed_release.canonical_descriptor_json()
    );
    assert!(
        !default_release
            .canonical_descriptor_json()
            .contains("limits")
    );
}

fn nested_array_json(depth: usize) -> String {
    assert!(depth >= 1);
    format!("{}0{}", "[".repeat(depth - 1), "]".repeat(depth - 1))
}

fn descriptor_with_methods(count: usize, derive_fingerprints: bool) -> String {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    let template = descriptor["methods"][0].clone();
    descriptor["methods"] = serde_json::Value::Array(
        (0..count)
            .map(|index| {
                let mut method = template.clone();
                method["name"] = serde_json::Value::String(format!("method_{index:03}"));
                method
            })
            .collect(),
    );
    if derive_fingerprints {
        set_derived_fingerprints(&mut descriptor, ContractLimits::v1());
    }
    descriptor.to_string()
}

fn descriptor_with_one_nested_spec(canonical_spec_bytes: u64, limits: ContractLimits) -> String {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    let mut method = descriptor["methods"][1].clone();
    method["return"] = serde_json::json!({"kind": "none"});
    method["bindings"]["output"] = serde_json::Value::Null;

    let empty_spec = serde_json::json!({"padding": ""}).to_string();
    let padding_bytes = usize::try_from(canonical_spec_bytes).unwrap() - empty_spec.len();
    method["bindings"]["input"]["spec"] = serde_json::json!({"padding": "x".repeat(padding_bytes)});
    assert_eq!(
        method["bindings"]["input"]["spec"].to_string().len(),
        usize::try_from(canonical_spec_bytes).unwrap()
    );

    descriptor["methods"] = serde_json::Value::Array(vec![method]);
    set_derived_fingerprints(&mut descriptor, limits);
    descriptor.to_string()
}

fn set_derived_fingerprints(descriptor: &mut serde_json::Value, limits: ContractLimits) {
    let fingerprints =
        derive_contract_fingerprints_json_with_limits(descriptor.to_string().as_bytes(), limits)
            .unwrap();
    descriptor["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
}
