//! Endpoint protocol propagation: one resolved choice names one OS endpoint.
//!
//! These tests own the production rule that an IPC client, a server, and an
//! administrative probe never probe across endpoint namespaces. A server
//! bound under `legacy-v1` is not reachable, and not stoppable, through a
//! `managed-v2` probe, and the reverse holds as well.
//!
//! Every process-wide environment mutation in this file runs under
//! [`env_lock`] and restores the previous value before releasing it, so the
//! tests stay correct under the normal parallel harness.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use c2_config::LocalEndpointProtocol;
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    direct_ipc_endpoint, direct_ipc_endpoint_with_protocol, ping_direct_ipc,
    ping_direct_ipc_with_protocol, shutdown_direct_ipc_with_protocol, Connect, EncodedClient,
    EncodedService, HostOptions, MethodDefinition, Runtime, RuntimeOptions, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};

/// Serializes process-environment mutation for the tests in this binary.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Restores `C2_IPC_ENDPOINT_PROTOCOL` on every exit path.
struct ProtocolEnv {
    previous: Option<std::ffi::OsString>,
}

impl ProtocolEnv {
    fn set(value: Option<&str>) -> Self {
        let previous = std::env::var_os("C2_IPC_ENDPOINT_PROTOCOL");
        // SAFETY: the caller holds `env_lock`, and the previous value is
        // restored before that guard is released.
        unsafe {
            match value {
                Some(value) => std::env::set_var("C2_IPC_ENDPOINT_PROTOCOL", value),
                None => std::env::remove_var("C2_IPC_ENDPOINT_PROTOCOL"),
            }
        }
        Self { previous }
    }
}

impl Drop for ProtocolEnv {
    fn drop(&mut self) {
        // SAFETY: restoration happens while the caller still holds the lock.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("C2_IPC_ENDPOINT_PROTOCOL", value),
                None => std::env::remove_var("C2_IPC_ENDPOINT_PROTOCOL"),
            }
        }
    }
}

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

/// One Host whose server and client domains are both pinned to `protocol`.
fn host_with_protocol(
    server_id: String,
    protocol: LocalEndpointProtocol,
) -> (Runtime, c2_core::Host, String) {
    let mut server_overrides = c2_config::ServerIpcConfigOverrides::default();
    server_overrides.base.endpoint_protocol = Some(protocol);
    let mut client_overrides = c2_config::ClientIpcConfigOverrides::default();
    client_overrides.base.endpoint_protocol = Some(protocol);
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id),
        server_ipc_overrides: Some(server_overrides),
        client_ipc_overrides: Some(client_overrides),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let address = runtime.server_address().expect("server address");
    (runtime, host, address)
}

/// Waits until a strict probe on `protocol` reaches the endpoint.
fn wait_until_pingable(address: &str, protocol: LocalEndpointProtocol) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ping_direct_ipc_with_protocol(address, protocol, Duration::from_millis(200))
            .expect("valid address")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{address} never answered on {}", protocol.as_str());
}

static PLATFORM_TEST_ID: AtomicU64 = AtomicU64::new(0);

/// Starts `test_count` distinct servers of `protocol`, unless this platform
/// rejects that protocol first.
///
/// Unix returns the real runtimes, hosts and bound addresses; the callers then
/// prove real managed-v2 communication through the public API — route
/// registration, an `IpcClient`-backed direct call, restart identity,
/// explicit admin ping/shutdown on the selected protocol, and a legacy probe
/// that never crosses into the managed namespace. Nothing on that path is
/// mocked.
///
/// Windows returns no servers for `managed-v2` and asserts the concrete
/// negative branch instead: the endpoint derivation is a normalized
/// configuration error carrying `ErrorKind::Unsupported`, exactly the
/// classification `c2-config` documents. Legacy named pipes stay on the real
/// serving path on both platforms.
#[cfg(unix)]
fn platform_servers(
    protocol: LocalEndpointProtocol,
    test_count: usize,
) -> (Vec<Runtime>, Vec<c2_core::Host>, Vec<String>) {
    let suffix = PLATFORM_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut runtimes = Vec::with_capacity(test_count);
    let mut hosts = Vec::with_capacity(test_count);
    let mut addresses = Vec::with_capacity(test_count);
    for index in 0..test_count {
        let (runtime, host, address) =
            host_with_protocol(unique_name(&format!("platform-{suffix}-{index}")), protocol);
        runtimes.push(runtime);
        hosts.push(host);
        addresses.push(address);
    }
    (runtimes, hosts, addresses)
}

