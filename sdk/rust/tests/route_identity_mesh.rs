//! Gate 7: independent real c3 relays keep existing proxy owners through a mesh conflict.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use c_two::generated::{
    C2Error, EncodedClient, EncodedService, ErrorCode, MethodAccess, MethodDefinition,
};
use c_two::observe::ObservedPath;
use c_two::{
    Client, ConfigSources, Connect, ContractRelease, Error, Host, HostOptions, LifecycleError,
    Registration, Runtime, ServiceDefinition,
};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
const DESCRIPTOR: &[u8] = br#"{
  "schema":"c-two.contract.v2",
  "crm":{"namespace":"test.contract-release","name":"Portable","version":"0.1.0"},
  "fingerprints":{
    "abi_hash":"bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe",
    "signature_hash":"c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940"
  },
  "methods":[
    {"access":"read","name":"ping","parameters":[],"return":{"kind":"none"},"bindings":{"input":null,"output":null}},
    {"access":"write","name":"echo","parameters":[{"name":"payload","kind":"POSITIONAL_OR_KEYWORD","default":{"kind":"missing"},"type":{"kind":"payload"}}],"return":{"kind":"payload"},"bindings":{
      "input":{"kind":"fastdb","spec":{"schema":"fastdb.payload.v1","profile":"record.v1","entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],"components":[]}},
      "output":{"kind":"fastdb","spec":{"schema":"fastdb.payload.v1","profile":"record.v1","entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],"components":[]}}
    }}
  ]
}"#;
const SPEC: &[u8] = br#"{"schema":"fastdb.payload.v1","profile":"record.v1","entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],"components":[]}"#;

struct Resource {
    label: &'static str,
    calls: Arc<AtomicUsize>,
}

impl EncodedService for Resource {
    fn invoke(&self, index: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(index, 1);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(encode(self.label))
    }
}

fn encode(value: &str) -> Vec<u8> {
    use fastdb::{BuildPolicy, Builder, CompiledSpec};
    let spec = CompiledSpec::compile(SPEC).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_str(value).unwrap();
    builder
        .freeze()
        .unwrap()
        .execute(BuildPolicy::AllowStaging)
        .unwrap()
        .payload
        .binary_bytes()
        .unwrap()
}

struct Relay {
    child: Child,
    url: String,
    id: String,
    root: PathBuf,
}

impl Relay {
    fn start(label: &str, directory: &Path) -> Self {
        let root = directory.join(label);
        let mut dir = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(&root).unwrap();
        let stdout_path = root.join("c3.stdout.log");
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repository.join("core/target"));
        let binary = std::env::var_os("CTWO_TEST_C3_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                target
                    .join("debug")
                    .join(if cfg!(windows) { "c3.exe" } else { "c3" })
            });
        assert!(
            binary.is_file(),
            "build c3 before this gate: {}",
            binary.display()
        );
        let mut command = Command::new(binary);
        command.args(["relay", "--bind", "127.0.0.1:0", "--idle-timeout", "0"]);
        #[cfg(unix)]
        command.arg("--ipc-root").arg(&root);
        let child = command
            .env_remove("C2_IPC_ROOT")
            .env_remove("C2_RELAY_ANCHOR_ADDRESS")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout_path).unwrap())
            .stderr(std::fs::File::create(root.join("c3.stderr.log")).unwrap())
            .spawn()
            .unwrap();
        let mut relay = Self {
            child,
            url: String::new(),
            id: String::new(),
            root,
        };
        wait_until(|| {
            assert!(
                relay.child.try_wait().unwrap().is_none(),
                "c3 exited: {}",
                relay.root.display()
            );
            if relay.url.is_empty() {
                let stdout = std::fs::read_to_string(&stdout_path).unwrap();
                let complete = stdout
                    .rsplit_once('\n')
                    .map(|(lines, _)| lines)
                    .unwrap_or("");
                let Some(url) = complete
                    .lines()
                    .find_map(|line| line.strip_prefix("C-Two relay listening at "))
                else {
                    return false;
                };
                relay.url = url.to_owned();
            }
            get(&relay.url, "/health")
                .map(|response| response["status"] == "ok")
                .unwrap_or(false)
        });
        let sync = get(&relay.url, "/_peer/sync").unwrap();
        relay.id = sync["snapshot"]["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|peer| peer["url"] == relay.url)
            .unwrap()["relay_id"]
            .as_str()
            .unwrap()
            .to_string();
        relay
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "mesh state did not settle within {WAIT:?}"
        );
        std::thread::yield_now();
    }
}

