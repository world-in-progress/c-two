//! End-to-end direct IPC 17 publish/subscribe (ADR 0005) through the public
//! Rust SDK: the typed `Publisher` / `Subscription` / `Delivery` owners, and
//! generated topic contracts compiled by `c2-codegen` at test time.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use c2_codegen::{ContractCodegenOptions, ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::{ContractRelease, derive_contract_fingerprints_json};
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};
use serde_json::{Value, json};

use c_two::generated::{
    AdapterFailurePhase, C2Error, EncodedService, ErrorCode, HeldResponse, MethodAccess,
    MethodDefinition, fastdb_adapter_error, open_held, open_owned,
};
use c_two::{
    CallOptions, CallTimeout, Connect, Delivery, Error, Held, HostOptions, LagPolicy, Publisher,
    Runtime, RuntimeOptions, ServiceDefinition, SubscribeOptions, Subscription, TopicStart,
};

const BASE_DESCRIPTOR: &str =
    include_str!("../../../tests/fixtures/contracts/portable-release.contract.json");
const VALUE_SPEC: &[u8] = br#"{
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],
    "components": []
}"#;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn payload_spec() -> CompiledSpec {
    CompiledSpec::compile(VALUE_SPEC).unwrap()
}

fn payload(value: &str) -> Payload {
    let spec = payload_spec();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_str(value).unwrap();
    builder
        .freeze()
        .unwrap()
        .execute(BuildPolicy::AllowStaging)
        .unwrap()
        .payload
}

fn text(value: &Payload) -> String {
    value
        .entry_view(0)
        .unwrap()
        .at(0)
        .unwrap()
        .acquire()
        .unwrap()
        .str()
        .unwrap()
        .to_string()
}

// Hand-written adapters exercising the same public generated seam the
// generated module uses, so the typed owners are tested without codegen.
fn encode_value(value: &Payload) -> Result<Vec<u8>, Error> {
    value
        .require_spec_sha256(&payload_spec().sha256().unwrap())
        .map_err(|error| {
            c_two::Error::Semantic(fastdb_adapter_error(
                AdapterFailurePhase::ResourceOutputSerializing,
                error,
            ))
        })?;
    value.binary_bytes().map_err(|error| {
        c_two::Error::Semantic(fastdb_adapter_error(
            AdapterFailurePhase::ResourceOutputSerializing,
            error,
        ))
    })
}

fn decode_value(bytes: &[u8]) -> Result<Payload, Error> {
    let decoded = open_owned(&payload_spec(), bytes).map_err(|error| {
        c_two::Error::Semantic(fastdb_adapter_error(
            AdapterFailurePhase::ClientOutputFromBuffer,
            error,
        ))
    })?;
    Ok(decoded)
}

fn decode_held(response: HeldResponse) -> Result<Held<Payload>, Error> {
    open_held(&payload_spec(), response).map_err(|error| {
        c_two::Error::Semantic(fastdb_adapter_error(
            AdapterFailurePhase::ClientOutputFromBuffer,
            error,
        ))
    })
}

fn typed_publisher(registration: &c_two::Registration) -> Publisher<Payload> {
    Publisher::from_core(registration.publisher("state").unwrap(), encode_value)
}

fn typed_remote(client: &c_two::Client, options: SubscribeOptions) -> Subscription<Payload> {
    Subscription::from_core(
        client.subscribe("state", options).unwrap(),
        decode_value,
        decode_held,
    )
}

fn typed_local(
    registration: &c_two::Registration,
    options: SubscribeOptions,
) -> Subscription<Payload> {
    Subscription::from_local(
        registration.subscribe_local("state", options).unwrap(),
        decode_value,
        decode_held,
    )
}

fn assert_semantic(error: Error, code: ErrorCode, cause: &str) {
    let Error::Semantic(semantic) = error else {
        panic!("expected {code:?}, got {error:?}");
    };
    assert_eq!(semantic.code, code, "{semantic:?}");
    assert!(
        semantic.details.get("cause").map(String::as_str) == Some(cause)
            || semantic.message.contains(cause),
        "unexpected cause in {semantic:?}"
    );
}

struct EchoService;

impl EncodedService for EchoService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected method index {other}"),
            )),
        }
    }
}