#[cfg(windows)]
fn platform_servers(
    protocol: LocalEndpointProtocol,
    test_count: usize,
) -> (Vec<Runtime>, Vec<c2_core::Host>, Vec<String>) {
    if protocol == LocalEndpointProtocol::ManagedV2 {
        let error = direct_ipc_endpoint_with_protocol(
            &format!("ipc://{}", unique_name("windows-managed")),
            protocol,
        )
        .expect_err("Windows has no managed-v2 endpoint to derive or bind");
        assert_windows_unsupported_configuration(&error);
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let suffix = PLATFORM_TEST_ID.fetch_add(1, Ordering::Relaxed);
    let mut runtimes = Vec::with_capacity(test_count);
    let mut hosts = Vec::with_capacity(test_count);
    let mut addresses = Vec::with_capacity(test_count);
    for index in 0..test_count {
        let (runtime, host, address) =
            host_with_protocol(unique_name(&format!("platform-{suffix}-{index}")), protocol);
        runtimes.push(runtime);
        hosts.push(host);
        addresses.push(address);
    }
    (runtimes, hosts, addresses)
}

/// Asserts a managed-v2 request on Windows fails as a normalized,
/// concretely-classified configuration error, not as a panic or a silent
/// fallback to the legacy namespace.
#[cfg(windows)]
fn assert_windows_unsupported_configuration(error: &c2_core::LifecycleError) {
    let c2_core::LifecycleError::Configuration(message) = error else {
        panic!("a platform rejection must stay a configuration error: {error:?}");
    };
    assert!(
        message.contains("managed-v2") && message.contains("not supported on Windows"),
        "the configuration error must name the unsupported protocol and platform: {message}"
    );
    let native = c2_config::LocalEndpoint::from_address_with_protocol(
        &format!("ipc://{}", unique_name("unsupported-kind")),
        LocalEndpointProtocol::ManagedV2,
    )
    .expect_err("Windows must reject the filesystem protocol");
    assert_eq!(native.kind(), std::io::ErrorKind::Unsupported);
}

#[test]
fn explicit_protocol_selects_exactly_one_os_endpoint() {
    let address = format!("ipc://{}", unique_name("protocol-endpoint"));
    let legacy = direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::LegacyV1)
        .expect("the legacy endpoint must derive on every platform");
    assert_eq!(legacy.protocol(), LocalEndpointProtocol::LegacyV1);

    #[cfg(unix)]
    {
        assert_eq!(
            legacy.os_name(),
            std::ffi::OsStr::new(&format!(
                "/tmp/c_two_ipc/{}.sock",
                address.strip_prefix("ipc://").unwrap()
            ))
        );
        let managed = direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::ManagedV2)
            .expect("managed endpoint");
        assert_eq!(managed.protocol(), LocalEndpointProtocol::ManagedV2);
        let managed_name = managed.os_name().to_string_lossy();
        assert!(
            managed_name.starts_with("/tmp/c2-") && managed_name.contains("/v2.2/"),
            "managed endpoint must live in the private versioned namespace: {managed_name}"
        );
        assert_ne!(legacy.os_name(), managed.os_name());
        // The derivation is pure and repeatable.
        assert_eq!(
            managed,
            direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::ManagedV2)
                .expect("repeat derivation")
        );
    }

    // Windows has no managed endpoint to select: the request must fail as the
    // concrete unsupported-platform configuration error instead of quietly
    // resolving the legacy pipe.
    #[cfg(windows)]
    {
        let error = direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::ManagedV2)
            .expect_err("Windows must reject a managed-v2 endpoint");
        assert_windows_unsupported_configuration(&error);
        // The derivation is pure and repeatable on this platform too.
        assert_eq!(
            legacy,
            direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::LegacyV1)
                .expect("repeat legacy derivation")
        );
    }
}