fn retire_clients(runtime: &Runtime) {
    let deadline = Instant::now() + WAIT;
    let receipt = runtime
        .begin_session_retirement(c2_core::RuntimeShutdownTarget::ClientsOnly, deadline)
        .unwrap();
    let (outcome, completed) = receipt.observe_until(deadline);
    assert!(completed, "{outcome:?}");
    assert!(outcome.ipc_client_close_error.is_none(), "{outcome:?}");
    assert_eq!(
        runtime.call_execution_snapshot().unwrap().used_operations,
        0
    );
}

fn http(
    url: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> std::io::Result<(u16, Value)> {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(WAIT))?;
    stream.set_write_timeout(Some(WAIT))?;
    let data = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{data}",
        data.len()
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (headers, content) = response.split_once("\r\n\r\n").unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let value = if content.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(content).unwrap()
    };
    Ok((status, value))
}

fn get(url: &str, path: &str) -> std::io::Result<Value> {
    let (status, value) = http(url, "GET", path, None)?;
    assert_eq!(status, 200, "{path}: {value}");
    Ok(value)
}

fn post(url: &str, path: &str, body: &Value) -> Value {
    let (status, value) = http(url, "POST", path, Some(body)).unwrap();
    assert_eq!(status, 200, "{path}: {value}");
    value
}

fn envelope(sender: &Relay, message: Value) -> Value {
    json!({"protocol_version": 7, "sender_relay_id": sender.id, "message": message})
}

fn join_and_exchange(a: &Relay, b: &Relay) {
    for (source, target) in [(a, b), (b, a)] {
        let joined = post(
            &target.url,
            "/_peer/join",
            &envelope(
                source,
                json!({"type":"RelayJoin", "relay_id":source.id, "url":source.url}),
            ),
        );
        assert_eq!(joined["protocol_version"], 7, "{joined}");
    }
    for (source, target) in [(a, b), (b, a)] {
        // The owner sends its active route as a genuine mesh DigestDiff reply.
        let diff = post(
            &source.url,
            "/_peer/digest",
            &envelope(target, json!({"type":"DigestExchange", "digest":[]})),
        );
        assert_eq!(diff["message"]["type"], "DigestDiff", "{diff}");
        assert_eq!(diff["message"]["entries"].as_array().unwrap().len(), 1);
        let accepted = post(&target.url, "/_peer/digest", &diff);
        assert!(accepted.is_null(), "{accepted}");
    }
}

fn routes(relay: &Relay, name: &str) -> Vec<Value> {
    get(&relay.url, "/_routes").unwrap()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|route| route["name"] == name)
        .cloned()
        .collect()
}

fn peer_status(relay: &Relay, peer: &Relay) -> Option<String> {
    get(&relay.url, "/_peers")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["relay_id"] == peer.id)
        .and_then(|item| item["status"].as_str())
        .map(str::to_owned)
}

fn peer_url(relay: &Relay, peer: &Relay) -> String {
    get(&relay.url, "/_peers")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["relay_id"] == peer.id)
        .unwrap()["url"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn test_directory(gate: &str, explicit: bool, release: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "c2-route-identity-{gate}-{}-{}-{}-{release}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        if explicit { "http" } else { "local" },
    ));
    std::fs::create_dir(&directory).unwrap();
    println!("C2_ROUTE_MESH_LOG_ROOT {}", directory.display());
    directory
}

fn unavailable(error: Error, name: &str, owner: &Relay, owner_url: &str, status: &str) {
    let Error::Semantic(error) = error else {
        panic!("expected semantic unavailable: {error:?}")
    };
    assert_eq!(error.code, ErrorCode::ResourceUnavailable, "{error:?}");
    assert_eq!(error.details["route"], name);
    assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
    let registrations: Vec<Value> = serde_json::from_str(&error.details["registrations"]).unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["relay_id"], owner.id);
    assert_eq!(registrations[0]["relay_url"], owner_url);
    assert_eq!(registrations[0]["relay_status"], status);
    assert!(
        registrations[0]["registered_at"]
            .as_f64()
            .unwrap()
            .is_finite()
    );
}