fn topic_release(topics: &[&str], empty_methods: bool) -> ContractRelease {
    let mut value: Value = serde_json::from_str(BASE_DESCRIPTOR).unwrap();
    let sample_binding = value["methods"][1]["bindings"]["output"].clone();
    if empty_methods {
        value["methods"] = Value::Array(Vec::new());
    }
    value["topics"] = Value::Array(
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
    let fingerprints =
        derive_contract_fingerprints_json(&serde_json::to_vec(&value).unwrap()).unwrap();
    value["fingerprints"]["abi_hash"] = Value::String(fingerprints.abi_hash().to_string());
    value["fingerprints"]["signature_hash"] =
        Value::String(fingerprints.signature_hash().to_string());
    ContractRelease::from_descriptor_json(&serde_json::to_vec(&value).unwrap()).unwrap()
}

fn echo_definition(release: &ContractRelease, route_name: &str) -> ServiceDefinition {
    ServiceDefinition::new(
        release,
        release.reference(),
        route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".into(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".into(),
                access: MethodAccess::Write,
            },
        ],
        Arc::new(EchoService),
    )
    .unwrap()
}

fn make_runtime(dir: &tempfile::TempDir) -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    #[cfg(unix)]
    runtime
        .set_local_endpoint(c_two::endpoint::LocalEndpointOptions {
            unix_root: Some(dir.path().to_path_buf()),
        })
        .unwrap();
    runtime
}

fn make_host(dir: &tempfile::TempDir, prefix: &str) -> (Runtime, c_two::Host) {
    let runtime = make_runtime(dir);
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_server_id(unique(prefix)),
        )
        .unwrap();
    (runtime, host)
}

fn connect(
    runtime: &Runtime,
    host: &c_two::Host,
    release: &ContractRelease,
    route: &str,
) -> c_two::Client {
    runtime
        .connect(
            release.expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().to_string(),
            },
        )
        .unwrap()
}

#[test]
fn remote_decode_deadline_preserves_the_original_topic_subscription() {
    fn slow_decode(bytes: &[u8]) -> Result<Payload, Error> {
        let value = decode_value(bytes)?;
        std::thread::sleep(Duration::from_millis(1050));
        Ok(value)
    }

    fn slow_decode_held(response: HeldResponse) -> Result<Held<Payload>, Error> {
        let value = decode_held(response)?;
        std::thread::sleep(Duration::from_millis(1050));
        Ok(value)
    }

    let dir = tempfile::tempdir().unwrap();
    let (runtime, host) = make_host(&dir, "topic-decode-deadline");
    let release = topic_release(&["state"], false);
    let mut registration = host
        .register(
            echo_definition(&release, "sim")
                .with_initial_topic_bytes("state", encode_value(&payload("init")).unwrap())
                .unwrap(),
        )
        .unwrap();
    let client = connect(&runtime, &host, &release, "sim");
    let options = CallOptions::with_timeout(CallTimeout::After(Duration::from_secs(1)));
    let subscribe = || {
        Subscription::from_core(
            client
                .subscribe_with_options("state", SubscribeOptions::default(), options)
                .unwrap(),
            slow_decode,
            slow_decode_held,
        )
    };
    let owned = subscribe();
    let error = owned
        .next_delivery()
        .err()
        .expect("deadline during owned decode");
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "deadline");
    let held = subscribe();
    let error = held
        .next_delivery_held()
        .err()
        .expect("deadline during held decode");
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "deadline");

    assert_eq!(registration.active_subscribers(), 2);
    registration.close().unwrap();
    for sub in [owned, held] {
        let receipt = sub.close();
        assert!(receipt.completed);
        let error = receipt.terminal.unwrap().unwrap_err();
        assert_semantic(error, ErrorCode::CallCancelled, "route_unregistered");
    }
}

