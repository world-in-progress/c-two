//! Compile and execute generated clients through the public Rust SDK facade.
use crate::{ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;
use std::path::Path;
use std::process::Command;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

#[test]
fn generated_call_options_consumer_compiles() {
    consume_generated(false);
}

#[test]
fn generated_call_options_consumer() {
    consume_generated(true);
}

fn consume_generated(execute: bool) {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    descriptor["methods"][1]["access"] = "read".into();
    let mut get = descriptor["methods"][1].clone();
    get["name"] = "get".into();
    get["parameters"] = serde_json::json!([]);
    get["bindings"]["input"] = serde_json::Value::Null;
    let mut put = descriptor["methods"][1].clone();
    put["name"] = "put".into();
    put["return"] = serde_json::json!({"kind": "none"});
    put["bindings"]["output"] = serde_json::Value::Null;
    descriptor["methods"]
        .as_array_mut()
        .unwrap()
        .extend([get, put]);
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(&serde_json::to_vec(&descriptor).unwrap())
            .unwrap();
    descriptor["fingerprints"] = serde_json::json!({
        "abi_hash": fingerprints.abi_hash(),
        "signature_hash": fingerprints.signature_hash(),
    });
    let release =
        ContractRelease::from_descriptor_json(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
    // Options are a client view only. The descriptor retains business arguments.
    assert_eq!(release.descriptor().methods().len(), 4);
    assert_eq!(
        descriptor["methods"][1]["parameters"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let tree = compile_contract_artifacts(
        &release,
        ContractCodegenTarget::Rust,
        &ContractCodegenOptions::default(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    tree.publish_new_tree(&temp.path().join("generated"))
        .unwrap();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"call-options-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nc-two = {{ version = \"0.1.0\", path = {:?} }}\nfastdb = \"=0.2.1\"\n",
            repository.join("sdk/rust")
        ),
    ).unwrap();
    std::fs::copy(
        // CI fetches the Core workspace lock before these offline consumers.
        // Seed the same dependency versions instead of the SDK's separate lock.
        repository.join("core/Cargo.lock"),
        temp.path().join("Cargo.lock"),
    )
    .unwrap();
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), CONSUMER).unwrap();
    // Use a separate target directory: the outer Cargo test may hold its lock.
    let output = Command::new(env!("CARGO"))
        .args([if execute { "run" } else { "build" }, "--offline"])
        .current_dir(temp.path())
        .env(
            "CARGO_TARGET_DIR",
            repository.join("sdk/rust/target/call-options-consumer"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "compiled generated consumer failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    if execute {
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "OK call options"
        );
    }
}

const CONSUMER: &str = r##"
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use c_two::{CallOptions, CallTimeout, CallExecutionLimitsOverrides, ConfigSources,
    Connect, Runtime, RuntimeOptions, HostOptions};
use c_two::generated::ErrorCode;
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload, View};
#[path = "../generated/rust/c_two_contract.rs"]
mod contract;

const SPEC: &[u8] = br#"{
  "schema":"fastdb.payload.v1", "profile":"record.v1",
  "entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],
  "components":[]
}"#;
fn payload(value: &str) -> Payload {
    let spec = CompiledSpec::compile(SPEC).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_str(value).unwrap();
    builder.freeze().unwrap().execute(BuildPolicy::AllowStaging).unwrap().payload
}
fn finite(ms: u64) -> CallOptions {
    CallOptions::with_timeout(CallTimeout::After(Duration::from_millis(ms)))
}
fn semantic<T>(result: Result<T, c_two::Error>, code: ErrorCode) {
    match result {
        Err(c_two::Error::Semantic(error)) => assert_eq!(error.code, code),
        Err(error) => panic!("wrong error: {error}"),
        Ok(_) => panic!("call unexpectedly succeeded"),
    }
}
#[derive(Default)]
struct State {
    calls: AtomicUsize,
    blocked: Mutex<usize>,
    ready: Condvar,
    released: Mutex<bool>,
    release: Condvar,
    escaped: Mutex<Vec<View>>,
}
impl State {
    fn wait_entered(&self, expected: usize) {
        let count = self.blocked.lock().unwrap();
        let (count, _) = self.ready.wait_timeout_while(count, Duration::from_secs(5),
            |count| *count < expected).unwrap();
        assert_eq!(*count, expected, "callbacks did not enter");
    }
    fn unblock(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
struct Echo(Arc<State>);
impl contract::Service for Echo {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn method_1_echo(&self, input: &Payload) -> Result<Payload, c_two::Error> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.escaped.lock().unwrap().push(input.entry_view(0).unwrap());
        if input.entry_view(0).unwrap().at(0).unwrap().acquire().unwrap().str().unwrap()
            == "blocked" {
            {
                let mut count = self.0.blocked.lock().unwrap();
                *count += 1;
                self.0.ready.notify_all();
            }
            let released = self.0.released.lock().unwrap();
            let (released, _) = self.0.release.wait_timeout_while(released,
                Duration::from_secs(10), |released| !*released).unwrap();
            assert!(*released, "controller did not release callback");
        }
        Ok(input.clone())
    }
    fn method_2_get(&self) -> Result<Payload, c_two::Error> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        Ok(payload("get"))
    }
    fn method_3_put(&self, _input: &Payload) -> Result<(), c_two::Error> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn runtime(label: &str, slots: u64, bytes: u64) -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(format!("options-{label}-{}", std::process::id())),
        use_process_relay_anchor: false,
        ..Default::default()
    }).unwrap();
    runtime.set_call_execution_limits_with_sources(CallExecutionLimitsOverrides {
        max_outstanding_calls: Some(slots), retained_input_budget_bytes: Some(bytes),
    }, ConfigSources::empty()).unwrap();
    runtime
}
fn connect(runtime: &Runtime, route: &str) -> contract::ContractClient {
    contract::ContractClient::new(runtime.connect(contract::expected_route(route).unwrap(),
        Connect::DirectIpc { address: runtime.server_address().unwrap() }).unwrap()).unwrap()
}
fn assert_idle(runtime: &Runtime) {
    let snapshot = runtime.call_execution_snapshot().unwrap();
    assert_eq!(snapshot.used_operations, 0);
    assert_eq!(snapshot.used_retained_bytes, 0);
}
fn main() {
    // Zero slots and zero timeout must win before even inspecting an invalid
    // FastDB input. If the serializer ran, this would be a FastDB stale error.
    for (label, slots, options, code) in [
        ("zero-slots", 0, finite(5000), ErrorCode::CallCapacityExceeded),
        ("expired", 8, finite(0), ErrorCode::CallDeadlineExceeded),
    ] {
        let runtime = runtime(label, slots, 1 << 20);
        let host = runtime.host(HostOptions::default().without_relay()).unwrap();
        let state = Arc::new(State::default());
        let mut registration = host.register(contract::service_definition(label,
            Echo(state.clone())).unwrap()).unwrap();
        let client = connect(&runtime, label);
        assert_eq!(client.call_options(), CallOptions::new());
        let view = client.with_call_options(options);
        assert_eq!(view.expected_route(), client.expected_route());
        let stale = payload("stale");
        stale.invalidate().unwrap();
        semantic(view.method_1_echo(&stale), code);
        semantic(view.hold_method_1_echo(&stale), code);
        semantic(view.method_0_ping(), code);
        semantic(view.method_2_get(), code);
        semantic(view.hold_method_2_get(), code);
        semantic(view.method_3_put(&stale), code);
        assert_eq!(state.calls.load(Ordering::SeqCst), 0);
        assert_idle(&runtime);
        // Explicit unlimited ignores finite capacity; inherited direct IPC
        // remains unlimited as well. Both keep business signatures unchanged.
        let source = payload("unlimited");
        view.with_call_options(CallOptions::with_timeout(CallTimeout::Unlimited))
            .method_3_put(&source).unwrap();
        client.method_0_ping().unwrap();
        assert_idle(&runtime);
        registration.close().unwrap();
    }
    // Positive bytes cannot pass a zero retained-input budget. Empty input
    // still works and can hold a portable response with the same policy.
    {
        let runtime = runtime("zero-bytes", 8, 0);
        let host = runtime.host(HostOptions::default().without_relay()).unwrap();
        let state = Arc::new(State::default());
        let mut registration = host.register(contract::service_definition("zero-bytes",
            Echo(state.clone())).unwrap()).unwrap();
        let client = connect(&runtime, "zero-bytes").with_call_options(finite(5000));
        semantic(client.method_1_echo(&payload("bytes")), ErrorCode::CallCapacityExceeded);
        semantic(client.hold_method_1_echo(&payload("bytes")), ErrorCode::CallCapacityExceeded);
        assert_eq!(state.calls.load(Ordering::SeqCst), 0);
        client.method_0_ping().unwrap();
        let mut held = client.hold_method_2_get().unwrap();
        let checked = held.value().unwrap().entry_view(0).unwrap();
        held.release().unwrap();
        assert_eq!(checked.acquire().unwrap_err().symbol(), "VIEW_INVALIDATED");
        assert_idle(&runtime);
        registration.close().unwrap();
    }
    // Concurrent views share one connection acquisition, but each call retains
    // its own deadline. Expiring one caller cannot cancel the other view/hold.
    {
        let runtime = runtime("views", 8, 1 << 20);
        let host = runtime.host(HostOptions::default().without_relay()).unwrap();
        let state = Arc::new(State::default());
        let mut registration = host.register(contract::service_definition("views",
            Echo(state.clone())).unwrap().with_concurrency(
                c_two::ServiceConcurrencyMode::ReadParallel, Some(8), Some(4)).unwrap()).unwrap();
        let client = connect(&runtime, "views");
        let paths = runtime.path_counters();
        let short = client.with_call_options(finite(750));
        let long = client.with_call_options(finite(5000));
        assert_eq!(runtime.path_counters(), paths, "view reconnected");
        assert_eq!(client.call_options(), CallOptions::new());
        let short_call = std::thread::spawn(move || short.method_1_echo(&payload("blocked")));
        state.wait_entered(1);
        let long_call = std::thread::spawn(move || long.hold_method_1_echo(&payload("blocked")));
        state.wait_entered(2);
        semantic(short_call.join().unwrap(), ErrorCode::CallDeadlineExceeded);
        drop(client);
        state.unblock();
        let mut held = long_call.join().unwrap().unwrap();
        let checked = held.value().unwrap().entry_view(0).unwrap();
        assert_eq!(checked.at(0).unwrap().acquire().unwrap().str().unwrap(), "blocked");
        held.release().unwrap();
        assert_eq!(checked.acquire().unwrap_err().symbol(), "VIEW_INVALIDATED");
        registration.close().unwrap(); // native drain fences escaped input checks
        for view in state.escaped.lock().unwrap().iter() {
            assert_eq!(view.acquire().unwrap_err().symbol(), "VIEW_INVALIDATED");
        }
        // Route close drains callbacks; allow the expired client's native
        // continuation to observe its terminal reply before checking charges.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while runtime.call_execution_snapshot().unwrap().used_operations != 0 {
            assert!(std::time::Instant::now() < deadline, "native input charges did not drain");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_idle(&runtime);
    }
    println!("OK call options");
}
"##;