fn held_registration(error: Error, name: &str, owner: &Relay, owner_url: &str, status: &str) {
    let Error::Lifecycle(LifecycleError::RegisterFailure(receipt)) = error else {
        panic!("expected registration failure receipt: {error:?}")
    };
    assert_eq!(receipt.route_name, name);
    assert_eq!(receipt.status_code, Some(409));
    assert_eq!(receipt.failure_source, "relay_prepare");
    let semantic = receipt.semantic_error.as_ref().expect("canonical C2 error");
    assert_eq!(semantic.code, ErrorCode::ResourceAlreadyRegistered);
    let registrations: Vec<Value> =
        serde_json::from_str(&semantic.details["registrations"]).unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["relay_id"], owner.id);
    assert_eq!(registrations[0]["relay_url"], owner_url);
    assert_eq!(registrations[0]["relay_status"], status);
    assert!(
        registrations[0]["registered_at"]
            .as_f64()
            .unwrap()
            .is_finite()
    );
    let rollback = receipt.rollback.as_ref().expect("registration rollback");
    assert_eq!(rollback.route_name, name);
    assert!(rollback.active_drained);
    assert!(rollback.close_error.is_none());
}

fn assert_conflict_log(relay: &Relay, name: &str, action: &str, other: &Relay) {
    let log = std::fs::read_to_string(relay.root.join("c3.stderr.log")).unwrap();
    let matching: Vec<_> = log
        .lines()
        .filter(|line| line.contains(&format!("route conflict {action}: route={name} ")))
        .collect();
    assert_eq!(matching.len(), 1, "{log}");
    assert!(matching[0].contains(&relay.id), "{log}");
    assert!(matching[0].contains(&other.id), "{log}");
}

fn register(
    host: &Host,
    release: &ContractRelease,
    name: &str,
    label: &'static str,
    calls: &Arc<AtomicUsize>,
) -> Registration {
    host.register(definition(release, name, label, calls))
        .unwrap()
}

fn definition(
    release: &ContractRelease,
    name: &str,
    label: &'static str,
    calls: &Arc<AtomicUsize>,
) -> ServiceDefinition {
    ServiceDefinition::new(
        release,
        release.reference(),
        name,
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
        Arc::new(Resource {
            label,
            calls: calls.clone(),
        }),
    )
    .unwrap()
}

