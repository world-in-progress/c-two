//! Topic-aware contract compilation (ADR 0005): mixed method+topic
//! contracts, topics-only contracts, and the byte-stability of topicless
//! contracts (whose artifact hashes stay pinned by `unary_golden.rs`).

use c2_codegen::{ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;
use serde_json::{Value, json};

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const STREAM_DESCRIPTOR: &str =
    include_str!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

fn refresh_fingerprints(descriptor: &mut Value) {
    let fingerprints = c2_contract::derive_contract_fingerprints_json(
        serde_json::to_vec(descriptor)
            .expect("fingerprint input")
            .as_slice(),
    )
    .expect("derived fingerprints");
    descriptor["fingerprints"]["abi_hash"] = Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        Value::String(fingerprints.signature_hash().to_string());
}

fn descriptor_with_topics(topics: &[&str], empty_methods: bool) -> Vec<u8> {
    let mut descriptor: Value = serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
    let sample_binding = descriptor["methods"][1]["bindings"]["output"].clone();
    if empty_methods {
        descriptor["methods"] = Value::Array(Vec::new());
    }
    descriptor["topics"] = Value::Array(
        topics
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "output": {"kind": "payload"},
                    "binding": sample_binding.clone(),
                })
            })
            .collect(),
    );
    refresh_fingerprints(&mut descriptor);
    serde_json::to_vec(&descriptor).expect("descriptor bytes")
}

fn mixed_descriptor(topics: &[&str]) -> Vec<u8> {
    descriptor_with_topics(topics, false)
}

fn generate(descriptor: &[u8], target: ContractCodegenTarget) -> c2_codegen::ContractArtifactSet {
    let release = ContractRelease::from_descriptor_json(descriptor).expect("admitted descriptor");
    compile_contract_artifacts(&release, target, &ContractCodegenOptions::default())
        .expect("generated artifact set")
}

fn source<'a>(set: &'a c2_codegen::ContractArtifactSet, target: ContractCodegenTarget) -> &'a str {
    let path = match target {
        ContractCodegenTarget::Rust => "rust/c_two_contract.rs",
        ContractCodegenTarget::Python => "python/c_two_contract.py",
        ContractCodegenTarget::TypeScript => "typescript/c_two_contract.ts",
    };
    std::str::from_utf8(set.get(path).expect("target module").bytes()).expect("UTF-8 source")
}

fn output_digest(descriptor: &[u8]) -> String {
    let release = ContractRelease::from_descriptor_json(descriptor).expect("admitted descriptor");
    let nested = release.descriptor().topics()[0].output();
    let compiled = fastdb::CompiledSpec::compile(nested.canonical_json().as_bytes())
        .expect("topic FastDB specification compiles");
    let digest = compiled.sha256().expect("FastDB digest");
    c2_contract::lower_hex(&digest)
}

#[test]
fn mixed_contract_generates_typed_rust_topic_surface() {
    let set = generate(
        &mixed_descriptor(&["state", "ticks"]),
        ContractCodegenTarget::Rust,
    );
    let rust = source(&set, ContractCodegenTarget::Rust);
    let state_digest = output_digest(&mixed_descriptor(&["state", "ticks"]));
    for required in [
        "pub const TOPICS: &[TopicBinding] = &[",
        &format!("TopicBinding {{ index: 0, name: \"state\", output_sha256: \"{state_digest}\" }}"),
        "pub fn subscribe_topic_0_state(",
        "pub fn subscribe_topic_1_ticks(",
        ") -> Result<c_two::Subscription<fastdb::Payload>, c_two::Error> {",
        "self.client.subscribe(\"state\", options)?;",
        "c_two::Subscription::from_core(",
        "pub struct TopicService {",
        "pub fn topic_service<S: Service>(",
        "pub fn topic_0_state_capacity(mut self, capacity: usize)",
        "self.definition.with_topic_capacity(\"state\", capacity)?;",
        "pub fn initial_topic_0_state(mut self, value: &fastdb::Payload)",
        "let payload = encode_topic_0_state_value(value)?;",
        "self.definition.with_initial_topic_bytes(\"state\", payload)?;",
        "pub fn build(self) -> c_two::ServiceDefinition {",
        "pub struct RegistrationTopics<'a> {",
        "pub fn registration_topics(registration: &c_two::Registration)",
        "pub fn topic_0_state(&self) -> Result<c_two::Publisher<fastdb::Payload>, c_two::Error> {",
        "self.registration.publisher(\"state\")?,",
        "pub fn encode_topic_0_state_value(",
        "pub fn decode_topic_0_state_value(",
        "pub fn decode_topic_0_state_value_held(",
        // Unary method surface stays intact next to the topic surface.
        "pub fn method_0_ping(&self) -> Result<(), c_two::Error> {",
    ] {
        assert!(
            rust.contains(required),
            "mixed Rust contract source is missing {required:?}"
        );
    }
    // The topic output shares the digest-named payload module family.
    assert!(rust.contains(&format!(
        "#[path = \"payloads/{state_digest}/fastdb_payload_{state_digest}.rs\"]"
    )));
}

