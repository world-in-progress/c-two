use c2_codegen::{CodegenError, ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;

const SERVER_STREAM: &str =
    include_str!("../../c2-contract/tests/fixtures/server-stream.contract.json");

#[test]
fn every_target_rejects_server_stream_at_the_generation_boundary() {
    let release = ContractRelease::from_descriptor_json(SERVER_STREAM.as_bytes()).unwrap();
    for target in [
        ContractCodegenTarget::Rust,
        ContractCodegenTarget::Python,
        ContractCodegenTarget::TypeScript,
    ] {
        let error = compile_contract_artifacts(&release, target, &Default::default())
            .err()
            .expect("server stream generation is implemented in later blocks");
        assert!(matches!(
            &error,
            CodegenError::UnsupportedServerStream {
                target: actual_target,
                method_index: 1,
                method_name,
            } if *actual_target == target && method_name == "echo"
        ));
        let message = error.to_string();
        assert!(message.contains(target.as_str()), "{message}");
        assert!(message.contains("server_stream"), "{message}");
        assert!(message.contains("echo"), "{message}");
        assert!(message.contains("$.methods[1].mode"), "{message}");
    }
}
