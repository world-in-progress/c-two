use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use c2_codegen::{
    ContractArtifactSet, ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts,
};
use c2_contract::ContractRelease;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const GRAPH_SPEC: &str = include_str!(
    "../../../../../fastdb/tests/golden/payload/v1/binary/spec/graph-all-values.source.json"
);
const RECORD_SPEC: &str = include_str!(
    "../../../../../fastdb/tests/golden/payload/v1/spec/valid/record-all-types.source.json"
);
const PORTABLE_INTEROP_RUST: &str =
    include_str!("../../../../sdk/python/tests/fixtures/portable_interop_rust.rs");

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("C-Two repository root")
        .to_path_buf()
}

fn generate(descriptor: &[u8]) -> ContractArtifactSet {
    let release = ContractRelease::from_descriptor_json(descriptor).expect("admitted descriptor");
    compile_contract_artifacts(
        &release,
        ContractCodegenTarget::Rust,
        &ContractCodegenOptions::default(),
    )
    .expect("generated Rust artifact set")
}

fn generated_source(set: &ContractArtifactSet) -> &str {
    std::str::from_utf8(
        set.get("rust/c_two_contract.rs")
            .expect("generated Rust contract module")
            .bytes(),
    )
    .expect("generated Rust source is UTF-8")
}

fn no_payload_descriptor() -> Vec<u8> {
    let mut descriptor: serde_json::Value =
        serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
    descriptor["methods"]
        .as_array_mut()
        .expect("method array")
        .truncate(1);
    refresh_fingerprints(&mut descriptor);
    serde_json::to_vec(&descriptor).expect("descriptor bytes")
}

fn graph_descriptor() -> Vec<u8> {
    let mut descriptor: serde_json::Value =
        serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
    let graph: serde_json::Value = serde_json::from_str(GRAPH_SPEC).expect("graph spec");
    descriptor["methods"][1]["bindings"]["input"]["spec"] = graph.clone();
    descriptor["methods"][1]["bindings"]["output"]["spec"] = graph;
    refresh_fingerprints(&mut descriptor);
    serde_json::to_vec(&descriptor).expect("descriptor bytes")
}

fn alternate_descriptor() -> Vec<u8> {
    let mut descriptor: serde_json::Value =
        serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
    descriptor["crm"]["version"] = serde_json::Value::String("0.2.0".to_string());
    refresh_fingerprints(&mut descriptor);
    serde_json::to_vec(&descriptor).expect("descriptor bytes")
}

fn portable_interop_descriptor() -> Vec<u8> {
    let mut descriptor: serde_json::Value =
        serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
    descriptor["crm"]["namespace"] = serde_json::Value::String("test.portable-interop".to_string());
    descriptor["crm"]["name"] = serde_json::Value::String("PortableInterop".to_string());

    let ping = descriptor["methods"][0].clone();
    let mut graph = descriptor["methods"][1].clone();
    graph["name"] = serde_json::Value::String("graph_roundtrip".to_string());
    let graph_spec: serde_json::Value = serde_json::from_str(GRAPH_SPEC).expect("graph spec");
    graph["bindings"]["input"]["spec"] = graph_spec.clone();
    graph["bindings"]["output"]["spec"] = graph_spec;

    let mut record = descriptor["methods"][1].clone();
    record["name"] = serde_json::Value::String("record_roundtrip".to_string());
    let record_spec: serde_json::Value = serde_json::from_str(RECORD_SPEC).expect("record spec");
    record["bindings"]["input"]["spec"] = record_spec.clone();
    record["bindings"]["output"]["spec"] = record_spec;
    descriptor["methods"] = serde_json::Value::Array(vec![graph, ping, record]);
    refresh_fingerprints(&mut descriptor);
    serde_json::to_vec(&descriptor).expect("portable interop descriptor bytes")
}