#[test]
fn topics_only_contract_compiles_an_empty_method_surface_with_topics() {
    let solo = descriptor_with_topics(&["telemetry"], true);
    let set = generate(&solo, ContractCodegenTarget::Rust);
    let rust = source(&set, ContractCodegenTarget::Rust);
    assert!(rust.contains("pub const METHODS: &[MethodBinding] = &[\n];"));
    assert!(rust.contains(
        "pub const TOPICS: &[TopicBinding] = &[\n    TopicBinding { index: 0, name: \"telemetry\""
    ));
    assert!(rust.contains("pub trait Service: Send + Sync + 'static {\n}"));
    assert!(rust.contains("pub fn subscribe_topic_0_telemetry("));
    assert!(rust.contains("pub fn topic_service<S: Service>("));
    assert!(rust.contains("pub fn initial_topic_0_telemetry("));
    // An empty-methods ServiceDefinition still registers through the streamless
    // constructor with zero MethodDefinition entries.
    assert!(rust.contains("c_two::ServiceDefinition::new("));
}

#[test]
fn python_target_generates_typed_subscription_client_methods() {
    let set = generate(&mixed_descriptor(&["state"]), ContractCodegenTarget::Python);
    let python = source(&set, ContractCodegenTarget::Python);
    let state_digest = output_digest(&mixed_descriptor(&["state"]));
    for required in [
        // A unary topic contract imports only the subscription surface.
        "from c_two import HeldResult, Subscription\n",
        "class TopicBinding:",
        "    output_sha256: str",
        "TOPICS = (\n    TopicBinding(0, \"state\", \"%s\"),\n)",
        // The bound-client protocol carries the underlying subscribe entry.
        "    def subscribe(self, topic: str, *, start: str = \"latest\", on_lag: str = \"skip\", hold: bool = False) -> Any: ...\n",
        // Typed client methods forward through the projected bound client.
        "    def subscribe_topic_0_state(self, *, start: str = \"latest\", on_lag: str = \"skip\") -> Subscription[Payload]:\n\
         \x20       return _project_call(lambda: self._bound_client.subscribe(\"state\", start=start, on_lag=on_lag, hold=False))\n",
        "    def hold_subscribe_topic_0_state(self, *, start: str = \"latest\", on_lag: str = \"skip\") -> Subscription[HeldResult[Payload]]:\n\
         \x20       return _project_call(lambda: self._bound_client.subscribe(\"state\", start=start, on_lag=on_lag, hold=True))\n",
        // Module-level topic codecs mirror the method output direction.
        &format!(
            "def encode_topic_0_state_value(payload: Payload) -> bytes:\n\
             \x20   try:\n\
             \x20       payload.require_spec_sha256(_PAYLOAD_{state_digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20       return payload.binary_bytes()\n\
             \x20   except BaseException as exc:\n\
             \x20       raise _serialization_error(ErrorCode.ResourceOutputSerializing, exc) from exc\n"
        ),
        &format!(
            "def decode_topic_0_state_value(data: bytes) -> Payload:\n\
             \x20   try:\n\
             \x20       spec = _PAYLOAD_{state_digest}.compile_spec()\n\
             \x20   except BaseException as exc:\n\
             \x20       raise _serialization_error(ErrorCode.ClientOutputFromBuffer, exc) from exc\n"
        ),
        &format!(
            "\x20   try:\n\
             \x20       payload.require_spec_sha256(_PAYLOAD_{state_digest}.PAYLOAD_SHA256_BYTES)\n\
             \x20   except BaseException as exc:\n\
             \x20       _release_failed_payload(payload)\n\
             \x20       raise _serialization_error(ErrorCode.ClientOutputDeserializing, exc) from exc\n\
             \x20   return payload\n"
        ),
        // The unary client surface stays intact next to the topic surface.
        "    def method_1_echo(self, payload: Payload, *, call_id: str | None = None) -> Payload:",
    ] {
        assert!(
            python.contains(required.replace("%s", &state_digest).as_str()),
            "generated Python contract is missing {required:?}"
        );
    }
    // No authoring-side CRM class is generated; contract authoring stays
    // with hand-written CRM classes.
    for forbidden in ["ContractCrm", "@cc.topic", "import c_two as cc"] {
        assert!(
            !python.contains(forbidden),
            "generated Python contract leaked {forbidden:?}"
        );
    }
}