#[test]
fn unresolved_admin_probe_uses_the_process_client_protocol() {
    // `direct_ipc_endpoint` and `ping_direct_ipc` without an explicit protocol
    // must derive the same endpoint the resolved process policy names. The
    // environment is pinned here because another test in this binary mutates
    // it and the harness runs tests in parallel.
    let _guard = env_lock();
    let _env = ProtocolEnv::set(None);
    let address = format!("ipc://{}", unique_name("resolved-default"));
    assert_eq!(
        direct_ipc_endpoint(&address).expect("default endpoint"),
        direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::LegacyV1)
            .expect("legacy endpoint")
    );
    assert!(
        !ping_direct_ipc(&address, Duration::from_millis(20)).expect("valid address"),
        "an absent server is not an error and is not present"
    );
}

/// Environment mutation is serialized in this binary, because the resolution
/// under test reads the real process environment.
#[test]
fn resolved_admin_probe_follows_an_environment_protocol() {
    let _guard = env_lock();
    let _env = ProtocolEnv::set(Some("managed-v2"));
    let address = format!("ipc://{}", unique_name("resolved-env"));

    // With a resolved process policy of managed-v2 both facades move together:
    // endpoint diagnostics and the probe can never disagree.
    let legacy = direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::LegacyV1)
        .expect("legacy endpoint");

    #[cfg(unix)]
    {
        let resolved = direct_ipc_endpoint(&address).expect("resolved endpoint");
        assert_eq!(
            resolved,
            direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::ManagedV2)
                .expect("managed endpoint")
        );
        assert_ne!(resolved, legacy);
    }

    // Resolving managed-v2 on Windows is the concrete unsupported-platform
    // configuration error: the diagnostics facade must report that rejection
    // rather than silently falling back to the legacy pipe, and the explicit
    // legacy derivation keeps working.
    #[cfg(windows)]
    {
        let error =
            direct_ipc_endpoint(&address).expect_err("Windows must reject the resolved policy");
        assert_windows_unsupported_configuration(&error);
        let re_requested =
            direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::ManagedV2)
                .expect_err("Windows must reject an explicit managed-v2 endpoint");
        assert_windows_unsupported_configuration(&re_requested);
        assert_eq!(
            legacy,
            direct_ipc_endpoint_with_protocol(&address, LocalEndpointProtocol::LegacyV1)
                .expect("legacy endpoint")
        );
    }
}

#[test]
fn client_cannot_change_the_endpoint_protocol_after_the_domain_freezes() {
    let mut overrides = c2_config::ClientIpcConfigOverrides::default();
    overrides.base.endpoint_protocol = Some(LocalEndpointProtocol::ManagedV2);
    let runtime = Runtime::new(RuntimeOptions {
        client_ipc_overrides: Some(overrides),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime");

    assert_eq!(protocol_of(&runtime), LocalEndpointProtocol::ManagedV2);
    assert!(!runtime.client_config_frozen());

    // A failed direct connect still freezes the domain: the endpoint protocol
    // is part of the frozen policy and cannot be swapped afterwards.
    let absent = format!("ipc://{}", unique_name("absent-protocol"));
    let expected = release().expected_route("unused-route").expect("route");
    assert!(runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: absent.clone()
            }
        )
        .is_err());
    assert!(runtime.client_config_frozen());

    let mut replacement = c2_config::ClientIpcConfigOverrides::default();
    replacement.base.endpoint_protocol = Some(LocalEndpointProtocol::LegacyV1);
    assert!(
        runtime.set_client_ipc_overrides(Some(replacement)).is_err(),
        "a frozen client domain must reject a protocol change"
    );
    assert_eq!(protocol_of(&runtime), LocalEndpointProtocol::ManagedV2);
}

/// Resolves the process client IPC policy the same way the pool does.
fn protocol_of(runtime: &Runtime) -> LocalEndpointProtocol {
    let overrides = runtime.client_ipc_overrides().unwrap_or_default();
    let resolved = c2_config::ConfigResolver::resolve_client_ipc(
        overrides,
        c2_config::RuntimeConfigOverrides::default(),
        c2_config::ConfigSources::from_process(),
    )
    .expect("resolved client config");
    resolved.base.endpoint_protocol
}