fn refresh_fingerprints(descriptor: &mut serde_json::Value) {
    let fingerprints = c2_contract::derive_contract_fingerprints_json(
        serde_json::to_vec(descriptor)
            .expect("fingerprint input")
            .as_slice(),
    )
    .expect("derived fingerprints");
    descriptor["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
}

#[test]
fn rust_generation_is_transport_neutral_for_no_payload_record_and_object_graph_contracts() {
    for (label, descriptor) in [
        ("no-payload", no_payload_descriptor()),
        ("record", DESCRIPTOR.as_bytes().to_vec()),
        ("object-graph", graph_descriptor()),
    ] {
        let set = generate(&descriptor);
        let source = generated_source(&set);
        for required in [
            "pub const CONTRACT_DESCRIPTOR_JSON",
            "include_str!(\"../metadata/contract.json\")",
            "ContractRelease::from_descriptor_json",
            "pub struct ContractClient",
            "client: c_two::Client",
            "c_two::generated::EncodedClient",
            "pub trait Service",
            "c_two::generated::EncodedService",
            "pub fn service_definition",
            "c_two::ServiceDefinition",
            ".expected_route(",
        ] {
            assert!(
                source.contains(required),
                "{label} generated source is missing {required:?}"
            );
        }
        for forbidden in [
            "c2_ipc",
            "c2_http",
            "c2_server",
            "SyncClient",
            "RouteBinding",
            "ClientPool",
            "HttpClientPool",
            "relay_url",
            "ipc_address",
            "let expected = c_two::ExpectedRouteContract {",
            "Ok(c_two::ExpectedRouteContract {",
        ] {
            assert!(
                !source.contains(forbidden),
                "{label} generated source leaked {forbidden:?}"
            );
        }
    }

    let portable = generated_source(&generate(DESCRIPTOR.as_bytes())).to_string();
    for phase in [
        "ClientInputSerializing",
        "ClientOutputFromBuffer",
        "ClientOutputDeserializing",
        "ResourceInputFromBuffer",
        "ResourceInputDeserializing",
        "ResourceFunctionExecuting",
        "ResourceOutputSerializing",
    ] {
        assert!(
            portable.contains(phase),
            "portable generated source omitted adapter phase {phase}"
        );
    }
}

#[test]
fn generated_rust_consumer_needs_only_versioned_c_two_and_fastdb_dependencies() {
    let repository = repository();
    let fastdb = repository
        .parent()
        .expect("workspace parent")
        .join("fastdb");
    if !fastdb.join("bindings/rust/fastdb/Cargo.toml").is_file() {
        eprintln!("skipping generated Rust consumer because FastDB source is unavailable");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary consumer");
    generate(DESCRIPTOR.as_bytes())
        .publish_new_tree(&temp.path().join("generated"))
        .expect("publish generated contract");
    generate(&alternate_descriptor())
        .publish_new_tree(&temp.path().join("generated-wrong"))
        .expect("publish alternate generated contract");
    write_consumer_manifest(temp.path(), &repository, &fastdb);
    std::fs::create_dir(temp.path().join("src")).expect("consumer source directory");
    std::fs::write(temp.path().join("src/main.rs"), valid_consumer_source())
        .expect("consumer source");

    let output = cargo(temp.path(), &["run", "--quiet", "--offline"]);
    assert!(
        output.status.success(),
        "generated Rust consumer failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "OK");
}

#[test]
fn generated_service_trait_rejects_the_wrong_payload_shape() {
    let repository = repository();
    let fastdb = repository
        .parent()
        .expect("workspace parent")
        .join("fastdb");
    if !fastdb.join("bindings/rust/fastdb/Cargo.toml").is_file() {
        eprintln!("skipping generated Rust compile failure because FastDB source is unavailable");
        return;
    }

    let temp = tempfile::tempdir().expect("temporary compile-fail consumer");
    generate(DESCRIPTOR.as_bytes())
        .publish_new_tree(&temp.path().join("generated"))
        .expect("publish generated contract");
    write_consumer_manifest(temp.path(), &repository, &fastdb);
    std::fs::create_dir(temp.path().join("src")).expect("consumer source directory");
    std::fs::write(
        temp.path().join("src/lib.rs"),
        r#"
#[path = "../generated/rust/c_two_contract.rs"]
mod contract;

struct WrongShape;

impl contract::Service for WrongShape {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        Ok(())
    }

    fn method_1_echo(&self, _input: &[u8]) -> Result<Vec<u8>, c_two::Error> {
        Ok(Vec::new())
    }
}
"#,
    )
    .expect("compile-fail source");

    let output = cargo(temp.path(), &["check", "--quiet", "--offline"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "wrong service shape compiled");
    assert!(
        stderr.contains("method `method_1_echo` has an incompatible type for trait"),
        "unexpected compile failure:\n{stderr}"
    );
}

#[test]
fn portable_interop_fixture_compiles_with_only_the_rust_sdk_and_fastdb() {
    let repository = repository();
    let fastdb = repository
        .parent()
        .expect("workspace parent")
        .join("fastdb");
    if !fastdb.join("bindings/rust/fastdb/Cargo.toml").is_file() {
        eprintln!("skipping portable interop fixture because FastDB source is unavailable");
        return;
    }

    for forbidden in [
        "c2_config",
        "c2_contract",
        "c2_ipc",
        "c2_mem",
        "c2_server",
        "SyncClient",
        "RouteBuildSpec",
    ] {
        assert!(
            !PORTABLE_INTEROP_RUST.contains(forbidden),
            "portable interop fixture still imports {forbidden}"
        );
    }

    let temp = tempfile::tempdir().expect("temporary portable fixture");
    generate(&portable_interop_descriptor())
        .publish_new_tree(&temp.path().join("generated"))
        .expect("publish portable interop contract");
    write_consumer_manifest(temp.path(), &repository, &fastdb);
    std::fs::create_dir_all(temp.path().join("src")).expect("fixture source directory");
    std::fs::create_dir_all(temp.path().join("fixtures")).expect("fixture data directory");
    std::fs::write(temp.path().join("src/main.rs"), PORTABLE_INTEROP_RUST).expect("fixture source");
    std::fs::write(
        temp.path().join("fixtures/record-all-types.source.json"),
        RECORD_SPEC,
    )
    .expect("record fixture");
    std::fs::write(
        temp.path().join("fixtures/graph-all-values.source.json"),
        GRAPH_SPEC,
    )
    .expect("graph fixture");

    let output = cargo(temp.path(), &["check", "--quiet", "--offline"]);
    assert!(
        output.status.success(),
        "portable interop fixture failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn write_consumer_manifest(root: &Path, repository: &Path, fastdb: &Path) {
    let manifest = format!(
        r#"[package]
name = "generated-rust-sdk-consumer"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ version = "0.1.0", path = {c_two:?} }}
fastdb = {{ version = "0.1.22", path = {fastdb:?} }}
"#,
        c_two = repository.join("sdk/rust"),
        fastdb = fastdb.join("bindings/rust/fastdb"),
    );
    std::fs::write(root.join("Cargo.toml"), manifest).expect("consumer manifest");
}

fn cargo(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO"))
        .args(arguments)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", repository().join("core/target"))
        .output()
        .expect("nested cargo")
}

fn valid_consumer_source() -> &'static str {
    r##"
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload, View};

#[path = "../generated/rust/c_two_contract.rs"]
mod contract;
#[path = "../generated-wrong/rust/c_two_contract.rs"]
mod wrong_contract;

const VALUE_SPEC: &[u8] = br#"{
  "schema": "fastdb.payload.v1",
  "profile": "record.v1",
  "entries": [
    {
      "id": "value",
      "cardinality": "one",
      "type": {"kind": "str", "nullable": true}
    }
  ],
  "components": []
}"#;

