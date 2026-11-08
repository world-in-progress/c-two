use c2_codegen::{ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;
use serde_json::{Value, json};

#[test]
fn unary_artifacts_for_all_three_languages_match_the_pre_stream_baseline() {
    // Captured from ba231cdf with the pinned FastDB 0.2.4 Core SDK. The corpus
    // includes the shared three-language portable matrix fixtures.
    let baseline: Value =
        serde_json::from_str(include_str!("fixtures/unary-artifacts.json")).unwrap();
    for row in baseline.as_array().unwrap() {
        let fixture = row["fixture"].as_str().unwrap();
        let source = match fixture {
            "portable-release" => {
                include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json")
            }
            "portable-no-payload" => include_str!(
                "../../../../tests/fixtures/contracts/portable-no-payload.contract.json"
            ),
            "portable-record-v1" => include_str!(
                "../../../../tests/fixtures/contracts/portable-record-v1.contract.json"
            ),
            "portable-object-graph-v1" => include_str!(
                "../../../../tests/fixtures/contracts/portable-object-graph-v1.contract.json"
            ),
            other => panic!("unknown baseline fixture {other}"),
        };
        let target = match row["target"].as_str().unwrap() {
            "rust" => ContractCodegenTarget::Rust,
            "python" => ContractCodegenTarget::Python,
            "typescript" => ContractCodegenTarget::TypeScript,
            other => panic!("unknown baseline target {other}"),
        };
        let release = ContractRelease::from_descriptor_json(source.as_bytes()).unwrap();
        let set = compile_contract_artifacts(&release, target, &Default::default()).unwrap();
        let artifacts = set
            .artifacts()
            .iter()
            .map(|artifact| {
                json!({
                    "path": artifact.relative_path(),
                    "sha256": artifact.sha256_hex(),
                    "bytes": artifact.bytes().len(),
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(json!(artifacts), row["artifacts"], "{fixture}, {target:?}");
    }
}
