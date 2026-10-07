//! Automatic native endpoint agreement across runtime domains and admin probes.
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, HostOptions, MethodDefinition, Runtime, RuntimeOptions,
    ServiceDefinition, direct_ipc_endpoint, ping_direct_ipc, shutdown_direct_ipc,
};
use c2_error::{C2Error, ErrorCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request.to_vec()),
            other => Err(C2Error::new(
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
    ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).expect("valid release")
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
    .expect("definition must match release")
}

fn native_host(server_id: String) -> (Runtime, c2_core::Host, String) {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let address = runtime.server_address().unwrap();
    (runtime, host, address)
}
fn wait_until_pingable(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ping_direct_ipc(address, Duration::from_millis(200)).unwrap() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{address} never answered");
}
#[test]
fn runtime_client_config_freezes_after_a_failed_native_connect() {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    assert!(!runtime.client_config_frozen());
    let absent = format!("ipc://{}", unique_name("absent"));
    let expected = release().expected_route("unused-route").unwrap();
    assert!(
        runtime
            .connect(expected, Connect::DirectIpc { address: absent })
            .is_err()
    );
    assert!(runtime.client_config_frozen());
    let mut replacement = c2_config::ClientIpcConfigOverrides::default();
    replacement.base.pool_enabled = Some(false);
    assert!(runtime.set_client_ipc_overrides(Some(replacement)).is_err());
}
#[test]
fn native_host_serves_calls_and_admin_shutdown_initiates_draining() {
    let (runtime, host, address) = native_host(unique_name("native-host"));
    let route = unique_name("native-route");
    let definition = definition(&route);
    let expected = definition.expected_route().clone();
    let _registration = host.register(definition).unwrap();
    wait_until_pingable(&address);
    let client_runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    let client = client_runtime
        .connect(
            expected,
            Connect::DirectIpc {
                address: address.clone(),
            },
        )
        .unwrap();
    assert_eq!(client.call_owned("echo", b"native").unwrap(), b"native");
    assert_eq!(
        direct_ipc_endpoint(&address).unwrap(),
        c2_config::LocalEndpoint::from_address(&address).unwrap()
    );
    let shutdown = shutdown_direct_ipc(&address, Duration::from_millis(500)).unwrap();
    assert!(shutdown.acknowledged && shutdown.shutdown_started && !shutdown.server_stopped);
    let completed = host.shutdown();
    assert!(completed.runtime_barrier_error.is_none(), "{completed:?}");
    drop(runtime);
}
#[test]
fn runtime_restart_reuses_endpoint_and_gets_a_fresh_instance() {
    let server_id = unique_name("restart");
    let (runtime, host, address) = native_host(server_id.clone());
    let route = unique_name("restart-route");
    let _registration = host.register(definition(&route)).unwrap();
    wait_until_pingable(&address);
    let first = direct_ipc_endpoint(&address).unwrap();
    #[cfg(unix)]
    let first_credential = loop {
        match c2_core::inspect_endpoint(&first) {
            c2_core::EndpointInspection::Present(value) => break value,
            c2_core::EndpointInspection::IoError(error)
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            other => panic!("unexpected inspection: {other:?}"),
        }
    };
    assert!(host.shutdown().runtime_barrier_error.is_none());
    drop(host);
    drop(runtime);
    let (_runtime, next, next_address) = native_host(server_id);
    let _registration = next.register(definition(&route)).unwrap();
    wait_until_pingable(&next_address);
    assert_eq!(first, direct_ipc_endpoint(&next_address).unwrap());
    #[cfg(unix)]
    {
        assert!(matches!(
            c2_core::reap_endpoint(&first, &first_credential),
            c2_core::EndpointReapResult::Busy | c2_core::EndpointReapResult::StaleTarget
        ));
    }
    assert!(ping_direct_ipc(&next_address, Duration::from_millis(200)).unwrap());
}
