use std::collections::BTreeSet;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use c_two::endpoint::{LocalEndpointContext, LocalEndpointOptions};
pub use c_two::generated::ErrorCode;
use c_two::generated::{C2Error, EncodedClient, EncodedService, MethodAccess, MethodDefinition};
use c_two::observe::{ConnectionEvent, ConnectionEvents, ConnectionSide, ObservedPath};
use c_two::{
    Client, ConfigSources, Connect, ContractRelease, Error, Host, HostOptions, Registration,
    Runtime, RuntimeOptions, ServiceDefinition,
};
use c2_http::relay::{RelayConfig, RelayServer};
use fastdb::{BuildPolicy, Builder, CompiledSpec};

pub const WAIT: Duration = Duration::from_secs(20);
static NEXT: AtomicUsize = AtomicUsize::new(0);

// Same portable record contract exercised by public_api.rs; this fixture's
// response carries resource-side instance evidence on HTTP as well as IPC.
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

pub fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

pub fn observe_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "native state did not settle");
        std::thread::yield_now();
    }
}

pub fn assert_semantic(error: Error, code: ErrorCode) {
    let Error::Semantic(error) = error else {
        panic!("expected {code:?}: {error:?}")
    };
    assert_eq!(error.code, code, "{error:?}");
}

pub fn expected(name: &str) -> c_two::contract::ExpectedRouteContract {
    ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route(name)
        .unwrap()
}

pub fn encode(value: &str) -> Vec<u8> {
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

fn decode(bytes: &[u8]) -> String {
    let spec = CompiledSpec::compile(SPEC).unwrap();
    let payload = c_two::generated::open_owned(&spec, bytes).unwrap();
    payload
        .entry_view(0)
        .unwrap()
        .at(0)
        .unwrap()
        .acquire()
        .unwrap()
        .str()
        .unwrap()
        .to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    Local,
    Http,
}

impl PathKind {
    pub fn observed(self) -> ObservedPath {
        match self {
            Self::Direct => ObservedPath::DirectIpc,
            Self::Local => ObservedPath::RelayAwareLocalIpc,
            Self::Http => ObservedPath::RelayAwareRelay,
        }
    }
}

struct Resource {
    calls: AtomicUsize,
    identity: serde_json::Value,
    label: String,
    gate: Gate,
}

#[derive(Default)]
pub struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    pub fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let (released, _) = self
            .changed
            .wait_timeout_while(self.released.lock().unwrap(), WAIT, |released| !*released)
            .unwrap();
        let is_released = *released;
        drop(released);
        assert!(is_released, "resource barrier was not released");
    }
}

impl EncodedService for Resource {
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        if index == 0 {
            return Ok(Vec::new());
        }
        assert_eq!(index, 1);
        let command = decode(&request);
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if command == "blocked" {
            self.gate.wait();
        }
        if command == "fail" {
            return Err(C2Error::new(
                ErrorCode::ResourceFunctionExecuting,
                "resource failed after entering",
            ));
        }
        Ok(encode(
            &serde_json::json!({
                "identity": self.identity,
                "resource": self.label,
                "calls": calls,
                "command": command,
            })
            .to_string(),
        ))
    }
}

pub struct Registered {
    pub registration: Registration,
    resource: Arc<Resource>,
}

impl Registered {
    pub fn calls(&self) -> usize {
        self.resource.calls.load(Ordering::SeqCst)
    }
    pub fn label(&self) -> &str {
        &self.resource.label
    }
    pub fn evidence(&self) -> Evidence {
        Evidence(self.resource.clone())
    }
}

pub struct Evidence(Arc<Resource>);
impl Evidence {
    pub fn calls(&self) -> usize {
        self.0.calls.load(Ordering::SeqCst)
    }
    pub fn release(&self) {
        self.0.gate.release();
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.resource.gate.release();
        let outcome = self.registration.outcome();
        println!(
            "C2_PERSISTENT_RESOURCE {}",
            serde_json::json!({"server_id":outcome.server_id,"server_instance_id":outcome.server_instance_id,"resource":self.resource.label,"business_calls":self.calls()})
        );
    }
}

impl Drop for Evidence {
    fn drop(&mut self) {
        self.0.gate.release();
    }
}

enum FixtureRelay {
    Embedded(RelayServer),
    Process(std::process::Child),
}

