use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use c2_contract::{ContractError, ContractLimitMetric, ContractLimitsProfile};
use c2_core::{
    AdapterFailure, AdapterFailurePhase, Error, ExternalCause, Runtime, RuntimeOptions,
    TransportPhase, external_cause_details, normalize_adapter_failure, normalize_http_error,
    normalize_ipc_error,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::client::HttpError;
use c2_ipc::IpcError;

#[test]
fn public_core_types_compile_from_the_new_import() {
    fn accepts_runtime(_: Option<Runtime>) {}
    fn accepts_options(_: RuntimeOptions) {}
    fn accepts_error(_: Option<Error>) {}

    accepts_runtime(None);
    accepts_options(RuntimeOptions::default());
    accepts_error(None);
}

#[test]
fn ipc_and_http_semantic_envelopes_normalize_identically() {
    let expected =
        C2Error::new(ErrorCode::ResourceFunctionExecuting, "service failed").with_details(
            BTreeMap::from([("request_id".to_string(), "req-7".to_string())]),
        );

    let ipc = normalize_ipc_error(
        IpcError::CrmError(expected.to_wire_bytes()),
        TransportPhase::DispatchUncertain,
    );
    let http = normalize_http_error(
        HttpError::ServerError(
            500,
            serde_json::to_string(&expected.envelope()).expect("serialize HTTP envelope"),
        ),
        TransportPhase::DispatchUncertain,
    );

    assert_semantic_eq(ipc, &expected);
    assert_semantic_eq(http, &expected);
}

#[test]
fn malformed_semantic_payloads_become_protocol_violations() {
    for error in [
        normalize_ipc_error(
            IpcError::CrmError(b"C2E1not-json".to_vec()),
            TransportPhase::DispatchUncertain,
        ),
        normalize_http_error(
            HttpError::ServerError(500, r#"{"version":1,"code":"wrong"}"#.to_string()),
            TransportPhase::DispatchUncertain,
        ),
    ] {
        let Error::Semantic(error) = error else {
            panic!("malformed semantic payload must normalize to a semantic protocol violation");
        };
        assert_eq!(error.code, ErrorCode::ProtocolViolation);
        assert_eq!(
            error.details.get("protocol"),
            Some(&"c-two.error.v1".to_string())
        );
    }
}

#[test]
fn contract_limit_failures_are_distinct_from_other_contract_failures() {
    let admission = Error::from(ContractError::LimitExceeded {
        profile: ContractLimitsProfile::V1,
        metric: ContractLimitMetric::SourceBytes,
        limit: 16,
        observed: 17,
        path: "$".to_string(),
    });
    let contract = Error::from(ContractError::Empty {
        field: "route_name",
    });

    assert!(matches!(admission, Error::Admission(_)));
    assert!(matches!(contract, Error::Contract(_)));
}

#[test]
fn transport_phase_is_the_only_fallback_eligibility_authority() {
    let pre_dispatch = normalize_ipc_error(IpcError::Closed, TransportPhase::PreDispatch);
    let dispatch_uncertain = normalize_http_error(
        HttpError::Transport("connection reset".to_string()),
        TransportPhase::DispatchUncertain,
    );

    let Error::Transport(pre_dispatch) = pre_dispatch else {
        panic!("non-semantic IPC error must remain a transport error");
    };
    let Error::Transport(dispatch_uncertain) = dispatch_uncertain else {
        panic!("non-semantic HTTP error must remain a transport error");
    };

    assert_eq!(pre_dispatch.phase(), TransportPhase::PreDispatch);
    assert!(pre_dispatch.is_fallback_eligible());
    assert_eq!(
        dispatch_uncertain.phase(),
        TransportPhase::DispatchUncertain
    );
    assert!(!dispatch_uncertain.is_fallback_eligible());
}

#[test]
fn external_fastdb_cause_uses_the_frozen_outer_field_contract() {
    let details = external_cause_details(ExternalCause {
        owner: "fastdb",
        code: "3006",
        symbol: "DIGEST_MISMATCH",
        path: "$.output",
        message: "payload digest mismatch",
        details_json: r#"{"expected":"a","actual":"b"}"#,
    });

    assert_eq!(
        details.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "cause_owner",
            "fastdb_code",
            "fastdb_details_json",
            "fastdb_message",
            "fastdb_path",
            "fastdb_symbol",
        ])
    );
    assert_eq!(details.get("cause_owner"), Some(&"fastdb".to_string()));
    assert_eq!(
        details.get("fastdb_symbol"),
        Some(&"DIGEST_MISMATCH".to_string())
    );
}

