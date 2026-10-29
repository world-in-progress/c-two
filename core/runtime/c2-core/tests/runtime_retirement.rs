use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, ConnectOptions, ConnectionEvent, ConnectionEventWait, EncodedClient, EncodedService,
    Error, HostOptions, LifecycleError, MethodDefinition, Runtime, RuntimeOptions,
    RuntimeShutdownReceipt, RuntimeShutdownTarget, ServiceDefinition, recv_connection_event,
};
use c2_error::{C2Error, ErrorCode};

const WAIT: Duration = Duration::from_secs(5);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");

struct Echo;
impl EncodedService for Echo {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(request)
    }
}

struct Fixture {
    runtime: Runtime,
    _root: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new() -> Self {
        let runtime = Runtime::new(RuntimeOptions {
            server_id: Some(format!("retire-{}", uuid::Uuid::new_v4().simple())),
            use_process_relay_anchor: false,
            ..Default::default()
        })
        .unwrap();
        #[cfg(unix)]
        let root = {
            let root = tempfile::Builder::new()
                .prefix("c2-retire-")
                .tempdir_in("/tmp")
                .unwrap();
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions {
                        unix_root: Some(root.path().to_path_buf()),
                    },
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            Some(root)
        };
        #[cfg(not(unix))]
        let root = {
            runtime
                .set_local_endpoint_with_sources(
                    c2_config::LocalEndpointOptions::default(),
                    c2_config::ConfigSources::empty(),
                )
                .unwrap();
            None
        };
        Self {
            runtime,
            _root: root,
        }
    }
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap()
}

fn definition() -> ServiceDefinition {
    let release = release();
    ServiceDefinition::new(
        &release,
        release.reference(),
        "echo",
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
        Arc::new(Echo),
    )
    .unwrap()
}

fn completed(receipt: &RuntimeShutdownReceipt) -> c2_core::ShutdownOutcome {
    let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
    assert!(completed, "retirement is still pending: {outcome:?}");
    assert!(outcome.ipc_clients_drained, "{outcome:?}");
    assert_eq!(receipt.terminal_outcome(), Some(outcome.clone()));
    outcome
}

fn assert_endpoint_closed(endpoint: &c2_config::LocalEndpoint) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        rt.block_on(c2_local::LocalStream::connect(endpoint, WAIT))
            .is_err(),
        "retired endpoint still accepts native connections"
    );
    #[cfg(unix)]
    {
        assert!(matches!(
            c2_core::inspect_endpoint(endpoint),
            c2_core::EndpointInspection::Absent
        ));
        let remaining = std::fs::read_dir(endpoint.context().unix_root().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            remaining
                .iter()
                .all(|name| name == ".gate" || name == ".gate.marker"),
            "socket/lease residue: {remaining:?}"
        );
    }
}

fn assert_retired<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(
        matches!(
            result,
            Err(Error::Lifecycle(LifecycleError::RuntimeRetired))
        ),
        "{result:?}"
    );
}