// The typed owners over a real direct IPC 17 connection and over the
// same-process local subscription seam: Delivery / Delivery<Held<T>>,
// Latest/Next, publisher closure after unregister, and holds that outlive
// their subscription.
#[test]
fn typed_topic_owners_cover_remote_and_local_subscriptions() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, host) = make_host(&dir, "topic-owners");
    let release = topic_release(&["state"], false);
    let mut registration = host
        .register(
            echo_definition(&release, "sim")
                .with_topic_capacity("state", 4)
                .unwrap()
                .with_initial_topic_bytes("state", encode_value(&payload("init")).unwrap())
                .unwrap(),
        )
        .unwrap();
    let publisher = typed_publisher(&registration);
    assert_eq!(publisher.topic_name(), "state");
    assert!(!publisher.is_closed());
    assert_eq!(
        publisher.head_seq(),
        1,
        "initial frame committed before open"
    );

    let client = connect(&runtime, &host, &release, "sim");

    // Remote subscriber with start=Latest reads the initial state frame.
    let remote = typed_remote(&client, SubscribeOptions::default());
    assert_eq!(remote.start_seq(), 1);
    assert_eq!(remote.topic_name(), "state");
    let delivery: Delivery<Payload> = remote.next_delivery().unwrap();
    assert_eq!(text(delivery.value()), "init");
    assert_eq!((delivery.seq(), delivery.dropped()), (1, 0));

    // Local subscriber with start=Next on the same registration.
    let local = typed_local(
        &registration,
        SubscribeOptions {
            start: TopicStart::Next,
            on_lag: LagPolicy::Skip,
        },
    );
    assert_eq!(local.start_seq(), 2);
    assert_eq!(publisher.publish(&payload("second")).unwrap(), 2);
    let local_delivery = local.next_delivery().unwrap();
    assert_eq!(text(local_delivery.value()), "second");
    assert_eq!(local_delivery.seq(), 2);

    // Held delivery owns its local copy and survives closing the subscription.
    let held_delivery: Delivery<Held<Payload>> = remote.next_delivery_held().unwrap();
    assert_eq!(held_delivery.seq(), 2);
    assert_eq!(text(held_delivery.value().value().unwrap()), "second");
    assert!(remote.close().completed);
    assert!(local.close().completed);
    assert_eq!(
        text(held_delivery.value().value().unwrap()),
        "second",
        "closed subscription invalidated a delivered hold"
    );

    // A live subscription ends with 718 route_unregistered when the route
    // unregisters; afterwards the publisher is closed (707) and never writes
    // to a later registration of the same route name.
    let live = typed_remote(&client, SubscribeOptions::default());
    assert_eq!(
        live.next_delivery().unwrap().seq(),
        2,
        "retained latest frame first"
    );
    let closer = std::thread::spawn(move || registration.close().unwrap());
    assert_semantic(
        live.next_delivery().unwrap_err(),
        ErrorCode::CallCancelled,
        "route_unregistered",
    );
    assert!(live.close().completed);
    assert!(publisher.is_closed());
    assert_semantic(
        publisher.publish(&payload("late")).unwrap_err(),
        ErrorCode::ResourceClosed,
        "state",
    );
    closer.join().unwrap();
    let _ = host.shutdown();
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("C-Two repository root")
        .to_path_buf()
}

fn write_consumer_manifest(temp: &Path, repository: &Path) {
    std::fs::write(
        temp.join("Cargo.toml"),
        format!(
            "[package]\nname = \"topic-pubsub-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nc-two = {{ version = \"0.1.0\", path = {:?} }}\nfastdb = \"=0.2.4\"\n",
            repository.join("sdk/rust")
        ),
    )
    .unwrap();
    // CI fetches the Core workspace lock before offline consumers; seed the
    // same dependency versions instead of the SDK's separate lock.
    std::fs::copy(repository.join("core/Cargo.lock"), temp.join("Cargo.lock")).unwrap();
}

// Generated topic contracts end to end over IPC 17: typed subscribe methods,
// TopicService capacity and atomic initial state, typed publishers, lag
// Skip/Error(720), unregister 718 followed by automatic re-registration, and
// a topics-only contract.
#[test]
fn generated_topic_contract_end_to_end_over_ipc17() {
    let repository = repository();
    let temp = tempfile::tempdir().unwrap();
    let mixed_release = topic_release(&["state", "ticks"], false);
    let solo_release = topic_release(&["telemetry"], true);
    for (name, release, target) in [
        (
            "generated-mixed",
            &mixed_release,
            ContractCodegenTarget::Rust,
        ),
        ("generated-solo", &solo_release, ContractCodegenTarget::Rust),
    ] {
        compile_contract_artifacts(release, target, &ContractCodegenOptions::default())
            .unwrap()
            .publish_new_tree(&temp.path().join(name))
            .unwrap();
    }
    write_consumer_manifest(temp.path(), &repository);
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), CONSUMER).unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["run", "--quiet", "--offline"])
        .current_dir(temp.path())
        .env(
            "CARGO_TARGET_DIR",
            std::env::var_os("CARGO_TARGET_DIR")
                .map(Into::into)
                .unwrap_or_else(|| repository.join("core/target")),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated topic consumer failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "OK topic pubsub"
    );
}

const CONSUMER: &str = r##"