#[test]
fn portable_adapter_phases_use_the_frozen_error_codes() {
    let expected = [
        (
            AdapterFailurePhase::ClientInputSerializing,
            ErrorCode::ClientInputSerializing,
        ),
        (
            AdapterFailurePhase::ClientOutputFromBuffer,
            ErrorCode::ClientOutputFromBuffer,
        ),
        (
            AdapterFailurePhase::ClientOutputDeserializing,
            ErrorCode::ClientOutputDeserializing,
        ),
        (
            AdapterFailurePhase::ResourceInputFromBuffer,
            ErrorCode::ResourceInputFromBuffer,
        ),
        (
            AdapterFailurePhase::ResourceInputDeserializing,
            ErrorCode::ResourceInputDeserializing,
        ),
        (
            AdapterFailurePhase::ResourceFunctionExecuting,
            ErrorCode::ResourceFunctionExecuting,
        ),
        (
            AdapterFailurePhase::ResourceOutputSerializing,
            ErrorCode::ResourceOutputSerializing,
        ),
    ];

    for (phase, code) in expected {
        let normalized = normalize_adapter_failure(
            phase,
            AdapterFailure::Local {
                message: "adapter failed".to_string(),
                details: BTreeMap::new(),
            },
        );
        assert_eq!(normalized.code, code);
    }
}

#[test]
fn existing_semantic_adapter_error_passes_through_unchanged() {
    let expected = C2Error::new(ErrorCode::ResourceClosed, "route is draining").with_details(
        BTreeMap::from([("route_uid".to_string(), "route-9".to_string())]),
    );

    let actual = normalize_adapter_failure(
        AdapterFailurePhase::ResourceFunctionExecuting,
        AdapterFailure::Semantic(expected.clone()),
    );

    assert_eq!(actual, expected);
}

#[test]
fn obsolete_runtime_crate_identity_is_absent_from_current_sources() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("c2-core must be nested at core/runtime/c2-core");
    let needles = [
        ["c2-", "runtime"].concat(),
        ["c2_", "runtime"].concat(),
        ["core/runtime/c2-", "runtime"].concat(),
    ];
    let scopes = [
        repo.join("core"),
        repo.join("cli"),
        repo.join("sdk"),
        repo.join("README.md"),
        repo.join("AGENTS.md"),
        repo.join("docs/roadmap.md"),
        repo.join("docs/issues"),
    ];

    let mut offenders = Vec::new();
    for scope in scopes {
        collect_current_source_offenders(&scope, &needles, &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "obsolete runtime crate identity remains in current sources:\n{}",
        offenders.join("\n")
    );
}

fn assert_semantic_eq(actual: Error, expected: &C2Error) {
    let Error::Semantic(actual) = actual else {
        panic!("expected normalized semantic error");
    };
    assert_eq!(&actual, expected);
}

fn collect_current_source_offenders(path: &Path, needles: &[String], offenders: &mut Vec<String>) {
    if ignored_path(path) {
        return;
    }
    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("read {} entry: {error}", path.display()));
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            collect_current_source_offenders(&entry.path(), needles, offenders);
        }
        return;
    }

    let Ok(source) = fs::read_to_string(path) else {
        return;
    };
    for (line_index, line) in source.lines().enumerate() {
        if line.to_ascii_lowercase().contains("historical") {
            continue;
        }
        for needle in needles {
            if line.contains(needle) {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_index + 1,
                    line.trim()
                ));
            }
        }
    }
}

fn ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(
                ".git"
                    | ".pytest_cache"
                    | ".uv_cache"
                    | ".venv"
                    | "__pycache__"
                    | "node_modules"
                    | "target"
            )
        )
    })
}
