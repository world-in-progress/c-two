use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, Error, HostOptions, MethodDefinition, Runtime,
    RuntimeOptions, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::relay::{RelayConfig, RelayServer};

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const WAIT: Duration = Duration::from_secs(10);

struct PausedEcho {
    entered: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    calls: Arc<AtomicUsize>,
}

impl EncodedService for PausedEcho {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.send(()).expect("call entered");
        self.resume
            .lock()
            .expect("resume lock")
            .recv_timeout(WAIT)
            .expect("call released");
        Ok(request)
    }
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

fn relay(context: c2_config::LocalEndpointContext) -> (RelayServer, String) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve relay port");
    let address = probe.local_addr().expect("relay address");
    drop(probe);
    let url = format!("http://{address}");
    let relay = RelayServer::start_with_context(
        RelayConfig {
            bind: address.to_string(),
            advertise_url: url.clone(),
            idle_timeout_secs: 0,
            anti_entropy_interval: Duration::ZERO,
            heartbeat_interval: Duration::ZERO,
            ..RelayConfig::default()
        },
        context,
    )
    .expect("relay starts");
    (relay, url)
}

fn resource_definition(
    route_name: &str,
    entered: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
    calls: Arc<AtomicUsize>,
) -> ServiceDefinition {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).expect("valid contract");
    ServiceDefinition::new(
        &release,
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
        Arc::new(PausedEcho {
            entered,
            resume: Mutex::new(resume),
            calls,
        }),
    )
    .expect("definition matches contract")
}

#[test]
fn host_shutdown_closes_route_acquisition_and_retains_typed_relay_local_cause() {
    #[cfg(unix)]
    let ipc_root = tempfile::Builder::new()
        .prefix("c2-route-shutdown-")
        .tempdir()
        .expect("private IPC root");
    #[cfg(unix)]
    let endpoint_context = c2_config::LocalEndpointContext::with_unix_root(ipc_root.path())
        .expect("isolated endpoint context");
    #[cfg(windows)]
    let endpoint_context =
        c2_config::LocalEndpointContext::default_for_platform().expect("default endpoint context");
    let (mut relay, relay_url) = relay(endpoint_context);
    let owner = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("route-shutdown-owner")),
        relay_anchor_address: Some(relay_url.clone()),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("owner Runtime");
    owner
        .set_local_endpoint_with_sources(
            c2_config::LocalEndpointOptions {
                #[cfg(unix)]
                unix_root: Some(ipc_root.path().to_path_buf()),
                ..Default::default()
            },
            c2_config::ConfigSources::empty(),
        )
        .expect("isolated IPC namespace");
    let host = owner
        .host(
            HostOptions::default()
                .with_relay_anchor_address(relay_url.clone())
                .with_relay_use_proxy(false),
        )
        .expect("Host");
    let route_name = unique_name("route-shutdown");
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).expect("valid contract");
    let expected = release
        .expected_route(&route_name)
        .expect("expected route contract");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = host
        .register(resource_definition(
            &route_name,
            entered_tx,
            resume_rx,
            Arc::clone(&calls),
        ))
        .expect("register route");

    let client_runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("route-shutdown-client")),
        relay_anchor_address: Some(relay_url),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("client Runtime");
    client_runtime
        .inherit_local_endpoint_selection(&owner)
        .expect("share isolated IPC namespace");
    let direct_address = host.local_endpoint().address().to_owned();
    let client = client_runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: direct_address.clone(),
            },
        )
        .expect("initial direct acquisition");
    let call = std::thread::spawn(move || client.call_owned("echo", b"in flight"));
    entered_rx.recv_timeout(WAIT).expect("resource entered");

    let shutdown_host = host.clone();
    let shutdown = std::thread::spawn(move || shutdown_host.shutdown_with_timeout(WAIT));

    // Each exact acquisition is a synchronization point with Host admission:
    // successful pre-close bindings are discarded until the authoritative
    // route lookup reports the closed state.
    let deadline = Instant::now() + WAIT;
    let direct_error = loop {
        match client_runtime.connect(
            expected.clone(),
            Connect::DirectIpc {
                address: direct_address.clone(),
            },
        ) {
            Err(Error::Semantic(error)) if error.code == ErrorCode::ResourceClosed => {
                break error;
            }
            Err(error) => panic!("unexpected direct acquisition result: {error:?}"),
            Ok(client) => drop(client),
        }
        assert!(Instant::now() < deadline, "Host did not close admission");
        std::thread::yield_now();
    };
    assert_eq!(direct_error.code, ErrorCode::ResourceClosed);

    let Error::Semantic(fallback) = client_runtime
        .connect(expected, Connect::RelayAware)
        .expect_err("local relay candidate is closed during Host drain")
    else {
        panic!("relay-local refusal must be semantic");
    };
    assert_eq!(fallback.code, ErrorCode::FallbackDenied);
    assert_eq!(
        fallback
            .details
            .get("direct_ipc_failure_kind")
            .map(String::as_str),
        Some("semantic:ResourceClosed")
    );
    assert_eq!(
        fallback
            .details
            .get("direct_ipc_failure_phase")
            .map(String::as_str),
        Some("pre_dispatch")
    );
    let cause: serde_json::Value = serde_json::from_str(
        fallback
            .details
            .get("direct_ipc_failure_envelope")
            .expect("typed IPC cause retained"),
    )
    .expect("cause envelope JSON");
    assert_eq!(cause["code"], ErrorCode::ResourceClosed as u16);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "acquisition entered no call"
    );

    resume_tx.send(()).expect("release running call");
    assert_eq!(call.join().expect("call thread").unwrap(), b"in flight");
    let outcome = shutdown.join().expect("shutdown thread");
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert!(outcome.route_close_error.is_none(), "{outcome:?}");
    assert!(outcome.relay_errors.is_empty(), "{outcome:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(_registration);
    relay.stop().expect("relay stops");
}