impl FixtureRelay {
    fn c3(root: &Path, idle: u64, seeds: &[String]) -> (Self, String) {
        use std::process::{Command, Stdio};
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
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
            "build c3 first or set CTWO_TEST_C3_BIN: {}",
            binary.display()
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{socket}");
        let mut command = Command::new(binary);
        command.args([
            "relay",
            "--bind",
            &socket.to_string(),
            "--advertise-url",
            &url,
            "--idle-timeout",
            &idle.to_string(),
        ]);
        if !seeds.is_empty() {
            command.args(["--seeds", &seeds.join(",")]);
        }
        #[cfg(unix)]
        command.arg("--ipc-root").arg(root);
        let child = command
            .env_remove("C2_IPC_ROOT")
            .env_remove("C2_RELAY_ANCHOR_ADDRESS")
            .stdin(Stdio::null())
            .stdout(
                std::fs::File::create(root.join(format!("{}.stdout.log", unique("c3")))).unwrap(),
            )
            .stderr(
                std::fs::File::create(root.join(format!("{}.stderr.log", unique("c3")))).unwrap(),
            )
            .spawn()
            .unwrap();
        let mut relay = Self::Process(child);
        let deadline = Instant::now() + WAIT;
        loop {
            let Self::Process(child) = &mut relay else {
                unreachable!()
            };
            assert!(
                child.try_wait().unwrap().is_none(),
                "c3 exited; inspect {}",
                root.display()
            );
            if let Ok(mut stream) = TcpStream::connect_timeout(&socket, Duration::from_millis(100))
            {
                stream.set_read_timeout(Some(WAIT)).unwrap();
                stream
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                if response.starts_with("HTTP/1.1 200 ") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "c3 readiness: {}",
                root.display()
            );
            std::thread::yield_now();
        }
        (relay, url)
    }

    fn stop(&mut self) -> Result<(), String> {
        match self {
            Self::Embedded(relay) => relay.stop(),
            Self::Process(child) => {
                if child
                    .try_wait()
                    .map_err(|error| error.to_string())?
                    .is_none()
                {
                    child.kill().map_err(|error| error.to_string())?;
                }
                child.wait().map_err(|error| error.to_string())?;
                Ok(())
            }
        }
    }
}

impl Drop for FixtureRelay {
    fn drop(&mut self) {
        if matches!(self, Self::Process(_)) {
            let _ = self.stop();
        }
    }
}

pub struct Fixture {
    pub path: PathKind,
    pub root: PathBuf,
    pub server: Runtime,
    pub host: Host,
    pub client_runtime: Runtime,
    pub client_events: ConnectionEvents,
    pub server_events: ConnectionEvents,
    outgoing: Mutex<BTreeSet<u64>>,
    accepted: Mutex<Vec<ConnectionEvent>>,
    relay: Option<FixtureRelay>,
    secondary_relay: Option<FixtureRelay>,
    anchor: Option<HttpAnchor>,
    finished: bool,
}

