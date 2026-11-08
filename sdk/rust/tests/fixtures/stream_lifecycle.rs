use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use c_two::endpoint::LocalEndpointOptions;
use c_two::generated::{
    AdapterFailurePhase, C2Error, EncodedService, EncodedStream, EncodedStreamService,
    HeldResponse, MethodAccess, MethodDefinition, PayloadStream, fastdb_adapter_error, open_held,
    open_owned,
};
use c_two::{
    CallExecutionLimitsOverrides, Client, ConfigSources, Connect, ContractRelease, Error, Held,
    Host, HostOptions, Registration, ResponseStream, Runtime, RuntimeOptions,
    ServiceConcurrencyMode, ServiceDefinition,
};
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};

const DESCRIPTOR: &[u8] = include_bytes!(
    "../../../../core/foundation/c2-contract/tests/fixtures/server-stream.contract.json"
);
const VALUE_SPEC: &[u8] = br#"{
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],
    "components": []
}"#;

pub fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap()
}

fn payload_spec() -> CompiledSpec {
    CompiledSpec::compile(VALUE_SPEC).unwrap()
}

fn decode_owned(response: HeldResponse) -> Result<Payload, Error> {
    open_owned(&payload_spec(), response.bytes()).map_err(|error| {
        fastdb_adapter_error(AdapterFailurePhase::ClientOutputDeserializing, error).into()
    })
}

fn decode_held(response: HeldResponse) -> Result<Held<Payload>, Error> {
    open_held(&payload_spec(), response).map_err(|error| {
        fastdb_adapter_error(AdapterFailurePhase::ClientOutputDeserializing, error).into()
    })
}

// Exercise the public typed owners through their existing generated-code seam,
// as stream_deadline.rs does, without storing generated code in the repository.
pub fn open_stream(client: &Client) -> Result<ResponseStream<Payload>, Error> {
    let core = client
        .begin_stream("echo")?
        .encode_vec(Vec::new())?
        .stream()?;
    Ok(ResponseStream::from_core(core, decode_owned))
}

pub fn open_held_stream(client: &Client) -> Result<ResponseStream<Held<Payload>>, Error> {
    let core = client
        .begin_stream("echo")?
        .encode_vec(Vec::new())?
        .stream()?;
    Ok(ResponseStream::from_core(core, decode_held))
}

pub const WAIT: Duration = Duration::from_secs(20);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

pub fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn observe_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "native observation did not settle"
        );
        std::thread::yield_now();
    }
}

#[derive(Default)]
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    pub fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "controller did not release the resource barrier");
    }
}

#[derive(Default)]
pub struct Settings {
    pub gated_step: Option<usize>,
    pub gated_cleanup: bool,
    pub gated_unary: bool,
    pub process_events: bool,
    pub client_slots: Option<u64>,
    pub client_bytes: Option<u64>,
}

pub struct Probe {
    settings: Settings,
    pub step_gate: Gate,
    pub cleanup_gate: Gate,
    pub unary_gate: Gate,
    pub opens: AtomicUsize,
    pub steps: AtomicUsize,
    pub cleanups: AtomicUsize,
    pub unary_calls: AtomicUsize,
    step_entered: mpsc::Sender<usize>,
    cleanup_entered: mpsc::Sender<()>,
    unary_entered: mpsc::Sender<()>,
}

impl Probe {
    fn process_event(&self, event: &str) {
        if self.settings.process_events {
            use std::io::Write;
            println!("\nC2_STREAM_{event}");
            std::io::stdout().flush().unwrap();
        }
    }
}

struct Resource(Arc<Probe>);

impl EncodedService for Resource {
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(index, 0);
        assert!(request.is_empty());
        self.0.unary_calls.fetch_add(1, Ordering::SeqCst);
        self.0.unary_entered.send(()).unwrap();
        if self.0.settings.gated_unary {
            self.0.unary_gate.wait();
        }
        Ok(Vec::new())
    }
}

impl EncodedStreamService for Resource {
    fn open_stream(&self, index: u16, request: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
        assert_eq!(index, 1);
        assert!(request.is_empty());
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        self.0.process_event("OPEN");
        Ok(Box::new(PayloadStream::new(
            Box::new(Items {
                probe: Arc::clone(&self.0),
                index: 0,
            }),
            None,
            payload_spec().sha256().unwrap(),
            "echo",
            1,
        )))
    }
}

struct Items {
    probe: Arc<Probe>,
    index: usize,
}

