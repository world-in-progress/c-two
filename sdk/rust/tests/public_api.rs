use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use c_two::generated::{
    C2Error, EncodedClient, EncodedService, ErrorCode, MethodAccess, MethodDefinition,
    ServiceDefinition, fastdb_cause_details,
};
use c_two::{
    Connect, ContractLimits, ContractRelease, ContractReleaseRef, Error, HostOptions, Runtime,
    RuntimeOptions, ServiceConcurrencyMode,
};
use c2_http::relay::{RelayConfig, RelayServer};
use fastdb::{BuildPolicy, Builder, CompiledSpec};

const DESCRIPTOR: &str = r#"{
  "schema": "c-two.contract.v2",
  "crm": {
    "namespace": "test.contract-release",
    "name": "Portable",
    "version": "0.1.0"
  },
  "fingerprints": {
    "abi_hash": "bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe",
    "signature_hash": "c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940"
  },
  "methods": [
    {
      "access": "read",
      "name": "ping",
      "parameters": [],
      "return": {"kind": "none"},
      "bindings": {"input": null, "output": null}
    },
    {
      "access": "write",
      "name": "echo",
      "parameters": [
        {
          "name": "payload",
          "kind": "POSITIONAL_OR_KEYWORD",
          "default": {"kind": "missing"},
          "type": {"kind": "payload"}
        }
      ],
      "return": {"kind": "payload"},
      "bindings": {
        "input": {
          "kind": "fastdb",
          "spec": {
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
          }
        },
        "output": {
          "kind": "fastdb",
          "spec": {
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
          }
        }
      }
    }
  ]
}"#;

const FASTDB_RECORD_U8_SPEC: &[u8] = br#"{
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

const FASTDB_RECORD_U16_SPEC: &[u8] = br#"{
  "schema": "fastdb.payload.v1",
  "profile": "record.v1",
  "entries": [
    {
      "id": "value",
      "cardinality": "one",
      "type": {"kind": "u16", "nullable": false}
    }
  ],
  "components": []
}"#;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match (method_index, request) {
            (0, _) => Ok(Vec::new()),
            (1, b"semantic-error") => Err(C2Error::new(
                ErrorCode::ResourceFunctionExecuting,
                "portable Python fixture failure",
            )
            .with_details(BTreeMap::from([
                ("fixture".to_string(), "python".to_string()),
                ("phase".to_string(), "resource".to_string()),
            ]))),
            (1, _) => Ok(request.to_vec()),
            (other, _) => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected method index {other}"),
            )),
        }
    }
}

fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json_with_limits(
        DESCRIPTOR.as_bytes(),
        ContractLimits::default(),
    )
    .expect("valid release")
}

fn definition(route_name: &str) -> ServiceDefinition {
    let release = release();
    ServiceDefinition::new(
        &release,
        release.reference(),
        route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        Arc::new(Echo),
    )
    .expect("release-verified service definition")
}

fn relay() -> (RelayServer, String) {
    let probe = TcpListener::bind("127.0.0.1:0").expect("reserve relay port");
    let address = probe.local_addr().expect("relay address");
    drop(probe);
    let relay_url = format!("http://{address}");
    let relay = RelayServer::start(RelayConfig {
        bind: address.to_string(),
        advertise_url: relay_url.clone(),
        idle_timeout_secs: 0,
        anti_entropy_interval: std::time::Duration::ZERO,
        heartbeat_interval: std::time::Duration::ZERO,
        ..RelayConfig::default()
    })
    .expect("relay starts");
    (relay, relay_url)
}