#[test]
fn managed_host_serves_calls_and_legacy_admin_probe_cannot_stop_it() {
    let (runtimes, hosts, addresses) = platform_servers(LocalEndpointProtocol::ManagedV2, 1);
    let managed_is_supported = !addresses.is_empty();
    if !managed_is_supported {
        // Windows: `platform_servers` already asserted the concrete
        // unsupported-platform configuration error. There is no managed
        // endpoint on this platform to serve, and the legacy named pipe keeps
        // its own real test below.
        assert!(cfg!(windows), "only Windows may lack a managed-v2 endpoint");
        return;
    }

    let runtime = &runtimes[0];
    let host = &hosts[0];
    let address = &addresses[0];
    let route_name = unique_name("managed-route");
    let definition = definition(&route_name);
    let expected = definition.expected_route().clone();
    let _registration = host.register(definition).expect("register");

    wait_until_pingable(address, LocalEndpointProtocol::ManagedV2);
    // The public call path reaches the managed endpoint over real IPC.
    let client = runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: address.clone(),
            },
        )
        .expect("managed-v2 client connects");
    assert_eq!(
        client.call_owned("echo", b"managed").expect("call"),
        b"managed"
    );

    // Nothing was probed sideways: the legacy namespace never answered and a
    // legacy admin shutdown cannot stop the managed server.
    assert!(
        !ping_direct_ipc_with_protocol(
            address,
            LocalEndpointProtocol::LegacyV1,
            Duration::from_millis(100)
        )
        .expect("valid address"),
        "a legacy probe must not reach a managed server"
    );
    let legacy_shutdown = shutdown_direct_ipc_with_protocol(
        address,
        LocalEndpointProtocol::LegacyV1,
        Duration::from_millis(100),
    )
    .expect("valid address");
    // The wrong namespace holds no listener, so the probe reports that
    // endpoint as stopped while `shutdown_started` stays false. What proves
    // correctness is that the managed server is untouched below: a probe that
    // had actually reached it would have set `shutdown_started`.
    assert!(
        !legacy_shutdown.shutdown_started,
        "a legacy probe reached the managed server: {legacy_shutdown:?}"
    );
    assert_eq!(
        client.call_owned("echo", b"still-alive").expect("call"),
        b"still-alive",
        "the managed server must survive a legacy admin probe"
    );

    // A managed-v2 admin shutdown reaches exactly the server that bound it.
    let managed_shutdown = shutdown_direct_ipc_with_protocol(
        address,
        LocalEndpointProtocol::ManagedV2,
        Duration::from_millis(500),
    )
    .expect("valid address");
    assert!(managed_shutdown.acknowledged, "{managed_shutdown:?}");
    assert!(managed_shutdown.shutdown_started);
    assert!(!managed_shutdown.server_stopped);
}

/// A resolved protocol is also a restart identity: the second binding of one
/// logical address must reuse the first bind's OS endpoint, never drift to the
/// other namespace.
#[test]
fn resolved_protocol_restart_reuses_the_same_os_endpoint() {
    for protocol in [
        LocalEndpointProtocol::LegacyV1,
        LocalEndpointProtocol::ManagedV2,
    ] {
        let (runtimes, hosts, addresses) = platform_servers(protocol, 1);
        if addresses.is_empty() {
            assert_eq!(protocol, LocalEndpointProtocol::ManagedV2);
            assert!(cfg!(windows), "only Windows may lack a managed-v2 endpoint");
            continue;
        }
        let address = &addresses[0];
        let route_name = unique_name("restart-route");
        let definition = definition(&route_name);
        let _registration = hosts[0].register(definition).expect("register");
        wait_until_pingable(address, protocol);

        let first = direct_ipc_endpoint_with_protocol(address, protocol).expect("first endpoint");
        let expected = release().expected_route(&route_name).expect("route");
        let client = runtimes[0]
            .connect(
                expected,
                Connect::DirectIpc {
                    address: address.clone(),
                },
            )
            .expect("the host serves the route");
        assert_eq!(client.call_owned("ping", b"").expect("ping"), b"");

        // A second, independent resolution of the same logical address and
        // protocol names the same OS endpoint: a restart never drifts.
        let second = direct_ipc_endpoint_with_protocol(address, protocol).expect("second endpoint");
        assert_eq!(first.os_name(), second.os_name());
        assert_eq!(first.protocol(), protocol);
        assert_eq!(first, second);
    }
}