fn runtime(root: &Path, relay: &Relay, name: &str) -> Runtime {
    let runtime = Runtime::new(c_two::RuntimeOptions {
        server_id: Some(name.into()),
        relay_anchor_address: Some(relay.url.clone()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_local_endpoint_with_sources(
            c_two::endpoint::LocalEndpointOptions {
                unix_root: cfg!(unix).then(|| root.to_path_buf()),
            },
            ConfigSources::empty(),
        )
        .unwrap();
    runtime
}

fn connect(
    runtime: &Runtime,
    release: &ContractRelease,
    name: &str,
    relay: &Relay,
    explicit: bool,
) -> Result<Client, Error> {
    runtime.connect(
        release.expected_route(name).unwrap(),
        if explicit {
            Connect::ExplicitRelay {
                relay_url: relay.url.clone(),
            }
        } else {
            Connect::RelayAware
        },
    )
}

fn call_owner(client: &Client, owner: &str) {
    assert_eq!(
        client.call_owned("echo", &encode("request")).unwrap(),
        encode(owner)
    );
}

fn conflict(error: Error, name: &str, a: &Relay, b: &Relay) {
    let Error::Semantic(error) = error else {
        panic!("expected semantic route conflict: {error:?}")
    };
    assert_eq!(error.code, ErrorCode::RouteConflict, "{error:?}");
    assert_eq!(error.details["route"], name);
    assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
    let registrations: Vec<Value> = serde_json::from_str(&error.details["registrations"]).unwrap();
    assert_eq!(registrations.len(), 2);
    let ids: BTreeSet<_> = registrations
        .iter()
        .map(|item| item["relay_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, BTreeSet::from([a.id.as_str(), b.id.as_str()]));
    for item in registrations {
        let relay = if item["relay_id"] == a.id { a } else { b };
        assert_eq!(item["relay_url"], relay.url);
        assert_eq!(item["relay_status"], "Alive");
        assert!(item["registered_at"].as_f64().unwrap().is_finite());
    }
}

// ADR 0004: "已有代理继续用它当前那次注册" while new acquisitions report
// every live registration; after one owner withdraws, its proxy follows.
#[test]
fn real_c3_join_conflict_preserves_old_proxies_and_clears_after_withdrawal() {
    for explicit in [false, true] {
        run_case(explicit);
    }
}

fn run_case(explicit: bool) {
    let directory = test_directory("g7", explicit, "withdraw");
    let a = Relay::start("a", &directory);
    let b = Relay::start("b", &directory);
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let name = format!(
        "mesh-conflict-{}-{}",
        std::process::id(),
        if explicit { "http" } else { "local" }
    );
    let server_a = runtime(&a.root, &a, "mesh-host-a");
    let server_b = runtime(&b.root, &b, "mesh-host-b");
    let host_a = server_a.host(HostOptions::default()).unwrap();
    let host_b = server_b.host(HostOptions::default()).unwrap();
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));
    let _registration_a = register(&host_a, &release, &name, "A", &calls_a);
    let mut registration_b = register(&host_b, &release, &name, "B", &calls_b);
    let client_a = runtime(&a.root, &a, "mesh-client-a");
    let client_b = runtime(&b.root, &b, "mesh-client-b");
    let old_a = connect(&client_a, &release, &name, &a, explicit).unwrap();
    let old_b = connect(&client_b, &release, &name, &b, explicit).unwrap();
    let initial_path = if explicit {
        ObservedPath::ExplicitRelay
    } else {
        ObservedPath::RelayAwareLocalIpc
    };
    assert_eq!(old_a.observed_path(), initial_path);
    assert_eq!(old_b.observed_path(), initial_path);
    let carrier_a = (!explicit).then(|| old_a.connection_observation().unwrap());
    let carrier_b = (!explicit).then(|| old_b.connection_observation().unwrap());
    if let (Some(carrier_a), Some(carrier_b)) = (&carrier_a, &carrier_b) {
        assert_ne!(carrier_a.connection_id(), carrier_b.connection_id());
        assert!(carrier_a.snapshot().is_none());
        assert!(carrier_b.snapshot().is_none());
    }
    call_owner(&old_a, "A");
    call_owner(&old_b, "B");
    assert_eq!(old_a.observed_path(), initial_path);
    assert_eq!(old_b.observed_path(), initial_path);
    if let (Some(carrier_a), Some(carrier_b)) = (&carrier_a, &carrier_b) {
        assert_eq!(
            old_a.connection_observation().unwrap().connection_id(),
            carrier_a.connection_id()
        );
        assert_eq!(
            old_b.connection_observation().unwrap().connection_id(),
            carrier_b.connection_id()
        );
    }
    join_and_exchange(&a, &b);
    for relay in [&a, &b] {
        wait_until(|| {
            let listed = routes(relay, &name);
            listed.len() == 2
                && listed
                    .iter()
                    .all(|item| item["conflict"] == true && item["relay_status"] == "Alive")
        });
    }
    assert_conflict_log(&a, &name, "detected", &b);
    assert_conflict_log(&b, &name, "detected", &a);
    conflict(
        connect(&client_a, &release, &name, &a, explicit).unwrap_err(),
        &name,
        &a,
        &b,
    );
    conflict(
        connect(&client_b, &release, &name, &b, explicit).unwrap_err(),
        &name,
        &a,
        &b,
    );
    call_owner(&old_a, "A");
    call_owner(&old_b, "B");
    assert_eq!(old_a.observed_path(), initial_path);
    assert_eq!(old_b.observed_path(), initial_path);
    if let (Some(carrier_a), Some(carrier_b)) = (&carrier_a, &carrier_b) {
        assert_eq!(
            old_a.connection_observation().unwrap().connection_id(),
            carrier_a.connection_id()
        );
        assert_eq!(
            old_b.connection_observation().unwrap().connection_id(),
            carrier_b.connection_id()
        );
    }
    assert_eq!(calls_a.load(Ordering::SeqCst), 2);
    assert_eq!(calls_b.load(Ordering::SeqCst), 2);
    registration_b.close().unwrap();
    for relay in [&a, &b] {
        wait_until(|| {
            let listed = routes(relay, &name);
            listed.len() == 1 && listed[0]["relay_id"] == a.id && listed[0]["conflict"] == false
        });
    }
    assert_conflict_log(&a, &name, "resolved", &b);
    assert_conflict_log(&b, &name, "resolved", &a);
    let error = host_b
        .register(definition(&release, &name, "B", &calls_b))
        .unwrap_err();
    let Error::Lifecycle(LifecycleError::RegisterFailure(receipt)) = error else {
        panic!("expected registration failure receipt: {error:?}")
    };
    assert_eq!(receipt.route_name, name);
    assert_eq!(receipt.status_code, Some(409));
    assert_eq!(receipt.failure_source, "relay_prepare");
    let body: Value = serde_json::from_str(&receipt.error_message).unwrap();
    assert_eq!(body["code"], 703, "{}", receipt.error_message);
    assert_eq!(body["name"], "ResourceAlreadyRegistered");
    assert_eq!(body["details"]["name"], name);
    let semantic = receipt.semantic_error.as_ref().expect("canonical C2 error");
    assert_eq!(semantic.code, ErrorCode::ResourceAlreadyRegistered);
    assert_eq!(semantic.message, body["message"]);
    assert_eq!(semantic.details["name"], name);
    assert_eq!(
        body["details"]["registrations"],
        semantic.details["registrations"]
    );
    let registrations: Vec<Value> =
        serde_json::from_str(&semantic.details["registrations"]).unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["relay_id"], a.id);
    assert_eq!(registrations[0]["relay_url"], a.url);
    assert_eq!(registrations[0]["relay_status"], "Alive");
    assert!(
        registrations[0]["registered_at"]
            .as_f64()
            .unwrap()
            .is_finite()
    );
    let rollback = receipt.rollback.as_ref().expect("registration rollback");
    assert_eq!(rollback.route_name, name);
    assert!(!rollback.local_removed);
    assert!(rollback.active_drained);
    assert_eq!(rollback.closed_reason, "registration_rollback");
    assert!(rollback.close_error.is_none());
    call_owner(&old_a, "A");
    let error = old_b.call_owned("echo", &encode("request")).unwrap_err();
    let Error::Semantic(semantic) = error else {
        panic!("unexpected old-proxy error: {error:?}")
    };
    assert_eq!(
        semantic.code,
        if explicit {
            ErrorCode::ResourceNotFound
        } else {
            ErrorCode::ResourceRemoved
        }
    );
    assert_eq!(
        semantic
            .details
            .get(if explicit {
                "dispatch_phase"
            } else {
                "transport_phase"
            })
            .map(String::as_str),
        Some("pre_dispatch")
    );
    assert!(!semantic.details.contains_key("route_uid"));
    call_owner(&old_b, "A");
    assert_eq!(old_a.observed_path(), initial_path);
    assert_eq!(
        old_b.observed_path(),
        if explicit {
            ObservedPath::ExplicitRelay
        } else {
            ObservedPath::RelayAwareRelay
        }
    );
    if let Some(carrier_a) = &carrier_a {
        assert_eq!(
            old_a.connection_observation().unwrap().connection_id(),
            carrier_a.connection_id()
        );
        assert!(old_b.connection_observation().is_err());
    }
    assert_eq!(calls_b.load(Ordering::SeqCst), 2);
    assert_eq!(calls_a.load(Ordering::SeqCst), 4);
    drop((old_a, old_b, registration_b));
    retire_clients(&client_a);
    retire_clients(&client_b);
    if let Some(connection) = &carrier_a {
        wait_until(|| connection.snapshot().is_some());
    }
    if let Some(connection) = &carrier_b {
        wait_until(|| connection.snapshot().is_some());
    }
    let outcome_a = host_a.shutdown_with_timeout(WAIT);
    let outcome_b = host_b.shutdown_with_timeout(WAIT);
    assert!(outcome_a.runtime_barrier_error.is_none(), "{outcome_a:?}");
    assert!(outcome_b.runtime_barrier_error.is_none(), "{outcome_b:?}");
    for relay in [&a, &b] {
        wait_until(|| routes(relay, &name).is_empty());
    }
    drop((
        _registration_a,
        host_a,
        host_b,
        client_a,
        client_b,
        server_a,
        server_b,
        a,
        b,
    ));
    std::fs::remove_dir_all(directory).unwrap();
}

fn unreachable_peer_listener() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (url, listener)
}

fn join_without_reachable_peer_urls(a: &Relay, b: &Relay, name: &str) -> Vec<TcpListener> {
    // Only the peer contact URLs are partitioned. Both c3 processes and the
    // resource's real IPC Host remain live while failure detection runs.
    let mut unreachable_listeners = Vec::new();
    for (source, target) in [(a, b), (b, a)] {
        let (url, listener) = unreachable_peer_listener();
        unreachable_listeners.push(listener);
        let joined = post(
            &target.url,
            "/_peer/join",
            &envelope(
                source,
                json!({"type":"RelayJoin", "relay_id":source.id, "url":url}),
            ),
        );
        assert_eq!(joined["protocol_version"], 7, "{joined}");
    }
    let diff = post(
        &a.url,
        "/_peer/digest",
        &envelope(b, json!({"type":"DigestExchange", "digest":[]})),
    );
    assert_eq!(diff["message"]["type"], "DigestDiff", "{diff}");
    assert_eq!(diff["message"]["entries"].as_array().unwrap().len(), 1);
    assert!(post(&b.url, "/_peer/digest", &diff).is_null());
    wait_until(|| routes(b, name).len() == 1);
    unreachable_listeners
}

fn restore_peer_urls(a: &Relay, b: &Relay) {
    for (source, target) in [(a, b), (b, a)] {
        let joined = post(
            &target.url,
            "/_peer/join",
            &envelope(
                source,
                json!({"type":"RelayJoin", "relay_id":source.id, "url":source.url}),
            ),
        );
        assert_eq!(joined["protocol_version"], 7, "{joined}");
    }
    wait_until(|| {
        peer_status(a, b).as_deref() == Some("Alive")
            && peer_status(b, a).as_deref() == Some("Alive")
    });
}

// ADR 0004: "联系不上不等于已经没了". Contracts and routing says a Suspect or
// Dead relay "keeps its registrations" until withdrawal or force-leave.
#[test]
fn real_c3_local_suspect_and_dead_names_recover_after_owner_withdrawal() {
    run_unavailable_case(false, false);
}

#[test]
fn real_c3_local_suspect_and_dead_names_recover_after_force_leave() {
    run_unavailable_case(false, true);
}

#[test]
fn real_c3_http_suspect_and_dead_names_recover_after_owner_withdrawal() {
    run_unavailable_case(true, false);
}

#[test]
fn real_c3_http_suspect_and_dead_names_recover_after_force_leave() {
    run_unavailable_case(true, true);
}

fn run_unavailable_case(explicit: bool, force_leave: bool) {
    let release_method = if force_leave {
        "force-leave"
    } else {
        "withdraw"
    };
    let directory = test_directory("g8", explicit, release_method);
    let a = Relay::start("a", &directory);
    let b = Relay::start("b", &directory);
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let name = format!(
        "mesh-unavailable-{}-{}-{}",
        std::process::id(),
        if explicit { "http" } else { "local" },
        release_method,
    );
    let server_a = runtime(&a.root, &a, &format!("mesh-unavailable-host-a-{name}"));
    let server_b = runtime(&b.root, &b, &format!("mesh-unavailable-host-b-{name}"));
    let host_a = server_a.host(HostOptions::default()).unwrap();
    let host_b = server_b.host(HostOptions::default()).unwrap();
    let identity_a = server_a.ensure_server().unwrap();
    let identity_b = server_b.ensure_server().unwrap();
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));
    let mut registration_a = register(&host_a, &release, &name, "A", &calls_a);
    let client_b = runtime(&b.root, &b, &format!("mesh-unavailable-client-b-{name}"));
    let unreachable_listeners = join_without_reachable_peer_urls(&a, &b, &name);

    // Gate 8: a name held only by a Suspect or Dead peer yields typed 702 on
    // acquisition and typed 703, with the holder details, on registration.
    for status in ["Suspect", "Dead"] {
        wait_until(|| peer_status(&b, &a).as_deref() == Some(status));
        let owner_url = peer_url(&b, &a);
        let listed = routes(&b, &name);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["relay_id"], a.id);
        assert_eq!(listed[0]["relay_url"], owner_url);
        assert_eq!(listed[0]["relay_status"], status);
        unavailable(
            connect(&client_b, &release, &name, &b, explicit).unwrap_err(),
            &name,
            &a,
            &owner_url,
            status,
        );
        held_registration(
            host_b
                .register(definition(&release, &name, "B", &calls_b))
                .unwrap_err(),
            &name,
            &a,
            &owner_url,
            status,
        );
        assert_eq!(calls_a.load(Ordering::SeqCst), 0);
        assert_eq!(calls_b.load(Ordering::SeqCst), 0);
    }

    if force_leave {
        assert_eq!(
            post(&b.url, "/_admin/force-leave", &json!({"relay_id":a.id}))["force_left"],
            a.id
        );
    } else {
        drop(unreachable_listeners);
        restore_peer_urls(&a, &b);
        registration_a.close().unwrap();
    }
    wait_until(|| routes(&b, &name).is_empty());
    let mut registration_b = register(&host_b, &release, &name, "B", &calls_b);
    if force_leave {
        let original = routes(&a, &name);
        assert_eq!(original.len(), 1);
        assert_eq!(original[0]["relay_id"], a.id);
    }
    let recovered = connect(&client_b, &release, &name, &b, explicit).unwrap();
    assert_eq!(
        recovered.observed_path(),
        if explicit {
            ObservedPath::ExplicitRelay
        } else {
            ObservedPath::RelayAwareLocalIpc
        }
    );
    let recovered_connection = (!explicit).then(|| recovered.connection_observation().unwrap());
    call_owner(&recovered, "B");
    if let Some(connection) = &recovered_connection {
        assert_eq!(
            recovered.connection_observation().unwrap().connection_id(),
            connection.connection_id()
        );
    }
    assert_eq!(calls_a.load(Ordering::SeqCst), 0);
    assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    registration_b.close().unwrap();
    wait_until(|| routes(&b, &name).is_empty());
    if force_leave {
        registration_a.close().unwrap();
    }
    let actual_path = recovered.observed_path();
    drop((recovered, registration_a, registration_b));
    retire_clients(&client_b);
    if let Some(connection) = &recovered_connection {
        wait_until(|| connection.snapshot().is_some());
    }
    let outcome_a = host_a.shutdown_with_timeout(WAIT);
    let outcome_b = host_b.shutdown_with_timeout(WAIT);
    assert!(outcome_a.runtime_barrier_error.is_none(), "{outcome_a:?}");
    assert!(outcome_b.runtime_barrier_error.is_none(), "{outcome_b:?}");
    assert!(outcome_a.route_close_error.is_none() && outcome_a.relay_errors.is_empty());
    assert!(outcome_b.route_close_error.is_none() && outcome_b.relay_errors.is_empty());
    assert!(host_a.lifecycle_snapshot().finished && host_b.lifecycle_snapshot().finished);
    println!(
        "C2_ROUTE_IDENTITY_G8 {}",
        json!({
            "release": release_method, "actual_path":format!("{actual_path:?}"),
            "old_identity":{"server_id":identity_a.server_id,"server_instance_id":identity_a.server_instance_id},
            "new_identity":{"server_id":identity_b.server_id,"server_instance_id":identity_b.server_instance_id},
            "callbacks":{"old":calls_a.load(Ordering::SeqCst),"new":calls_b.load(Ordering::SeqCst)},
            "physical_client_connections":usize::from(!explicit),"reconnects":0,
            "cleanup":{"clients_retired":true,"hosts_finished":true,
                       "local_connection_closed":recovered_connection.as_ref().map(|connection|connection.snapshot().is_some())},
        })
    );
    drop((host_a, host_b, client_b, server_a, server_b, a, b));
    std::fs::remove_dir_all(directory).unwrap();
}