#[test]
fn python_import_matrix_follows_stream_and_topic_presence() {
    // stream + topic
    let mut stream: Value = serde_json::from_str(STREAM_DESCRIPTOR).expect("stream descriptor");
    let binding = stream["methods"][1]["bindings"]["output"].clone();
    stream["topics"] = Value::Array(vec![serde_json::json!({
        "name": "events",
        "output": {"kind": "payload"},
        "binding": binding,
    })]);
    refresh_fingerprints(&mut stream);
    let both = generate(
        &serde_json::to_vec(&stream).expect("descriptor bytes"),
        ContractCodegenTarget::Python,
    );
    assert!(
        source(&both, ContractCodegenTarget::Python)
            .contains("from c_two import HeldResult, Stream, Subscription\n")
    );

    // stream only
    let stream_only = generate(STREAM_DESCRIPTOR.as_bytes(), ContractCodegenTarget::Python);
    assert!(
        source(&stream_only, ContractCodegenTarget::Python)
            .contains("from c_two import HeldResult, Stream\n")
    );

    // topic only
    let solo = descriptor_with_topics(&["telemetry"], true);
    let set = generate(&solo, ContractCodegenTarget::Python);
    let python = source(&set, ContractCodegenTarget::Python);
    assert!(python.contains("from c_two import HeldResult, Subscription\n"));
    assert!(python.contains("METHODS = (\n)"));
    assert!(python.contains("TOPICS = (\n    TopicBinding(0, \"telemetry\","));
    assert!(python.contains("def subscribe_topic_0_telemetry("));
    assert!(python.contains("def hold_subscribe_topic_0_telemetry("));
    // A topics-only contract still separates its payload module lines from
    // the error helpers by one blank line.
    let solo_digest = output_digest(&solo);
    assert!(python.contains(&format!(
        "_load_payload_module(\"{solo_digest}\")\n\ndef _safe_error_text"
    )));

    // Neither: no eager stream or subscription import.
    let plain = generate(DESCRIPTOR.as_bytes(), ContractCodegenTarget::Python);
    assert!(
        !source(&plain, ContractCodegenTarget::Python)
            .lines()
            .any(|line| line.starts_with("from c_two import "))
    );
}

#[test]
fn typescript_target_generates_typed_topic_subscriptions() {
    let set = generate(
        &mixed_descriptor(&["state"]),
        ContractCodegenTarget::TypeScript,
    );
    let typescript = source(&set, ContractCodegenTarget::TypeScript);
    let state_digest = output_digest(&mixed_descriptor(&["state"]));
    for required in [
        "  subscribe_topic_0_state(options?: TopicSubscribeOptions): Promise<C2TopicSubscription<Payload>>",
        "  hold_subscribe_topic_0_state(options?: TopicSubscribeOptions): Promise<C2TopicSubscription<C2HeldResult<Payload>>>",
        "decode_topic_0_state_output, options, false, this.callOptions",
        "decode_topic_0_state_output, options, true, this.callOptions",
        "export function decode_topic_0_state_output(data: Uint8Array): Payload",
        "export interface TopicBinding {",
        "  readonly index: number;",
        "  readonly name: string;",
        "  readonly outputSha256: string;",
        "export const TOPICS: readonly TopicBinding[] = Object.freeze([",
        &format!("  {{ index: 0, name: \"state\", outputSha256: \"{state_digest}\" }}"),
    ] {
        assert!(
            typescript.contains(required),
            "generated TypeScript contract is missing {required:?}"
        );
    }
}

