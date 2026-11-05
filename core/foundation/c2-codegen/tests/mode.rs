use c2_codegen::{CodegenError, ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;

const SERVER_STREAM: &str =
    include_str!("../../c2-contract/tests/fixtures/server-stream.contract.json");

#[test]
fn rust_and_python_generate_server_stream_while_typescript_rejects_it() {
    let release = ContractRelease::from_descriptor_json(SERVER_STREAM.as_bytes()).unwrap();
    let rust =
        compile_contract_artifacts(&release, ContractCodegenTarget::Rust, &Default::default())
            .expect("Rust server stream generation");
    let source = std::str::from_utf8(rust.get("rust/c_two_contract.rs").unwrap().bytes()).unwrap();
    for required in [
        "pub fn method_1_echo(&self, payload: &fastdb::Payload) -> Result<c_two::ResponseStream<fastdb::Payload>, c_two::Error>",
        "pub fn hold_method_1_echo(",
        "Result<c_two::ResponseStream<c_two::Held<fastdb::Payload>>, c_two::Error>",
        "Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>",
        "c_two::ServiceDefinition::new_stream(",
        "c_two::generated::EncodedStreamService for ServiceAdapter<S>",
        "c_two::generated::PayloadStream::new(",
    ] {
        assert!(source.contains(required), "missing {required:?}");
    }
    assert!(!source.contains("fn encode_method_1_echo_output("));
    assert!(source.contains("fn method_0_ping(&self) -> Result<(), c_two::Error>"));

    let python =
        compile_contract_artifacts(&release, ContractCodegenTarget::Python, &Default::default())
            .expect("Python server stream generation");
    let source =
        std::str::from_utf8(python.get("python/c_two_contract.py").unwrap().bytes()).unwrap();
    for required in [
        "def method_1_echo(self, payload: Payload) -> Stream[Payload]:",
        "def hold_method_1_echo(self, payload: Payload) -> Stream[HeldResult[Payload]]:",
        "self._bound_client.begin_stream(\"echo\")",
        "prepared.charge_input(payload.execution_report().used_bytes)",
        "prepared.stream(request)",
        "Stream._from_encoded(",
        "decode_method_1_echo_output",
    ] {
        assert!(source.contains(required), "missing {required:?}");
    }
    assert!(source.contains("def method_0_ping(self) -> None:"));

    let target = ContractCodegenTarget::TypeScript;
    let error = compile_contract_artifacts(&release, target, &Default::default())
        .err()
        .expect("TypeScript server stream generation remains unsupported");
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