#[test]
fn managed_admin_shutdown_cannot_stop_a_legacy_server() {
    let (runtimes, hosts, addresses) = platform_servers(LocalEndpointProtocol::LegacyV1, 1);
    let runtime = &runtimes[0];
    let host = &hosts[0];
    let address = &addresses[0];
    let route_name = unique_name("legacy-route");
    let definition = definition(&route_name);
    let expected = definition.expected_route().clone();
    let _registration = host.register(definition).expect("register");
    let client = runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: address.clone(),
            },
        )
        .expect("legacy client connects");

    #[cfg(unix)]
    {
        assert!(
            !ping_direct_ipc_with_protocol(
                address,
                LocalEndpointProtocol::ManagedV2,
                Duration::from_millis(100)
            )
            .expect("valid address"),
            "a managed probe must not reach a legacy server"
        );
        let managed_shutdown = shutdown_direct_ipc_with_protocol(
            address,
            LocalEndpointProtocol::ManagedV2,
            Duration::from_millis(100),
        )
        .expect("valid address");
        assert!(
            !managed_shutdown.shutdown_started,
            "a managed probe reached the legacy server: {managed_shutdown:?}"
        );
    }
    #[cfg(windows)]
    {
        // A managed-v2 admin probe is refused by the platform before any I/O,
        // so it can never reach the legacy named pipe.
        let error = ping_direct_ipc_with_protocol(
            address,
            LocalEndpointProtocol::ManagedV2,
            Duration::from_millis(100),
        )
        .expect_err("Windows must reject a managed-v2 probe");
        assert_windows_unsupported_configuration(&error);
        let error = shutdown_direct_ipc_with_protocol(
            address,
            LocalEndpointProtocol::ManagedV2,
            Duration::from_millis(100),
        )
        .expect_err("Windows must reject a managed-v2 shutdown probe");
        assert_windows_unsupported_configuration(&error);
    }
    assert_eq!(
        client.call_owned("echo", b"legacy-alive").expect("call"),
        b"legacy-alive"
    );
    // The shared endpoint projection agrees with the admin probe's protocol;
    // the default policy is legacy-v1 unless the process configured otherwise,
    // and `direct_ipc_endpoint` with an explicit legacy protocol is exact.
    let endpoint = direct_ipc_endpoint_with_protocol(address, LocalEndpointProtocol::LegacyV1)
        .expect("legacy endpoint");
    assert_eq!(endpoint.protocol(), LocalEndpointProtocol::LegacyV1);
}

#[test]
fn client_and_server_must_agree_on_the_endpoint_protocol() {
    // A managed-v2 client must never reach a legacy-v1 server for the same
    // logical address: on Unix the strict connect fails against the absent
    // managed endpoint, and on Windows the managed derivation is itself the
    // concrete unsupported-platform error. Neither platform may silently fall
    // back to the other namespace.
    let (_server_runtime, host, address) =
        host_with_protocol(unique_name("legacy-only"), LocalEndpointProtocol::LegacyV1);
    let route_name = unique_name("legacy-only-route");
    let definition = definition(&route_name);
    let expected = definition.expected_route().clone();
    let _registration = host.register(definition).expect("register");
    wait_until_pingable(&address, LocalEndpointProtocol::LegacyV1);

    let mut overrides = c2_config::ClientIpcConfigOverrides::default();
    overrides.base.endpoint_protocol = Some(LocalEndpointProtocol::ManagedV2);
    let client_runtime = Runtime::new(RuntimeOptions {
        client_ipc_overrides: Some(overrides),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("client runtime");
    let outcome = client_runtime.connect(
        expected.clone(),
        Connect::DirectIpc {
            address: address.clone(),
        },
    );
    let error = outcome.expect_err("a managed-v2 client must not fall back to the legacy endpoint");
    #[cfg(windows)]
    {
        let message = error.to_string();
        assert!(
            message.contains("managed-v2") && message.contains("not supported on Windows"),
            "the refused connect must carry the concrete platform classification: {message}"
        );
    }
    #[cfg(unix)]
    {
        let _ = &error;
    }
}