pub fn runtime(root: &Path, server_id: String, relay: Option<String>, idle: f64) -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id),
        relay_anchor_address: relay,
        use_process_relay_anchor: false,
        client_ipc_overrides: Some(c2_config::ClientIpcConfigOverrides {
            client_idle_timeout_secs: Some(idle),
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_local_endpoint_with_sources(
            LocalEndpointOptions {
                unix_root: cfg!(unix).then(|| root.to_path_buf()),
            },
            ConfigSources::empty(),
        )
        .unwrap();
    runtime
}

impl Fixture {
    pub fn new(path: PathKind, idle: f64, relay_idle: u64) -> Self {
        Self::new_inner(path, idle, relay_idle, false)
    }

    pub fn with_c3() -> Self {
        Self::new_inner(PathKind::Http, 0.0, 0, true)
    }

    pub fn with_c3_path(path: PathKind) -> Self {
        assert_ne!(path, PathKind::Direct);
        Self::new_inner(path, 0.0, 0, true)
    }

    pub fn direct_with_unavailable_relay() -> Self {
        let mut fixture = Self::new(PathKind::Direct, 0.0, 0);
        fixture.client_runtime = runtime(
            &fixture.root,
            unique("persistent-direct-client"),
            Some("http://127.0.0.1:0".into()),
            0.0,
        );
        fixture.client_events = fixture.client_runtime.connection_events();
        fixture
    }

    fn new_inner(path: PathKind, idle: f64, relay_idle: u64, c3: bool) -> Self {
        #[cfg(unix)]
        let root = Path::new("/tmp").join(unique("c2-persist"));
        #[cfg(windows)]
        let root = std::env::temp_dir().join(unique("c2-persist"));
        let mut dir = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(&root).unwrap();
        #[cfg(unix)]
        let context = LocalEndpointContext::with_unix_root(&root).unwrap();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let (relay, relay_url) = if c3 {
            let (relay, url) = FixtureRelay::c3(&root, relay_idle, &[]);
            (Some(relay), Some(url))
        } else if path != PathKind::Direct {
            let mut attempt = 0;
            let (relay, url) = loop {
                attempt += 1;
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let address = listener.local_addr().unwrap();
                drop(listener);
                let url = format!("http://{address}");
                match RelayServer::start_with_context(
                    RelayConfig {
                        bind: address.to_string(),
                        advertise_url: url.clone(),
                        idle_timeout_secs: relay_idle,
                        anti_entropy_interval: Duration::ZERO,
                        heartbeat_interval: Duration::ZERO,
                        ..Default::default()
                    },
                    context.clone(),
                ) {
                    Ok(relay) => break (relay, url),
                    // A parallel test can claim the port after the probe closes.
                    Err(error)
                        if attempt < 8
                            && error.starts_with("Relay failed to start: Failed to bind ") => {}
                    Err(error) => panic!("relay starts: {error}"),
                }
            };
            (Some(FixtureRelay::Embedded(relay)), Some(url))
        } else {
            (None, None)
        };
        let anchor = (path == PathKind::Http).then(|| HttpAnchor::new(relay_url.as_ref().unwrap()));
        let server = runtime(&root, unique("persistent-host"), relay_url.clone(), 0.0);
        let server_events = server.connection_events();
        let host_options = if relay.is_some() {
            HostOptions::default()
        } else {
            HostOptions::default().without_relay()
        };
        let host = server.host(host_options).unwrap();
        let client_runtime = runtime(
            &root,
            unique("persistent-client"),
            anchor.as_ref().map(|a| a.url.clone()).or(relay_url),
            idle,
        );
        let client_events = client_runtime.connection_events();
        Self {
            path,
            root,
            server,
            host,
            client_runtime,
            client_events,
            server_events,
            outgoing: Mutex::new(BTreeSet::new()),
            accepted: Mutex::new(Vec::new()),
            relay,
            secondary_relay: None,
            anchor,
            finished: false,
        }
    }

    pub fn register(&self, name: &str) -> Registered {
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        let identity = self.server.ensure_server().unwrap();
        let resource = Arc::new(Resource {
            calls: AtomicUsize::new(0),
            label: unique("resource"),
            gate: Gate::default(),
            identity: serde_json::json!({"server_id":identity.server_id,"server_instance_id":identity.server_instance_id}),
        });
        let definition = ServiceDefinition::new(
            &release,
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
            resource.clone(),
        )
        .unwrap();
        let registration = self.host.register(definition).unwrap();
        Registered {
            registration,
            resource,
        }
    }

    pub fn connect_name(&self, name: &str) -> Result<Client, Error> {
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        self.client_runtime
            .connect(release.expected_route(name).unwrap(), self.connect_mode())
    }

    pub fn connect_mode(&self) -> Connect {
        if self.path == PathKind::Direct {
            Connect::DirectIpc {
                address: self.host.local_endpoint().address().into(),
            }
        } else {
            Connect::RelayAware
        }
    }

    pub fn connect(&self, route: &Registered) -> Client {
        let client = self.connect_name(route.registration.route_name()).unwrap();
        assert_eq!(client.observed_path(), self.path.observed());
        let registered = route.registration.outcome();
        if let Some(identity) = client.ipc_server_identity() {
            assert_eq!(identity.server_id, registered.server_id);
            assert_eq!(identity.server_instance_id, registered.server_instance_id);
        }
        println!(
            "C2_PERSISTENT_ACQUIRE {}",
            serde_json::json!({"path":format!("{:?}", client.observed_path()),"server_id":registered.server_id,"server_instance_id":registered.server_instance_id,"resource":route.resource.label,"physical_connections":self.outgoing_ids()})
        );
        client
    }

    pub fn call(&self, client: &Client, route: &Registered, command: &str) {
        let bytes = client.call_owned("echo", &encode(command)).unwrap();
        self.assert_response(client, route, command, &bytes);
    }

    pub fn assert_response(
        &self,
        client: &Client,
        route: &Registered,
        command: &str,
        bytes: &[u8],
    ) {
        let response: serde_json::Value = serde_json::from_str(&decode(bytes)).unwrap();
        let registered = route.registration.outcome();
        assert_eq!(response["identity"]["server_id"], registered.server_id);
        assert_eq!(
            response["identity"]["server_instance_id"],
            registered.server_instance_id
        );
        assert_eq!(response["resource"], route.resource.label);
        assert_eq!(response["command"], command);
        println!(
            "C2_PERSISTENT {}",
            serde_json::json!({"path":format!("{:?}", client.observed_path()),"response":response,"physical_connections":self.outgoing_ids()})
        );
    }

    pub fn outgoing_ids(&self) -> BTreeSet<u64> {
        let mut ids = self.outgoing.lock().unwrap();
        for event in drain_events(&self.client_events) {
            if let ConnectionEvent::Opened(observation) = event {
                assert_eq!(observation.side(), ConnectionSide::Client);
                ids.insert(observation.connection_id());
            }
        }
        ids.clone()
    }

    pub fn accepted_events(&self) -> Vec<ConnectionEvent> {
        let mut events = self.accepted.lock().unwrap();
        events.extend(drain_events(&self.server_events));
        events.clone()
    }

    pub fn live_accepted(&self) -> Vec<c_two::observe::ConnectionObservation> {
        self.accepted_events()
            .into_iter()
            .filter_map(|event| match event {
                ConnectionEvent::Opened(observation) if observation.snapshot().is_none() => {
                    Some(observation)
                }
                _ => None,
            })
            .collect()
    }

    pub fn relay_url(&self) -> Option<String> {
        self.server.relay_anchor_address_override()
    }

    pub fn add_c3_peer(&mut self) -> String {
        assert!(self.secondary_relay.is_none());
        let anchor = self.relay_url().expect("primary c3 relay");
        let (relay, url) = FixtureRelay::c3(&self.root, 0, &[anchor]);
        self.secondary_relay = Some(relay);
        url
    }

    pub fn assert_paths(&self, acquisitions: u64) {
        let counters = self.client_runtime.path_counters();
        assert_eq!(
            counters.direct_ipc(),
            if self.path == PathKind::Direct {
                acquisitions
            } else {
                0
            }
        );
        assert_eq!(
            counters.relay_aware_local_ipc(),
            if self.path == PathKind::Local {
                acquisitions
            } else {
                0
            }
        );
        assert_eq!(
            counters.relay_aware_relay(),
            if self.path == PathKind::Http {
                acquisitions
            } else {
                0
            }
        );
        assert_eq!(counters.explicit_relay(), 0);
    }

    fn cleanup(&mut self) {
        let receipt = self
            .client_runtime
            .begin_session_retirement(
                c2_core::RuntimeShutdownTarget::ClientsOnly,
                Instant::now() + WAIT,
            )
            .unwrap();
        let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
        assert!(completed, "{outcome:?}");
        assert!(outcome.ipc_client_close_error.is_none(), "{outcome:?}");
        let outcome = self.host.shutdown_with_timeout(WAIT);
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        assert!(outcome.route_close_error.is_none(), "{outcome:?}");
        assert!(outcome.relay_errors.is_empty(), "{outcome:?}");
        assert!(self.host.lifecycle_snapshot().finished);
        if let Some(relay) = &mut self.secondary_relay {
            relay.stop().unwrap();
        }
        if let Some(relay) = &mut self.relay {
            relay.stop().unwrap();
        }
        self.anchor.take();
        if let Ok(snapshot) = self.client_runtime.call_execution_snapshot() {
            assert_eq!(snapshot.used_operations, 0);
        }
        if let Some(stats) = self.client_runtime.memory_stats().payload_buffer {
            assert_eq!(stats.used_bytes, 0);
        }
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let entry = entry.unwrap();
            assert_ne!(
                entry.path().extension().and_then(|e| e.to_str()),
                Some("lease")
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                assert!(!entry.file_type().unwrap().is_socket());
            }
        }
        println!(
            "C2_PERSISTENT_CLEANUP path={:?} finished=true physical_connections={:?}",
            self.path,
            self.outgoing_ids()
        );
        let accepted = self.accepted_events();
        let opened = accepted
            .iter()
            .filter(|event| matches!(event, ConnectionEvent::Opened(_)))
            .count();
        let closed = accepted
            .iter()
            .filter(|event| matches!(event, ConnectionEvent::Closed(_)))
            .count();
        assert_eq!(
            opened, closed,
            "accepted physical carriers must all retire: {accepted:?}"
        );
        println!(
            "C2_PERSISTENT_SERVER_CONNECTIONS path={:?} opened={opened} closed={closed}",
            self.path
        );
        self.finished = true;
        std::fs::remove_dir_all(&self.root).unwrap();
    }

    pub fn finish(mut self) {
        self.cleanup();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.finished {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.cleanup()));
            if let Err(error) = outcome {
                if !std::thread::panicking() {
                    std::panic::resume_unwind(error);
                }
                eprintln!(
                    "retained private root after incomplete cleanup: {}",
                    self.root.display()
                );
            }
        }
    }
}