#[test]
fn package_identity_and_root_imports_are_exact() {
    fn accepts_release_ref(_: &ContractReleaseRef) {}
    fn accepts_error(_: &Error) {}

    let release = release();
    accepts_release_ref(&release.reference());
    let _runtime = Runtime::new(RuntimeOptions::default()).expect("runtime");
    let _host_options = HostOptions::default();
    let _connect = Connect::RelayAware;

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--manifest-path",
            manifest.to_str().expect("UTF-8 manifest path"),
            "--format-version",
            "1",
            "--no-deps",
        ])
        .output()
        .expect("cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata JSON");
    let package = &metadata["packages"][0];
    assert_eq!(package["name"], "c-two");
    assert_eq!(package["version"], "0.1.0");
    assert_eq!(package["publish"], serde_json::json!([]));
    assert_eq!(package["targets"][0]["name"], "c_two");
    assert_eq!(package["targets"][0]["kind"], serde_json::json!(["lib"]));

    let semantic = Error::Semantic(C2Error::new(ErrorCode::Unknown, "type assertion"));
    accepts_error(&semantic);
}

#[test]
fn one_client_type_covers_direct_explicit_relay_and_relay_aware_calls() {
    fn accepts_client(_: &c_two::Client) {}

    let (mut relay, relay_url) = relay();
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("rust-sdk")),
        relay_anchor_address: Some(relay_url.clone()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let route_name = unique_name("echo");
    let mut registration = host
        .register(definition(&route_name))
        .expect("route registration");
    let registered_route_uid = registration.outcome().route_uid.clone();
    let registered_route_revision = registration.outcome().route_revision;
    let expected = release()
        .expected_route(&route_name)
        .expect("expected route");

    let direct = runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("direct IPC client");
    accepts_client(&direct);
    assert_eq!(direct.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        direct.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(
        direct.call_owned("echo", b"direct").expect("direct call"),
        b"direct"
    );

    let explicit = runtime
        .connect(
            expected.clone(),
            Connect::ExplicitRelay {
                relay_url: relay_url.clone(),
            },
        )
        .expect("explicit relay client");
    accepts_client(&explicit);
    assert_eq!(explicit.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        explicit.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(
        explicit
            .call_owned("echo", b"explicit")
            .expect("explicit relay call"),
        b"explicit"
    );

    let relay_aware = runtime
        .connect(expected, Connect::RelayAware)
        .expect("relay-aware client");
    accepts_client(&relay_aware);
    assert_eq!(relay_aware.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        relay_aware.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(
        relay_aware
            .call_owned("echo", b"aware")
            .expect("relay-aware call"),
        b"aware"
    );

    registration.close().expect("registration cleanup");
    drop(host);
    relay.stop().expect("relay cleanup");
}

#[test]
fn registration_projects_the_core_owned_route_concurrency() {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("rust-sdk-concurrency")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let route_name = unique_name("concurrency");
    let mut registration = host
        .register(
            definition(&route_name)
                .with_concurrency(ServiceConcurrencyMode::Exclusive, Some(3), Some(1))
                .expect("concurrency policy"),
        )
        .expect("route registration");
    let concurrency = registration.route_concurrency();
    let snapshot = concurrency.snapshot();
    assert_eq!(snapshot.mode, ServiceConcurrencyMode::Exclusive);
    assert_eq!(snapshot.max_pending, Some(3));
    assert_eq!(snapshot.max_workers, Some(1));
    assert!(!snapshot.closed);

    let guard = concurrency
        .blocking_acquire(0)
        .expect("same-process route guard");
    assert_eq!(concurrency.snapshot().active_workers, 1);
    drop(guard);
    registration.close().expect("registration cleanup");
    assert!(concurrency.snapshot().closed);
}

#[test]
fn semantic_error_fields_match_the_cross_language_fixture_shape() {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("semantic")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let route_name = unique_name("semantic");
    let _registration = host
        .register(definition(&route_name))
        .expect("route registration");
    let client = runtime
        .connect(
            release()
                .expected_route(route_name)
                .expect("expected route"),
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("client");

    let Error::Semantic(error) = client
        .call_owned("echo", b"semantic-error")
        .expect_err("service semantic error must cross the wire")
    else {
        panic!("service error must remain semantic");
    };
    assert_eq!(u16::from(error.code), 3);
    assert_eq!(error.code.name(), "ResourceFunctionExecuting");
    assert_eq!(error.message, "portable Python fixture failure");
    assert_eq!(
        error.details,
        BTreeMap::from([
            ("fixture".to_string(), "python".to_string()),
            ("phase".to_string(), "resource".to_string()),
        ])
    );
}

#[test]
fn official_fastdb_error_projects_only_the_six_frozen_outer_keys() {
    let expected_spec =
        CompiledSpec::compile(FASTDB_RECORD_U8_SPEC).expect("compile expected FastDB spec");
    let actual_spec =
        CompiledSpec::compile(FASTDB_RECORD_U16_SPEC).expect("compile actual FastDB spec");
    let mut builder = Builder::create(&actual_spec).expect("create mismatched payload builder");
    builder
        .entry_begin(0, 1)
        .expect("begin mismatched value")
        .value_u16(7)
        .expect("write mismatched value");
    let payload = builder
        .freeze()
        .expect("freeze mismatched payload")
        .execute(BuildPolicy::AllowStaging)
        .expect("build mismatched payload")
        .payload;
    let expected_digest = expected_spec.sha256().expect("expected spec digest");
    let error = payload
        .require_spec_sha256(&expected_digest)
        .expect_err("mismatched official FastDB payload must fail");
    let details = fastdb_cause_details(&error);
    let expected: BTreeMap<String, String> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/fastdb-digest-mismatch-cause.json"
    ))
    .expect("shared FastDB cause vector");

    assert_eq!(details, expected);
}

#[test]
fn low_level_transports_are_not_importable_and_encoded_client_is_sealed() {
    assert_external_crate_rejected(
        "transport-surface",
        r#"
use c_two::{c2_http, c2_ipc, c2_server, RouteBinding, SyncClient};
fn main() {}
"#,
        "unresolved imports",
    );
    assert_external_crate_rejected(
        "sealed-client",
        r#"
use c_two::Error;
use c_two::generated::{EncodedClient, HeldResponse};

struct Forged;

impl EncodedClient for Forged {
    fn call_owned(&self, _method: &str, _request: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(Vec::new())
    }

    fn call_held(&self, _method: &str, _request: &[u8]) -> Result<HeldResponse, Error> {
        Ok(HeldResponse::from_owned_bytes(Vec::new()))
    }
}

fn main() {}
"#,
        "Sealed",
    );
}

fn assert_external_crate_rejected(name: &str, source: &str, expected_stderr: &str) {
    let root = std::env::temp_dir().join(format!(
        "c-two-rust-sdk-{name}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("temporary external crate");
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nc-two = {{ path = '{}' }}\n",
            sdk.display()
        ),
    )
    .expect("external manifest");
    // Reuse the dependency versions tested by this SDK, including any locked
    // registry version that has since been yanked. The positive control lets
    // Cargo add this temporary root without resolving a fresh dependency graph.
    std::fs::copy(sdk.join("Cargo.lock"), root.join("Cargo.lock"))
        .expect("seed external dependency lock from the tested SDK");
    std::fs::write(src.join("main.rs"), "fn main() {}\n")
        .expect("external positive-control source");
    let check = |locked: bool| {
        let mut command = Command::new(env!("CARGO"));
        command.args(["check", "--quiet", "--offline"]);
        if locked {
            command.arg("--locked");
        }
        command
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", sdk.join("target/compile-fail"))
            .output()
            .expect("external cargo check")
    };
    let control = check(false);
    assert!(
        control.status.success(),
        "external positive control failed before the API rejection probe:\n{}",
        String::from_utf8_lossy(&control.stderr),
    );
    std::fs::write(src.join("main.rs"), source).expect("external rejection source");
    let output = check(true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "external crate unexpectedly compiled:\n{source}"
    );
    assert!(
        stderr.contains(expected_stderr),
        "expected compiler output containing {expected_stderr:?}, got:\n{stderr}"
    );
    std::fs::remove_dir_all(root).expect("temporary crate cleanup");
}