#[test]
fn topicless_contracts_omit_contract_topic_symbols_in_all_targets() {
    // `unary_golden.rs` pins the artifact hashes; this test pins the semantic
    // half of the invariant: topicless modules emit no contract topic symbols.
    let fixtures = [
        include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json"),
        include_str!("../../../../tests/fixtures/contracts/portable-no-payload.contract.json"),
        include_str!("../../../../tests/fixtures/contracts/portable-record-v1.contract.json"),
        include_str!("../../../../tests/fixtures/contracts/portable-object-graph-v1.contract.json"),
    ];
    for fixture in fixtures {
        for target in [
            ContractCodegenTarget::Rust,
            ContractCodegenTarget::Python,
            ContractCodegenTarget::TypeScript,
        ] {
            let set = generate(fixture.as_bytes(), target);
            let text = source(&set, target);
            for forbidden in [
                "TopicBinding",
                "TOPICS",
                "TopicService",
                "topic_service",
                "registration_topics",
                "RegistrationTopics",
                "subscribe_topic",
                "hold_subscribe",
                "ContractCrm",
                "@cc.topic",
                "import c_two as cc",
                "from c_two import HeldResult",
                "_value(",
                "topic_0",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "topicless {target:?} module leaked {forbidden:?}"
                );
            }
            let manifest = std::str::from_utf8(
                set.get("metadata/composition-manifest.json")
                    .expect("composition manifest")
                    .bytes(),
            )
            .expect("manifest JSON");
            let manifest: Value = serde_json::from_str(manifest).expect("manifest parses");
            for binding in manifest["bindings"].as_array().expect("bindings array") {
                let path = binding["outer_path"].as_str().expect("binding path");
                assert!(
                    !path.contains("topics"),
                    "topicless manifest carries a topic binding at {path:?}"
                );
            }
        }
    }
}

#[test]
fn shared_fastdb_spec_between_method_and_topic_compiles_one_payload_subtree() {
    // The mixed descriptor's topic binding reuses the echo method's output
    // specification, so one digest-named payload subtree serves both.
    let mixed = mixed_descriptor(&["state"]);
    let with_topic = generate(&mixed, ContractCodegenTarget::Rust);
    let without_topic_bytes: Vec<u8> = {
        let mut descriptor: Value = serde_json::from_str(DESCRIPTOR).expect("descriptor JSON");
        refresh_fingerprints(&mut descriptor);
        serde_json::to_vec(&descriptor).expect("descriptor bytes")
    };
    let without_topic = generate(&without_topic_bytes, ContractCodegenTarget::Rust);
    let payload_count = |set: &c2_codegen::ContractArtifactSet| {
        set.artifacts()
            .iter()
            .filter(|artifact| artifact.relative_path().contains("/payloads/"))
            .count()
    };
    assert_eq!(payload_count(&with_topic), payload_count(&without_topic));
    let manifest = std::str::from_utf8(
        with_topic
            .get("metadata/composition-manifest.json")
            .expect("composition manifest")
            .bytes(),
    )
    .expect("manifest JSON");
    let manifest: Value = serde_json::from_str(manifest).expect("manifest parses");
    let digests: Vec<&str> = manifest["bindings"]
        .as_array()
        .expect("bindings array")
        .iter()
        .map(|binding| binding["fastdb_sha256"].as_str().expect("digest"))
        .collect();
    let unique: std::collections::BTreeSet<&str> = digests.iter().copied().collect();
    assert_eq!(
        unique.len(),
        1,
        "method and topic bindings must share one FastDB digest: {digests:?}"
    );
}
