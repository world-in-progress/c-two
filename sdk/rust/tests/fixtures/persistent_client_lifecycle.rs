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
    fn observed(self) -> ObservedPath {
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
    uid: Mutex<String>,
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
        Ok(encode(
            &serde_json::json!({
                "identity": self.identity,
                "route_uid": *self.uid.lock().unwrap(),
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
            serde_json::json!({"server_id":outcome.server_id,"server_instance_id":outcome.server_instance_id,"route_uid":outcome.route_uid,"business_calls":self.calls()})
        );
    }
}

impl Drop for Evidence {
    fn drop(&mut self) {
        self.0.gate.release();
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
    relay: Option<RelayServer>,
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
        let (relay, relay_url) = if path != PathKind::Direct {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            let url = format!("http://{address}");
            let relay = RelayServer::start_with_context(
                RelayConfig {
                    bind: address.to_string(),
                    advertise_url: url.clone(),
                    idle_timeout_secs: relay_idle,
                    anti_entropy_interval: Duration::ZERO,
                    heartbeat_interval: Duration::ZERO,
                    ..Default::default()
                },
                context,
            )
            .unwrap();
            (Some(relay), Some(url))
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
            relay,
            anchor,
            finished: false,
        }
    }

    pub fn register(&self, name: &str) -> Registered {
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        let identity = self.server.ensure_server().unwrap();
        let resource = Arc::new(Resource {
            calls: AtomicUsize::new(0),
            uid: Mutex::new(String::new()),
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
        *resource.uid.lock().unwrap() = registration.outcome().route_uid.clone();
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
        assert_eq!(client.observed_route().route_uid, registered.route_uid);
        if let Some(identity) = client.ipc_server_identity() {
            assert_eq!(identity.server_id, registered.server_id);
            assert_eq!(identity.server_instance_id, registered.server_instance_id);
        }
        println!(
            "C2_PERSISTENT_ACQUIRE {}",
            serde_json::json!({"path":format!("{:?}", client.observed_path()),"server_id":registered.server_id,"server_instance_id":registered.server_instance_id,"route_uid":registered.route_uid,"physical_connections":self.outgoing_ids()})
        );
        client
    }

    pub fn call(&self, client: &Client, route: &Registered, command: &str) {
        let bytes = client.call_owned("echo", &encode(command)).unwrap();
        let response: serde_json::Value = serde_json::from_str(&decode(&bytes)).unwrap();
        let registered = route.registration.outcome();
        assert_eq!(response["identity"]["server_id"], registered.server_id);
        assert_eq!(
            response["identity"]["server_instance_id"],
            registered.server_instance_id
        );
        assert_eq!(response["route_uid"], registered.route_uid);
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
        let accepted = drain_events(&self.server_events);
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
