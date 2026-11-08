use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Client, ConfigSources, Connect, ConnectOptions, EncodedClient, EncodedService, Host,
    HostOptions, LocalEndpointOptions, MethodDefinition, Registration, Runtime, RuntimeOptions,
    ServiceDefinition,
};
use c2_error::C2Error;
use c2_local::{ConnectionEndReason, ConnectionObservation};

const WAIT: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_millis(60);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, _: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(request)
    }
}

struct Fixture {
    runtime: Runtime,
    host: Host,
    registration: Registration,
    release: ContractRelease,
    observer: tokio::runtime::Runtime,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new(idle: Duration) -> Self {
        let root = tempfile::Builder::new().prefix("c2idle").tempdir().unwrap();
        let mut endpoint = LocalEndpointOptions::default();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            endpoint.unix_root = Some(root.path().to_path_buf());
        }
        let mut options = RuntimeOptions {
            use_process_relay_anchor: false,
            ..Default::default()
        };
        options
            .client_ipc_overrides
            .get_or_insert_default()
            .client_idle_timeout_secs = Some(idle.as_secs_f64());
        let runtime = Runtime::new(options).unwrap();
        runtime
            .set_local_endpoint_with_sources(endpoint, ConfigSources::empty())
            .unwrap();
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        let release = ContractRelease::from_descriptor_json(include_bytes!(
            "../../../../tests/fixtures/contracts/portable-release.contract.json"
        ))
        .unwrap();
        let definition = ServiceDefinition::new(
            &release,
            release.reference(),
            "idle",
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
        .unwrap();
        let registration = host.register(definition).unwrap();
        Self {
            runtime,
            host,
            registration,
            release,
            observer: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            _root: root,
        }
    }

    fn connect(&self) -> Client {
        let client = self
            .runtime
            .connect_with_options(
                self.release.expected_route("idle").unwrap(),
                Connect::DirectIpc {
                    address: self.runtime.server_address().unwrap(),
                },
                ConnectOptions::new().with_timeout(WAIT),
            )
            .unwrap();
        assert_eq!(client.call_owned("echo", b"value").unwrap(), b"value");
        client
    }

    fn closed(&self, observation: &ConnectionObservation, timeout: Duration) -> bool {
        self.observer
            .block_on(observation.wait_closed(Some(timeout)))
            .is_some()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let deadline = Instant::now() + WAIT;
        let (_, completed) = self
            .runtime
            .begin_session_retirement(c2_core::RuntimeShutdownTarget::ClientsOnly, deadline)
            .unwrap()
            .observe_until(deadline);
        assert!(completed, "outgoing connections drained");
        self.registration.close().unwrap();
        let outcome = self.host.shutdown_with_timeout(WAIT);
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        assert!(outcome.route_close_error.is_none(), "{outcome:?}");
    }
}

#[test]
fn configured_idle_timeout_evicts_in_background_and_reconnects() {
    let fixture = Fixture::new(IDLE);
    let first = fixture.connect();
    let observation = first.connection_observation().unwrap();
    // A live proxy remains usable beyond the configured idle window.
    assert!(!fixture.closed(&observation, IDLE * 3));
    assert_eq!(
        first.call_owned("echo", b"still active").unwrap(),
        b"still active"
    );
    drop(first);
    // No acquire or explicit sweep drives eviction while waiting.
    assert!(
        fixture.closed(&observation, WAIT),
        "background eviction closed the idle connection"
    );
    assert_eq!(
        observation.snapshot().unwrap().reason(),
        ConnectionEndReason::LocalClose
    );
    let replacement = fixture.connect();
    assert_ne!(
        observation.connection_id(),
        replacement
            .connection_observation()
            .unwrap()
            .connection_id()
    );
}

#[test]
fn configured_idle_timeout_reuses_within_window() {
    // Keep the idle window larger than the connect operation's budget. This
    // checks cache reuse without requiring a busy runner to complete IPC in
    // an arbitrarily short fraction of a second.
    let fixture = Fixture::new(WAIT * 2);
    let first = fixture.connect();
    let observation = first.connection_observation().unwrap();
    drop(first);
    let reused = fixture.connect();
    assert_eq!(
        observation.connection_id(),
        reused.connection_observation().unwrap().connection_id()
    );
}

#[test]
fn configured_zero_idle_timeout_keeps_idle_connection() {
    let fixture = Fixture::new(Duration::ZERO);
    let first = fixture.connect();
    let observation = first.connection_observation().unwrap();
    drop(first);
    assert!(
        !fixture.closed(&observation, IDLE * 3),
        "zero disables timed eviction"
    );
    let reused = fixture.connect();
    assert_eq!(
        observation.connection_id(),
        reused.connection_observation().unwrap().connection_id()
    );
}