impl Iterator for Items {
    type Item = Result<Payload, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.index += 1;
        self.probe.steps.fetch_add(1, Ordering::SeqCst);
        self.probe.step_entered.send(self.index).unwrap();
        self.probe.process_event(&format!("STEP {}", self.index));
        if self.probe.settings.gated_step == Some(self.index) {
            self.probe.step_gate.wait();
        }
        Some(Ok(payload(self.index)))
    }
}

impl Drop for Items {
    fn drop(&mut self) {
        self.probe.cleanups.fetch_add(1, Ordering::SeqCst);
        self.probe.cleanup_entered.send(()).unwrap();
        if self.probe.settings.gated_cleanup {
            self.probe.cleanup_gate.wait();
        }
    }
}

pub fn payload(index: usize) -> Payload {
    let spec = payload_spec();
    let mut builder = Builder::create(&spec).unwrap();
    builder
        .entry_begin(0, 1)
        .unwrap()
        .value_str(&format!("{index:08}"))
        .unwrap();
    builder
        .freeze()
        .unwrap()
        .execute(BuildPolicy::AllowStaging)
        .unwrap()
        .payload
}

pub fn text(payload: &Payload) -> String {
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

pub fn runtime(root: &Path, server_id: String) -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id),
        use_process_relay_anchor: false,
        server_ipc_overrides: Some(c2_config::ServerIpcConfigOverrides {
            max_execution_workers: Some(2),
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

pub struct Fixture {
    pub server: Runtime,
    pub host: Host,
    pub registration: Option<Registration>,
    pub client_runtime: Runtime,
    pub client: Client,
    pub probe: Arc<Probe>,
    pub step_events: mpsc::Receiver<usize>,
    pub cleanup_events: mpsc::Receiver<()>,
    pub unary_events: mpsc::Receiver<()>,
    pub root: PathBuf,
    pub route: String,
}

impl Fixture {
    pub fn new(settings: Settings) -> Self {
        // Use a private short Unix path, as public_api::test_context does.
        // Windows uses its native Named Pipe scope and ignores this path.
        // Do not touch default namespaces or manually delete coordination files.
        let root = Path::new("/tmp").join(unique("c2-sdk-stream"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&root)
                .unwrap();
        }
        let server = runtime(&root, unique("stream-host"));
        let client_runtime = runtime(&root, unique("stream-client"));
        client_runtime
            .set_call_execution_limits_with_sources(
                CallExecutionLimitsOverrides {
                    max_outstanding_calls: settings.client_slots,
                    payload_buffer_budget_bytes: settings.client_bytes,
                },
                ConfigSources::empty(),
            )
            .unwrap();
        let (step_entered, step_events) = mpsc::channel();
        let (cleanup_entered, cleanup_events) = mpsc::channel();
        let (unary_entered, unary_events) = mpsc::channel();
        let probe = Arc::new(Probe {
            settings,
            step_gate: Gate::default(),
            cleanup_gate: Gate::default(),
            unary_gate: Gate::default(),
            opens: AtomicUsize::new(0),
            steps: AtomicUsize::new(0),
            cleanups: AtomicUsize::new(0),
            unary_calls: AtomicUsize::new(0),
            step_entered,
            cleanup_entered,
            unary_entered,
        });
        let host = server.host(HostOptions::default().without_relay()).unwrap();
        let route = unique("stream-route");
        let release = release();
        let definition = ServiceDefinition::new_stream(
            &release,
            release.reference(),
            &route,
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
            Arc::new(Resource(Arc::clone(&probe))),
        )
        .unwrap()
        .with_concurrency(ServiceConcurrencyMode::Parallel, Some(8), Some(2))
        .unwrap();
        let registration = host.register(definition).unwrap();
        let client = client_runtime
            .connect(
                release.expected_route(&route).unwrap(),
                Connect::DirectIpc {
                    address: host.local_endpoint().address().into(),
                },
            )
            .unwrap();
        Self {
            server,
            host,
            registration: Some(registration),
            client_runtime,
            client,
            probe,
            step_events,
            cleanup_events,
            unary_events,
            root,
            route,
        }
    }

    pub fn used_slots(&self) -> u64 {
        self.client_runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations
    }

    pub fn used_bytes(&self) -> u64 {
        self.client_runtime
            .memory_stats()
            .payload_buffer
            .unwrap()
            .used_bytes
    }

    pub fn wait_for_step(&self, index: usize) {
        loop {
            let entered = self.step_events.recv_timeout(WAIT).unwrap();
            if entered == index {
                return;
            }
            assert!(entered < index);
        }
    }
}
