//! Core owns local endpoint resolution and its immutable Runtime I/O domain.
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    ConfigSources, Connect, EncodedClient, EncodedService, EnvFilePolicy, EnvMap, Error,
    HostOptions, LifecycleError, LocalEndpointContext, LocalEndpointOptions, MethodDefinition,
    Runtime, RuntimeOptions, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const TIMEOUT: Duration = Duration::from_secs(5);

fn runtime() -> Runtime {
    Runtime::new(RuntimeOptions {
        server_id: Some("same-server".into()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap()
}

fn default_runtime() -> Runtime {
    let runtime = runtime();
    runtime
        .set_local_endpoint_with_sources(LocalEndpointOptions::default(), ConfigSources::empty())
        .unwrap();
    runtime
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap()
}

#[test]
fn default_query_preserves_native_name_and_does_not_freeze_any_domain() {
    let runtime = default_runtime();
    let expected = LocalEndpointContext::default_for_platform().unwrap();
    let address = runtime.ensure_server().unwrap().ipc_address;
    assert_eq!(address, "ipc://same-server");
    assert_eq!(runtime.local_endpoint_context().unwrap(), expected);
    assert_eq!(
        runtime.local_endpoint(&address).unwrap(),
        c2_core::LocalEndpoint::from_address(&address).unwrap()
    );
    assert!(!runtime.local_endpoint_frozen());
    assert!(!runtime.client_config_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
    assert!(runtime.outgoing_memory_observer().is_none());
    runtime.shutdown_without_host(TIMEOUT);
    assert!(!runtime.local_endpoint_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
}

#[test]
fn default_context_admin_freeze_and_same_context_noop_are_platform_neutral() {
    let runtime = default_runtime();
    let context = runtime.local_endpoint_context().unwrap();
    // Zero-budget control attempts still select the immutable I/O domain,
    // without creating an endpoint or a memory domain.
    assert!(
        !runtime
            .ping_direct_ipc("ipc://absent", Duration::ZERO)
            .unwrap()
    );
    assert!(runtime.local_endpoint_frozen());
    runtime
        .set_local_endpoint_with_sources(LocalEndpointOptions::default(), ConfigSources::empty())
        .unwrap();
    assert_eq!(runtime.local_endpoint_context().unwrap(), context);
    assert!(!runtime.client_config_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
}

#[test]
fn invalid_root_is_explicit_and_does_not_change_runtime() {
    let runtime = default_runtime();
    let original = runtime.local_endpoint_context().unwrap();
    let error = runtime
        .set_local_endpoint_with_sources(
            LocalEndpointOptions {
                unix_root: Some(PathBuf::from("relative")),
            },
            ConfigSources::empty(),
        )
        .unwrap_err();
    assert!(matches!(error, LifecycleError::Configuration(_)));
    #[cfg(windows)]
    assert!(error.to_string().contains("not applicable"));
    #[cfg(unix)]
    assert!(error.to_string().contains("absolute"));
    assert_eq!(runtime.local_endpoint_context().unwrap(), original);
    assert!(!runtime.local_endpoint_frozen());
}

#[test]
fn http_only_runtime_does_not_resolve_invalid_local_root() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "http_only_process_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("C2_IPC_ROOT", "invalid-relative-root")
        .env_remove("C2_ENV_FILE")
        .env_remove("C2_RELAY_ANCHOR_ADDRESS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated process fixture, invoked by its parent test"]
fn http_only_process_child() {
    let runtime = runtime();
    runtime.ensure_server().unwrap();
    assert!(runtime.outgoing_memory_stats().is_none());
    // An unsupported HTTP URL is rejected without network I/O. The actual
    // ExplicitRelay path still resolves its client settings and control client.
    let error = runtime
        .connect(
            release().expected_route("route").unwrap(),
            Connect::ExplicitRelay {
                relay_url: "unsupported://endpoint.invalid".into(),
            },
        )
        .unwrap_err();
    assert!(!error.to_string().contains("local endpoint"), "{error}");
    assert!(!runtime.local_endpoint_frozen());
    assert!(!runtime.client_config_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
    assert!(runtime.local_endpoint_context().is_err());
}

#[cfg(unix)]
mod unix {
    use super::*;

    struct Roots(PathBuf);
    impl Roots {
        fn new() -> Self {
            // The application provisions the final private directories.
            let path = PathBuf::from(format!(
                "/tmp/e{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn root(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn create(&self, name: &str) -> PathBuf {
            let root = self.root(name);
            std::fs::create_dir(&root).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            root
        }
    }
    impl Drop for Roots {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn options(root: &Path) -> LocalEndpointOptions {
        LocalEndpointOptions {
            unix_root: Some(root.to_owned()),
        }
    }

    fn at_root(runtime: &Runtime, root: &Path) {
        runtime
            .set_local_endpoint_with_sources(options(root), ConfigSources::empty())
            .unwrap();
    }

    fn sources(root: &Path, file: EnvFilePolicy) -> ConfigSources {
        ConfigSources {
            env_file: file,
            process_env: EnvMap::from([("C2_IPC_ROOT".into(), root.to_str().unwrap().into())]),
        }
    }

    #[test]
    fn nonexistent_root_query_is_pure_and_normalized_options_can_replace_it() {
        let roots = Roots::new();
        let missing = roots.root("m");
        let runtime = runtime();
        at_root(&runtime, &missing);
        let context = runtime.local_endpoint_context().unwrap();
        assert_eq!(context.unix_root(), Some(missing.as_path()));
        runtime.ensure_server().unwrap();
        runtime.local_endpoint("ipc://same-server").unwrap();
        assert!(!missing.exists());
        assert!(!runtime.local_endpoint_frozen());
        assert!(!runtime.client_config_frozen());
        at_root(&runtime, &roots.root("o"));
    }

    #[test]
    fn explicit_process_file_default_precedence_uses_real_source_maps() {
        let roots = Roots::new();
        let file = roots.root("settings.env");
        let from_file = roots.root("f");
        let process = roots.root("p");
        let explicit = roots.root("e");
        std::fs::write(&file, format!("C2_IPC_ROOT={}\n", from_file.display())).unwrap();
        let runtime = runtime();
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions::default(),
                ConfigSources {
                    env_file: EnvFilePolicy::Path(file.clone()),
                    process_env: EnvMap::new(),
                },
            )
            .unwrap();
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(from_file.as_path())
        );
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions::default(),
                sources(&process, EnvFilePolicy::Path(file.clone())),
            )
            .unwrap();
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(process.as_path())
        );
        runtime
            .set_local_endpoint_with_sources(
                options(&explicit),
                sources(&process, EnvFilePolicy::Path(file)),
            )
            .unwrap();
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(explicit.as_path())
        );
        assert!(!runtime.local_endpoint_frozen());
        assert!(!from_file.exists() && !process.exists() && !explicit.exists());
    }

    #[test]
    fn failed_connect_freezes_root_and_memory_domain_across_close_and_reopen() {
        let roots = Roots::new();
        let missing = roots.root("m");
        let other = roots.root("o");
        let runtime = runtime();
        at_root(&runtime, &missing);
        runtime
            .set_client_ipc_overrides(Some(c2_config::ClientIpcConfigOverrides {
                pool_enabled: Some(false),
                shm_backing_budget_bytes: Some(12345),
                ..Default::default()
            }))
            .unwrap();
        let connect = || {
            runtime.connect(
                release().expected_route("route").unwrap(),
                Connect::DirectIpc {
                    address: "ipc://absent".into(),
                },
            )
        };
        assert!(connect().is_err());
        assert!(!missing.exists());
        assert!(runtime.local_endpoint_frozen());
        assert!(runtime.client_config_frozen());
        let observer = runtime.outgoing_memory_observer().unwrap();
        assert_eq!(observer.limits().shm_backing_budget_bytes, 12345);
        assert_eq!(
            runtime.set_local_endpoint(options(&other)),
            Err(LifecycleError::ConfigFrozen)
        );
        at_root(&runtime, &missing.join("."));
        assert!(matches!(
            runtime.set_client_ipc_overrides(None),
            Err(LifecycleError::ClientConfigFrozen)
        ));
        runtime.shutdown_without_host(TIMEOUT);
        assert!(connect().is_err());
        assert!(
            observer
                .downgrade()
                .ptr_eq(&runtime.outgoing_memory_observer().unwrap().downgrade())
        );
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(missing.as_path())
        );
    }

    #[test]
    fn setter_racing_first_freeze_selects_one_domain() {
        let roots = Roots::new();
        let a = roots.root("a");
        let b = roots.root("b");
        for _ in 0..32 {
            let runtime = runtime();
            at_root(&runtime, &a);
            let barrier = Arc::new(Barrier::new(2));
            let worker_runtime = runtime.clone();
            let worker_barrier = barrier.clone();
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                worker_runtime
                    .ping_direct_ipc("ipc://absent", Duration::ZERO)
                    .unwrap();
                worker_runtime.local_endpoint_context().unwrap()
            });
            barrier.wait();
            let set = runtime.set_local_endpoint_with_sources(options(&b), ConfigSources::empty());
            let selected = worker.join().unwrap();
            match set {
                Ok(()) => assert_eq!(selected.unix_root(), Some(b.as_path())),
                Err(LifecycleError::ConfigFrozen) => {
                    assert_eq!(selected.unix_root(), Some(a.as_path()))
                }
                other => panic!("unexpected setter result: {other:?}"),
            }
            assert_eq!(runtime.local_endpoint_context().unwrap(), selected);
            assert!(!runtime.client_config_frozen());
        }
    }

    #[test]
    fn frozen_query_does_not_reopen_resolver_files() {
        let roots = Roots::new();
        let file = roots.root("env");
        let root = roots.root("a");
        std::fs::write(&file, format!("C2_IPC_ROOT={}\n", root.display())).unwrap();
        let runtime = runtime();
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions::default(),
                ConfigSources {
                    env_file: EnvFilePolicy::Path(file.clone()),
                    process_env: EnvMap::new(),
                },
            )
            .unwrap();
        runtime
            .ping_direct_ipc("ipc://absent", Duration::ZERO)
            .unwrap();
        let original = runtime.local_endpoint_context().unwrap();
        // A directory is unreadable as an env file. Frozen observers and
        // subsequent controls must bypass resolver reads entirely.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert_eq!(runtime.local_endpoint_context().unwrap(), original);
        runtime
            .ping_direct_ipc("ipc://absent", Duration::ZERO)
            .unwrap();
        assert_eq!(runtime.local_endpoint_context().unwrap(), original);
        assert!(runtime.outgoing_memory_stats().is_none());
    }

    struct Tagged(u8);
    impl EncodedService for Tagged {
        fn invoke(&self, method: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
            match method {
                0 => Ok(Vec::new()),
                1 => Ok([&[self.0], request].concat()),
                _ => Err(C2Error::new(ErrorCode::ProtocolViolation, "unknown method")),
            }
        }
    }

    fn definition(tag: u8) -> ServiceDefinition {
        let release = release();
        ServiceDefinition::new(
            &release,
            release.reference(),
            "same-route",
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
            Arc::new(Tagged(tag)),
        )
        .unwrap()
    }

    fn connect(runtime: &Runtime) -> c2_core::Client {
        runtime
            .connect(
                release().expected_route("same-route").unwrap(),
                Connect::DirectIpc {
                    address: "ipc://same-server".into(),
                },
            )
            .unwrap()
    }

    fn two_domains_and_restart(a: &Path, b: &Path, process_fixture: bool) {
        let memory_runtime = |live_budget| {
            Runtime::new(RuntimeOptions {
                server_id: Some("same-server".into()),
                use_process_relay_anchor: false,
                shm_threshold: Some(1024),
                server_ipc_overrides: Some(c2_config::ServerIpcConfigOverrides {
                    pool_segment_size: Some(64 * 1024),
                    reassembly_segment_size: Some(64 * 1024),
                    shm_backing_budget_bytes: Some(0),
                    file_backing_budget_bytes: Some(0),
                    chunk_size: Some(8 * 1024),
                    ..Default::default()
                }),
                client_ipc_overrides: Some(c2_config::ClientIpcConfigOverrides {
                    pool_segment_size: Some(64 * 1024),
                    reassembly_segment_size: Some(64 * 1024),
                    shm_backing_budget_bytes: Some(1024 * 1024),
                    live_reassembly_budget_bytes: Some(live_budget),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap()
        };
        let runtime_a = memory_runtime(1024 * 1024);
        let runtime_b = memory_runtime(2 * 1024 * 1024);
        if !process_fixture {
            at_root(&runtime_a, a);
        }
        at_root(&runtime_b, b);
        let host_a = runtime_a
            .host(HostOptions::default().without_relay())
            .unwrap();
        let host_b = runtime_b
            .host(HostOptions::default().without_relay())
            .unwrap();
        let _route_a = host_a.register(definition(b'A')).unwrap();
        let _route_b = host_b.register(definition(b'B')).unwrap();
        assert_eq!(runtime_a.server_address(), runtime_b.server_address());
        assert_ne!(host_a.local_endpoint(), host_b.local_endpoint());
        assert_eq!(
            host_a.local_endpoint_context(),
            &runtime_a.local_endpoint_context().unwrap()
        );
        assert_eq!(
            host_b.local_endpoint_context(),
            &runtime_b.local_endpoint_context().unwrap()
        );
        assert_ne!(
            runtime_a.ensure_server().unwrap().server_instance_id,
            runtime_b.ensure_server().unwrap().server_instance_id
        );

        let client_a = connect(&runtime_a);
        let client_b = connect(&runtime_b);
        assert_eq!(client_a.call_owned("echo", b"test").unwrap(), b"Atest");
        assert_eq!(client_b.call_owned("echo", b"test").unwrap(), b"Btest");
        assert_eq!(
            connect(&runtime_a).call_owned("echo", b"cached").unwrap(),
            b"Acached"
        );
        assert_eq!(
            connect(&runtime_b).call_owned("echo", b"cached").unwrap(),
            b"Bcached"
        );
        let mut wrong = release().expected_route("same-route").unwrap();
        wrong.signature_hash = "0".repeat(64);
        assert!(matches!(runtime_a.connect(wrong, Connect::DirectIpc {
            address: "ipc://same-server".into(),
        }), Err(Error::Semantic(error)) if error.code == ErrorCode::ContractMismatch));
        let missing = release().expected_route("missing-route").unwrap();
        assert!(matches!(runtime_b.connect(missing, Connect::DirectIpc {
            address: "ipc://same-server".into(),
        }), Err(Error::Semantic(error)) if error.code == ErrorCode::ResourceNotFound));

        // An explicitly scoped native pool projects the actual handshake
        // identities and retains one cache domain for the same logical address.
        let pool_a = c2_ipc::ClientPool::with_endpoint_context(
            TIMEOUT,
            runtime_a.local_endpoint_context().unwrap(),
        );
        let pool_b = c2_ipc::ClientPool::with_endpoint_context(
            TIMEOUT,
            runtime_b.local_endpoint_context().unwrap(),
        );
        let pooled_a = pool_a.acquire("ipc://same-server", None).unwrap();
        let pooled_b = pool_b.acquire("ipc://same-server", None).unwrap();
        assert!(Arc::ptr_eq(
            &pooled_a,
            &pool_a.acquire("ipc://same-server", None).unwrap()
        ));
        assert!(!Arc::ptr_eq(&pooled_a, &pooled_b));
        assert_eq!(
            pooled_a.server_instance_id(),
            Some(
                runtime_a
                    .ensure_server()
                    .unwrap()
                    .server_instance_id
                    .as_str()
            )
        );
        assert_eq!(
            pooled_b.server_instance_id(),
            Some(
                runtime_b
                    .ensure_server()
                    .unwrap()
                    .server_instance_id
                    .as_str()
            )
        );
        pooled_a
            .validate_route_contract(&release().expected_route("same-route").unwrap())
            .unwrap();
        pooled_b
            .validate_route_contract(&release().expected_route("same-route").unwrap())
            .unwrap();
        let credential_a = match c2_core::inspect_endpoint(host_a.local_endpoint()) {
            c2_core::EndpointInspection::Present(credential) => credential,
            other => panic!("credential after native readiness: {other:?}"),
        };
        assert_eq!(
            credential_a.endpoint().context(),
            host_a.local_endpoint_context()
        );
        assert!(pool_a.close_all(TIMEOUT).error.is_none());
        assert!(pool_b.close_all(TIMEOUT).error.is_none());

        let outgoing_a = runtime_a.outgoing_memory_observer().unwrap();
        let outgoing_b = runtime_b.outgoing_memory_observer().unwrap();
        assert_eq!(
            outgoing_a.limits().live_reassembly_budget_bytes,
            1024 * 1024
        );
        assert_eq!(
            outgoing_b.limits().live_reassembly_budget_bytes,
            2 * 1024 * 1024
        );
        let payload = vec![42; 32 * 1024];
        let mut held = client_a.call_held("echo", &payload).unwrap();
        assert_eq!(held.bytes(), [&[b'A'], payload.as_slice()].concat());
        let retained = outgoing_a.snapshot().reassembly.used_bytes;
        assert!(
            retained > 0,
            "chunked response retains its outgoing-domain charge"
        );
        assert!(!outgoing_a.downgrade().ptr_eq(&outgoing_b.downgrade()));
        let endpoint = host_a.local_endpoint().clone();
        let old_identity = runtime_a.ensure_server().unwrap();

        if process_fixture {
            // Only this dedicated --exact child fixture writes its environment.
            unsafe {
                std::env::set_var("C2_IPC_ROOT", b);
            }
            assert_eq!(
                runtime_a.local_endpoint_context().unwrap(),
                endpoint.context().clone()
            );
            assert_eq!(
                c2_core::direct_ipc_endpoint("ipc://same-server").unwrap(),
                host_b.local_endpoint().clone()
            );
        }
        assert!(
            runtime_a
                .ping_direct_ipc("ipc://same-server", TIMEOUT)
                .unwrap()
        );
        let ack = runtime_a
            .shutdown_direct_ipc("ipc://same-server", TIMEOUT)
            .unwrap();
        assert!(ack.acknowledged && ack.shutdown_started);
        assert!(
            runtime_b
                .ping_direct_ipc("ipc://same-server", TIMEOUT)
                .unwrap()
        );
        let outcome = host_a.shutdown();
        assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
        assert_eq!(outgoing_a.snapshot().reassembly.used_bytes, retained);
        assert!(outgoing_a.snapshot().shm.used_bytes > 0);
        runtime_a.clear_server_identity().unwrap();
        let restarted = runtime_a
            .host(HostOptions::default().without_relay())
            .unwrap();
        let _new_route = restarted.register(definition(b'C')).unwrap();
        assert_eq!(restarted.local_endpoint(), &endpoint);
        assert_ne!(
            runtime_a.ensure_server().unwrap().server_instance_id,
            old_identity.server_instance_id
        );
        assert_eq!(
            connect(&runtime_a).call_owned("echo", b"new").unwrap(),
            b"Cnew"
        );
        assert_eq!(client_b.call_owned("echo", b"live").unwrap(), b"Blive");
        assert!(
            outgoing_a
                .downgrade()
                .ptr_eq(&runtime_a.outgoing_memory_observer().unwrap().downgrade())
        );
        assert!(
            outgoing_b
                .downgrade()
                .ptr_eq(&runtime_b.outgoing_memory_observer().unwrap().downgrade())
        );
        assert_eq!(outgoing_a.snapshot().reassembly.used_bytes, retained);
        held.invalidate_then_release(|| Ok(())).unwrap();
        assert_eq!(outgoing_a.snapshot().reassembly.used_bytes, 0);
        assert!(restarted.shutdown().runtime_barrier_error.is_none());
        assert!(host_b.shutdown().runtime_barrier_error.is_none());
    }

    #[test]
    fn long_directories_preserve_rpc_contract_hold_pools_and_restart() {
        use std::os::unix::fs::DirBuilderExt;
        let roots = Roots::new();
        let segment = format!("目录 with spaces {}", "x".repeat(110));
        let long = (0..4).fold(roots.root("long"), |path, _| path.join(&segment));
        let a = long.join("a");
        let b = long.join("b");
        assert!(a.as_os_str().len() >= 512);
        for root in [&a, &b] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)
                .unwrap();
        }
        two_domains_and_restart(&a, &b, false);
    }

    #[test]
    fn two_runtime_hosts_and_pools_isolate_identical_logical_addresses() {
        let roots = Roots::new();
        let a = roots.create("a");
        let b = roots.create("b");
        two_domains_and_restart(&a, &b, false);
    }

    #[test]
    fn process_environment_flip_is_confined_to_child_fixture() {
        let roots = Roots::new();
        let a = roots.create("a");
        let b = roots.create("b");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::endpoint_process_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("C2_ENDPOINT_PROCESS_CHILD", "1")
            .env("C2_IPC_ROOT", &a)
            .env("C2_ENDPOINT_OTHER_ROOT", &b)
            .env_remove("C2_ENV_FILE")
            .env_remove("C2_RELAY_ANCHOR_ADDRESS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated process fixture, invoked by its parent test"]
    fn endpoint_process_child() {
        if std::env::var("C2_ENDPOINT_PROCESS_CHILD").as_deref() != Ok("1") {
            return;
        }
        let a = PathBuf::from(std::env::var("C2_IPC_ROOT").unwrap());
        let b = PathBuf::from(std::env::var("C2_ENDPOINT_OTHER_ROOT").unwrap());
        two_domains_and_restart(&a, &b, true);
    }
}