#[test]
fn client_only_retirement_closes_retained_connection_before_peer_shutdown() {
    let peer = Fixture::new();
    let host = peer
        .runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _registration = host.register(definition()).unwrap();
    let clients = Fixture::new();
    clients
        .runtime
        .inherit_local_endpoint_selection(&peer.runtime)
        .unwrap();
    let events = clients.runtime.connection_events();
    let mode = Connect::DirectIpc {
        address: host.local_endpoint().address().into(),
    };
    let client = clients
        .runtime
        .connect(release().expected_route("echo").unwrap(), mode.clone())
        .unwrap();
    assert_eq!(client.call_owned("echo", b"live").unwrap(), b"live");
    let observation = client.connection_observation().unwrap();
    let Some(ConnectionEventWait::Event(ConnectionEvent::Opened(opened))) =
        recv_connection_event(&events, Some(WAIT)).unwrap()
    else {
        panic!("physical connection opened");
    };
    assert_eq!(opened.connection_id(), observation.connection_id());
    let receipt = clients
        .runtime
        .begin_session_retirement(RuntimeShutdownTarget::ClientsOnly, Instant::now())
        .unwrap();
    let outcome = completed(&receipt);
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert!(
        observation.snapshot().is_some(),
        "physical close must precede completion"
    );
    let Some(ConnectionEventWait::Event(ConnectionEvent::Closed(closed))) =
        recv_connection_event(&events, Some(WAIT)).unwrap()
    else {
        panic!("retirement published physical close");
    };
    assert_eq!(closed.connection_id(), observation.connection_id());
    assert_eq!(observation.snapshot(), Some(closed));
    assert!(host.is_running());
    assert_retired(
        clients
            .runtime
            .connect(release().expected_route("echo").unwrap(), mode),
    );
    assert_retired(clients.runtime.host(HostOptions::default().without_relay()));
    assert!(matches!(
        clients.runtime.ensure_server(),
        Err(LifecycleError::RuntimeRetired)
    ));
    assert_eq!(
        clients
            .runtime
            .retirement_receipt(Instant::now() + WAIT)
            .unwrap()
            .unwrap()
            .terminal_outcome(),
        Some(outcome)
    );
    assert!(host.shutdown().runtime_barrier_error.is_none());
    assert_endpoint_closed(host.local_endpoint());
}

#[test]
fn hosted_zero_budget_retirement_keeps_draining_and_rejects_new_work() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let registration = host.register(definition()).unwrap();
    let mut active = registration
        .route_concurrency()
        .blocking_acquire(0)
        .unwrap();
    active.commit().unwrap();
    let receipt = runtime
        .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now())
        .unwrap();
    assert!(
        !receipt.observe_until(Instant::now()).1,
        "active native route guard must drain"
    );
    assert!(receipt.terminal_outcome().is_none());
    assert_retired(host.register(definition()));
    assert_retired(runtime.host(HostOptions::default().without_relay()));
    assert_retired(runtime.connect(
        release().expected_route("echo").unwrap(),
        Connect::RelayAware,
    ));
    let repeated = runtime
        .begin_session_retirement(RuntimeShutdownTarget::ClientsOnly, Instant::now())
        .unwrap();
    assert!(!repeated.observe_until(Instant::now()).1);
    drop(receipt); // The caller and its receipt do not own driver cancellation.
    drop(active);
    let outcome = completed(&repeated);
    assert!(!host.is_running());
    assert_endpoint_closed(host.local_endpoint());
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert_eq!(outcome.removed_routes, ["echo"]);
    assert!(outcome.route_outcomes[0].active_drained);
    assert!(runtime.ensure_active().is_err());
}