const WRONG_SPEC: &[u8] = br#"{
  "schema": "fastdb.payload.v1",
  "profile": "record.v1",
  "entries": [
    {
      "id": "value",
      "cardinality": "one",
      "type": {"kind": "u8", "nullable": false}
    }
  ],
  "components": []
}"#;

struct Echo {
    escaped_inputs: Arc<Mutex<Vec<View>>>,
    calls: AtomicUsize,
}

impl contract::Service for Echo {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        Ok(())
    }

    fn method_1_echo(&self, input: &Payload) -> Result<Payload, c_two::Error> {
        self.escaped_inputs
            .lock()
            .expect("escaped input lock")
            .push(input.entry_view(0).expect("input view"));
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 | 1 => Ok(input.clone()),
            2 => Err(c_two::Error::Semantic(
                c_two::generated::C2Error::new(
                    c_two::generated::ErrorCode::WriteConflict,
                    "service fixture conflict",
                )
                .with_details(BTreeMap::from([(
                    "fixture".to_string(),
                    "generated-rust".to_string(),
                )])),
            )),
            3 => Ok(wrong_payload()),
            4 => panic!("service fixture panic"),
            call => panic!("unexpected service fixture call {call}"),
        }
    }
}

struct WrongEcho;

impl wrong_contract::Service for WrongEcho {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        Ok(())
    }

    fn method_1_echo(&self, input: &Payload) -> Result<Payload, c_two::Error> {
        Ok(input.clone())
    }
}

fn payload(value: &str) -> Result<Payload, Box<dyn std::error::Error>> {
    let spec = CompiledSpec::compile(VALUE_SPEC)?;
    let mut builder = Builder::create(&spec)?;
    builder.entry_begin(0, 1)?.value_str(value)?;
    Ok(builder.freeze()?.execute(BuildPolicy::AllowStaging)?.payload)
}

fn wrong_payload() -> Payload {
    let spec = CompiledSpec::compile(WRONG_SPEC).expect("compile wrong spec");
    let mut builder = Builder::create(&spec).expect("create wrong builder");
    builder
        .entry_begin(0, 1)
        .expect("wrong entry")
        .value_u8(1)
        .expect("wrong value");
    builder
        .freeze()
        .expect("freeze wrong payload")
        .execute(BuildPolicy::AllowStaging)
        .expect("build wrong payload")
        .payload
}