use c_two::generated::ErrorCode;
use c_two::{
    Connect, Error, Held, HostOptions, LagPolicy, Runtime, RuntimeOptions, SubscribeOptions,
    TopicStart,
};

#[path = "../generated-mixed/rust/c_two_contract.rs"]
pub mod mixed;
#[path = "../generated-solo/rust/c_two_contract.rs"]
pub mod solo;

use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};

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

fn text(value: &Payload) -> String {
    value.entry_view(0).unwrap().at(0).unwrap().acquire().unwrap().str().unwrap().to_string()
}

fn assert_semantic(error: Error, code: ErrorCode, needle: &str) {
    let Error::Semantic(semantic) = error else {
        panic!("expected {code:?}, got {error:?}");
    };
    assert_eq!(semantic.code, code, "{semantic:?}");
    assert!(semantic.message.contains(needle) || semantic.details.values().any(|v| v.contains(needle)), "{semantic:?}");
}

struct Echo;

impl mixed::Service for Echo {
    fn method_0_ping(&self) -> Result<(), c_two::Error> {
        Ok(())
    }
    fn method_1_echo(&self, input: &Payload) -> Result<Payload, c_two::Error> {
        Ok(input.clone())
    }
}

struct Sensor;

impl solo::Service for Sensor {}

fn main() {
    // 1. Mixed contract: atomic initial state, capacity, typed publisher,
    //    Latest/Next starts, Delivery and Delivery<Held<T>>.
    {
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_server_id(format!("topic-mixed-{}", std::process::id())),
            )
            .unwrap();
        let definition = mixed::topic_service("sim", Echo)
            .unwrap()
            .topic_0_state_capacity(4)
            .unwrap()
            .initial_topic_0_state(&payload("init"))
            .unwrap()
            .topic_1_ticks_capacity(2)
            .unwrap()
            .build();
        let mut registration = host.register(definition).unwrap();

        let client = mixed::ContractClient::new(
            runtime
                .connect(
                    mixed::expected_route("sim").unwrap(),
                    Connect::DirectIpc {
                        address: runtime.server_address().unwrap(),
                    },
                )
                .unwrap(),
        )
        .unwrap();

        // Unary methods still work next to topic subscriptions.
        assert_eq!(text(&client.method_1_echo(&payload("ctrl")).unwrap()), "ctrl");

        let publishers = mixed::registration_topics(&registration);
        let publisher = publishers.topic_0_state().unwrap();
        assert_eq!(publisher.topic_name(), "state");
        assert_eq!(publisher.head_seq(), 1, "initial frame committed");

        // start=Latest reads the initial state immediately.
        let state = client
            .subscribe_topic_0_state(SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            })
            .unwrap();
        assert_eq!((state.start_seq(), state.topic_name()), (1, "state"));
        let initial = state.next_delivery().unwrap();
        assert_eq!((text(initial.value()), initial.seq(), initial.dropped()), ("init".to_string(), 1, 0));

        // Held delivery owns its local copy through subscription close.
        assert_eq!(publisher.publish(&payload("held-value")).unwrap(), 2);
        let held: c_two::Delivery<Held<Payload>> = state.next_delivery_held().unwrap();
        assert_eq!((held.seq(), held.dropped()), (2, 0));
        assert_eq!(text(held.value().value().unwrap()), "held-value");

        // start=Next waits for the frame after the current head.
        let following = client
            .subscribe_topic_0_state(SubscribeOptions {
                start: TopicStart::Next,
                on_lag: LagPolicy::Skip,
            })
            .unwrap();
        assert_eq!(following.start_seq(), 3);
        assert_eq!(publisher.publish(&payload("third")).unwrap(), 3);
        let third = following.next_delivery().unwrap();
        assert_eq!((text(third.value()), third.seq(), third.dropped()), ("third".to_string(), 3, 0));
        assert!(following.close().completed);
        assert!(state.close().completed);
        assert_eq!(text(held.value().value().unwrap()), "held-value", "hold died with its subscription");
        let (mut held_owner, _, _) = held.into_parts();
        held_owner.release().unwrap();

        // 2. Lag policies on the capacity-2 "ticks" topic.
        let skip = client
            .subscribe_topic_1_ticks(SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Skip,
            })
            .unwrap();
        let strict = client
            .subscribe_topic_1_ticks(SubscribeOptions {
                start: TopicStart::Latest,
                on_lag: LagPolicy::Error,
            })
            .unwrap();
        let ticks = mixed::registration_topics(&registration).topic_1_ticks().unwrap();
        assert_eq!(ticks.publish(&payload("v1")).unwrap(), 1);
        assert_eq!(skip.next_delivery().unwrap().seq(), 1);
        assert_eq!(strict.next_delivery().unwrap().seq(), 1);
        for seq in 2..=7u64 {
            assert_eq!(ticks.publish(&payload(&format!("v{seq}"))).unwrap(), seq);
        }
        // Skip jumps to the newest retained frame with the exact drop count.
        let mut last_seq = 1u64;
        let mut saw_skip = false;
        for _ in 0..8 {
            let delivery = skip.next_delivery().unwrap();
            assert!(delivery.seq() > last_seq);
            if delivery.dropped() > 0 {
                assert_eq!(delivery.seq() - (last_seq + 1), delivery.dropped());
                assert_eq!(text(delivery.value()), format!("v{}", delivery.seq()));
                saw_skip = true;
                break;
            }
            last_seq = delivery.seq();
        }
        assert!(saw_skip, "skip subscriber must report dropped > 0");
        // Error terminates the subscription with SubscriptionLagged (720).
        let mut lag_error = None;
        for _ in 0..8 {
            match strict.next_delivery() {
                Ok(delivery) => assert_eq!(delivery.dropped(), 0),
                Err(error) => {
                    lag_error = Some(error);
                    break;
                }
            }
        }
        let lag_error = lag_error.expect("on_lag=error subscriber must terminate");
        assert_semantic(lag_error, ErrorCode::SubscriptionLagged, "ticks");
        assert!(skip.close().completed);
        assert!(strict.close().completed);

        // 3. Unregister ends the live subscription with 718 route_unregistered;
        //    the same client follows a re-registered route automatically.
        let live = client
            .subscribe_topic_0_state(SubscribeOptions::default())
            .unwrap();
        assert_eq!(live.next_delivery().unwrap().seq(), 3, "retained latest frame first");
        let route = registration.route_name().to_string();
        let closer = std::thread::spawn(move || registration.close().unwrap());
        assert_semantic(
            live.next_delivery().unwrap_err(),
            ErrorCode::CallCancelled,
            "route_unregistered",
        );
        assert!(live.close().completed);
        closer.join().unwrap();
        let successor = mixed::topic_service(&route, Echo)
            .unwrap()
            .initial_topic_0_state(&payload("r2-init"))
            .unwrap()
            .build();
        let mut registration2 = host.register(successor).unwrap();
        let follower = client
            .subscribe_topic_0_state(SubscribeOptions::default())
            .unwrap();
        assert_eq!(follower.start_seq(), 1, "sequence restarts with the new registration");
        let reboot = follower.next_delivery().unwrap();
        assert_eq!((text(reboot.value()), reboot.seq()), ("r2-init".to_string(), 1));
        assert!(follower.close().completed);
        registration2.close().unwrap();
        let _ = host.shutdown();
    }

    // 4. Topics-only contract: empty Service trait, registration, publisher
    //    and typed subscription without any unary method.
    {
        let runtime = Runtime::new(RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_server_id(format!("topic-solo-{}", std::process::id())),
            )
            .unwrap();
        let definition = solo::topic_service("sensor", Sensor)
            .unwrap()
            .initial_topic_0_telemetry(&payload("boot"))
            .unwrap()
            .build();
        let mut registration = host.register(definition).unwrap();
        let telemetry = solo::registration_topics(&registration).topic_0_telemetry().unwrap();
        let client = solo::ContractClient::new(
            runtime
                .connect(
                    solo::expected_route("sensor").unwrap(),
                    Connect::DirectIpc {
                        address: runtime.server_address().unwrap(),
                    },
                )
                .unwrap(),
        )
        .unwrap();
        let subscription = client
            .subscribe_topic_0_telemetry(SubscribeOptions::default())
            .unwrap();
        assert_eq!(subscription.start_seq(), 1);
        assert_eq!(text(subscription.next_delivery().unwrap().value()), "boot");
        assert_eq!(telemetry.publish(&payload("reading-2")).unwrap(), 2);
        let second = subscription.next_delivery().unwrap();
        assert_eq!((text(second.value()), second.seq()), ("reading-2".to_string(), 2));
        assert!(subscription.close().completed);
        registration.close().unwrap();
        let _ = host.shutdown();
    }

    println!("OK topic pubsub");
}
"##;