#[test]
fn ordinary_host_shutdown_preserves_runtime_restart_and_reacquire() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _registration = host.register(definition()).unwrap();
    let mode = Connect::DirectIpc {
        address: host.local_endpoint().address().into(),
    };
    let client = runtime
        .connect(release().expected_route("echo").unwrap(), mode.clone())
        .unwrap();
    assert_eq!(client.call_owned("echo", b"first").unwrap(), b"first");
    assert!(host.shutdown().runtime_barrier_error.is_none());
    assert!(
        runtime
            .retirement_receipt(Instant::now())
            .unwrap()
            .is_none()
    );
    runtime.ensure_active().unwrap();
    runtime.clear_server_identity().unwrap();
    let restarted = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _new_registration = restarted.register(definition()).unwrap();
    let new_client = runtime
        .connect(release().expected_route("echo").unwrap(), mode)
        .unwrap();
    assert_eq!(new_client.call_owned("echo", b"second").unwrap(), b"second");
    assert!(restarted.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn terminal_delivered_host_journal_survives_retirement_of_late_clients() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _registration = host.register(definition()).unwrap();
    let original = host.shutdown();
    assert_eq!(original.removed_routes, ["echo"]);
    let peer = Fixture::new();
    peer.runtime
        .inherit_local_endpoint_selection(runtime)
        .unwrap();
    let peer_host = peer
        .runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _peer_registration = peer_host.register(definition()).unwrap();
    let client = runtime
        .connect(
            release().expected_route("echo").unwrap(),
            Connect::DirectIpc {
                address: peer_host.local_endpoint().address().into(),
            },
        )
        .unwrap();
    let observation = client.connection_observation().unwrap();
    let receipt = runtime
        .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now() + WAIT)
        .unwrap();
    let outcome = completed(&receipt);
    assert!(observation.snapshot().is_some());
    assert!(peer_host.is_running());
    assert_eq!(outcome.route_outcomes, original.route_outcomes);
    assert_eq!(outcome.removed_routes, original.removed_routes);
    assert_eq!(outcome.relay_errors, original.relay_errors);
    assert_eq!(host.shutdown_outcome(), Some(original));
    assert_eq!(completed(&receipt), outcome);
    assert!(peer_host.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn retirement_keeps_deadline_and_contract_precedence_without_creating_ipc_pool() {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let receipt = runtime
        .begin_session_retirement(RuntimeShutdownTarget::ClientsOnly, Instant::now())
        .unwrap();
    completed(&receipt);
    let expected = release().expected_route("echo").unwrap();
    let mode = Connect::ExplicitRelay {
        relay_url: "http://127.0.0.1:1".into(),
    };
    let expired = runtime
        .connect_with_options(
            expected.clone(),
            mode.clone(),
            ConnectOptions::new().with_timeout(Duration::ZERO),
        )
        .unwrap_err();
    assert!(
        matches!(expired, Error::Semantic(error) if error.code == ErrorCode::CallDeadlineExceeded)
    );
    let mut invalid = expected.clone();
    invalid.abi_hash.clear();
    assert!(matches!(
        runtime.connect(invalid, mode.clone()),
        Err(Error::Contract(_))
    ));
    assert_retired(runtime.connect(expected, mode));
    assert!(!runtime.local_endpoint_frozen());
    assert!(!runtime.client_config_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
}

#[test]
fn retirement_publishes_before_last_host_drop_and_releases_driver_owners() {
    let Fixture { runtime, _root } = Fixture::new();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let events = runtime.connection_events();
    let receipt = runtime
        .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now())
        .unwrap();
    drop(host);
    completed(&receipt);
    drop(runtime);
    drop(receipt);
    loop {
        match recv_connection_event(&events, Some(WAIT)).unwrap() {
            Some(ConnectionEventWait::Ended) => break,
            Some(ConnectionEventWait::Event(_)) => {}
            other => panic!("driver retained its Host/Runtime after publication: {other:?}"),
        }
    }
    assert_endpoint_closed(&endpoint);
}

#[test]
fn foreign_host_target_is_rejected_before_retirement() {
    let owner = Fixture::new();
    let other = Fixture::new();
    let host = owner
        .runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    assert!(
        other
            .runtime
            .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now() + WAIT)
            .is_err()
    );
    assert!(
        other
            .runtime
            .retirement_receipt(Instant::now())
            .unwrap()
            .is_none()
    );
    owner.runtime.ensure_active().unwrap();
    other.runtime.ensure_active().unwrap();
    assert!(host.is_running());
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn client_scope_cannot_report_completion_for_an_undeclared_host() {
    let fixture = Fixture::new();
    let host = fixture
        .runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let clients = fixture
        .runtime
        .begin_session_retirement(RuntimeShutdownTarget::ClientsOnly, Instant::now() + WAIT)
        .unwrap();
    completed(&clients);
    assert!(host.is_running());
    assert!(matches!(
        fixture
            .runtime
            .begin_session_retirement(RuntimeShutdownTarget::Host(&host), Instant::now() + WAIT,),
        Err(LifecycleError::Configuration(_))
    ));
    // Rejection borrows the supplied Host; its owner remains available.
    assert!(host.is_running());
    assert!(host.shutdown().runtime_barrier_error.is_none());
}