fn require_latest_input_invalidated(escaped_inputs: &Arc<Mutex<Vec<View>>>, expected_len: usize) {
    let inputs = escaped_inputs.lock().expect("escaped input lock");
    assert_eq!(inputs.len(), expected_len);
    let error = inputs
        .last()
        .expect("latest input")
        .kind()
        .expect_err("borrowed input escaped callback validity");
    assert_eq!(error.symbol(), "VIEW_INVALIDATED");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = c_two::Runtime::new(c_two::RuntimeOptions {
        server_id: Some(format!("generated-consumer-{}", std::process::id())),
        use_process_relay_anchor: false,
        ..c_two::RuntimeOptions::default()
    })?;
    let host = runtime.host(c_two::HostOptions::default().without_relay())?;
    let route = format!("generated-echo-{}", std::process::id());
    let wrong_route = format!("generated-wrong-{}", std::process::id());
    let escaped_inputs = Arc::new(Mutex::new(Vec::new()));
    let mut registration = host.register(contract::service_definition(
        &route,
        Echo {
            escaped_inputs: Arc::clone(&escaped_inputs),
            calls: AtomicUsize::new(0),
        },
    )?)?;
    let mut wrong_registration =
        host.register(wrong_contract::service_definition(&wrong_route, WrongEcho)?)?;
    let address = runtime.server_address().ok_or("missing server address")?;

    let malformed = runtime.connect(
        contract::expected_route(&route)?,
        c_two::Connect::DirectIpc {
            address: address.clone(),
        },
    )?;
    let malformed_error = c_two::generated::EncodedClient::call_owned(
        &malformed,
        "ping",
        b"unexpected",
    )
    .expect_err("no-payload input accepted bytes");
    let c_two::Error::Semantic(malformed_error) = malformed_error else {
        panic!("no-payload mismatch was not semantic");
    };
    assert_eq!(
        malformed_error.code,
        c_two::generated::ErrorCode::ResourceInputDeserializing
    );

    let core_client = runtime.connect(
        contract::expected_route(&route)?,
        c_two::Connect::DirectIpc {
            address: address.clone(),
        },
    )?;
    let client = contract::ContractClient::new(core_client)?;
    client.method_0_ping()?;
    let source = payload("owned")?;
    let response = client.method_1_echo(&source)?;
    require_latest_input_invalidated(&escaped_inputs, 1);
    assert_eq!(
        response.entry_view(0)?.at(0)?.acquire()?.str()?,
        "owned"
    );
    let mut held = client.hold_method_1_echo(&source)?;
    require_latest_input_invalidated(&escaped_inputs, 2);
    assert_eq!(
        held.value()
            .ok_or("held payload released early")?
            .entry_view(0)?
            .at(0)?
            .acquire()?
            .str()?,
        "owned"
    );
    held.release()?;

    let semantic = match client.method_1_echo(&source) {
        Err(c_two::Error::Semantic(error)) => error,
        Err(error) => panic!("service semantic error changed owner: {error}"),
        Ok(_) => panic!("service semantic error unexpectedly succeeded"),
    };
    require_latest_input_invalidated(&escaped_inputs, 3);
    assert_eq!(
        semantic.code,
        c_two::generated::ErrorCode::WriteConflict
    );
    assert_eq!(semantic.message, "service fixture conflict");
    assert_eq!(
        semantic.details,
        BTreeMap::from([(
            "fixture".to_string(),
            "generated-rust".to_string(),
        )])
    );

    let serialization = match client.method_1_echo(&source) {
        Err(c_two::Error::Semantic(error)) => error,
        Err(error) => panic!("FastDB serialization error changed owner: {error}"),
        Ok(_) => panic!("wrong output payload unexpectedly succeeded"),
    };
    require_latest_input_invalidated(&escaped_inputs, 4);
    assert_eq!(
        serialization.code,
        c_two::generated::ErrorCode::ResourceOutputSerializing
    );
    assert_eq!(
        serialization.details.get("cause_owner").map(String::as_str),
        Some("fastdb")
    );
    assert_eq!(
        serialization
            .details
            .get("fastdb_symbol")
            .map(String::as_str),
        Some("DIGEST_MISMATCH")
    );

    let unwind = match client.method_1_echo(&source) {
        Err(c_two::Error::Semantic(error)) => error,
        Err(error) => panic!("service unwind changed owner: {error}"),
        Ok(_) => panic!("service unwind unexpectedly succeeded"),
    };
    require_latest_input_invalidated(&escaped_inputs, 5);
    assert_eq!(
        unwind.code,
        c_two::generated::ErrorCode::ResourceFunctionExecuting
    );
    assert!(unwind.message.contains("panicked"));

    let wrong_core_client = runtime.connect(
        wrong_contract::expected_route(&wrong_route)?,
        c_two::Connect::DirectIpc { address },
    )?;
    assert!(
        contract::ContractClient::new(wrong_core_client).is_err(),
        "generated client accepted a different contract release"
    );

    wrong_registration.close()?;
    registration.close()?;
    println!("OK");
    Ok(())
}
"##
}