pub fn drain_events(events: &ConnectionEvents) -> Vec<ConnectionEvent> {
    let mut values = Vec::new();
    loop {
        let mut future = std::pin::pin!(events.recv());
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(Ok(Some(event))) => values.push(event),
            Poll::Pending | Poll::Ready(Ok(None)) => return values,
            Poll::Ready(Err(error)) => panic!("connection event loss: {error}"),
        }
    }
}

// Discovery-only HTTP forwarding changes the existing namespace header to a
// different valid namespace. The real relay supplies all route authority and
// handles business POSTs. This selects relay-aware HTTP on Windows too, where
// constructing a second Unix root is unavailable.
struct HttpAnchor {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HttpAnchor {
    fn new(relay_url: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let relay = relay_url.strip_prefix("http://").unwrap().to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            for connection in listener.incoming() {
                let mut downstream = connection.unwrap();
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                downstream.set_read_timeout(Some(WAIT)).unwrap();
                let mut input = BufReader::new(downstream.try_clone().unwrap());
                let mut request = String::new();
                input.read_line(&mut request).unwrap();
                assert!(request.starts_with("GET /_resolve/"), "{request}");
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if !line
                        .to_ascii_lowercase()
                        .starts_with("x-c2-local-namespace:")
                        && !line.to_ascii_lowercase().starts_with("connection:")
                    {
                        request.push_str(&line);
                    }
                }
                request.push_str(&format!(
                    "x-c2-local-namespace: {}\r\nConnection: close\r\n\r\n",
                    "0".repeat(64)
                ));
                let mut upstream = TcpStream::connect(&relay).unwrap();
                upstream.set_read_timeout(Some(WAIT)).unwrap();
                upstream.write_all(request.as_bytes()).unwrap();
                let mut response = Vec::new();
                upstream.read_to_end(&mut response).unwrap();
                downstream.write_all(&response).unwrap();
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for HttpAnchor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.strip_prefix("http://").unwrap());
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

const STREAM_DESCRIPTOR: &[u8] = include_bytes!(
    "../../../../core/foundation/c2-contract/tests/fixtures/server-stream.contract.json"
);
const PROCESS_MARKER: &str = "C2_PERSISTENT_PROCESS ";

pub fn stream_expected(name: &str) -> c_two::contract::ExpectedRouteContract {
    ContractRelease::from_descriptor_json(STREAM_DESCRIPTOR)
        .unwrap()
        .expected_route(name)
        .unwrap()
}

pub fn wait_relay_owner(fixture: &Fixture, url: &str, instance: &str) {
    let context = fixture.server.local_endpoint_context().unwrap();
    let control = c2_http::client::RelayControlClient::new(url, false)
        .unwrap()
        .with_local_endpoint_context(&context);
    let expected = stream_expected("process-route");
    observe_until(|| {
        let routes = c2_core::native_client_executor()
            .unwrap()
            .block_on(control.resolve_matching_async(&expected))
            .unwrap_or_default();
        routes
            .iter()
            .any(|route| route.server_instance_id.as_deref() == Some(instance))
    });
}

pub fn wait_relay_target(fixture: &Fixture, url: &str, owner: &str) {
    let context = fixture.server.local_endpoint_context().unwrap();
    let control = c2_http::client::RelayControlClient::new(url, false)
        .unwrap()
        .with_local_endpoint_context(&context);
    let expected = stream_expected("process-route");
    observe_until(|| {
        let resolved = c2_core::native_client_executor()
            .unwrap()
            .block_on(control.resolve_matching_async(&expected));
        match resolved {
            Ok(routes) => routes.iter().any(|route| route.relay_url == owner),
            Err(c2_http::client::HttpError::ServerError(404, body)) => {
                let envelope = serde_json::from_str(&body)
                    .expect("relay resolve must return a structured error");
                let error = C2Error::from_envelope(envelope).unwrap();
                assert_eq!(error.code, ErrorCode::ResourceNotFound, "{error:?}");
                false
            }
            Err(error) => panic!("relay target lookup failed: {error:?}"),
        }
    });
}

pub fn wait_relay_absent(fixture: &Fixture, url: &str) {
    let context = fixture.server.local_endpoint_context().unwrap();
    let control = c2_http::client::RelayControlClient::new(url, false)
        .unwrap()
        .with_local_endpoint_context(&context);
    let expected = stream_expected("process-route");
    observe_until(|| {
        let routes = c2_core::native_client_executor()
            .unwrap()
            .block_on(control.resolve_matching_async(&expected))
            .unwrap_or_default();
        routes.is_empty()
    });
}

pub fn open_stream(client: &Client, command: &str) -> c_two::ResponseStream<fastdb::Payload> {
    let core = client
        .begin_stream("echo")
        .unwrap()
        .encode_vec(encode(command))
        .unwrap()
        .stream()
        .unwrap();
    c_two::ResponseStream::from_core(core, |response| {
        c_two::generated::open_owned(&CompiledSpec::compile(SPEC).unwrap(), response.bytes())
            .map_err(|error| {
                Error::Semantic(c_two::generated::fastdb_adapter_error(
                    c_two::generated::AdapterFailurePhase::ClientOutputDeserializing,
                    error,
                ))
            })
    })
}

pub fn stream_value(payload: &fastdb::Payload) -> serde_json::Value {
    serde_json::from_str(
        payload
            .entry_view(0)
            .unwrap()
            .at(0)
            .unwrap()
            .acquire()
            .unwrap()
            .str()
            .unwrap(),
    )
    .unwrap()
}

fn process_event(value: serde_json::Value) {
    println!("{PROCESS_MARKER}{value}");
    std::io::stdout().flush().unwrap();
}

struct ProcessProbe {
    identity: serde_json::Value,
    gate: Gate,
    opens: AtomicUsize,
    steps: AtomicUsize,
    finishes: AtomicUsize,
    unary: AtomicUsize,
}
struct ProcessService {
    probe: Arc<ProcessProbe>,
    label: String,
}
impl EncodedService for ProcessService {
    fn invoke(&self, index: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(index, 0);
        let calls = self.probe.unary.fetch_add(1, Ordering::SeqCst) + 1;
        process_event(serde_json::json!({"kind":"unary", "calls":calls, "resource":self.label}));
        Ok(Vec::new())
    }
}
impl c_two::generated::EncodedStreamService for ProcessService {
    fn open_stream(
        &self,
        index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn c_two::generated::EncodedStream>, C2Error> {
        assert_eq!(index, 1);
        let command = decode(&request);
        let calls = self.probe.opens.fetch_add(1, Ordering::SeqCst) + 1;
        process_event(
            serde_json::json!({"kind":"open", "calls":calls, "command":command, "resource":self.label}),
        );
        Ok(Box::new(ProcessIterator {
            probe: self.probe.clone(),
            label: self.label.clone(),
            command,
            cursor: 0,
        }))
    }
}
struct ProcessIterator {
    probe: Arc<ProcessProbe>,
    label: String,
    command: String,
    cursor: usize,
}
impl c_two::generated::EncodedStream for ProcessIterator {
    fn step(&mut self) -> Result<Option<Box<dyn c_two::generated::EncodedStreamItem>>, C2Error> {
        self.probe.steps.fetch_add(1, Ordering::SeqCst);
        process_event(
            serde_json::json!({"kind":"step", "index":self.cursor, "command":self.command}),
        );
        if self.command == "blocked" && self.cursor == 1 {
            self.probe.gate.wait();
        }
        if self.command == "fail" && self.cursor == 1 {
            return Err(C2Error::new(
                ErrorCode::ResourceFunctionExecuting,
                "resource step failed after one item",
            ));
        }
        if self.cursor == if self.command == "blocked" { 2 } else { 1 } {
            return Ok(None);
        }
        self.cursor += 1;
        Ok(Some(Box::new(ProcessItem(encode(
            &serde_json::json!({
                "identity":self.probe.identity,
                "resource":self.label,
                "index":self.cursor,
            })
            .to_string(),
        )))))
    }
    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        let finishes = self.probe.finishes.fetch_add(1, Ordering::SeqCst) + 1;
        process_event(serde_json::json!({"kind":"finish", "finishes":finishes}));
        Ok(())
    }
}
struct ProcessItem(Vec<u8>);
impl c_two::generated::EncodedStreamItem for ProcessItem {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        destination.copy_from_slice(&self.0[offset..offset + destination.len()]);
        Ok(())
    }
}

// Exact subprocess entry. The parent supplies every endpoint parameter;
// the child never selects a default Unix namespace.
pub fn run_resource_process() {
    let Some(root) = std::env::var_os("C2_PERSISTENT_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let id = std::env::var("C2_PERSISTENT_CHILD_ID").unwrap();
    let relay = std::env::var("C2_PERSISTENT_CHILD_RELAY")
        .ok()
        .filter(|url| !url.is_empty());
    let server = runtime(&root, id, relay.clone(), 0.0);
    let connections = server.connection_events();
    let host = server
        .host(if relay.is_some() {
            HostOptions::default()
        } else {
            HostOptions::default().without_relay()
        })
        .unwrap();
    let identity = server.ensure_server().unwrap();
    let probe = Arc::new(ProcessProbe {
        identity: serde_json::json!({"server_id":identity.server_id,"server_instance_id":identity.server_instance_id}),
        gate: Gate::default(),
        opens: AtomicUsize::new(0),
        steps: AtomicUsize::new(0),
        finishes: AtomicUsize::new(0),
        unary: AtomicUsize::new(0),
    });
    let release = ContractRelease::from_descriptor_json(STREAM_DESCRIPTOR).unwrap();
    let mut label = unique("process-resource");
    let mut registration = host
        .register(
            ServiceDefinition::new_stream(
                &release,
                release.reference(),
                "process-route",
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
                Arc::new(ProcessService {
                    probe: probe.clone(),
                    label: label.clone(),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let mut events = drain_events(&connections);
    // The test process captures its own internal registration evidence through
    // the existing IPC attestation path. Public relay resolution omits UID.
    let old_route_uid = c2_core::native_client_executor().unwrap().block_on(async {
        let mut attestor = c2_ipc::IpcClient::with_endpoint(
            host.local_endpoint_context()
                .endpoint(host.local_endpoint().address())
                .unwrap(),
            c2_config::ClientIpcConfig::default(),
        )
        .unwrap();
        attestor.connect().await.unwrap();
        attestor
            .attest_route_for_registration(&stream_expected("process-route"))
            .await
            .unwrap()
            .route_uid
    });
    process_event(
        serde_json::json!({"kind":"ready", "identity":probe.identity, "resource":label, "address":host.local_endpoint().address(), "credential":host.endpoint_credential().map(|credential| credential.to_json().unwrap()), "route_uid":old_route_uid}),
    );
    for line in std::io::stdin().lock().lines() {
        match line.unwrap().as_str() {
            "release" => probe.gate.release(),
            "unregister" => {
                let outcome = registration.close().unwrap();
                assert!(outcome.local_removed && outcome.close.active_drained);
                process_event(serde_json::json!({"kind":"unregistered", "resource":label}));
            }
            "register" => {
                label = unique("process-resource");
                registration = host
                    .register(
                        ServiceDefinition::new_stream(
                            &release,
                            release.reference(),
                            "process-route",
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
                            Arc::new(ProcessService {
                                probe: probe.clone(),
                                label: label.clone(),
                            }),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                process_event(serde_json::json!({"kind":"registered", "resource":label}));
            }
            "snapshot" => {
                events.extend(drain_events(&connections));
                let opened = events
                    .iter()
                    .filter(|event| matches!(event, ConnectionEvent::Opened(_)))
                    .count();
                let closed = events
                    .iter()
                    .filter(|event| matches!(event, ConnectionEvent::Closed(_)))
                    .count();
                process_event(
                    serde_json::json!({"kind":"snapshot", "opens":probe.opens.load(Ordering::SeqCst), "steps":probe.steps.load(Ordering::SeqCst), "finishes":probe.finishes.load(Ordering::SeqCst), "unary":probe.unary.load(Ordering::SeqCst), "physical_opened":opened, "physical_closed":closed}),
                );
            }
            "quit" => break,
            command => panic!("unknown resource process command: {command}"),
        }
    }
    probe.gate.release();
    let outcome = host.shutdown_with_timeout(WAIT);
    assert!(
        outcome.runtime_barrier_error.is_none()
            && outcome.route_close_error.is_none()
            && outcome.relay_errors.is_empty(),
        "{outcome:?}"
    );
    assert!(host.lifecycle_snapshot().finished);
    events.extend(drain_events(&connections));
    let mut opened = events
        .iter()
        .filter(|event| matches!(event, ConnectionEvent::Opened(_)))
        .count();
    let mut closed = events
        .iter()
        .filter(|event| matches!(event, ConnectionEvent::Closed(_)))
        .count();
    // The Host receipt fences listener closure and business drain. Physical
    // retirement publishes separately through the connection event source.
    let deadline = Instant::now() + WAIT;
    while closed < opened {
        match c2_core::recv_connection_event(
            &connections,
            Some(deadline.saturating_duration_since(Instant::now())),
        )
        .unwrap()
        {
            Some(c2_core::ConnectionEventWait::Event(ConnectionEvent::Opened(_))) => opened += 1,
            Some(c2_core::ConnectionEventWait::Event(ConnectionEvent::Closed(_))) => closed += 1,
            other => panic!(
                "resource process carriers did not all close within {WAIT:?}: opened={opened}, closed={closed}, receive={other:?}"
            ),
        }
    }
    assert_eq!(opened, closed, "resource process carriers must all close");
    process_event(
        serde_json::json!({"kind":"stopped", "finished":true, "physical_opened":opened, "physical_closed":closed, "opens":probe.opens.load(Ordering::SeqCst), "finishes":probe.finishes.load(Ordering::SeqCst)}),
    );
}

pub struct ResourceProcess {
    child: Option<std::process::Child>,
    reader: Option<std::thread::JoinHandle<()>>,
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    pub ready: serde_json::Value,
}
impl ResourceProcess {
    pub fn start(fixture: &Fixture, server_id: &str) -> Self {
        Self::start_with_relay(fixture, server_id, fixture.relay_url())
    }

    pub fn start_with_relay(fixture: &Fixture, server_id: &str, relay_url: Option<String>) -> Self {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "persistent_resource_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("C2_PERSISTENT_CHILD_ROOT", &fixture.root)
            .env("C2_PERSISTENT_CHILD_ID", server_id)
            .env("C2_PERSISTENT_CHILD_RELAY", relay_url.unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = events.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if let Some(start) = line.find(PROCESS_MARKER) {
                    let event: serde_json::Value =
                        serde_json::from_str(&line[start + PROCESS_MARKER.len()..]).unwrap();
                    println!("{PROCESS_MARKER}{event}");
                    output.lock().unwrap().push(event);
                }
            }
        });
        let mut process = Self {
            child: Some(child),
            reader: Some(reader),
            events,
            ready: serde_json::Value::Null,
        };
        observe_until(|| process.records("ready").len() == 1);
        process.ready = process.records("ready").pop().unwrap();
        process
    }
    pub fn records(&self, kind: &str) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == kind)
            .cloned()
            .collect()
    }
    pub fn send(&mut self, command: &str) {
        let stdin = self.child.as_mut().unwrap().stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
    }
    pub fn snapshot(&mut self) -> serde_json::Value {
        let before = self.records("snapshot").len();
        self.send("snapshot");
        observe_until(|| self.records("snapshot").len() > before);
        self.records("snapshot").pop().unwrap()
    }
    pub fn command_barrier(&mut self, command: &str, event: &str) -> serde_json::Value {
        let before = self.records(event).len();
        self.send(command);
        observe_until(|| self.records(event).len() > before);
        self.records(event).pop().unwrap()
    }
    pub fn connect(&self, fixture: &Fixture) -> Client {
        let expected = stream_expected("process-route");
        let mode = if fixture.path == PathKind::Direct {
            Connect::DirectIpc {
                address: self.ready["address"].as_str().unwrap().into(),
            }
        } else {
            Connect::RelayAware
        };
        let client = fixture.client_runtime.connect(expected, mode).unwrap();
        assert_eq!(client.observed_path(), fixture.path.observed());
        if let Some(identity) = client.ipc_server_identity() {
            assert_eq!(identity.server_id, self.ready["identity"]["server_id"]);
            assert_eq!(
                identity.server_instance_id,
                self.ready["identity"]["server_instance_id"]
            );
        }
        println!(
            "C2_PERSISTENT_PROCESS_ACQUIRE {}",
            serde_json::json!({"path":format!("{:?}", client.observed_path()), "identity":self.ready["identity"], "resource":self.ready["resource"], "physical_connections":fixture.outgoing_ids()})
        );
        client
    }
    pub fn assert_item(&self, payload: &fastdb::Payload, index: usize) {
        let value = stream_value(payload);
        assert_eq!(value["identity"], self.ready["identity"]);
        assert_eq!(value["resource"], self.ready["resource"]);
        assert_eq!(value["index"], index);
    }
    pub fn credential(&self) -> Option<c_two::endpoint::EndpointCredential> {
        self.ready["credential"]
            .as_str()
            .map(|json| c_two::endpoint::EndpointCredential::from_json(json).unwrap())
    }
    pub fn kill(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        if let Some(credential) = self.credential() {
            use c_two::endpoint::{EndpointReapResult, reap_endpoint};
            let outcome = reap_endpoint(credential.endpoint(), &credential);
            assert!(
                matches!(
                    outcome,
                    EndpointReapResult::Reaped
                        | EndpointReapResult::AlreadyAbsent
                        | EndpointReapResult::NotApplicable
                ),
                "{outcome:?}"
            );
            println!("C2_PERSISTENT_PROCESS_REAP {outcome:?}");
        }
    }
    pub fn stop(mut self) {
        self.send("quit");
        let mut child = self.child.take().unwrap();
        observe_until(|| child.try_wait().unwrap().is_some());
        assert!(child.wait().unwrap().success());
        self.reader.take().unwrap().join().unwrap();
        assert_eq!(self.records("stopped").len(), 1);
        assert_eq!(self.records("stopped")[0]["finished"], true);
    }
}
impl Drop for ResourceProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill();
        }
    }
}

pub fn assert_uncertain(error: Error) {
    println!("C2_PERSISTENT_UNCERTAIN {error:?}");
    match error {
        Error::Transport(error) => {
            assert_eq!(error.phase(), c2_core::TransportPhase::DispatchUncertain);
            assert!(!error.is_fallback_eligible());
        }
        Error::Semantic(error) => {
            assert_eq!(error.code, ErrorCode::ResourceUnavailable);
            assert_eq!(error.details["dispatch_phase"], "dispatch_uncertain");
        }
        error => panic!("expected dispatch-uncertain peer end: {error:?}"),
    }
}

pub fn race_old_owner_evidence(fixture: &Fixture, old: &ResourceProcess) {
    use c_two::endpoint::{EndpointReapResult, reap_endpoint};
    if let Some(credential) = old.credential() {
        let outcome = reap_endpoint(credential.endpoint(), &credential);
        if cfg!(unix) {
            // The new listener holds the live lease; maintenance refuses it
            // before an old credential can authorize any collection.
            assert!(matches!(outcome, EndpointReapResult::Busy), "{outcome:?}");
        } else {
            // Windows kernel-owned pipes have no filesystem object to reap;
            // the common old-proxy/instance checks still run on Windows.
            assert!(
                matches!(outcome, EndpointReapResult::NotApplicable),
                "{outcome:?}"
            );
        }
        println!("C2_PERSISTENT_OLD_CREDENTIAL {outcome:?}");
    }
    let Some(url) = fixture.relay_url() else {
        return;
    };
    let context = fixture.server.local_endpoint_context().unwrap();
    let old_relay_uid = old.ready["route_uid"].as_str().unwrap().to_owned();
    let old = &old.ready;
    let barrier = Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        let url_register = url.clone();
        let context_register = context.clone();
        let candidate_gate = barrier.clone();
        let candidate = scope.spawn(move || {
            let control = c2_http::client::RelayControlClient::new(&url_register, false)
                .unwrap()
                .with_local_endpoint_context(&context_register);
            let expected = stream_expected("process-route");
            candidate_gate.wait();
            let error = control
                .register(c2_http::client::RelayRegistration {
                    expected: &expected,
                    server_id: old["identity"]["server_id"].as_str().unwrap(),
                    server_instance_id: old["identity"]["server_instance_id"].as_str().unwrap(),
                    address: old["address"].as_str().unwrap(),
                    max_payload_size: c2_config::ServerIpcConfig::default().max_payload_size,
                })
                .unwrap_err();
            // Registration's identity attestation refuses this retired instance
            // with HTTP 502, before the resource's business callback.
            assert!(
                matches!(
                    &error,
                    c2_http::client::HttpError::ServerError(502, _)
                        | c2_http::client::HttpError::ResponseError(502, _)
                ),
                "{error:?}"
            );
            println!("C2_PERSISTENT_OLD_CANDIDATE {error:?}");
        });
        let withdrawal_gate = barrier.clone();
        let withdrawal = scope.spawn(move || {
            let control = c2_http::client::RelayControlClient::new(&url, false)
                .unwrap()
                .with_local_endpoint_context(&context);
            let retired = c2_http::client::RelayRegistrationScope {
                name: "process-route".into(),
                server_id: old["identity"]["server_id"].as_str().unwrap().into(),
                server_instance_id: old["identity"]["server_instance_id"]
                    .as_str()
                    .unwrap()
                    .into(),
                route_uid: old_relay_uid,
            };
            withdrawal_gate.wait();
            control.unregister_registration(&retired).unwrap();
            println!("C2_PERSISTENT_OLD_WITHDRAWAL {retired:?}");
        });
        barrier.wait();
        candidate.join().unwrap();
        withdrawal.join().unwrap();
    });
}
