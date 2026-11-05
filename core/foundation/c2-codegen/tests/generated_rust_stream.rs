use std::path::{Path, PathBuf};
use std::process::Command;

use c2_codegen::{ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;

const SERVER_STREAM: &str =
    include_str!("../../c2-contract/tests/fixtures/server-stream.contract.json");

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("C-Two repository root")
        .to_path_buf()
}

#[test]
fn generated_mixed_rust_service_runs_unary_and_server_stream_calls() {
    let repository = repository();
    let mut descriptor: serde_json::Value = serde_json::from_str(SERVER_STREAM).unwrap();
    let mut states = descriptor["methods"][1].clone();
    states["name"] = "states".into();
    states["parameters"] = serde_json::json!([]);
    states["bindings"]["input"] = serde_json::Value::Null;
    descriptor["methods"].as_array_mut().unwrap().push(states);
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(&serde_json::to_vec(&descriptor).unwrap())
            .unwrap();
    descriptor["fingerprints"] = serde_json::json!({
        "abi_hash": fingerprints.abi_hash(),
        "signature_hash": fingerprints.signature_hash(),
    });
    let release =
        ContractRelease::from_descriptor_json(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
    let artifacts =
        compile_contract_artifacts(&release, ContractCodegenTarget::Rust, &Default::default())
            .expect("stream code generation");
    let temp = tempfile::tempdir().unwrap();
    artifacts
        .publish_new_tree(&temp.path().join("generated"))
        .unwrap();
    std::fs::copy(
        repository.join("core/Cargo.lock"),
        temp.path().join("Cargo.lock"),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"generated-rust-stream-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nc-two = {{ version = \"0.1.0\", path = {:?} }}\nfastdb = \"=0.2.4\"\n",
            repository.join("sdk/rust")
        ),
    )
    .unwrap();
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), CONSUMER).unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["run", "--quiet", "--offline"])
        .current_dir(temp.path())
        .env("CARGO_TARGET_DIR", repository.join("core/target"))
        .output()
        .expect("run generated Rust stream consumer");
    assert!(
        output.status.success(),
        "generated stream consumer failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "OK");
}

const CONSUMER: &str = r##"
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload, View};

#[path = "../generated/rust/c_two_contract.rs"]
mod contract;

const SPEC: &[u8] = br#"{
  "schema": "fastdb.payload.v1",
  "profile": "record.v1",
  "entries": [{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],
  "components": []
}"#;

fn payload(value: &str) -> Payload {
    let spec = CompiledSpec::compile(SPEC).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_str(value).unwrap();
    builder.freeze().unwrap().execute(BuildPolicy::AllowStaging).unwrap().payload
}

fn text(payload: &Payload) -> String {
    payload.entry_view(0).unwrap().at(0).unwrap().acquire().unwrap().str().unwrap().to_string()
}

struct DomainIterator {
    items: VecDeque<Result<Payload, c_two::Error>>,
    panic_on_drop: bool,
    panic_after_items: bool,
}

impl Iterator for DomainIterator {
    type Item = Result<Payload, c_two::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.items.is_empty() && self.panic_after_items {
            self.panic_after_items = false;
            panic!("domain next failed");
        }
        self.items.pop_front()
    }
}

impl Drop for DomainIterator {
    fn drop(&mut self) {
        if self.panic_on_drop {
            panic!("domain cleanup failed");
        }
    }
}

struct Resource {
    escaped_inputs: Arc<Mutex<Vec<View>>>,
}

impl contract::Service for Resource {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        Ok(())
    }

    fn method_2_states(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<Payload, c_two::Error>> + Send>, c_two::Error> {
        Ok(Box::new(std::iter::once(Ok(payload("state")))))
    }

    fn method_1_echo(
        &self,
        input: &Payload,
    ) -> Result<Box<dyn Iterator<Item = Result<Payload, c_two::Error>> + Send>, c_two::Error> {
        self.escaped_inputs.lock().unwrap().push(input.entry_view(0).unwrap());
        let (items, panic_on_drop, panic_after_items) = match text(input).as_str() {
            "zero" => (VecDeque::new(), false, false),
            "multi" => (VecDeque::from([Ok(payload("one")), Ok(payload("two"))]), false, false),
            "error" => (VecDeque::from([
                Ok(payload("one")),
                Err(c_two::Error::Semantic(c_two::generated::C2Error::new(
                    c_two::generated::ErrorCode::WriteConflict,
                    "domain step failed",
                ))),
            ]), false, false),
            "cleanup" => (VecDeque::from([Ok(payload("one"))]), true, false),
            "next_panic" => (VecDeque::from([Ok(payload("one"))]), false, true),
            "factory_panic" => panic!("domain factory failed"),
            other => panic!("unexpected fixture input {other}"),
        };
        Ok(Box::new(DomainIterator { items, panic_on_drop, panic_after_items }))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = c_two::Runtime::new(c_two::RuntimeOptions {
        server_id: Some(format!("generated-stream-{}", std::process::id())),
        use_process_relay_anchor: false,
        ..c_two::RuntimeOptions::default()
    })?;
    let host = runtime.host(c_two::HostOptions::default().without_relay())?;
    let route = format!("generated-stream-route-{}", std::process::id());
    let escaped_inputs = Arc::new(Mutex::new(Vec::new()));
    let mut registration = host.register(contract::service_definition(
        &route,
        Resource { escaped_inputs: Arc::clone(&escaped_inputs) },
    )?)?;
    let client = contract::ContractClient::new(runtime.connect(
        contract::expected_route(&route)?,
        c_two::Connect::DirectIpc { address: runtime.server_address().unwrap() },
    )?)?;

    client.method_0_ping()?;

    let mut states = client.method_2_states()?;
    assert_eq!(text(&states.next().unwrap()?), "state");
    assert!(states.next().is_none());
    assert_eq!(states.close().complete_items, Some(1));
    let mut held_states = client.hold_method_2_states()?;
    let mut state = held_states.next().unwrap()?;
    assert_eq!(text(state.value().unwrap()), "state");
    state.release()?;
    assert!(held_states.next().is_none());
    assert_eq!(held_states.close().complete_items, Some(1));

    let mut zero = client.method_1_echo(&payload("zero"))?;
    assert!(zero.next().is_none());
    let receipt = zero.close();
    assert!(receipt.completed);
    assert!(matches!(receipt.terminal, Some(Ok(()))));
    assert_eq!(receipt.complete_items, Some(0));

    let mut multi = client.method_1_echo(&payload("multi"))?;
    assert_eq!(text(&multi.next().unwrap()?), "one");
    assert_eq!(text(&multi.next().unwrap()?), "two");
    assert!(multi.next().is_none());
    let receipt = multi.close();
    assert!(receipt.completed);
    assert!(matches!(receipt.terminal, Some(Ok(()))));
    assert_eq!(receipt.complete_items, Some(2));

    let mut held = client.hold_method_1_echo(&payload("multi"))?;
    let mut first = held.next().unwrap()?;
    let mut second = held.next().unwrap()?;
    assert_eq!(text(first.value().unwrap()), "one");
    assert_eq!(text(second.value().unwrap()), "two");
    first.release()?;
    assert!(first.value().is_none());
    assert_eq!(text(second.value().unwrap()), "two");
    second.release()?;
    assert!(held.next().is_none());
    assert_eq!(held.close().complete_items, Some(2));

    let mut error = client.method_1_echo(&payload("error"))?;
    assert_eq!(text(&error.next().unwrap()?), "one");
    match error.next().unwrap() {
        Err(c_two::Error::Semantic(semantic)) => {
            assert_eq!(semantic.code, c_two::generated::ErrorCode::WriteConflict)
        }
        _ => panic!("domain error was not preserved"),
    }
    assert!(error.next().is_none());
    assert_eq!(error.close().complete_items, Some(1));

    let mut cleanup = client.method_1_echo(&payload("cleanup"))?;
    assert_eq!(text(&cleanup.next().unwrap()?), "one");
    match cleanup.next().unwrap() {
        Err(c_two::Error::Semantic(semantic)) => {
            assert_eq!(semantic.code, c_two::generated::ErrorCode::ResourceFunctionExecuting)
        }
        _ => panic!("cleanup panic was not reported"),
    }
    assert!(cleanup.next().is_none());
    assert_eq!(cleanup.close().complete_items, Some(1));

    let mut next_panic = client.method_1_echo(&payload("next_panic"))?;
    assert_eq!(text(&next_panic.next().unwrap()?), "one");
    match next_panic.next().unwrap() {
        Err(c_two::Error::Semantic(semantic)) => {
            assert_eq!(semantic.code, c_two::generated::ErrorCode::ResourceFunctionExecuting)
        }
        _ => panic!("next panic was not reported"),
    }
    assert!(next_panic.next().is_none());
    assert_eq!(next_panic.close().complete_items, Some(1));

    match client.method_1_echo(&payload("factory_panic")) {
        Err(c_two::Error::Semantic(semantic)) => {
            assert_eq!(semantic.code, c_two::generated::ErrorCode::ResourceFunctionExecuting)
        }
        Ok(mut stream) => match stream.next().unwrap() {
            Err(c_two::Error::Semantic(semantic)) => {
                assert_eq!(semantic.code, c_two::generated::ErrorCode::ResourceFunctionExecuting)
            }
            _ => panic!("factory panic was not reported"),
        },
        Err(error) => panic!("factory panic changed error owner: {error}"),
    }

    let inputs = escaped_inputs.lock().unwrap();
    assert_eq!(inputs.len(), 7);
    for input in inputs.iter() {
        assert_eq!(input.kind().unwrap_err().symbol(), "VIEW_INVALIDATED");
    }
    drop(inputs);

    drop((zero, multi, held, error, cleanup, next_panic));
    registration.close()?;
    println!("OK");
    Ok(())
}
"##;
