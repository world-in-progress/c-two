use super::*;
use crate::test_support::isolated_pair;
use c2_wire::buddy::encode_buddy_payload;
use c2_wire::flags::FLAG_BUDDY;
use std::collections::HashMap;
use std::sync::Arc;

use crate::dispatcher::{CrmCallback, CrmError, CrmRoute};
use crate::scheduler::{ConcurrencyMode, Scheduler};

#[cfg(unix)]
struct TestEndpointRoot {
    _directory: tempfile::TempDir,
    context: c2_config::LocalEndpointContext,
}

#[cfg(unix)]
std::thread_local! {
    static TEST_ENDPOINT_ROOT: TestEndpointRoot = {
        let directory = tempfile::Builder::new()
            .prefix("c2-p5-server-")
            .tempdir_in(std::env::temp_dir())
            .expect("create isolated test endpoint root");
        let context = c2_config::LocalEndpointContext::with_unix_root(directory.path())
            .expect("use isolated test endpoint root");
        TestEndpointRoot { _directory: directory, context }
    };
}

fn test_parse_local_endpoint(address: &str) -> Result<LocalEndpoint, ServerError> {
    #[cfg(unix)]
    {
        TEST_ENDPOINT_ROOT.with(|root| {
            root.context.endpoint(address).map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidInput {
                    ServerError::Config(error.to_string())
                } else {
                    ServerError::Io(error)
                }
            })
        })
    }
    #[cfg(windows)]
    {
        parse_local_endpoint(address)
    }
}

fn test_server_new(address: &str, config: ServerIpcConfig) -> Result<Server, ServerError> {
    Server::new_with_endpoint(test_parse_local_endpoint(address)?, config)
}

fn test_server_with_identity(
    address: &str,
    config: ServerIpcConfig,
    identity: ServerIdentity,
) -> Result<Server, ServerError> {
    Server::new_with_identity_and_endpoint(test_parse_local_endpoint(address)?, config, identity)
}

// -- address parsing --

#[test]
fn parse_ipc_address() {
    let p = test_parse_local_endpoint("ipc://my_region").unwrap();
    assert_eq!(p.address(), "ipc://my_region");
}

#[test]
fn parse_legacy_v3_rejected() {
    assert!(parse_local_endpoint("ipc-v3://region42").is_err());
}

#[test]
fn parse_invalid_scheme() {
    assert!(parse_local_endpoint("tcp://host").is_err());
}

#[test]
fn parse_empty_region() {
    assert!(parse_local_endpoint("ipc://").is_err());
}

#[test]
fn parse_rejects_path_like_region() {
    for address in [
        "ipc://../escape",
        "ipc://bad/name",
        "ipc://bad\\name",
        "ipc://.",
        "ipc://..",
        "ipc:// leading",
        "ipc://trailing ",
        "ipc://bad\nname",
    ] {
        assert!(
            parse_local_endpoint(address).is_err(),
            "address should be rejected: {address:?}"
        );
    }
}

// -- server construction --

#[test]
fn server_new_default_config() {
    let s = test_server_new("ipc://test_srv", ServerIpcConfig::default()).unwrap();
    assert_eq!(s.local_endpoint().address(), "ipc://test_srv");
}

#[tokio::test]
async fn server_restart_reuses_the_native_endpoint() {
    // A restart bind must reuse the platform native endpoint.
    let config = ServerIpcConfig::default();
    let address = "ipc://protocol_restart_srv";
    let first = test_server_new(address, config.clone()).unwrap();
    let first_endpoint = first.local_endpoint().clone();
    drop(first);

    let second = test_server_new(address, config).unwrap();
    assert_eq!(second.local_endpoint(), &first_endpoint);
}

#[test]
fn server_new_derives_stable_server_id_and_instance_identity() {
    let first = test_server_new("ipc://identity_srv", ServerIpcConfig::default()).unwrap();
    let second = test_server_new("ipc://identity_srv", ServerIpcConfig::default()).unwrap();

    assert_eq!(first.server_id(), "identity_srv");
    assert_eq!(first.identity().server_id, "identity_srv");
    assert_eq!(first.server_instance_id().len(), 32);
    assert_ne!(first.server_instance_id(), second.server_instance_id());
}

#[test]
fn server_new_with_identity_uses_validated_identity() {
    let identity = c2_wire::handshake::ServerIdentity {
        server_id: "server-explicit".to_string(),
        server_instance_id: "instance-explicit".to_string(),
    };

    let server = test_server_with_identity(
        "ipc://identity_explicit",
        ServerIpcConfig::default(),
        identity.clone(),
    )
    .unwrap();

    assert_eq!(server.identity(), &identity);
    assert_eq!(server.server_id(), "server-explicit");
    assert_eq!(server.server_instance_id(), "instance-explicit");
}

#[test]
fn server_new_with_identity_rejects_invalid_instance_id() {
    let too_long = "a".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
    for server_instance_id in ["../bad", "实例", too_long.as_str()] {
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "server-explicit".to_string(),
            server_instance_id: server_instance_id.to_string(),
        };

        let err =
            test_server_with_identity("ipc://identity_bad", ServerIpcConfig::default(), identity)
                .err()
                .expect("invalid identity should be rejected");

        assert!(err.to_string().contains("server_instance_id"));
    }
}

#[test]
fn server_new_with_identity_rejects_server_id_too_long_for_wire() {
    let identity = c2_wire::handshake::ServerIdentity {
        server_id: "s".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
        server_instance_id: "instance-explicit".to_string(),
    };

    let err = test_server_with_identity(
        "ipc://identity_bad_server_id",
        ServerIpcConfig::default(),
        identity,
    )
    .err()
    .expect("overlong server_id should be rejected");

    assert!(err.to_string().contains("server_id"));
}

#[test]
fn server_new_bad_address() {
    assert!(test_server_new("http://bad", ServerIpcConfig::default()).is_err());
}

#[test]
fn server_new_bad_config() {
    let cfg = ServerIpcConfig {
        max_payload_size: 0,
        ..ServerIpcConfig::default()
    };
    assert!(test_server_new("ipc://x", cfg).is_err());
}

fn unique_readiness_address(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("ipc://{prefix}_{}_{}", std::process::id(), n)
}

fn unique_response_pool_prefix(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("/c2sw{:04x}{:04x}", std::process::id() as u16, n as u16) + label
}

fn small_response_pool(label: &str) -> crate::ResponsePool {
    crate::ResponsePool::new(
        MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                ..PoolConfig::default()
            },
            unique_response_pool_prefix(label),
        )
        .unwrap(),
    )
}

async fn closed_writer() -> Arc<ConnectionWriter> {
    let (client, server) = isolated_pair().await;
    client.abort_handle().abort();
    drop(client);
    let (_reader, write_half) = server.into_split();
    let conn = Arc::new(Connection::new(0));
    conn.set_handshake_done(true).unwrap();
    ConnectionWriter::spawn(
        write_half,
        conn,
        Duration::from_secs(30),
        Arc::new(Semaphore::new(1024)),
    )
    .0
}

async fn endpoint_connects(endpoint: &LocalEndpoint) -> bool {
    LocalStream::connect(endpoint, DEFAULT_CONNECT_TIMEOUT)
        .await
        .is_ok()
}

#[cfg(unix)]
struct TestEndpointCleanup(c2_local::EndpointCredential);

#[cfg(unix)]
impl Drop for TestEndpointCleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Preserve the failing assertion first. Cleanup is restricted
            // to this test's exact object and cannot remove a successor.
            eprintln!(
                "test endpoint cleanup: {:?}",
                c2_local::reap_endpoint(self.0.endpoint(), &self.0)
            );
        }
    }
}

#[cfg(unix)]
async fn cleanup_endpoint_on_failure(endpoint: &LocalEndpoint) -> TestEndpointCleanup {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match c2_local::inspect_endpoint(endpoint) {
                c2_local::EndpointInspection::Present(credential) => {
                    return TestEndpointCleanup(credential);
                }
                c2_local::EndpointInspection::IoError(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("cannot capture test endpoint identity: {other:?}"),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("test endpoint credential acquisition must finish")
}

#[tokio::test]
async fn wait_until_ready_times_out_before_start() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("ready_timeout"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );

    let err = server
        .wait_until_ready(Duration::from_millis(1))
        .await
        .expect_err("server that was never started must time out");

    assert!(
        matches!(err, ServerError::StartupTimeout(ServerStartupTimeout {
            stage: ServerStartupStage::Readiness,
            timeout,
        }) if timeout == Duration::from_millis(1))
    );
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Initialized);
    assert!(!server.is_ready());
    assert!(!server.is_running());
}

#[tokio::test(start_paused = true)]
async fn delayed_ready_and_ping_share_the_startup_budget() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("ready_ping_budget"),
            Default::default(),
        )
        .unwrap(),
    );
    let publish = server.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(35)).await;
        publish.set_lifecycle_state(ServerLifecycleState::Ready);
    });
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_millis(40);
    let error = server.wait_until_responsive(timeout).await.unwrap_err();
    assert!(
        matches!(error, ServerError::StartupTimeout(ServerStartupTimeout {
            stage: ServerStartupStage::Responsiveness,
            timeout: budget,
        }) if budget == timeout)
    );
    // Virtual time proves that publishing Ready does not restart the budget.
    assert!(started.elapsed() >= timeout && started.elapsed() < Duration::from_millis(45));
    server.finalize_runtime_stopped();
}

#[tokio::test]
async fn run_sets_ready_then_shutdown_sets_stopped() {
    #[cfg(unix)]
    let directory = tempfile::Builder::new()
        .prefix("c2-ready-credential-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(unix)]
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
        .unwrap()
        .endpoint(&unique_readiness_address("ready_state"))
        .unwrap();
    #[cfg(unix)]
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    #[cfg(not(unix))]
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("ready_state"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    assert!(server.endpoint_credential().is_none());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };

    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
    assert!(server.is_ready());
    assert!(server.is_running());
    assert!(endpoint_connects(server.local_endpoint()).await);
    let credential = server
        .endpoint_credential()
        .expect("bound listener credential");
    assert_eq!(credential.endpoint(), server.local_endpoint());

    server.request_shutdown_signal();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopping);
    assert_eq!(server.endpoint_credential(), Some(credential.clone()));
    runner.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    assert!(server.endpoint_credential().is_none());
    assert!(!server.is_ready());
    assert!(!server.is_running());

    server.begin_start_attempt().unwrap();
    assert!(server.endpoint_credential().is_none());
    let restarted = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let next = server.endpoint_credential().expect("restarted credential");
    #[cfg(unix)]
    assert_ne!(next, credential);
    #[cfg(not(unix))]
    assert_eq!(next, credential);
    server.request_shutdown_signal();
    restarted.await.unwrap().unwrap();
    assert!(server.endpoint_credential().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_run_keeps_snapshot_until_runtime_finalization() {
    let directory = tempfile::Builder::new()
        .prefix("c2-cancel-credential-")
        .tempdir_in("/tmp")
        .unwrap();
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
        .unwrap()
        .endpoint(&unique_readiness_address("cancel_credential"))
        .unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let credential = server.endpoint_credential().expect("ready credential");
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
    assert_eq!(server.endpoint_credential(), Some(credential.clone()));
    server.finalize_runtime_stopped();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    assert!(server.endpoint_credential().is_none());
    assert!(matches!(
        c2_local::reap_endpoint(server.local_endpoint(), &credential),
        EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent
    ));
}

/// A transient namespace operation must not turn a successful shutdown
/// into the best-effort Drop path, which leaves the socket for a reaper.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_join_retires_socket_after_namespace_gate_contention() {
    let directory = tempfile::Builder::new()
        .prefix("c2-shutdown-")
        .tempdir_in("/tmp")
        .unwrap();
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
        .unwrap()
        .endpoint(&unique_readiness_address("shutdown_gate_contention"))
        .unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let _cleanup = cleanup_endpoint_on_failure(server.local_endpoint()).await;

    let socket = std::path::Path::new(server.local_endpoint().os_name());
    // Open the existing coordinator only; do not create or replace any
    // namespace object. On Unix File::lock uses the same flock as bind.
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(socket.parent().unwrap().join(".gate"))
        .unwrap();
    gate.lock().unwrap();
    server.request_shutdown_signal();

    // Release on a concrete close observation, not after a guessed delay.
    // Old run publishes Stopped after Drop misses this held gate. Explicit
    // close closes its descriptor first, letting this probe release the
    // gate while owner retirement is still waiting. No sleeps or reap.
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !server.is_running() || tokio::net::UnixStream::connect(socket).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    gate.unlock().unwrap();
    closed.expect("shutdown must close the listener while its namespace gate is busy");

    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    assert!(
        !socket.exists(),
        "successful shutdown/join must retire its socket"
    );
    assert!(!socket.with_extension("lease").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_cleanup_failure_is_terminal_observable_and_restartable() {
    use std::os::unix::fs::FileExt;

    let directory = tempfile::Builder::new()
        .prefix("c2-shutdown-")
        .tempdir_in("/tmp")
        .unwrap();
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
        .unwrap()
        .endpoint(&unique_readiness_address("shutdown_bad_record"))
        .unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let _cleanup = cleanup_endpoint_on_failure(server.local_endpoint()).await;
    let socket = std::path::Path::new(server.local_endpoint().os_name());
    let lease_path = socket.with_extension("lease");
    let record = std::fs::read(&lease_path).unwrap();
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lease_path)
        .unwrap();
    // Corrupt only our own record through its pinned descriptor, leaving
    // the shared coordinator and every other endpoint untouched.
    lease.write_all_at(b"invalid", 0).unwrap();
    let shutdown = server.shutdown_and_wait(Duration::from_secs(2)).await;
    let joined = tokio::time::timeout(Duration::from_secs(2), runner).await;
    // Restore the fixture before any assertions can panic. Recovery and
    // later bind use the original identity; no name-only unlink or reap.
    lease.write_all_at(&record, 0).unwrap();

    let run_error = joined
        .unwrap()
        .unwrap()
        .expect_err("retirement failure must fail run");
    assert!(
        run_error.to_string().contains("InvalidRecord"),
        "{run_error}"
    );
    assert!(shutdown.unwrap_err().to_string().contains("InvalidRecord"));
    assert!(matches!(
        server.lifecycle_state(),
        ServerLifecycleState::Failed(_)
    ));
    assert!(server.endpoint_credential().is_none());
    assert!(!server.is_running());
    assert!(socket.exists(), "unverified socket must be preserved");
    assert!(tokio::net::UnixStream::connect(socket).await.is_err());
    lease
        .try_lock()
        .expect("failed close must release its listener lease");
    lease.unlock().unwrap();
    drop(lease);

    // Lifecycle observation still fences terminal work. Every shutdown
    // completion facade must retain the actual error, including reobservation.
    server.wait_until_terminal().await.unwrap();
    server
        .wait_until_stopped(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(server.shutdown_and_wait_unbounded().await.is_err());
    assert!(
        server
            .observe_external_shutdown_outcomes_unbounded()
            .await
            .is_err()
    );
    assert!(
        server
            .observe_external_shutdown_outcomes(Duration::from_secs(2))
            .await
            .is_err()
    );
    server.finalize_runtime_stopped();
    assert!(matches!(
        server.lifecycle_state(),
        ServerLifecycleState::Failed(_)
    ));

    server.begin_start_attempt().unwrap();
    let restarted = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let _restart_cleanup = cleanup_endpoint_on_failure(server.local_endpoint()).await;
    assert!(endpoint_connects(server.local_endpoint()).await);
    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    restarted.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    assert!(!socket.exists());
    assert!(!lease_path.exists());
}

#[test]
fn wait_until_responsive_only_returns_after_ping_round_trip() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("responsive_ready"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let runner = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(server.run())
        })
    };
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(server.wait_until_responsive(Duration::from_secs(2)))
        .expect("server should answer control ping before readiness returns");

    server.request_shutdown_signal();
    runner.join().unwrap().unwrap();
}

#[tokio::test]
async fn false_shutdown_updates_cannot_truncate_fragmented_handshake() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("fragmented_start"),
            ServerIpcConfig {
                ..ServerIpcConfig::default()
            },
        )
        .unwrap(),
    );
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    negotiate_test_version(&mut client).await;
    let payload = c2_wire::handshake::encode_client_handshake(0, "").unwrap();
    let frame = encode_frame(0, FLAG_HANDSHAKE, &payload);
    for bytes in frame.chunks(3) {
        client.write_all(bytes).await.unwrap();
        // Startup resets shutdown to false. Repeated observations of that
        // state must never cancel a partly consumed protocol frame.
        server.shutdown_tx.send_replace(false);
        tokio::task::yield_now().await;
    }
    let mut header = [0_u8; frame::HEADER_SIZE];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    let (length, body) = frame::decode_total_len(&header).unwrap();
    let (header, _) = frame::decode_frame_body(body, length).unwrap();
    assert!(header.is_handshake());
    let mut payload = vec![0_u8; header.payload_len()];
    client.read_exact(&mut payload).await.unwrap();
    assert!(
        decode_handshake(&payload, true)
            .unwrap()
            .server_identity
            .is_some()
    );
    drop(client);
    server
        .shutdown_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn active_socket_is_not_unlinked_by_second_server() {
    let address = unique_readiness_address("active_socket");
    let first = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let first_runner = {
        let first = Arc::clone(&first);
        tokio::spawn(async move { first.run().await })
    };
    first
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(endpoint_connects(first.local_endpoint()).await);

    let second = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let second_result = tokio::time::timeout(Duration::from_millis(200), {
        let second = Arc::clone(&second);
        async move { second.run().await }
    })
    .await;

    match second_result {
        Ok(Err(err)) => {
            assert!(
                matches!(err, ServerError::Bind(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
                "unexpected error: {err}",
            );
        }
        Ok(Ok(())) => panic!("second server unexpectedly started and stopped cleanly"),
        Err(_) => {
            second.request_shutdown_signal();
            panic!("second server hung instead of rejecting the active socket");
        }
    }

    assert!(first.is_ready());
    assert!(endpoint_connects(first.local_endpoint()).await);
    first.request_shutdown_signal();
    first_runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_after_failed_bind_does_not_unlink_active_socket() {
    let address = unique_readiness_address("failed_bind_shutdown");
    let first = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let first_runner = {
        let first = Arc::clone(&first);
        tokio::spawn(async move { first.run().await })
    };
    first
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(endpoint_connects(first.local_endpoint()).await);

    let second = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let second_result = tokio::time::timeout(Duration::from_millis(200), {
        let second = Arc::clone(&second);
        async move { second.run().await }
    })
    .await;
    let err = second_result
        .expect("second server hung instead of rejecting the active socket")
        .expect_err("second bind must fail");
    assert!(
        matches!(err, ServerError::Bind(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
        "unexpected error: {err}",
    );
    second.request_shutdown_signal();

    assert!(first.is_ready());
    assert!(endpoint_connects(first.local_endpoint()).await);
    first.request_shutdown_signal();
    first_runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn same_server_can_start_again_after_shutdown() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("restart_after_shutdown"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );

    server.begin_start_attempt().unwrap();
    let first_runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(endpoint_connects(server.local_endpoint()).await);

    server.request_shutdown_signal();
    first_runner.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);

    server.begin_start_attempt().unwrap();
    let second_runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(endpoint_connects(server.local_endpoint()).await);

    server.request_shutdown_signal();
    second_runner.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
}

fn retained_backing_server(label: &str) -> Arc<Server> {
    let mut config = ServerIpcConfig::default();
    config.base.pool_segment_size = 64 * 1024;
    config.base.reassembly_segment_size = 64 * 1024;
    config.base.pool_prewarm_segments = 1;
    config.base.pool_min_retained_segments = 1;
    config.pool_decay_seconds = 3600.0;
    Arc::new(test_server_new(&unique_readiness_address(label), config).unwrap())
}

#[tokio::test]
async fn stopped_server_releases_idle_backing_before_terminal_and_rewarms_on_restart() {
    let server = retained_backing_server("retired_idle_backing");
    let observer = server.memory_budget_observer();
    let reassembly = server.chunk_registry.pool();
    let handle = reassembly.write().alloc_shm_handle(4096).unwrap();
    reassembly.write().release_handle(handle);
    assert_eq!(observer.snapshot().shm.used_bytes, 2 * 64 * 1024);
    let generation = server.response_pool.read().segment_generation(0).unwrap();
    let runner = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(5))
        .await
        .unwrap();
    server
        .shutdown_and_wait(Duration::from_secs(5))
        .await
        .unwrap();
    // Keep Server, response-pool metadata and reassembly-pool metadata alive.
    // Its terminal watch must already prove idle backing was actually unmapped.
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
    assert_eq!(observer.snapshot().file.used_bytes, 0);
    assert_eq!(observer.snapshot().reassembly.used_bytes, 0);
    assert_eq!(server.response_pool.read().stats().buddy_idle_bytes, 0);
    assert_eq!(reassembly.read().stats().buddy_idle_bytes, 0);
    runner.await.unwrap().unwrap();

    server.begin_start_attempt().unwrap();
    assert_eq!(observer.snapshot().shm.used_bytes, 64 * 1024);
    assert!(server.response_pool.read().segment_generation(0).unwrap() > generation);
    let runner = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(5))
        .await
        .unwrap();
    server
        .shutdown_and_wait(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn stopped_server_keeps_live_backing_until_its_original_allocations_release() {
    let server = retained_backing_server("retired_live_backing");
    let observer = server.memory_budget_observer();
    let response = server.response_pool_arc();
    let reassembly = server.chunk_registry.pool();
    let response_handle = response.inner.write().alloc_shm_handle(4096).unwrap();
    let reassembly_handle = reassembly.write().alloc_shm_handle(4096).unwrap();
    let charged = observer.snapshot().shm.used_bytes;
    assert_eq!(charged, 2 * 64 * 1024);
    let runner = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(5))
        .await
        .unwrap();
    server
        .shutdown_and_wait(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(observer.snapshot().shm.used_bytes, charged);
    assert_eq!(response.read().handle_slice(&response_handle).len(), 4096);
    assert_eq!(
        reassembly.read().handle_slice(&reassembly_handle).len(),
        4096
    );
    response.inner.write().release_handle(response_handle);
    assert_eq!(observer.snapshot().shm.used_bytes, 64 * 1024);
    reassembly.write().release_handle(reassembly_handle);
    // No GC tick, Server Drop or extra shutdown call is needed to refund it.
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn failed_bind_can_retry_after_socket_released() {
    let address = unique_readiness_address("retry_after_failed_bind");
    let first = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    first.begin_start_attempt().unwrap();
    let first_runner = {
        let first = Arc::clone(&first);
        tokio::spawn(async move { first.run().await })
    };
    first
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();

    let second = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    second.begin_start_attempt().unwrap();
    let (ready_a, ready_b, failed) = tokio::join!(
        second.wait_until_ready(Duration::from_secs(2)),
        second.wait_until_ready(Duration::from_secs(2)),
        second.run(),
    );
    let failed = failed.expect_err("active socket should reject the first attempt");
    assert!(
        matches!(failed, ServerError::Bind(ref error) if error.kind() == std::io::ErrorKind::AddrInUse),
        "unexpected error: {failed}",
    );
    assert!(matches!(
        second.lifecycle_state(),
        ServerLifecycleState::Failed(_)
    ));
    assert!(second.endpoint_credential().is_none());

    first.request_shutdown_signal();
    first_runner.await.unwrap().unwrap();

    second.begin_start_attempt().unwrap();
    assert!(second.endpoint_credential().is_none());
    let second_runner = {
        let second = Arc::clone(&second);
        tokio::spawn(async move { second.run().await })
    };
    second
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    assert!(endpoint_connects(second.local_endpoint()).await);
    assert!(second.endpoint_credential().is_some());

    second.request_shutdown_signal();
    second_runner.await.unwrap().unwrap();
    let (ServerError::Failed(a), ServerError::Failed(b)) =
        (ready_a.unwrap_err(), ready_b.unwrap_err())
    else {
        panic!("readiness waiters must retain the same bind failure");
    };
    assert_eq!(a, b);
    assert!(!a.message.contains("config error"));
    assert!(a.startup_timeout.is_none());
    let cause = a.bind_cause.expect("actual bind cause");
    assert_eq!(cause.error.kind, std::io::ErrorKind::AddrInUse);
    assert_eq!(
        cause.error.raw_os_error, None,
        "listener lease owns the rejection"
    );
    assert_eq!(second.lifecycle_state(), ServerLifecycleState::Stopped);
}

#[tokio::test]
async fn wait_until_stopped_fences_restart_after_shutdown() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("wait_stop_restart"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );

    server.begin_start_attempt().unwrap();
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();

    server.request_shutdown_signal();
    server
        .wait_until_stopped(Duration::from_secs(2))
        .await
        .unwrap();
    server
        .begin_start_attempt()
        .expect("stopped lifecycle should permit a new start attempt");

    runner.await.unwrap().unwrap();
}

#[test]
fn finalize_runtime_stopped_terminalizes_running_states_but_preserves_failed() {
    let server = test_server_new(
        &unique_readiness_address("finalize_runtime"),
        ServerIpcConfig::default(),
    )
    .unwrap();

    server.set_lifecycle_state(ServerLifecycleState::Stopping);
    server.finalize_runtime_stopped();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    assert!(server.endpoint_credential().is_none());

    server.set_lifecycle_state(ServerLifecycleState::Failed(ServerFailure::new(
        "bind failed".to_string(),
    )));
    server.finalize_runtime_stopped();
    assert_eq!(
        server.lifecycle_state(),
        ServerLifecycleState::Failed(ServerFailure::new("bind failed".to_string())),
    );
}

// -- route registration --

struct Echo;
impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        _request: Vec<u8>,
        _response_pool: Arc<crate::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::Inline(b"echo".to_vec()))
    }
}

fn make_route(name: &str) -> CrmRoute {
    CrmRoute {
        name: name.into(),
        route_uid: format!("{name}-uid-0001"),
        crm_ns: "test.grid".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
        scheduler: Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            HashMap::new(),
        )),
        callback: Arc::new(Echo),
        method_names: vec!["step".into(), "query".into()],
    }
}

fn make_host_route(server: &Server, name: &str) -> CrmRoute {
    let mut route = make_route(name);
    route.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::ReadParallel,
        HashMap::new(),
        SchedulerLimits::default(),
        Arc::clone(&server.admission),
    ));
    route
}

struct AdmissionFixture {
    server: Arc<Server>,
    #[cfg(unix)]
    _root: tempfile::TempDir,
}

fn admission_fixture() -> AdmissionFixture {
    let address = unique_readiness_address("admission");
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("c2-admission-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(unix)]
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(root.path())
        .unwrap()
        .endpoint(&address)
        .unwrap();
    #[cfg(windows)]
    let endpoint = test_parse_local_endpoint(&address).unwrap();
    let server = Server::new_with_identity_and_endpoint(
        endpoint,
        ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        },
        ServerIdentity {
            server_id: "admission-host".into(),
            server_instance_id: uuid::Uuid::new_v4().simple().to_string(),
        },
    )
    .unwrap();
    AdmissionFixture {
        server: Arc::new(server),
        #[cfg(unix)]
        _root: root,
    }
}

fn admission_built_route(server: &Server, name: &str, mode: ConcurrencyMode) -> BuiltRoute {
    let route = make_route(name);
    server
        .build_route(
            RouteBuildSpec {
                name: route.name,
                crm_ns: route.crm_ns,
                crm_name: route.crm_name,
                crm_ver: route.crm_ver,
                abi_hash: route.abi_hash,
                signature_hash: route.signature_hash,
                method_names: route.method_names,
                access_map: HashMap::new(),
                concurrency_mode: mode,
                limits: SchedulerLimits::default(),
            },
            route.callback,
        )
        .unwrap()
}

async fn admission_route(server: &Server, name: &str, mode: ConcurrencyMode) -> Arc<CrmRoute> {
    let built = admission_built_route(server, name, mode);
    let reservation = server.reserve_route(built).await.unwrap();
    server.commit_reserved_route(reservation).await.unwrap();
    server.dispatcher.read().await.resolve(name).unwrap()
}

async fn admission_poll_once<F: std::future::Future>(
    mut future: std::pin::Pin<&mut F>,
) -> std::task::Poll<F::Output> {
    std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test]
async fn admission_dispatcher_waiter_keeps_original_generation() {
    let fixture = admission_fixture();
    let server = &fixture.server;
    let route = admission_route(server, "grid", ConcurrencyMode::Parallel).await;
    let mut identity = call_identity("grid");
    identity.route_uid = route.route_uid.clone();
    let dispatcher = server.dispatcher.write().await;
    let mut request = Box::pin(reserve_accepted_route_execution(server, &identity, 0));
    assert!(admission_poll_once(request.as_mut()).await.is_pending());
    {
        let mut admission = server.admission.lock();
        admission.terminate();
        admission.begin_start();
    }
    server.admission.notify.notify_waiters();
    drop(dispatcher);
    assert!(matches!(
        request.await,
        Err(RouteAdmissionError::Acquire(
            SchedulerAcquireError::Unavailable
        ))
    ));
    assert_eq!(route.scheduler.snapshot().pending, 0);
}

#[tokio::test]
async fn admission_global_queue_rejects_old_route_guard() {
    let fixture = admission_fixture();
    let server = &fixture.server;
    let route = admission_route(server, "grid", ConcurrencyMode::Parallel).await;
    let global = server.execution_scheduler.blocking_acquire(0).unwrap();
    let conn = Connection::new(1);
    let pending = route.scheduler.reserve_pending(0).unwrap();
    let mut request = Box::pin(execute_route_request(
        server.execution_scheduler.clone(),
        pending,
        &conn,
        RequestData::Inline(vec![]),
        |_| panic!("old global waiter must not invoke the resource"),
    ));
    assert!(admission_poll_once(request.as_mut()).await.is_pending());
    assert_eq!(route.scheduler.snapshot().active_workers, 1);
    let global_snapshot = server.execution_scheduler.snapshot();
    assert_eq!(
        (global_snapshot.pending, global_snapshot.active_workers),
        (2, 1)
    );
    {
        let mut admission = server.admission.lock();
        admission.terminate();
        admission.begin_start();
    }
    server.admission.notify.notify_waiters();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap(),
        Err(RouteExecutionError::Acquire(
            SchedulerAcquireError::Unavailable
        ))
    ));
    assert_eq!(route.scheduler.snapshot().pending, 0);
    let snapshot = server.execution_scheduler.snapshot();
    assert_eq!((snapshot.pending, snapshot.active_workers), (1, 1));
    drop(global);
    let pending = route.scheduler.reserve_pending(0).unwrap();
    execute_route_request(
        server.execution_scheduler.clone(),
        pending,
        &conn,
        RequestData::Inline(vec![]),
        |_| Ok(ResponseMeta::Inline(vec![])),
    )
    .await
    .unwrap();
    assert_eq!(server.execution_scheduler.snapshot().pending, 0);
}

#[tokio::test]
async fn admission_keeps_contract_identity_and_method_error_priority() {
    let fixture = admission_fixture();
    let server = &fixture.server;
    let route = admission_route(server, "grid", ConcurrencyMode::Parallel).await;
    let mut identity = call_identity("grid");
    identity.route_uid = route.route_uid.clone();
    server.close_business_admission("test").await;
    assert!(matches!(
        reserve_accepted_route_execution(server, &identity, 0).await,
        Err(RouteAdmissionError::Acquire(SchedulerAcquireError::Closed))
    ));
    assert!(matches!(
        reserve_accepted_route_execution(server, &identity, 99).await,
        Err(RouteAdmissionError::UnknownMethod { .. })
    ));
    let mut wrong = identity.clone();
    wrong.route_uid.push_str("-stale");
    assert!(matches!(
        reserve_accepted_route_execution(server, &wrong, 99).await,
        Err(RouteAdmissionError::RouteStale { .. })
    ));
    wrong = identity;
    wrong.abi_hash = "0".repeat(64);
    assert!(matches!(
        reserve_accepted_route_execution(server, &wrong, 99).await,
        Err(RouteAdmissionError::ContractMismatch { .. })
    ));
}

#[tokio::test]
async fn admission_rejects_routes_built_for_another_host() {
    let first = admission_fixture();
    let second = admission_fixture();
    let route = admission_built_route(&first.server, "grid", ConcurrencyMode::Parallel);
    let err = match second.server.reserve_route(route).await {
        Err(err) => err,
        Ok(_) => panic!("foreign scheduler must not bypass the concrete host gate"),
    };
    assert!(err.to_string().contains("another server admission gate"));
}

fn call_identity(name: &str) -> c2_wire::control::RouteCallIdentity {
    c2_wire::control::RouteCallIdentity {
        route_name: name.into(),
        route_uid: format!("{name}-uid-0001"),
        crm_ns: "test.grid".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
    }
}

#[tokio::test]
async fn register_unregister_route() {
    let s = Arc::new(test_server_new("ipc://reg_test", ServerIpcConfig::default()).unwrap());

    let route = make_host_route(&s, "grid");
    let scheduler = route.scheduler.as_ref().clone();
    s.register_route(route).await.unwrap();
    assert!(s.dispatcher.read().await.resolve("grid").is_some());
    assert!(s.contains_route("grid").await);

    assert!(s.unregister_route("grid").await);
    assert!(s.dispatcher.read().await.resolve("grid").is_none());
    assert!(!s.contains_route("grid").await);
    assert!(scheduler.snapshot().closed);
    assert_eq!(
        scheduler.try_acquire(0).unwrap_err(),
        crate::scheduler::SchedulerAcquireError::Closed,
    );
}

#[tokio::test]
async fn reserve_rejects_same_name_while_removed_route_is_draining() {
    let server = Arc::new(
        test_server_new(
            "ipc://route_reuse_while_draining",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let route = make_host_route(&server, "grid");
    let scheduler = route.scheduler.as_ref().clone();
    server.register_route(route).await.unwrap();
    let active_guard = scheduler.try_acquire(0).unwrap();
    let unregister_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.unregister_route("grid").await })
    };
    while !scheduler.snapshot().closed {
        tokio::task::yield_now().await;
    }
    assert!(server.contains_route("grid").await);

    let replacement = make_host_route(&server, "grid");
    let err = match server
        .reserve_route(BuiltRoute::new(replacement).unwrap())
        .await
    {
        Ok(_) => panic!("dispatcher must reject same-name replacement while old route drains"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("route already registered"));
    drop(active_guard);
    assert!(unregister_task.await.unwrap());

    let replacement = make_host_route(&server, "grid");
    let reservation = server
        .reserve_route(BuiltRoute::new(replacement).unwrap())
        .await
        .expect("same-name reserve should succeed after old route is removed");
    server.abort_reserved_route(reservation).await;
}

#[tokio::test]
async fn reserved_route_cannot_commit_after_shutdown_generation_advances() {
    let address = unique_readiness_address("stale_reservation_after_shutdown");
    let server = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .expect("server ready");

    let route = make_host_route(&server, "late");
    let reservation = server
        .reserve_route(BuiltRoute::new(route).unwrap())
        .await
        .unwrap();
    server.request_shutdown_signal();
    server
        .wait_until_stopped(Duration::from_secs(2))
        .await
        .expect("server stopped");

    let err = server
        .commit_reserved_route(reservation)
        .await
        .expect_err("stale pre-shutdown reservation must not commit after stop");
    assert!(
        err.to_string().contains("shutting down"),
        "unexpected error: {err}",
    );
    assert!(
        !server.contains_route("late").await,
        "stale reservation committed a route after shutdown completed",
    );

    runner.await.unwrap().unwrap();
}

async fn check_route_open_interrupted_at_dispatcher(close_admission: bool) {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let address = unique_readiness_address("route_open_interrupted");
    // inspect_endpoint is deliberately nonblocking. A private namespace
    // keeps unrelated parallel tests from holding its inspection gate.
    #[cfg(unix)]
    let directory = tempfile::Builder::new()
        .prefix("c2-admission-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(unix)]
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(directory.path())
        .unwrap()
        .endpoint(&address)
        .unwrap();
    #[cfg(windows)]
    let endpoint = test_parse_local_endpoint(&address).unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let route = make_host_route(&server, "grid");
    let scheduler = Arc::clone(&route.scheduler);
    let token = commit_closed_test_route(&server, route).await;

    // Poll the actual open operation with dispatcher held: it owns the
    // registration mutex and is parked after its initial lifecycle check.
    let dispatcher = server.dispatcher.write().await;
    let mut open = Box::pin(server.open_route_admission(token));
    let first_open_poll = poll_fn(|cx| Poll::Ready(open.as_mut().poll(cx))).await;
    let registration_locked = server.route_registration.try_lock().is_err();
    let mut admission_close = Box::pin(server.close_business_admission("maintenance"));
    let first_close_poll = if close_admission {
        Some(poll_fn(|cx| Poll::Ready(admission_close.as_mut().poll(cx))).await)
    } else {
        server.request_shutdown_signal();
        None
    };
    let interrupted = if close_admission {
        server.business_admission_closed()
    } else {
        matches!(server.lifecycle_state(), ServerLifecycleState::Stopping)
    };
    drop(dispatcher);

    let was_parked = first_open_poll.is_pending();
    let result = match first_open_poll {
        Poll::Pending => open.as_mut().await,
        Poll::Ready(result) => result,
    };
    // Observe before the queued close operation can hide a transient reopen.
    let stayed_closed = scheduler.snapshot().closed;
    let routes = route_records(&server).await;
    if matches!(first_close_poll, Some(Poll::Pending)) {
        admission_close.as_mut().await;
    }
    drop(admission_close);
    drop(open);
    server.request_shutdown_signal();
    let stopped = server.wait_until_stopped(Duration::from_secs(2)).await;
    let run_result = runner.await;
    let endpoint_after_stop = c2_local::inspect_endpoint(server.local_endpoint());

    // The failing baseline must release its listener/lease and join first.
    assert!(stopped.is_ok(), "{stopped:?}");
    run_result.unwrap().unwrap();
    assert!(
        matches!(
            endpoint_after_stop,
            c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
        ),
        "{endpoint_after_stop:?}"
    );
    assert!(
        was_parked && registration_locked,
        "open must be parked at dispatcher"
    );
    assert!(
        interrupted,
        "the real close/signal must precede dispatcher release"
    );
    assert!(result.is_err(), "an interrupted open must fail: {result:?}");
    assert!(
        stayed_closed,
        "open must not reopen admission after the interruption"
    );
    assert!(
        routes
            .iter()
            .all(|route| { route.state == c2_wire::route_catalog_control::RouteStateWire::Closed }),
        "the interrupted open must not publish a Ready catalog record"
    );
}

#[tokio::test]
async fn route_open_rechecks_admission_closure_after_dispatcher_wait() {
    check_route_open_interrupted_at_dispatcher(true).await;
}

#[tokio::test]
async fn route_open_rechecks_shutdown_after_dispatcher_wait() {
    check_route_open_interrupted_at_dispatcher(false).await;
}

async fn commit_closed_test_route(server: &Server, route: CrmRoute) -> RouteAdmissionToken {
    let reservation = server
        .reserve_route(BuiltRoute::new(route).unwrap())
        .await
        .unwrap();
    server
        .commit_reserved_route_closed(reservation)
        .await
        .unwrap()
}

#[tokio::test]
async fn route_admission_token_cannot_open_another_server() {
    let first = test_server_new(
        &unique_readiness_address("admission_token_origin"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let second = test_server_new(
        &unique_readiness_address("admission_token_target"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    // Identical route UID isolates the concrete server binding.
    let token = commit_closed_test_route(&first, make_host_route(&first, "grid")).await;
    let _second_token = commit_closed_test_route(&second, make_host_route(&second, "grid")).await;
    let result = second.open_route_admission(token).await;
    let target = second.route_scheduler_snapshot("grid").await;
    let routes = route_records(&second).await;
    let first_removed = first.unregister_route("grid").await;
    let second_removed = second.unregister_route("grid").await;

    assert!(first_removed && second_removed);
    assert!(
        result.is_err(),
        "a token cannot authorize another server: {result:?}"
    );
    assert!(target.unwrap().closed);
    assert!(
        routes
            .iter()
            .all(|route| { route.state == c2_wire::route_catalog_control::RouteStateWire::Closed })
    );
}

#[tokio::test]
async fn route_admission_token_cannot_open_same_name_replacements() {
    let server = test_server_new(
        &unique_readiness_address("admission_token_replacement"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let mut observations = Vec::new();
    for suffix in ["-replacement-a", "-replacement-b"] {
        let token = commit_closed_test_route(&server, make_host_route(&server, "grid")).await;
        let old_removed = server.unregister_route("grid").await;
        let mut replacement = make_host_route(&server, "grid");
        replacement.route_uid.push_str(suffix);
        let _replacement_token = commit_closed_test_route(&server, replacement).await;
        let result = server.open_route_admission(token).await;
        let snapshot = server.route_scheduler_snapshot("grid").await;
        let routes = route_records(&server).await;
        let replacement_removed = server.unregister_route("grid").await;
        observations.push((
            suffix,
            old_removed,
            replacement_removed,
            result,
            snapshot,
            routes,
        ));
    }

    for (suffix, old_removed, replacement_removed, result, snapshot, routes) in observations {
        assert!(old_removed && replacement_removed);
        assert!(
            result.is_err(),
            "stale token opened replacement ({suffix}): {result:?}"
        );
        assert!(
            snapshot.unwrap().closed,
            "replacement must retain closed admission"
        );
        assert!(routes.iter().all(|route| {
            route.state == c2_wire::route_catalog_control::RouteStateWire::Closed
        }));
    }
}

#[tokio::test]
async fn duplicate_route_registration_is_rejected() {
    let s = Arc::new(test_server_new("ipc://dup_route_test", ServerIpcConfig::default()).unwrap());

    s.register_route(make_host_route(&s, "grid")).await.unwrap();
    let err = s
        .register_route(make_host_route(&s, "grid"))
        .await
        .unwrap_err();

    assert!(err.to_string().contains("already registered"));
}

#[test]
fn built_route_rejects_wire_invalid_route_name() {
    let route = make_route(&"x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1));
    let err = BuiltRoute::new(route).err().expect("invalid route name");
    assert!(err.to_string().contains("route name"));
}

#[test]
fn built_route_rejects_invalid_crm_tag_fields() {
    let mut route = make_route("grid");
    route.crm_name = "Grid\0Injected".to_string();
    let err = BuiltRoute::new(route).err().expect("invalid CRM tag");
    assert!(err.to_string().contains("control characters"));
}

#[test]
fn built_route_rejects_too_many_methods() {
    let mut route = make_route("grid");
    route.method_names = (0..=c2_wire::handshake::MAX_METHODS)
        .map(|i| format!("m{i}"))
        .collect();
    let err = BuiltRoute::new(route).err().expect("too many methods");
    assert!(err.to_string().contains("method count"));
}

#[tokio::test]
async fn register_route_rejects_route_count_overflow_under_write_lock() {
    let s =
        Arc::new(test_server_new("ipc://route_count_test", ServerIpcConfig::default()).unwrap());

    for i in 0..c2_wire::handshake::MAX_ROUTES {
        s.register_route(make_host_route(&s, &format!("route_{i}")))
            .await
            .unwrap();
    }

    let err = s
        .register_route(make_host_route(&s, "overflow"))
        .await
        .unwrap_err();

    assert!(err.to_string().contains("route count"));
    assert!(s.dispatcher.read().await.resolve("overflow").is_none());
}

#[tokio::test]
async fn inline_dispatch_rejects_unknown_method_index_before_callback() {
    use c2_wire::control::{decode_reply_control, encode_call_control};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingCallback {
        calls: Arc<AtomicUsize>,
    }

    impl CrmCallback for CountingCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResponseMeta::Inline(b"should-not-run".to_vec()))
        }
    }

    let server =
        Arc::new(test_server_new("ipc://unknown_method_idx", ServerIpcConfig::default()).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(CountingCallback {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let payload = encode_call_control(&call_identity("grid"), 99).unwrap();
    let (request_id, _flags, reply_payload) = call_over_connection(&server, 42, &payload).await;

    assert_eq!(request_id, 42);
    match decode_reply_control(&reply_payload, 0).unwrap().0 {
        ReplyControl::Error(err) => {
            let message = String::from_utf8_lossy(&err);
            assert!(message.contains("unknown method index 99"));
        }
        other => panic!("expected method-index error reply, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn inline_dispatch_rejects_stale_route_token_before_callback() {
    use c2_wire::control::{decode_reply_control, encode_call_control};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingCallback {
        calls: Arc<AtomicUsize>,
    }

    impl CrmCallback for CountingCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ResponseMeta::Inline(b"should-not-run".to_vec()))
        }
    }

    let server =
        Arc::new(test_server_new("ipc://stale_route_uid", ServerIpcConfig::default()).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(CountingCallback {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let mut identity = call_identity("grid");
    identity.route_uid = "retired-grid-uid".into();

    let payload = encode_call_control(&identity, 0).unwrap();
    let (request_id, _flags, reply_payload) = call_over_connection(&server, 42, &payload).await;

    assert_eq!(request_id, 42);
    match decode_reply_control(&reply_payload, 0).unwrap().0 {
        ReplyControl::Error(err) => {
            let decoded = C2Error::from_wire_bytes(&err)
                .unwrap()
                .expect("route stale must be encoded as a C2 error");
            assert_eq!(decoded.code, ErrorCode::RouteStale);
            assert!(decoded.message.contains("stale route token"));
            assert!(decoded.message.contains("retired-grid-uid"));
        }
        other => panic!("expected route-stale error reply, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_callbacks_respect_server_wide_execution_limit_across_routes() {
    use c2_wire::control::encode_call_control;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep, timeout};

    struct BlockingCallback {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
            self.started.fetch_add(1, Ordering::SeqCst);

            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }

            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let config = ServerIpcConfig {
        max_execution_workers: 1,
        ..ServerIpcConfig::default()
    };
    let server = Arc::new(test_server_new("ipc://server_execution_limit", config).unwrap());

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    for name in ["grid_a", "grid_b"] {
        let mut route = make_host_route(&server, name);
        route.callback = Arc::new(BlockingCallback {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        server.register_route(route).await.unwrap();
    }

    let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
    let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

    let first = {
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            call_over_connection(&server, 1, &payload_a).await;
        })
    };

    timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first callback should start");

    let second = {
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            call_over_connection(&server, 2, &payload_b).await;
        })
    };

    sleep(Duration::from_millis(50)).await;
    let started_while_blocked = started.load(Ordering::SeqCst);
    let peak_while_blocked = peak.load(Ordering::SeqCst);

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(
        started_while_blocked, 1,
        "second route started executing while first callback still held the server-wide execution slot",
    );
    assert_eq!(peak_while_blocked, 1);
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[test]
fn route_waiters_do_not_occupy_blocking_execution_threads() {
    use crate::runtime::ServerRuntimeBuilder;
    use c2_wire::control::encode_call_control;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep, timeout};

    struct BlockingCallback {
        route_a_started: Arc<AtomicUsize>,
        route_b_started: Arc<AtomicUsize>,
        release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            route_name: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            if route_name == "grid_a" {
                self.route_a_started.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*self.release_route_a;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
            } else {
                self.route_b_started.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let config = ServerIpcConfig {
        max_execution_workers: 2,
        max_pending_requests: 8,
        ..ServerIpcConfig::default()
    };
    let rt = ServerRuntimeBuilder::build(&config).unwrap();

    rt.block_on(async move {
        let server = Arc::new(test_server_new("ipc://route_waiter_starvation", config).unwrap());
        let route_a_started = Arc::new(AtomicUsize::new(0));
        let route_b_started = Arc::new(AtomicUsize::new(0));
        let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let route_a_scheduler = Scheduler::with_admission(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(3).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
            Arc::clone(&server.admission),
        );
        let route_b_scheduler = Scheduler::with_admission(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
            Arc::clone(&server.admission),
        );

        for (name, scheduler) in [
            ("grid_a", route_a_scheduler.clone()),
            ("grid_b", route_b_scheduler),
        ] {
            let mut route = make_host_route(&server, name);
            route.scheduler = Arc::new(scheduler);
            route.callback = Arc::new(BlockingCallback {
                route_a_started: Arc::clone(&route_a_started),
                route_b_started: Arc::clone(&route_b_started),
                release_route_a: Arc::clone(&release_route_a),
            });
            server.register_route(route).await.unwrap();
        }

        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

        let first_a = {
            let server = Arc::clone(&server);
            let payload = payload_a.clone();
            tokio::spawn(async move {
                call_over_connection(&server, 1, &payload).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while route_a_started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first route A callback should start");

        let second_a = {
            let server = Arc::clone(&server);
            let payload = payload_a.clone();
            tokio::spawn(async move {
                call_over_connection(&server, 2, &payload).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = route_a_scheduler.snapshot();
                if snapshot.pending >= 2 && snapshot.active_workers == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second route A request should be waiting on route capacity");
        sleep(Duration::from_millis(50)).await;

        let route_b = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                call_over_connection(&server, 3, &payload_b).await;
            })
        };

        let route_b_ready = timeout(Duration::from_millis(250), async {
            while route_b_started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;

        {
            let (lock, cvar) = &*release_route_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first_a.await.unwrap();
        second_a.await.unwrap();
        route_b.await.unwrap();

        route_b_ready.expect("ready route B callback should not be blocked by route A waiter");
    });
}

#[test]
fn unregister_closes_admission_before_waiting_on_blocking_pool_drain() {
    use crate::runtime::ServerRuntimeBuilder;
    use c2_wire::control::encode_call_control;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep, timeout};

    struct BlockingCallback {
        route_a_started: Arc<AtomicUsize>,
        route_b_started: Arc<AtomicUsize>,
        release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            route_name: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            if route_name == "grid_a" {
                self.route_a_started.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*self.release_route_a;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
            } else {
                self.route_b_started.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let config = ServerIpcConfig {
        max_execution_workers: 1,
        max_pending_requests: 8,
        ..ServerIpcConfig::default()
    };
    let rt = ServerRuntimeBuilder::build(&config).unwrap();

    rt.block_on(async move {
        let server = Arc::new(
            test_server_new("ipc://unregister_closes_before_blocking_wait", config).unwrap(),
        );
        let route_a_started = Arc::new(AtomicUsize::new(0));
        let route_b_started = Arc::new(AtomicUsize::new(0));
        let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let route_b_scheduler = Scheduler::with_admission(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits::default(),
            Arc::clone(&server.admission),
        );

        for (name, scheduler) in [
            (
                "grid_a",
                Scheduler::with_admission(
                    ConcurrencyMode::Parallel,
                    HashMap::new(),
                    SchedulerLimits::default(),
                    Arc::clone(&server.admission),
                ),
            ),
            ("grid_b", route_b_scheduler.clone()),
        ] {
            let mut route = make_host_route(&server, name);
            route.scheduler = Arc::new(scheduler);
            route.callback = Arc::new(BlockingCallback {
                route_a_started: Arc::clone(&route_a_started),
                route_b_started: Arc::clone(&route_b_started),
                release_route_a: Arc::clone(&release_route_a),
            });
            server.register_route(route).await.unwrap();
        }

        let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
        let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

        let first = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                call_over_connection(&server, 1, &payload_a).await;
            })
        };

        timeout(Duration::from_secs(1), async {
            while route_a_started.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("route A callback should occupy the only blocking execution thread");

        let second = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                call_over_connection(&server, 2, &payload_b).await;
            })
        };

        let route_b_admitted = timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = route_b_scheduler.snapshot();
                if snapshot.active_workers == 1 && route_b_started.load(Ordering::SeqCst) == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        sleep(Duration::from_millis(50)).await;

        let mut unregister = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.unregister_route("grid_b").await })
        };
        let mut removed_before_release = None;
        let unregister_finished_before_release =
            match timeout(Duration::from_millis(100), &mut unregister).await {
                Ok(result) => {
                    removed_before_release = Some(result.unwrap());
                    true
                }
                Err(_) => false,
            };
        assert_eq!(
            route_b_started.load(Ordering::SeqCst),
            0,
            "route B callback must not start while unregister waits for drain",
        );

        {
            let (lock, cvar) = &*release_route_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        first.await.unwrap();
        second.await.unwrap();
        let removed = match removed_before_release {
            Some(removed) => removed,
            None => unregister.await.unwrap(),
        };
        assert!(
            route_b_admitted,
            "route B request should wait after route admission and before callback start",
        );
        assert!(
            unregister_finished_before_release,
            "unregister must close route admission without waiting for blocking pool capacity",
        );
        assert!(removed);
        assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
    });
}

#[tokio::test]
async fn shutdown_signal_closes_all_route_admissions_before_waiting_for_drain() {
    use std::collections::HashSet;
    use tokio::time::{Duration, timeout};

    let server = Arc::new(
        test_server_new(
            "ipc://shutdown_closes_all_before_drain",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let scheduler_a = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits::default(),
        Arc::clone(&server.admission),
    );
    let scheduler_b = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits::default(),
        Arc::clone(&server.admission),
    );
    for (name, scheduler) in [
        ("grid_a", scheduler_a.clone()),
        ("grid_b", scheduler_b.clone()),
    ] {
        let mut route = make_host_route(&server, name);
        route.scheduler = Arc::new(scheduler);
        server.register_route(route).await.unwrap();
    }

    let guard_a = scheduler_a
        .blocking_acquire(0)
        .expect("route A guard should enter");
    let guard_b = scheduler_b
        .blocking_acquire(0)
        .expect("route B guard should enter");
    server.request_shutdown_signal();
    let close_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            server
                .close_registered_routes_for_shutdown("direct_ipc_shutdown")
                .await
        })
    };

    timeout(Duration::from_secs(1), async {
        while !scheduler_a.snapshot().closed && !scheduler_b.snapshot().closed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown should close at least one route before waiting for drain");
    assert!(
        scheduler_a.snapshot().closed && scheduler_b.snapshot().closed,
        "direct shutdown must close every route admission before waiting for any route to drain"
    );

    drop(guard_a);
    drop(guard_b);
    let outcomes = close_task.await.unwrap();
    let route_names: HashSet<_> = outcomes
        .iter()
        .map(|outcome| outcome.route_name.as_str())
        .collect();
    assert_eq!(route_names, HashSet::from(["grid_a", "grid_b"]));
}

#[tokio::test]
async fn shutdown_signal_waits_for_active_connection_drain_before_stopped() {
    use c2_wire::control::encode_call_control;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, timeout};

    struct BlockingCallback {
        started: Arc<AtomicUsize>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.started.fetch_add(1, Ordering::SeqCst);

            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }

            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let address = unique_readiness_address("shutdown_connection_drain");
    let server = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(BlockingCallback {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    server.register_route(route).await.unwrap();

    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .expect("server ready");

    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .expect("connect idle client");
    let conn_id = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(conn_id) = server.active_connection_ids().into_iter().next() {
                return conn_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection should register");
    assert!(server.active_connection(conn_id).is_some());
    handshake_test_client(&mut client).await;
    let payload = encode_call_control(&call_identity("grid"), 0).unwrap();
    client
        .write_all(&encode_frame(1, c2_wire::flags::FLAG_CALL_V2, &payload))
        .await
        .unwrap();

    timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("callback should start");

    server.request_shutdown_signal();
    server
        .wait_until_stopped(Duration::from_millis(50))
        .await
        .expect_err("server must not report stopped while connection callback is still active");

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    runner.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
}

#[tokio::test]
async fn duplicate_shutdown_signal_does_not_rebroadcast_watch() {
    use tokio::time::{Duration, timeout};

    let server = test_server_new(
        "ipc://duplicate_shutdown_signal_no_rebroadcast",
        ServerIpcConfig::default(),
    )
    .unwrap();
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    let mut shutdown_rx = server.shutdown_tx.subscribe();

    server.request_shutdown_signal();
    timeout(Duration::from_secs(1), shutdown_rx.changed())
        .await
        .expect("first shutdown signal should notify")
        .expect("shutdown watch should remain open");
    assert!(*shutdown_rx.borrow_and_update());

    server.request_shutdown_signal();
    timeout(Duration::from_millis(50), shutdown_rx.changed())
        .await
        .expect_err("duplicate shutdown signal must not wake active connection reads again");
}

#[tokio::test]
async fn unregister_cancels_request_waiting_for_server_execution_slot() {
    use c2_wire::control::encode_call_control;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep, timeout};

    struct BlockingCallback {
        route_a_started: Arc<AtomicUsize>,
        route_b_started: Arc<AtomicUsize>,
        release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            route_name: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            if route_name == "grid_a" {
                self.route_a_started.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*self.release_route_a;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
            } else {
                self.route_b_started.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let config = ServerIpcConfig {
        max_execution_workers: 1,
        ..ServerIpcConfig::default()
    };
    let server =
        Arc::new(test_server_new("ipc://unregister_cancels_global_waiter", config).unwrap());

    let route_a_started = Arc::new(AtomicUsize::new(0));
    let route_b_started = Arc::new(AtomicUsize::new(0));
    let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    for name in ["grid_a", "grid_b"] {
        let mut route = make_host_route(&server, name);
        route.callback = Arc::new(BlockingCallback {
            route_a_started: Arc::clone(&route_a_started),
            route_b_started: Arc::clone(&route_b_started),
            release_route_a: Arc::clone(&release_route_a),
        });
        server.register_route(route).await.unwrap();
    }

    let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
    let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

    let first = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            call_over_connection(&server, 1, &payload_a).await;
        })
    };

    timeout(Duration::from_secs(1), async {
        while route_a_started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("route A callback should occupy the only execution slot");

    let second = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            call_over_connection(&server, 2, &payload_b).await;
        })
    };

    sleep(Duration::from_millis(50)).await;
    let unregister_result = timeout(
        Duration::from_millis(100),
        server.unregister_route("grid_b"),
    )
    .await;
    assert_eq!(
        route_b_started.load(Ordering::SeqCst),
        0,
        "route B callback must not run after route unregister cancels the queued request",
    );

    {
        let (lock, cvar) = &*release_route_a;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    first.await.unwrap();
    second.await.unwrap();
    let removed = unregister_result
        .expect("unregister must cancel a not-started request waiting for server execution");
    assert!(removed);
    assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn connection_close_cancels_request_waiting_for_server_execution_slot() {
    use c2_wire::control::encode_call_control;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, timeout};

    struct BlockingCallback {
        route_a_started: Arc<AtomicUsize>,
        route_b_started: Arc<AtomicUsize>,
        release_route_a: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            route_name: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            if route_name == "grid_a" {
                self.route_a_started.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*self.release_route_a;
                let (released, _) = cvar
                    .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(10), |released| {
                        !*released
                    })
                    .unwrap();
                if !*released {
                    return Err(CrmError::InternalError(
                        "test callback release timed out".into(),
                    ));
                }
            } else {
                self.route_b_started.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    for reason in [
        ConnectionEndReason::PeerCloseMessage,
        ConnectionEndReason::PeerEof,
        ConnectionEndReason::LocalClose,
    ] {
        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        };
        let server = Arc::new(
            test_server_new(
                &unique_readiness_address("connection_cancels_waiter"),
                config,
            )
            .unwrap(),
        );
        let events = server.connection_events();
        let route_a_started = Arc::new(AtomicUsize::new(0));
        let route_b_started = Arc::new(AtomicUsize::new(0));
        let release_route_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut schedulers = Vec::new();
        for name in ["grid_a", "grid_b"] {
            let mut route = make_host_route(&server, name);
            route.callback = Arc::new(BlockingCallback {
                route_a_started: Arc::clone(&route_a_started),
                route_b_started: Arc::clone(&route_b_started),
                release_route_a: Arc::clone(&release_route_a),
            });
            schedulers.push(Arc::clone(&route.scheduler));
            server.register_route(route).await.unwrap();
        }

        let (peer, stream) = isolated_pair().await;
        let mut peer = Some(peer);
        let abort = stream.abort_handle();
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        handshake_test_client(peer.as_mut().unwrap()).await;
        let observation = match events.recv().await.unwrap().unwrap() {
            c2_local::ConnectionEvent::Opened(observation) => observation,
            _ => panic!("expected Opened"),
        };
        let conn = server
            .active_connection(observation.connection_id())
            .unwrap();
        let available = server.pending_requests.available_permits();
        for (rid, route) in [(1, "grid_a"), (2, "grid_b")] {
            let payload = encode_call_control(&call_identity(route), 0).unwrap();
            peer.as_mut()
                .unwrap()
                .write_all(&encode_frame(rid, c2_wire::flags::FLAG_CALL_V2, &payload))
                .await
                .unwrap();
            if rid == 1 {
                wait_until("route A occupies the only execution slot", || {
                    route_a_started.load(Ordering::SeqCst) == 1
                })
                .await;
            }
        }
        wait_until("route B queued behind the active callback", || {
            conn.inflight_count() == 2
                && server.pending_requests.available_permits() == available - 2
        })
        .await;

        match reason {
            ConnectionEndReason::PeerCloseMessage => {
                peer.as_mut()
                    .unwrap()
                    .write_all(&encode_frame(
                        3,
                        FLAG_SIGNAL,
                        &[MsgType::Disconnect.as_byte()],
                    ))
                    .await
                    .unwrap();
            }
            ConnectionEndReason::PeerEof => drop(peer.take()),
            ConnectionEndReason::LocalClose => {
                // The same native abort used by a cancelled partial write
                // races the parked reader; it must win before waking it.
                abort.abort();
            }
            _ => unreachable!(),
        }
        wait_until("connection deactivated", || conn.is_deactivated()).await;
        assert!(
            !handler.is_finished(),
            "active work keeps the connection handler alive"
        );
        assert_eq!(server.active_connection_count(), 1);
        drop(peer);
        wait_until("deactivated connection cancels only queued work", || {
            conn.inflight_count() == 1
                && server.pending_requests.available_permits() == available - 1
        })
        .await;
        assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
        assert_eq!(schedulers[1].snapshot().pending, 0);
        assert_eq!(schedulers[1].snapshot().active_workers, 0);
        assert!(!handler.is_finished());
        // The active callback owns a local snapshot; with B=L=0 its
        // physical stream can settle before that callback returns.
        wait_until(
            "physical stream settles independently of active callback",
            || observation.snapshot().is_some(),
        )
        .await;
        assert_eq!(observation.snapshot().unwrap().reason(), reason);

        {
            let (lock, cvar) = &*release_route_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap();
        let closed = observation
            .wait_closed(Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(closed.reason(), reason);
        wait_until("drain returns the original pending permits", || {
            server.pending_requests.available_permits() == available
        })
        .await;
        assert_eq!(conn.inflight_count(), 0);
        assert_eq!(server.active_connection_count(), 0);
        assert_eq!(route_a_started.load(Ordering::SeqCst), 1);
        assert_eq!(route_b_started.load(Ordering::SeqCst), 0);
        for scheduler in schedulers {
            assert_eq!(scheduler.snapshot().pending, 0);
            assert_eq!(scheduler.snapshot().active_workers, 0);
        }
        assert_eq!(observation.snapshot(), Some(closed));
        drop(server);
        assert!(
            matches!(events.recv().await.unwrap(), Some(c2_local::ConnectionEvent::Closed(record)) if record == closed)
        );
        assert!(
            events.recv().await.unwrap().is_none(),
            "all competing terminal paths share one Closed"
        );
    }
}

#[tokio::test]
async fn shutdown_signal_interrupts_partial_frame_body_reads() {
    use tokio::time::timeout;

    let address = unique_readiness_address("shutdown_partial_frame");
    let server = Arc::new(test_server_new(&address, ServerIpcConfig::default()).unwrap());
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .expect("server ready");

    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .expect("connect client");
    handshake_test_client(&mut client).await;
    client
        .write_all(&12u32.to_le_bytes())
        .await
        .expect("write partial frame header");

    let conn_id = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(conn_id) = server.active_connection_ids().into_iter().next() {
                return conn_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection should register");
    assert!(
        server.active_connection(conn_id).is_some(),
        "tracked connection should remain visible while body read is pending",
    );

    server.request_shutdown_signal();
    server
        .wait_until_stopped(Duration::from_secs(1))
        .await
        .expect("shutdown should interrupt partial body reads");
    runner.await.unwrap().unwrap();
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
}

#[tokio::test]
async fn inline_dispatch_rejects_when_server_pending_capacity_is_exhausted() {
    use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    struct BlockingCallback {
        started: Arc<AtomicUsize>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl CrmCallback for BlockingCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _request: Vec<u8>,
            _response_pool: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.started.fetch_add(1, Ordering::SeqCst);

            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }

            Ok(ResponseMeta::Inline(b"done".to_vec()))
        }
    }

    let config = ServerIpcConfig {
        max_pending_requests: 1,
        max_execution_workers: 2,
        ..ServerIpcConfig::default()
    };
    let server = Arc::new(test_server_new("ipc://server_pending_limit", config).unwrap());

    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    for name in ["grid_a", "grid_b"] {
        let mut route = make_host_route(&server, name);
        route.callback = Arc::new(BlockingCallback {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        server.register_route(route).await.unwrap();
    }

    let payload_a = encode_call_control(&call_identity("grid_a"), 0).unwrap();
    let payload_b = encode_call_control(&call_identity("grid_b"), 0).unwrap();

    let first = {
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            call_over_connection(&server, 1, &payload_a).await;
        })
    };

    timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first callback should start");

    let (reply_tx, reply_rx) = oneshot::channel();
    let second = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let (_, _, reply_payload) = call_over_connection(&server, 2, &payload_b).await;
            let message = match decode_reply_control(&reply_payload, 0).unwrap().0 {
                ReplyControl::Error(err) => String::from_utf8_lossy(&err).to_string(),
                other => panic!("expected pending-capacity error reply, got {other:?}"),
            };
            let _ = reply_tx.send(message);
        })
    };

    let reply = match timeout(Duration::from_millis(200), reply_rx).await {
        Ok(Ok(message)) => message,
        Ok(Err(_)) => panic!("second reply channel dropped unexpectedly"),
        Err(_) => {
            {
                let (lock, cvar) = &*release;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }
            first.await.unwrap();
            second.await.unwrap();
            panic!("second call waited instead of rejecting at server pending admission");
        }
    };

    assert!(reply.contains("max_pending_requests=1"), "{reply}");
    assert_eq!(started.load(Ordering::SeqCst), 1);

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_shutdown_signal_does_not_stop_server() {
    let server = test_server_new(
        "ipc://malformed_shutdown_signal",
        ServerIpcConfig::default(),
    )
    .unwrap();
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    let writer = closed_writer().await;
    let malformed = [MsgType::ShutdownClient.as_byte(), 0x01, 0x02];

    handle_shutdown_signal(&server, &malformed, 99, &writer).await;

    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Ready);
    assert!(!*server.shutdown_tx.borrow());
}

#[tokio::test]
async fn unpolled_connection_events_do_not_retain_host_or_connection() {
    let source = ConnectionEventSource::new();
    let events = source.subscribe();
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("observation_unpolled"),
            ServerIpcConfig::default(),
        )
        .unwrap()
        .with_connection_event_source(source),
    );
    assert_eq!(server.lifecycle_state(), ServerLifecycleState::Initialized);
    let weak_server = Arc::downgrade(&server);
    let (_peer, stream) = isolated_pair().await;
    let handler = handle_connection(Arc::clone(&server), stream);
    // Construction admits the connection and publishes Opened even when
    // the future is never polled or spawned.
    assert_eq!(server.active_connection_count(), 1);
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened before polling"),
    };
    let weak_connection = Arc::downgrade(
        &server
            .active_connection(observation.connection_id())
            .unwrap(),
    );
    assert!(observation.snapshot().is_none());
    drop(handler);
    assert_eq!(server.active_connection_count(), 0);
    let closed = observation.snapshot().expect("unpolled registration fence");
    assert_eq!(closed.reason(), ConnectionEndReason::LocalClose);
    assert!(matches!(
        events.recv().await.unwrap(),
        Some(c2_local::ConnectionEvent::Closed(record)) if record == closed
    ));
    drop(server);
    assert!(weak_server.upgrade().is_none());
    assert!(weak_connection.upgrade().is_none());
    assert!(events.recv().await.unwrap().is_none());
    assert_eq!(observation.wait_closed(None).await, Some(closed));
}

#[tokio::test]
async fn idle_connection_events_observe_peer_eof_without_requests() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("observation_idle"),
            ServerIpcConfig {
                ..ServerIpcConfig::default()
            },
        )
        .unwrap(),
    );
    let events = server.connection_events();
    let (peer, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened"),
    };
    assert_eq!(server.active_connection_count(), 1);
    drop(peer);
    let closed = observation
        .wait_closed(Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::PeerEof);
    assert!(matches!(
        events.recv().await.unwrap(),
        Some(c2_local::ConnectionEvent::Closed(record)) if record == closed
    ));
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.active_connection_count(), 0);
    assert_eq!(observation.snapshot(), Some(closed));
}

#[tokio::test]
async fn connection_accepted_after_shutdown_reads_duplicate_shutdown_signal() {
    use c2_wire::shutdown_control::{decode_shutdown_ack, encode_shutdown_initiate};

    let server = Arc::new(
        test_server_new(
            "ipc://duplicate_shutdown_after_signal",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    server.request_shutdown_signal();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            handle_connection(server, server_stream).await;
        })
    };

    let request = encode_frame(7, FLAG_SIGNAL, &encode_shutdown_initiate());
    client.write_all(&request).await.expect("write request");
    let mut header = [0u8; frame::HEADER_SIZE];
    client
        .read_exact(&mut header)
        .await
        .expect("read ack header");
    let (total_len, body) = frame::decode_total_len(&header).unwrap();
    let (frame_header, payload_prefix) = decode_frame_body(body, total_len).unwrap();
    let payload_len = frame_header.payload_len();
    let mut payload = payload_prefix.to_vec();
    if payload.len() < payload_len {
        let mut tail = vec![0u8; payload_len - payload.len()];
        client.read_exact(&mut tail).await.expect("read ack body");
        payload.extend_from_slice(&tail);
    }
    decode_shutdown_ack(&payload).expect("shutdown ack");

    assert_eq!(frame_header.request_id, 7);
    assert!(frame_header.flags & FLAG_SIGNAL != 0);
    assert!(frame_header.flags & FLAG_RESPONSE != 0);
    handler.await.expect("handler completes");
}

#[tokio::test]
async fn connection_accepted_after_shutdown_idle_peer_does_not_block_handler() {
    use tokio::time::{Duration, timeout};

    let server = Arc::new(
        test_server_new(
            "ipc://duplicate_shutdown_idle_peer",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    server.request_shutdown_signal();

    let (_client, server_stream) = isolated_pair().await;

    timeout(
        Duration::from_millis(250),
        handle_connection(server, server_stream),
    )
    .await
    .expect("post-shutdown idle peer must not park the handler");
}

#[tokio::test]
async fn connection_accepted_after_shutdown_partial_shutdown_frame_does_not_wait_for_body() {
    use tokio::time::{Duration, timeout};

    let server = Arc::new(
        test_server_new(
            "ipc://duplicate_shutdown_partial_frame",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    server.request_shutdown_signal();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = tokio::spawn(async move {
        handle_connection(server, server_stream).await;
    });

    client
        .write_all(&SHUTDOWN_INITIATE_FRAME_BODY_LEN.to_le_bytes())
        .await
        .expect("write shutdown initiate frame length only");

    timeout(Duration::from_millis(250), handler)
        .await
        .expect("post-shutdown partial shutdown frame must not wait for body")
        .expect("handler completes");
}

#[tokio::test]
async fn connection_accepted_after_shutdown_rejects_non_shutdown_frame_without_body_wait() {
    use tokio::time::{Duration, timeout};

    let server = Arc::new(
        test_server_new(
            "ipc://duplicate_shutdown_rejects_non_shutdown",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    server.set_lifecycle_state(ServerLifecycleState::Ready);
    server.request_shutdown_signal();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            handle_connection(server, server_stream).await;
        })
    };

    let non_shutdown_body_len = SHUTDOWN_INITIATE_FRAME_BODY_LEN + 1;
    client
        .write_all(&non_shutdown_body_len.to_le_bytes())
        .await
        .expect("write oversized post-shutdown frame prefix");

    timeout(Duration::from_millis(100), handler)
        .await
        .expect("post-shutdown non-shutdown frame must close without waiting for body")
        .expect("handler completes");
}

#[test]
fn chunk_processing_permit_is_bounded_by_max_total_chunks() {
    let config = ServerIpcConfig {
        base: c2_config::BaseIpcConfig {
            max_total_chunks: 1,
            ..c2_config::BaseIpcConfig::default()
        },
        ..ServerIpcConfig::default()
    };
    let server = test_server_new("ipc://chunk_processing_limit", config).unwrap();

    let first = server.try_acquire_chunk_processing_permit().unwrap();
    assert_eq!(server.try_acquire_chunk_processing_permit().unwrap_err(), 1);
    drop(first);
    assert!(server.try_acquire_chunk_processing_permit().is_ok());
}

// -- shutdown --

#[tokio::test]
async fn shutdown_sets_signal() {
    let s = test_server_new("ipc://shut_test", ServerIpcConfig::default()).unwrap();
    let mut rx = s.shutdown_tx.subscribe();
    s.request_shutdown_signal();
    rx.changed().await.unwrap();
    assert!(*rx.borrow());
}

// -- error display --

#[test]
fn server_error_display() {
    let e = ServerError::Config("bad".into());
    assert!(format!("{e}").contains("bad"));

    let e2 = ServerError::Protocol("oops".into());
    assert!(format!("{e2}").contains("oops"));
}

// -- buddy payload decode + call control --

#[test]
fn decode_buddy_then_call_control() {
    use c2_wire::buddy::{BUDDY_PAYLOAD_SIZE, BuddyPayload, encode_buddy_payload};
    use c2_wire::control::encode_call_control;

    let bp = BuddyPayload {
        seg_idx: 0,
        generation: 1,
        offset: 4096,
        data_size: 256,
        is_dedicated: false,
        loan_id: 1,
    };
    let bp_bytes = encode_buddy_payload(&bp);
    let ctrl_bytes = encode_call_control(&call_identity("grid"), 1).unwrap();

    let mut payload = Vec::new();
    payload.extend_from_slice(&bp_bytes);
    payload.extend_from_slice(&ctrl_bytes);

    // Decode buddy part.
    let (decoded_bp, bp_consumed) = decode_buddy_payload(&payload).unwrap();
    assert_eq!(decoded_bp, bp);
    assert_eq!(bp_consumed, BUDDY_PAYLOAD_SIZE);

    // Decode call control after buddy header.
    let (ctrl, _) = decode_call_control(&payload, BUDDY_PAYLOAD_SIZE).unwrap();
    assert_eq!(ctrl.identity.route_name, "grid");
    assert_eq!(ctrl.method_idx, 1);
}

#[tokio::test]
async fn first_chunked_chunk_holds_route_pending_until_feed_error_aborts() {
    use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
    use c2_wire::chunk::header::encode_chunk_header;
    use c2_wire::control::encode_call_control;
    use std::num::NonZeroUsize;

    let server = Arc::new(
        test_server_new(
            "ipc://chunk_route_pending_abort",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits {
            max_pending: Some(NonZeroUsize::new(1).unwrap()),
            max_workers: Some(NonZeroUsize::new(1).unwrap()),
        },
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    server.register_route(route).await.unwrap();

    let conn = Arc::new(Connection::new(77));
    let writer = closed_writer().await;
    let mut first_payload = Vec::new();
    first_payload.extend_from_slice(&encode_chunk_header(0, 2));
    first_payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    first_payload.extend_from_slice(b"abcd");

    let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
    dispatch_chunked_call(
        &server,
        &conn,
        42,
        FLAG_CHUNKED,
        &first_payload,
        &writer,
        chunk_permit,
        chunk_frame_ordering(&server, conn.conn_id(), 42, FLAG_CHUNKED, &first_payload).unwrap(),
        None,
    )
    .await;

    assert!(server.chunk_registry.contains(conn.conn_id(), 42));
    assert!(matches!(
        scheduler.try_acquire(0),
        Err(SchedulerAcquireError::Capacity {
            field: "max_pending",
            limit: 1,
        })
    ));

    let mut bad_second_payload = Vec::new();
    bad_second_payload.extend_from_slice(&encode_chunk_header(7, 2));
    bad_second_payload.extend_from_slice(b"zzzz");
    let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
    dispatch_chunked_call(
        &server,
        &conn,
        42,
        FLAG_CHUNKED,
        &bad_second_payload,
        &writer,
        chunk_permit,
        chunk_frame_ordering(
            &server,
            conn.conn_id(),
            42,
            FLAG_CHUNKED,
            &bad_second_payload,
        )
        .unwrap(),
        None,
    )
    .await;

    assert!(!server.chunk_registry.contains(conn.conn_id(), 42));
    let guard = scheduler
        .try_acquire(0)
        .expect("feed error abort should release route pending capacity");
    drop(guard);
}

#[tokio::test]
async fn chunked_connection_cleanup_releases_route_pending_capacity() {
    use crate::scheduler::{SchedulerAcquireError, SchedulerLimits};
    use c2_wire::chunk::header::encode_chunk_header;
    use c2_wire::control::encode_call_control;
    use std::num::NonZeroUsize;

    let server = Arc::new(
        test_server_new(
            "ipc://chunk_route_pending_cleanup",
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits {
            max_pending: Some(NonZeroUsize::new(1).unwrap()),
            max_workers: Some(NonZeroUsize::new(1).unwrap()),
        },
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    server.register_route(route).await.unwrap();

    let conn = Arc::new(Connection::new(88));
    let writer = closed_writer().await;
    let mut payload = Vec::new();
    payload.extend_from_slice(&encode_chunk_header(0, 2));
    payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    payload.extend_from_slice(b"abcd");

    let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
    dispatch_chunked_call(
        &server,
        &conn,
        9,
        FLAG_CHUNKED,
        &payload,
        &writer,
        chunk_permit,
        chunk_frame_ordering(&server, conn.conn_id(), 9, FLAG_CHUNKED, &payload).unwrap(),
        None,
    )
    .await;

    assert!(server.chunk_registry.contains(conn.conn_id(), 9));
    assert!(matches!(
        scheduler.try_acquire(0),
        Err(SchedulerAcquireError::Capacity {
            field: "max_pending",
            limit: 1,
        })
    ));

    server.cleanup_chunk_requests_for_connection(conn.conn_id());

    assert!(!server.chunk_registry.contains(conn.conn_id(), 9));
    let guard = scheduler
        .try_acquire(0)
        .expect("connection cleanup should release route pending capacity");
    drop(guard);
}

struct EchoRequestBytes {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl CrmCallback for EchoRequestBytes {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        request: Vec<u8>,
        _response_pool: Arc<crate::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResponseMeta::Inline(request))
    }
}

/// Read one complete reply frame from the client side of a local pair.
async fn read_reply_frame(client: &mut LocalStream) -> (u64, u32, Vec<u8>) {
    let mut len_buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut len_buf))
        .await
        .expect("reply length within timeout")
        .expect("reply length");
    let total_len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; total_len];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut body))
        .await
        .expect("reply body within timeout")
        .expect("reply body");
    let mut frame_bytes = Vec::with_capacity(4 + total_len);
    frame_bytes.extend_from_slice(&len_buf);
    frame_bytes.extend_from_slice(&body);
    let (header, payload) = c2_wire::frame::decode_frame(&frame_bytes).expect("decode reply");
    (header.request_id, header.flags, payload.to_vec())
}

async fn negotiate_test_version(client: &mut LocalStream) {
    client
        .write_all(&encode_version_preamble(VersionPreambleKind::Hello))
        .await
        .expect("write version Hello");
    let mut accepted = [0u8; 25];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut accepted))
        .await
        .expect("version acceptance within timeout")
        .expect("read version Accepted");
    assert_eq!(
        accepted,
        encode_version_preamble(VersionPreambleKind::Accepted)
    );
}

/// Ordinary raw business fixtures use the same complete two-stage handshake.
async fn handshake_test_client(client: &mut LocalStream) {
    let ack = handshake_test_client_with(client, 0, "").await;
    assert!(ack.server_identity.is_some());
}

async fn handshake_test_client_with(
    client: &mut LocalStream,
    capability_flags: u16,
    prefix: &str,
) -> c2_wire::handshake::Handshake {
    negotiate_test_version(client).await;
    let metadata = c2_wire::handshake::encode_client_handshake(capability_flags, prefix).unwrap();
    client
        .write_all(&encode_frame(0, FLAG_HANDSHAKE, &metadata))
        .await
        .expect("write client metadata");
    let (rid, flags, payload) = read_reply_frame(client).await;
    assert_eq!(rid, 0);
    assert_eq!(flags, FLAG_HANDSHAKE | FLAG_RESPONSE);
    let handshake = decode_handshake(&payload, true).unwrap();
    // The client can read ACK bytes before the server resumes from its write
    // receipt. A following PONG proves the server's serial reader has also
    // committed handshake completion before fixtures access its private book.
    client
        .write_all(&encode_frame(0, FLAG_SIGNAL, &[MsgType::Ping.as_byte()]))
        .await
        .unwrap();
    let (rid, flags, payload) = read_reply_frame(client).await;
    assert_eq!(rid, 0);
    assert_eq!(flags, FLAG_SIGNAL | FLAG_RESPONSE);
    assert_eq!(payload, PONG_BYTES);
    handshake
}

#[tokio::test]
async fn shutdown_settles_connections_stalled_in_valid_handshake_header_or_body() {
    for split in [8usize, 17] {
        let server = Arc::new(
            test_server_new(
                &unique_readiness_address("partial_handshake_shutdown"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let (mut client, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        negotiate_test_version(&mut client).await;
        let metadata = c2_wire::handshake::encode_client_handshake(0, "").unwrap();
        let frame = encode_frame(0, FLAG_HANDSHAKE, &metadata);
        assert!(split < frame.len());
        client.write_all(&frame[..split]).await.unwrap();
        server.request_shutdown_signal();
        tokio::time::timeout(Duration::from_secs(2), server.wait_connections_settled())
            .await
            .expect("partial handshake connection settles after shutdown");
        tokio::time::timeout(Duration::from_secs(2), handler)
            .await
            .expect("connection handler completes")
            .unwrap();
        assert_eq!(server.active_connection_count(), 0);
    }
}

#[tokio::test]
async fn established_raw_disconnect_receives_ack() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("raw_disconnect_ack"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    client
        .write_all(&encode_frame(
            3,
            FLAG_SIGNAL,
            &[MsgType::Disconnect.as_byte()],
        ))
        .await
        .unwrap();
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 3);
    assert_eq!(flags, FLAG_SIGNAL | FLAG_RESPONSE);
    assert_eq!(payload, DISCONNECT_ACK_BYTES);
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .expect("disconnect handler completes")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_disconnect_before_eof_preserves_peer_close_message_as_first_cause() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("disconnect_first_cause"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    for _ in 0..16 {
        let events = server.connection_events();
        let (mut client, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        handshake_test_client(&mut client).await;
        let observation = match tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            c2_local::ConnectionEvent::Opened(observation) => observation,
            event => panic!("expected accepted connection: {event:?}"),
        };
        tokio::io::AsyncWriteExt::write_all(
            &mut client,
            &encode_frame(0, FLAG_SIGNAL, &c2_wire::msg_type::DISCONNECT_BYTES),
        )
        .await
        .unwrap();
        // The ordinary closer does not consume the ACK before dropping its OS stream.
        drop(client);
        let closed = observation
            .wait_closed(Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::PeerCloseMessage);
        assert_eq!(closed.operation(), "disconnect");
        tokio::time::timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn established_raw_shutdown_receives_single_byte_ack() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("raw_shutdown_ack"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    client
        .write_all(&encode_frame(
            3,
            FLAG_SIGNAL,
            &c2_wire::shutdown_control::encode_shutdown_initiate(),
        ))
        .await
        .unwrap();
    let (id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(id, 3);
    assert_eq!(flags, FLAG_SIGNAL | FLAG_RESPONSE);
    assert_eq!(payload, c2_wire::shutdown_control::encode_shutdown_ack());
    assert!(*server.shutdown_tx.borrow());
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn deactivated_readable_connection_still_answers_manual_ping() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("deactivated_ping"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    let conn = server
        .active_connection(server.active_connection_ids()[0])
        .unwrap();
    // An outstanding remote read keeps this physical connection readable.
    let remote = conn
        .core
        .admit_remote_loan(17, Box::new(conn.remote_read_guard()))
        .unwrap();
    conn.end(ConnectionEndReason::LocalClose, "test_deactivation");
    client
        .write_all(&encode_frame(9, FLAG_SIGNAL, &[MsgType::Ping.as_byte()]))
        .await
        .unwrap();
    let (id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(id, 9);
    assert_eq!(flags, FLAG_SIGNAL | FLAG_RESPONSE);
    assert_eq!(payload, PONG_BYTES);
    remote.finish();
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn first_server_end_is_published_after_last_remote_read_on_task_drop() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("first_end_remote_read"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let events = server.connection_events();
    let (mut peer, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut peer).await;
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened"),
    };
    let conn = server
        .active_connection(observation.connection_id())
        .unwrap();
    let remote = conn
        .core
        .admit_remote_loan(71, Box::new(conn.remote_read_guard()))
        .unwrap();
    conn.end(ConnectionEndReason::ProtocolError, "first_end");
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    assert!(observation.snapshot().is_none());
    let mut byte = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(25), peer.read(&mut byte))
            .await
            .is_err(),
        "the remote read guard retains the accepted OS stream"
    );
    remote.finish();
    let closed = observation
        .wait_closed(Some(Duration::from_secs(2)))
        .await
        .expect("last remote read retires the stream");
    assert_eq!(closed.reason(), ConnectionEndReason::ProtocolError);
    assert_eq!(closed.operation(), "first_end");
    assert!(matches!(events.recv().await.unwrap(),
        Some(c2_local::ConnectionEvent::Closed(record)) if record == closed));
}

#[tokio::test]
async fn unrecoverable_read_waits_for_remote_reader_before_publishing_close() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("unrecoverable_read_remote_guard"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let events = server.connection_events();
    let (mut peer, stream) = isolated_pair().await;
    let abort = stream.abort_handle();
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut peer).await;
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened"),
    };
    let conn = server
        .active_connection(observation.connection_id())
        .unwrap();
    let remote = conn
        .core
        .admit_remote_loan(72, Box::new(conn.remote_read_guard()))
        .unwrap();
    conn.end(ConnectionEndReason::ProtocolError, "first_end_before_abort");
    abort.abort();
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .expect("unrecoverable read completes its handler")
        .unwrap();
    assert!(observation.snapshot().is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "Closed must wait for the last remote reader"
    );
    remote.finish();
    let closed = observation
        .wait_closed(Some(Duration::from_secs(2)))
        .await
        .expect("last remote reader publishes Closed");
    assert_eq!(closed.reason(), ConnectionEndReason::ProtocolError);
    assert_eq!(closed.operation(), "first_end_before_abort");
    assert!(matches!(events.recv().await.unwrap(),
        Some(c2_local::ConnectionEvent::Closed(record)) if record == closed));
}

#[tokio::test]
async fn unrecoverable_server_read_closes_with_published_loan_in_original_pool() {
    use c2_wire::connection::WriteRequest;

    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("read_error_published_loan"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let events = server.connection_events();
    let (mut peer, stream) = isolated_pair().await;
    let abort = stream.abort_handle();
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut peer).await;
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened"),
    };
    let conn = server
        .active_connection(observation.connection_id())
        .unwrap();

    let directory = tempfile::tempdir().unwrap();
    let pool = Arc::new(parking_lot::RwLock::new(
        MemPool::new_with_prefix(
            PoolConfig {
                max_segments: 0,
                max_dedicated_segments: 0,
                min_retained_segments: 0,
                buddy_enabled: false,
                spill_dir: directory.path().to_path_buf(),
                ..PoolConfig::default()
            },
            unique_response_pool_prefix("unread_loan"),
        )
        .unwrap(),
    ));
    let handle = pool.write().alloc_handle(4096).unwrap();
    assert!(handle.is_file_spill());
    // PONG delivery can precede its writer's final flush receipt. This fixture
    // temporarily owns the book's writer directly, so await the real drain.
    conn.wait_idle().await;
    let loan = conn.core.prepare_loan(Arc::clone(&pool), handle).unwrap();
    let mut batch = conn
        .core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    batch.begin_io().unwrap();
    batch.frame_completed();
    batch.flush_completed();
    assert_eq!(
        pool.read().budget().unwrap().snapshot().file.used_bytes,
        4096
    );

    abort.abort();
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .expect("unrecoverable read must not wait for an unknowable release")
        .unwrap();
    let closed = observation.snapshot().expect("physical stream retired");
    assert_eq!(closed.reason(), ConnectionEndReason::LocalClose);
    assert_eq!(closed.operation(), "local_abort");
    assert!(
        !conn.core.try_close(),
        "the published loan is still recorded"
    );
    assert_eq!(
        pool.read().budget().unwrap().snapshot().file.used_bytes,
        4096
    );
    drop(conn);
    drop(server);
}

async fn read_release_id(client: &mut LocalStream) -> u64 {
    let (request_id, flags, payload) = read_reply_frame(client).await;
    assert_eq!(request_id, 0);
    assert_eq!(flags, FLAG_CTRL | FLAG_RESPONSE);
    let frame = encode_frame(request_id, flags, &payload);
    let (header, body) = c2_wire::frame::decode_frame(&frame).unwrap();
    let batch = c2_wire::release::decode_release_frame(&header, body, true, true).unwrap();
    assert_eq!(batch.ids().len(), 1);
    batch.ids()[0]
}

#[tokio::test]
async fn raw_buddy_request_releases_before_blocked_callback_completes() {
    use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
    use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct BlockingEcho {
        started: Arc<AtomicBool>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }
    impl CrmCallback for BlockingEcho {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            request: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            assert_eq!(request, b"copied before callback");
            self.started.store(true, Ordering::SeqCst);
            let (lock, changed) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(ResponseMeta::Inline(b"callback done".to_vec()))
        }
    }

    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("raw_request_release"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(BlockingEcho {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    server.register_route(route).await.unwrap();
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();

    let mut pool = MemPool::new_with_prefix(
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 1,
            ..PoolConfig::default()
        },
        unique_response_pool_prefix("rawreq"),
    )
    .unwrap();
    let input = b"copied before callback";
    let mut handle = pool.alloc_shm_handle(input.len()).unwrap();
    pool.handle_slice_mut(&mut handle).copy_from_slice(input);
    let allocation = handle.shm_allocation().unwrap();
    let loan_id = 71;
    let descriptor = BuddyPayload {
        seg_idx: allocation.seg_idx as u16,
        generation: allocation.generation,
        offset: allocation.offset,
        data_size: input.len() as u32,
        is_dedicated: allocation.is_dedicated,
        loan_id,
    };
    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    let ack = handshake_test_client_with(&mut client, CAP_SHM_REQUEST, pool.prefix()).await;
    assert_ne!(ack.capability_flags & CAP_SHM_REQUEST, 0);
    let mut body = encode_buddy_payload(&descriptor).to_vec();
    body.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    client
        .write_all(&encode_frame(
            41,
            c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY,
            &body,
        ))
        .await
        .unwrap();

    assert_eq!(read_release_id(&mut client).await, loan_id);
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(pool.stats().alloc_count, 1);
    pool.release_handle(handle);
    assert_eq!(pool.stats().alloc_count, 0);
    {
        let (lock, changed) = &*release;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 41);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    let (control, consumed) = decode_reply_control(&payload, 0).unwrap();
    assert!(matches!(control, ReplyControl::Success));
    assert_eq!(&payload[consumed..], b"callback done");
    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn raw_buddy_mapping_failure_still_queues_release() {
    use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
    use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("raw_request_bad_span"),
            ServerIpcConfig::default(),
        )
        .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();
    let runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();

    let mut pool = MemPool::new_with_prefix(
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            ..PoolConfig::default()
        },
        unique_response_pool_prefix("badspan"),
    )
    .unwrap();
    let handle = pool.alloc_shm_handle(128).unwrap();
    let allocation = handle.shm_allocation().unwrap();
    let loan_id = 72;
    let descriptor = BuddyPayload {
        seg_idx: allocation.seg_idx as u16,
        generation: allocation.generation,
        offset: u32::MAX,
        data_size: 128,
        is_dedicated: false,
        loan_id,
    };
    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    let ack = handshake_test_client_with(&mut client, CAP_SHM_REQUEST, pool.prefix()).await;
    assert_ne!(ack.capability_flags & CAP_SHM_REQUEST, 0);
    let mut body = encode_buddy_payload(&descriptor).to_vec();
    body.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    client
        .write_all(&encode_frame(
            42,
            c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY,
            &body,
        ))
        .await
        .unwrap();

    assert_eq!(read_release_id(&mut client).await, loan_id);
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 42);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    let (control, _) = decode_reply_control(&payload, 0).unwrap();
    assert!(matches!(control, ReplyControl::Error(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    pool.release_handle(handle);
    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

async fn raw_response_loan_settlement(release_frame: bool, shutdown_before_release: bool) {
    use c2_wire::buddy::decode_buddy_payload;
    use c2_wire::control::{ReplyControl, decode_reply_control, encode_call_control};
    use c2_wire::release::{ReleaseBatch, encode_release_frame};
    use std::sync::atomic::AtomicUsize;

    let mut config = ServerIpcConfig::default();
    config.shm_threshold = 1;
    let server =
        Arc::new(test_server_new(&unique_readiness_address("raw_response_loan"), config).unwrap());
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    server.register_route(route).await.unwrap();
    let mut runner = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();

    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    let ack = handshake_test_client_with(&mut client, CAP_SHM_RESPONSE, "").await;
    assert_ne!(ack.capability_flags & CAP_SHM_RESPONSE, 0);
    let expected = b"response loan bytes".repeat(512);
    let mut body = encode_call_control(&call_identity("grid"), 0).unwrap();
    body.extend_from_slice(&expected);
    client
        .write_all(&encode_frame(91, c2_wire::flags::FLAG_CALL_V2, &body))
        .await
        .unwrap();
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 91);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    assert_ne!(flags & FLAG_BUDDY, 0);
    let (descriptor, consumed) = decode_buddy_payload(&payload).unwrap();
    let (control, _) = decode_reply_control(&payload, consumed).unwrap();
    assert!(matches!(control, ReplyControl::Success));

    let peer = c2_mem::PeerMappingCache::new(PoolConfig::default(), ack.prefix).unwrap();
    let read = peer
        .admit(
            descriptor.seg_idx,
            descriptor.generation,
            descriptor.offset,
            descriptor.data_size,
            descriptor.is_dedicated,
        )
        .unwrap();
    let copied = unsafe { read.copy() }.unwrap();
    drop(read);
    assert_eq!(copied, expected);
    assert_eq!(server.response_pool.read().stats().alloc_count, 1);

    if shutdown_before_release {
        server.request_shutdown_signal();
        server
            .wait_until_stopped(Duration::from_secs(2))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), &mut runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(server.response_pool.read().stats().alloc_count, 1);
        assert_eq!(server.active_connection_count(), 1);
    }

    let mut client = Some(client);
    if release_frame {
        let batch = ReleaseBatch::new(&[descriptor.loan_id]).unwrap();
        client
            .as_mut()
            .unwrap()
            .write_all(&encode_release_frame(&batch, false))
            .await
            .unwrap();
    } else {
        drop(client.take());
    }
    wait_until("response loan settled", || {
        server.response_pool.read().stats().alloc_count == 0
    })
    .await;
    drop(client);
    if !shutdown_before_release {
        server.request_shutdown_signal();
        runner.await.unwrap().unwrap();
    }
    server.wait_connections_settled().await;
}

#[tokio::test]
async fn raw_response_release_after_reply_settles_loan() {
    raw_response_loan_settlement(true, false).await;
}

#[tokio::test]
async fn raw_response_peer_end_reclaims_unreleased_loan() {
    raw_response_loan_settlement(false, false).await;
}

#[tokio::test]
async fn host_shutdown_finishes_before_late_response_release() {
    raw_response_loan_settlement(true, true).await;
}

#[tokio::test]
async fn response_write_failure_keeps_published_loan_until_peer_end() {
    let pool = small_response_pool("failed_write");
    let prepared = try_prepare_shm_response(&pool, 0, 8192, |dst| {
        dst.fill(0x5a);
        Ok(())
    })
    .unwrap()
    .unwrap();
    let (client, stream) = isolated_pair().await;
    client.abort_handle().abort();
    drop(client);
    let (_reader, half) = stream.into_split();
    let conn = Arc::new(Connection::new(93));
    conn.set_handshake_done(true).unwrap();
    let (writer, writer_task) = ConnectionWriter::spawn(
        half,
        Arc::clone(&conn),
        Duration::from_secs(30),
        Arc::new(Semaphore::new(1024)),
    );

    let result = send_response_meta(
        &pool,
        &writer,
        93,
        ResponseMeta::ShmAlloc(prepared),
        0,
        4096,
        128 * 1024,
        true,
    )
    .await;
    assert!(
        matches!(result, Err(ResponseSendError::Transport(_))),
        "{result:?}"
    );
    assert_eq!(pool.read().stats().alloc_count, 1);
    drop(conn.core.peer_end());
    assert_eq!(pool.read().stats().alloc_count, 0);
    drop(writer);
    writer_task.await.unwrap();
}

async fn call_over_connection(
    server: &Arc<Server>,
    request_id: u64,
    payload: &[u8],
) -> (u64, u32, Vec<u8>) {
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(server), stream));
    handshake_test_client(&mut client).await;
    client
        .write_all(&encode_frame(
            request_id,
            c2_wire::flags::FLAG_CALL_V2,
            payload,
        ))
        .await
        .unwrap();
    let reply = read_reply_frame(&mut client).await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .expect("call connection drains")
        .unwrap();
    reply
}

async fn assert_bad_call_isolated(flags: u32, payload: &[u8], deactivates: bool) {
    let config = ServerIpcConfig {
        max_execution_workers: 1,
        ..Default::default()
    };
    let server = Arc::new(test_server_new("ipc://malformed_call", config).unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: calls.clone(),
    });
    server.register_route(route).await.unwrap();
    let execution = server.execution_scheduler.try_acquire(0).unwrap();
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(server.clone(), stream));
    let ack = handshake_test_client_with(&mut client, CAP_SHM_REQUEST, "malformed-call-peer").await;
    assert_ne!(ack.capability_flags & CAP_SHM_REQUEST, 0);
    let conn = server
        .active_connection(server.active_connection_ids()[0])
        .unwrap();
    let mut good = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
    good.extend_from_slice(b"accepted");
    client
        .write_all(&encode_frame(41, c2_wire::flags::FLAG_CALL_V2, &good))
        .await
        .unwrap();
    wait_until("valid call waits for execution capacity", || {
        conn.inflight_count() == 1
    })
    .await;
    client
        .write_all(&encode_frame(42, flags, payload))
        .await
        .unwrap();
    let mut releases = Vec::new();
    let reply = loop {
        let (id, flags, body) = read_reply_frame(&mut client).await;
        if flags == FLAG_CTRL | FLAG_RESPONSE {
            let frame = encode_frame(id, flags, &body);
            let (header, payload) = c2_wire::frame::decode_frame(&frame).unwrap();
            let release =
                c2_wire::release::decode_release_frame(&header, payload, true, true).unwrap();
            releases.extend_from_slice(release.ids());
        } else {
            assert_eq!(id, 42, "only the malformed call fails");
            break body;
        }
    };
    assert_eq!(
        reply_error_code(&reply),
        ErrorCode::ResourceInputDeserializing
    );
    assert_eq!(conn.core.is_deactivated(), deactivates);
    assert_eq!(conn.inflight_count(), 1, "accepted call remains queued");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    if c2_wire::flags::is_buddy(flags) && !deactivates {
        assert_eq!(
            releases,
            vec![1],
            "failed local open still returns the known loan"
        );
    }
    // A new call after deactivation is explicitly rejected before execution.
    client
        .write_all(&encode_frame(43, c2_wire::flags::FLAG_CALL_V2, &good))
        .await
        .unwrap();
    if deactivates {
        let (id, _, reply) = read_reply_frame(&mut client).await;
        assert_eq!(id, 43);
        assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
        assert!(reply_error_message(&reply).contains("not executed"));
    }
    drop(execution);
    // Parallel calls can finish in either order after execution is released.
    // Match each original request exactly once without imposing reply order.
    let expected = if deactivates { vec![41] } else { vec![41, 43] };
    let mut replies = HashMap::new();
    for _ in &expected {
        let (id, _, reply) = read_reply_frame(&mut client).await;
        assert!(expected.contains(&id), "unexpected reply {id}");
        assert!(replies.insert(id, reply).is_none(), "duplicate reply {id}");
    }
    for id in expected {
        let reply = replies.remove(&id).unwrap();
        let (control, consumed) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
        assert!(matches!(control, ReplyControl::Success));
        assert_eq!(&reply[consumed..], b"accepted");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        if deactivates { 1 } else { 2 }
    );
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), handler)
        .await
        .unwrap()
        .unwrap();
}

fn reply_error_code(payload: &[u8]) -> ErrorCode {
    let ReplyControl::Error(error) = c2_wire::control::decode_reply_control(payload, 0)
        .unwrap()
        .0
    else {
        panic!("expected a correlated error reply");
    };
    C2Error::from_wire_bytes(&error).unwrap().unwrap().code
}

fn bad_call_buddy_payload(loan_id: u64) -> Vec<u8> {
    encode_buddy_payload(&BuddyPayload {
        seg_idx: 0,
        generation: 1,
        offset: 4096,
        data_size: 16,
        is_dedicated: false,
        loan_id,
    })
    .to_vec()
}

#[tokio::test]
async fn malformed_inline_call_control_only_fails_its_call() {
    assert_bad_call_isolated(c2_wire::flags::FLAG_CALL_V2, &[0xff], false).await;
}

#[tokio::test]
async fn malformed_buddy_call_control_only_fails_its_call() {
    let mut payload = bad_call_buddy_payload(1);
    payload.push(0xff);
    assert_bad_call_isolated(c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY, &payload, false).await;
}

#[tokio::test]
async fn malformed_buddy_payload_deactivates_but_preserves_accepted_call() {
    let mut payload = bad_call_buddy_payload(1);
    payload[14] = 0x80;
    payload.extend_from_slice(
        &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
    );
    assert_bad_call_isolated(c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY, &payload, true).await;
}

#[tokio::test]
async fn zero_buddy_loan_deactivates_but_preserves_accepted_call() {
    let mut payload = bad_call_buddy_payload(0);
    payload.extend_from_slice(
        &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
    );
    assert_bad_call_isolated(c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY, &payload, true).await;
}

#[tokio::test]
async fn buddy_mapping_open_failure_only_fails_its_call() {
    let mut payload = bad_call_buddy_payload(1);
    payload.extend_from_slice(
        &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
    );
    assert_bad_call_isolated(c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY, &payload, false).await;
}

async fn wait_until(label: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached within timeout: {label}");
}

fn chunked_call_frame(
    request_id: u64,
    chunk_idx: u16,
    total_chunks: u16,
    data: &[u8],
    identity: &str,
) -> Vec<u8> {
    use c2_wire::chunk::header::encode_chunk_header;
    use c2_wire::control::encode_call_control;

    let mut payload = Vec::new();
    payload.extend_from_slice(&encode_chunk_header(chunk_idx, total_chunks));
    if chunk_idx == 0 {
        payload.extend_from_slice(&encode_call_control(&call_identity(identity), 0).unwrap());
    }
    payload.extend_from_slice(data);
    let mut flags = c2_wire::flags::FLAG_CALL_V2 | FLAG_CHUNKED;
    if chunk_idx + 1 == total_chunks {
        flags |= c2_wire::flags::FLAG_CHUNK_LAST;
    }
    encode_frame(request_id, flags, &payload)
}

fn ordering_test_server(address: &str, max_total_chunks: u32) -> Arc<Server> {
    let mut config = ServerIpcConfig::default();
    config.base.max_total_chunks = max_total_chunks;
    Arc::new(test_server_new(address, config).unwrap())
}

#[tokio::test]
async fn forced_chunked_100_mib_request_succeeds_above_inflight_chunk_limit() {
    const PAYLOAD_SIZE: usize = 100 * 1024 * 1024;
    const CHUNK_SIZE: usize = 128 * 1024;
    const TOTAL_CHUNKS: u16 = (PAYLOAD_SIZE / CHUNK_SIZE) as u16;

    struct VerifyLargeRequest;
    impl CrmCallback for VerifyLargeRequest {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            request: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            let data = request;
            assert_eq!(data.len(), PAYLOAD_SIZE);
            assert!(data.iter().all(|byte| *byte == 0x5a));
            Ok(ResponseMeta::Inline(
                (data.len() as u64).to_le_bytes().to_vec(),
            ))
        }
    }

    let config = ServerIpcConfig {
        max_payload_size: PAYLOAD_SIZE as u64,
        ..Default::default()
    };
    assert!(u32::from(TOTAL_CHUNKS) > config.max_total_chunks);
    let server = Arc::new(test_server_new("ipc://chunked_100_mib", config).unwrap());
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(VerifyLargeRequest);
    server.register_route(route).await.unwrap();
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    let data = vec![0x5a; CHUNK_SIZE];
    for chunk in 0..TOTAL_CHUNKS {
        client
            .write_all(&chunked_call_frame(42, chunk, TOTAL_CHUNKS, &data, "grid"))
            .await
            .unwrap();
        // A PING fences frame dispatch; waiting for processing permits
        // keeps this sequential request below the global in-flight bound.
        if chunk % 32 == 0 && chunk + 1 < TOTAL_CHUNKS {
            client
                .write_all(&encode_frame(99, FLAG_SIGNAL, &[MsgType::Ping.as_byte()]))
                .await
                .unwrap();
            let (rid, _, payload) = read_reply_frame(&mut client).await;
            assert_eq!(
                rid,
                99,
                "unexpected reply: {}",
                String::from_utf8_lossy(&payload)
            );
            wait_until("chunk processing drain", || {
                server.chunk_processing_permits.available_permits()
                    == server.config.max_total_chunks as usize
            })
            .await;
        }
    }
    let (rid, _, reply) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 42);
    let (control, consumed) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
    assert!(matches!(control, ReplyControl::Success), "{control:?}");
    assert_eq!(&reply[consumed..], &(PAYLOAD_SIZE as u64).to_le_bytes());
    drop(client);
    handler.await.unwrap();
    assert_eq!(server.chunk_registry.active_count(), 0);
}

#[tokio::test]
async fn chunked_request_payload_limit_reports_size_before_callback() {
    let config = ServerIpcConfig {
        max_payload_size: 8,
        ..Default::default()
    };
    let server = Arc::new(test_server_new("ipc://chunk_payload_limit", config).unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    for (chunk, data) in [b"abcd".as_slice(), b"efgh", b"i"].into_iter().enumerate() {
        client
            .write_all(&chunked_call_frame(42, chunk as u16, 3, data, "grid"))
            .await
            .unwrap();
    }
    let (_, _, reply) = read_reply_frame(&mut client).await;
    let (control, _) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
    let ReplyControl::Error(error) = control else {
        panic!("expected payload size error")
    };
    let error = C2Error::from_wire_bytes(&error).unwrap().unwrap();
    drop(client);
    handler.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(error.code, ErrorCode::ResourceUnavailable);
    assert!(
        error
            .message
            .contains("request payload size 9 exceeds max_payload_size 8"),
        "{error}"
    );
}

/// Wire-order frame dispatch must not let later chunks run ahead of the
/// first chunk's route admission. Hold the route/dispatcher gate, push a
/// first chunk plus two later chunks through the real receive loop, prove
/// nothing was published or discarded while the gate is held, prove control
/// frames still work, then release the gate and prove every byte was
/// delivered exactly once with all ordering/registry/permit state cleaned.
#[tokio::test]
async fn later_chunks_wait_for_stalled_first_chunk_route_admission() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_ordering_hold", 3);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // Hold the route gate the first chunk must pass through.
    let dispatcher_guard = server.dispatcher.write().await;

    const TOTAL: usize = 3;
    const CHUNK: usize = 64;
    let data: Vec<u8> = (0..(2 * CHUNK + 7)).map(|i| (i % 251) as u8).collect();
    for idx in 0..TOTAL {
        let start = idx * CHUNK;
        let end = usize::min(start + CHUNK, data.len());
        client
            .write_all(&chunked_call_frame(
                77,
                idx as u16,
                TOTAL as u16,
                &data[start..end],
                "grid",
            ))
            .await
            .expect("write chunk frame");
    }

    // Everything is parked: first chunk on the held gate, later chunks on
    // first-chunk admission, all three chunk-processing permits held.
    wait_until("three chunk frames dispatched", || {
        server.chunk_admission_gate.len() == 1
            && server.chunk_processing_permits.available_permits() == 0
    })
    .await;
    let conn_id = server.active_connection_ids()[0];
    assert!(
        !server.chunk_registry.contains(conn_id, 77),
        "later chunks must not publish or discard an assembly before first-chunk admission"
    );
    assert_eq!(server.chunk_registry.active_count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Control liveness while the route gate is stalled: the receive loop
    // still answers a ping on the same connection.
    client
        .write_all(&encode_frame(
            5,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .expect("write ping");
    let (ping_rid, ping_flags, ping_payload) = read_reply_frame(&mut client).await;
    assert_eq!(ping_rid, 5);
    assert!(ping_flags & FLAG_SIGNAL != 0);
    assert_eq!(ping_payload, PONG_BYTES);

    // Release the gate: the first chunk admits, the later chunks feed, and
    // the request is executed once with the exact reassembled bytes.
    drop(dispatcher_guard);
    let (reply_rid, reply_flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 77);
    assert!(reply_flags & FLAG_RESPONSE != 0);
    let (control, consumed) =
        c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
    match control {
        ReplyControl::Success => {}
        other => panic!("expected successful reply, got {other:?}"),
    }
    assert_eq!(&reply_payload[consumed..], data.as_slice());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one execution");

    // No ordering entry, assembly, stored admission, or permit is left.
    assert_eq!(server.chunk_admission_gate.len(), 0);
    assert_eq!(server.chunk_registry.active_count(), 0);
    assert!(!server.chunk_registry.contains(conn_id, 77));
    assert_eq!(server.chunk_processing_permits.available_permits(), 3);
    assert!(scheduler.try_acquire(0).is_ok());

    drop(client);
    handler.await.expect("connection handler completes");
}

// ── Same-version metadata SHM direction negotiation ─────────────────────

/// The ACK's SHM bits are the intersection of the client's offer with the
/// connection's actual native shared-memory reachability, in either
/// direction independently. A connection whose stream was never observed
/// (native reachability false) never negotiates SHM.
#[tokio::test]
async fn handshake_ack_intersects_offer_with_native_reachability() {
    use c2_wire::handshake::{CAP_SHM_REQUEST, CAP_SHM_RESPONSE, decode_handshake};

    let base_caps = 0;
    for (native, offer, expect_request, expect_response) in [
        (false, CAP_SHM_REQUEST | CAP_SHM_RESPONSE, false, false),
        (true, CAP_SHM_REQUEST | CAP_SHM_RESPONSE, true, true),
        (true, CAP_SHM_REQUEST, true, false),
        (true, CAP_SHM_RESPONSE, false, true),
        (true, 0, false, false),
    ] {
        let server = ordering_test_server("ipc://shm_ack_intersection", 8);
        let (mut client, stream) = isolated_pair().await;
        let (_reader, write_half) = stream.into_split();
        let conn = Arc::new(Connection::new(41));
        conn.set_native_shm_reachable(native);
        let (writer, writer_task) = ConnectionWriter::spawn(
            write_half,
            Arc::clone(&conn),
            Duration::from_secs(30),
            Arc::new(Semaphore::new(1024)),
        );

        let handshake =
            c2_wire::handshake::encode_client_handshake(base_caps | offer, "shm-ack-prefix")
                .unwrap();
        handle_handshake(&server, &conn, &handshake, 0, &writer)
            .await
            .unwrap();
        let (_rid, flags, payload) = read_reply_frame(&mut client).await;
        assert_eq!(flags, FLAG_HANDSHAKE | FLAG_RESPONSE);
        let decoded = decode_handshake(&payload, true).unwrap();
        assert_eq!(
            decoded.capability_flags & CAP_SHM_REQUEST != 0,
            expect_request,
            "request direction (native={native}, offer={offer})"
        );
        assert_eq!(
            decoded.capability_flags & CAP_SHM_RESPONSE != 0,
            expect_response,
            "response direction (native={native}, offer={offer})"
        );
        assert_eq!(conn.shm_request_capable(), expect_request);
        assert_eq!(conn.shm_response_capable(), expect_response);
        assert_eq!(
            decoded.capability_flags & !(CAP_SHM_REQUEST | CAP_SHM_RESPONSE),
            0
        );
        assert!(conn.chunked_capable());
        conn.end(ConnectionEndReason::LocalClose, "test_done");
        drop((client, writer, server));
        writer_task.await.unwrap();
    }
}

async fn version_fence_connection() -> (Arc<Server>, LocalStream, LocalStream) {
    use c2_local::EndpointReapResult::{AlreadyAbsent, NotApplicable, Reaped};

    let address = unique_readiness_address("version_fence");
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp").unwrap();
    #[cfg(unix)]
    let endpoint = c2_config::LocalEndpointContext::with_unix_root(root.path())
        .unwrap()
        .endpoint(&address)
        .unwrap();
    #[cfg(windows)]
    let endpoint = test_parse_local_endpoint(&address).unwrap();
    let server = Server::new_with_endpoint(endpoint.clone(), Default::default()).unwrap();
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    let pair = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept()
    );
    let closed = tokio::task::spawn_blocking(move || listener.close())
        .await
        .unwrap();
    assert!(matches!(closed, Reaped | AlreadyAbsent | NotApplicable));
    let (client, stream) = pair.unwrap();
    (Arc::new(server), client, stream)
}

#[tokio::test]
async fn handshake_version_mismatch_returns_fixed_preamble() {
    let local = HANDSHAKE_VERSION;
    for (max_frame, peer) in [
        (u32::MAX, 1),
        (u32::MAX, local + 1),
        (u32::MAX, u32::MAX),
        (526, local + 1),
    ] {
        let (mut server, mut client, stream) = version_fence_connection().await;
        let config = &mut Arc::get_mut(&mut server).unwrap().config;
        config.max_frame_size = u64::from(max_frame);
        config.shm_threshold = config.shm_threshold.min(config.max_frame_size);
        config.validate().unwrap();
        let handler = tokio::spawn(handle_connection(server, stream));
        let mut hello = *b"\x15\0\0\0\0\0\0\0\0\0\0\0\x04\0\0\0C2IP\x01\0\0\0\0";
        hello[21..].copy_from_slice(&peer.to_le_bytes());
        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            client.write_all(&hello).await?;
            let mut reply = [0; 25];
            client.read_exact(&mut reply).await?;
            Ok::<_, std::io::Error>(reply)
        })
        .await;
        drop(client);
        handler.await.expect("connection handler completes");
        let mut expected = *b"\x15\0\0\0\0\0\0\0\0\0\0\0\x06\0\0\0C2IP\x03\0\0\0\0";
        expected[21..].copy_from_slice(&local.to_le_bytes());
        assert_eq!(reply.unwrap().expect("version refusal"), expected);
    }
}

#[tokio::test]
async fn invalid_preamble_headers_are_rejected_without_reading_payload() {
    for (max_frame, length, rid, flags) in [
        (u32::MAX, 20u32, 0u64, FLAG_HANDSHAKE),
        (u32::MAX, 21, 1, FLAG_HANDSHAKE),
        (u32::MAX, 21, 0, FLAG_HANDSHAKE | FLAG_RESPONSE),
        (u32::MAX, 21, 0, FLAG_HANDSHAKE | FLAG_CTRL),
        (u32::MAX, 21, 0, FLAG_RESPONSE),
        (u32::MAX, 21, 0, 0),
        (526, 527, 0, FLAG_SIGNAL),
    ] {
        let (mut server, mut client, stream) = version_fence_connection().await;
        let config = &mut Arc::get_mut(&mut server).unwrap().config;
        config.max_frame_size = u64::from(max_frame);
        config.shm_threshold = config.shm_threshold.min(config.max_frame_size);
        config.validate().unwrap();
        let handler = tokio::spawn(handle_connection(server, stream));
        let mut header = [0u8; 16];
        header[..4].copy_from_slice(&length.to_le_bytes());
        header[4..12].copy_from_slice(&rid.to_le_bytes());
        header[12..].copy_from_slice(&flags.to_le_bytes());
        client.write_all(&header).await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut [0])).await;
        drop(client);
        handler.await.unwrap();
        assert!(
            matches!(read, Ok(Ok(0) | Err(_))),
            "waited for payload: {read:?}"
        );
    }
}

#[tokio::test]
async fn metadata_and_established_connections_reject_invalid_handshakes() {
    let metadata = c2_wire::handshake::encode_client_handshake(0, "").unwrap();
    let mut wrong_rid = encode_frame(1, FLAG_HANDSHAKE, &metadata);
    wrong_rid.truncate(16);
    let mut wrong_flags = encode_frame(0, FLAG_HANDSHAKE | FLAG_RESPONSE, &metadata);
    wrong_flags.truncate(16);
    let hello = encode_version_preamble(VersionPreambleKind::Hello).to_vec();
    for (established, frame) in [
        (false, wrong_rid),
        (false, wrong_flags),
        (false, hello.clone()),
        (true, hello),
    ] {
        let (server, mut client, stream) = version_fence_connection().await;
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        if established {
            handshake_test_client(&mut client).await;
        } else {
            negotiate_test_version(&mut client).await;
            let conn_id = server.active_connection_ids()[0];
            let conn = server.active_connection(conn_id).unwrap();
            assert!(!conn.handshake_done() && !conn.ordinary_protocol());
        }
        client.write_all(&frame).await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut [0])).await;
        drop(client);
        handler.await.unwrap();
        assert!(
            matches!(read, Ok(Ok(0) | Err(_))),
            "accepted invalid handshake: {read:?}"
        );
        assert_eq!(server.active_connection_count(), 0);
    }
}

#[tokio::test]
async fn version_rejection_survives_delayed_read_and_shutdown_interrupts_peer_wait() {
    let (server, mut client, stream) = version_fence_connection().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    let mut hello = encode_version_preamble(VersionPreambleKind::Hello);
    hello[21..].copy_from_slice(&(HANDSHAKE_VERSION + 1).to_le_bytes());
    client.write_all(&hello).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !handler.is_finished(),
        "50 ms control ACK window must not close version refusal"
    );
    let mut reply = [0u8; 25];
    tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reply,
        encode_version_preamble(VersionPreambleKind::VersionMismatch)
    );
    assert!(
        !handler.is_finished(),
        "reply consumption does not close the peer"
    );
    server.request_shutdown_signal();
    tokio::time::timeout(Duration::from_secs(1), handler)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.active_connection_count(), 0);
}

#[tokio::test]
async fn version_rejection_peer_wait_expires_within_the_single_send_budget() {
    let (server, mut client, stream) = version_fence_connection().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    let started = tokio::time::Instant::now();
    let mut hello = encode_version_preamble(VersionPreambleKind::Hello);
    hello[21..].copy_from_slice(&(HANDSHAKE_VERSION + 1).to_le_bytes());
    client.write_all(&hello).await.unwrap();
    let mut reply = [0u8; 25];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        reply,
        encode_version_preamble(VersionPreambleKind::VersionMismatch)
    );
    tokio::time::timeout_at(
        started + DEFAULT_CONNECT_TIMEOUT + Duration::from_secs(1),
        handler,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() >= DEFAULT_CONNECT_TIMEOUT);
    assert_eq!(server.active_connection_count(), 0);
}

#[tokio::test]
async fn unhandshaken_call_and_route_control_are_rejected() {
    let frames = [
        inline_call_frame(1, "grid", b"unhandshaken"),
        encode_frame(2, FLAG_CTRL, b"\x0e{\"selector\":{\"type\":\"all\"}}"),
    ];
    let mut observations = Vec::new();
    for frame in frames {
        let (server, mut client, stream) = version_fence_connection().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut route = make_host_route(&server, "grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::clone(&calls),
        });
        server.register_route(route).await.unwrap();
        let handler = tokio::spawn(handle_connection(server, stream));
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            client.write_all(&frame).await?;
            client.read(&mut [0]).await
        })
        .await;
        drop(client);
        handler.await.expect("connection handler completes");
        observations.push((read, calls.load(Ordering::SeqCst)));
    }
    for (read, calls) in observations {
        assert_eq!(calls, 0, "unhandshaken CALL must not invoke the callback");
        // Rejecting before reading the body may close with an OS reset.
        assert!(
            matches!(read, Ok(Ok(0) | Err(_))),
            "unhandshaken RPC must close without a reply: {read:?}"
        );
    }
}

/// Retain rejection of the pre-negotiation, single-frame version 12 format.
#[tokio::test]
async fn handshake_version_twelve_is_rejected_without_fallback() {
    let (server, mut client, stream) = version_fence_connection().await;
    let handler = tokio::spawn(handle_connection(server, stream));
    let legacy = encode_frame(0, FLAG_HANDSHAKE, &[12, 0, 0, 0, 7, 0]);
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        client.write_all(&legacy).await?;
        client.read(&mut [0]).await
    })
    .await;
    drop(client);
    handler.await.expect("connection handler completes");
    assert!(
        matches!(read, Ok(Ok(0) | Err(_))),
        "server must close the v12 connection: {read:?}"
    );
}

#[tokio::test]
async fn indeterminate_reachability_terminates_connection_before_handshake() {
    let server = ordering_test_server("ipc://shm_reachability_error", 8);
    let events = server.connection_events();
    let (mut client, stream) = isolated_pair().await;
    stream.abort_handle().abort();
    assert!(stream.shared_memory_available().is_err());
    let handler = handle_connection(Arc::clone(&server), stream);
    let observation = match events.recv().await.unwrap().unwrap() {
        c2_local::ConnectionEvent::Opened(observation) => observation,
        _ => panic!("expected Opened"),
    };
    handler.await;
    let closed = observation
        .snapshot()
        .expect("query failure closes connection");
    assert_eq!(closed.reason(), ConnectionEndReason::IoError);
    assert_eq!(closed.operation(), "shm_reachability");
    assert_eq!(server.active_connection_count(), 0);
    assert!(matches!(events.recv().await.unwrap(),
            Some(c2_local::ConnectionEvent::Closed(record)) if record == closed));
    use tokio::io::AsyncReadExt as _;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

/// Shared fixture for the un-negotiated buddy frame rejections: a real
/// connection handler over a local pair with one echo route, a two-stage
/// handshake that offers no SHM directions, and the observed ACK.
async fn unnegotiated_shm_connection(
    server: &Arc<Server>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> (LocalStream, tokio::task::JoinHandle<()>) {
    let mut route = make_host_route(server, "grid");
    route.callback = Arc::new(EchoRequestBytes { calls });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };

    negotiate_test_version(&mut client).await;
    let handshake = c2_wire::handshake::encode_client_handshake(0, "unnegotiated-prefix").unwrap();
    client
        .write_all(&encode_frame(0, FLAG_HANDSHAKE, &handshake))
        .await
        .expect("write handshake");
    let (_rid, _flags, payload) = read_reply_frame(&mut client).await;
    let ack = c2_wire::handshake::decode_handshake(&payload, true).unwrap();
    assert_eq!(ack.capability_flags & CAP_SHM_REQUEST, 0);
    assert_eq!(ack.capability_flags & CAP_SHM_RESPONSE, 0);
    (client, handler)
}

fn fabricated_buddy_call_frame(request_id: u64) -> Vec<u8> {
    use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
    use c2_wire::control::encode_call_control;

    let mut payload = encode_buddy_payload(&BuddyPayload {
        seg_idx: 0,
        generation: 7,
        offset: 4096,
        data_size: 4096,
        is_dedicated: false,
        loan_id: 4242,
    })
    .to_vec();
    payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    payload.extend_from_slice(b"never-mapped");
    encode_frame(
        request_id,
        c2_wire::flags::FLAG_CALL_V2 | c2_wire::flags::FLAG_BUDDY,
        &payload,
    )
}

/// An inbound buddy request frame on a connection that never negotiated
/// the SHM request direction is refused with a correlated protocol error
/// before any admission, lazy-open, or cleanup could touch a mapping, and
/// the connection itself stays healthy.
#[tokio::test]
async fn unnegotiated_buddy_request_is_refused_before_any_mapping() {
    let server = ordering_test_server("ipc://shm_buddy_refuse", 8);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut client, handler) = unnegotiated_shm_connection(&server, Arc::clone(&calls)).await;

    client
        .write_all(&fabricated_buddy_call_frame(31))
        .await
        .expect("write un-negotiated buddy frame");
    let (rid, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 31);
    assert!(flags & FLAG_RESPONSE != 0);
    assert!(
        flags & c2_wire::flags::FLAG_BUDDY == 0,
        "no coordinates back"
    );
    let (control, _) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
    match control {
        ReplyControl::Error(bytes) => {
            let error = C2Error::from_wire_bytes(&bytes)
                .unwrap()
                .expect("error payload");
            assert_eq!(error.code, ErrorCode::ProtocolViolation);
            assert!(error.message.contains("not negotiated"));
        }
        other => panic!("expected a protocol violation, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "resource must not run");

    // The connection is not poisoned: a plain inline call still succeeds.
    client
        .write_all(&inline_call_frame(32, "grid", b"still-alive"))
        .await
        .expect("write inline call");
    let (rid, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 32);
    assert!(flags & FLAG_RESPONSE != 0);
    let (control, consumed) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
    match control {
        ReplyControl::Success => {}
        other => panic!("expected successful reply, got {other:?}"),
    }
    assert_eq!(&payload[consumed..], b"still-alive");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    handler.await.expect("connection handler completes");
}

/// A BUDDY|CHUNKED frame on an un-negotiated connection is refused the
/// same way — before chunk admission or processing permits — and a
/// subsequent fully inline chunked request still completes.
#[tokio::test]
async fn unnegotiated_buddy_chunk_frame_is_refused_with_correlated_error() {
    let server = ordering_test_server("ipc://shm_buddy_chunk_refuse", 8);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut client, handler) = unnegotiated_shm_connection(&server, Arc::clone(&calls)).await;

    // Fabricate a buddy-backed first chunk: buddy payload + chunk header +
    // call control + data, flagged CALL_V2|CHUNKED|BUDDY|CHUNK_LAST.
    use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
    let mut payload = encode_buddy_payload(&BuddyPayload {
        seg_idx: 1,
        generation: 3,
        offset: 0,
        data_size: 128,
        is_dedicated: true,
        loan_id: 1,
    })
    .to_vec();
    payload.extend_from_slice(&c2_wire::chunk::header::encode_chunk_header(0, 1));
    payload.extend_from_slice(
        &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
    );
    payload.extend_from_slice(b"chunk-body");
    client
        .write_all(&encode_frame(
            44,
            c2_wire::flags::FLAG_CALL_V2
                | FLAG_CHUNKED
                | c2_wire::flags::FLAG_BUDDY
                | c2_wire::flags::FLAG_CHUNK_LAST,
            &payload,
        ))
        .await
        .expect("write un-negotiated buddy chunk frame");

    let (rid, flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 44);
    assert!(flags & c2_wire::flags::FLAG_BUDDY == 0);
    let (control, _) = c2_wire::control::decode_reply_control(&reply_payload, 0).unwrap();
    match control {
        ReplyControl::Error(bytes) => {
            let error = C2Error::from_wire_bytes(&bytes)
                .unwrap()
                .expect("error payload");
            assert_eq!(error.code, ErrorCode::ProtocolViolation);
        }
        other => panic!("expected a protocol violation, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // No chunk admission was published for the refused request id.
    assert_eq!(server.chunk_admission_gate.len(), 0);
    assert_eq!(server.chunk_registry.active_count(), 0);

    // A fully inline chunked request still completes on this connection.
    let data = b"inline-chunked-payload";
    client
        .write_all(&chunked_call_frame(45, 0, 1, data, "grid"))
        .await
        .expect("write inline chunked call");
    let (rid, _flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 45);
    let (control, consumed) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
    match control {
        ReplyControl::Success => {}
        other => panic!("expected successful reply, got {other:?}"),
    }
    assert_eq!(&payload[consumed..], data);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    handler.await.expect("connection handler completes");
}

fn inline_call_frame(request_id: u64, identity: &str, data: &[u8]) -> Vec<u8> {
    use c2_wire::control::encode_call_control;
    let mut payload = encode_call_control(&call_identity(identity), 0).unwrap();
    payload.extend_from_slice(data);
    encode_frame(request_id, c2_wire::flags::FLAG_CALL_V2, &payload)
}

/// Both normal and prepared responses use the same owned path when the
/// peer cannot map SHM: small replies stay inline, large replies are
/// ordered chunks, and a prepared allocation is freed by its original pool.
#[tokio::test]
async fn responses_without_shm_negotiation_keep_owned_framing_and_release() {
    for prepared in [false, true] {
        for size in [512usize, 8193] {
            let pool = small_response_pool("shm_direction_off");
            let payload: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
            let meta = if prepared {
                ResponseMeta::ShmAlloc(
                    try_prepare_shm_response(&pool, 0, payload.len(), |dst| {
                        dst.copy_from_slice(&payload);
                        Ok(())
                    })
                    .unwrap()
                    .unwrap(),
                )
            } else {
                ResponseMeta::Inline(payload.clone())
            };
            let segments_before = pool.read().segment_count();
            let (mut peer, stream) = isolated_pair().await;
            let (reader, half) = stream.into_split();
            let conn = Arc::new(Connection::new(7));
            conn.set_handshake_done(true).unwrap();
            let (writer, writer_task) = ConnectionWriter::spawn(
                half,
                Arc::clone(&conn),
                Duration::from_secs(30),
                Arc::new(Semaphore::new(1024)),
            );
            let receive = async {
                let mut assembled = Vec::new();
                let chunks = if size > 1024 { size.div_ceil(4096) } else { 1 };
                for index in 0..chunks {
                    let (rid, flags, body) = read_reply_frame(&mut peer).await;
                    assert_eq!(rid, 7);
                    assert_eq!(flags & FLAG_BUDDY, 0, "no SHM coordinates");
                    assert_ne!(flags & FLAG_RESPONSE, 0);
                    let consumed = if chunks > 1 {
                        let last = if index + 1 == chunks {
                            FLAG_CHUNK_LAST
                        } else {
                            0
                        };
                        assert_eq!(
                            flags & (FLAG_CHUNKED | FLAG_CHUNK_LAST),
                            FLAG_CHUNKED | last
                        );
                        c2_wire::chunk::header::decode_reply_chunk_meta(&body, 0)
                            .unwrap()
                            .3
                    } else {
                        assert_eq!(flags & FLAG_CHUNKED, 0);
                        let (control, consumed) =
                            c2_wire::control::decode_reply_control(&body, 0).unwrap();
                        assert!(matches!(control, ReplyControl::Success));
                        consumed
                    };
                    assembled.extend_from_slice(&body[consumed..]);
                }
                assembled
            };
            let (sent, assembled) = tokio::join!(
                send_response_meta(&pool, &writer, 7, meta, 1024, 4096, 128 * 1024, false),
                receive
            );
            sent.unwrap();
            assert_eq!(assembled, payload);
            conn.end(ConnectionEndReason::LocalClose, "test_done");
            drop((reader, writer));
            writer_task.await.unwrap();
            assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
            assert_eq!(pool.read().stats().alloc_count, 0);
            assert_eq!(
                pool.read().segment_count(),
                segments_before,
                "owned path must not allocate SHM"
            );
        }
    }
}

/// Route admission refusal while later chunks are parked on the ordering
/// gate must wake them, complete the caller with the structured canonical
/// error, and leave no ordering entry, assembly, or permit behind.
#[tokio::test]
async fn route_refusal_wakes_parked_later_chunks_and_releases_state() {
    let server = ordering_test_server("ipc://chunked_ordering_refusal", 3);
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // First chunk targets a route that is not in the catalog, so admission
    // is refused immediately; later chunks must not feed anything.
    client
        .write_all(&chunked_call_frame(88, 0, 2, b"first-half", "missing"))
        .await
        .expect("write first chunk");
    client
        .write_all(&chunked_call_frame(88, 1, 2, b"second-half", "missing"))
        .await
        .expect("write later chunk");

    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 88);
    let (control, _) =
        c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
    match control {
        ReplyControl::RouteNotFound(route) => assert_eq!(route, "missing"),
        other => panic!("expected structured route-not-found reply, got {other:?}"),
    }

    // The refused request leaves no ordering entry, assembly, or permit.
    let conn_id = server.active_connection_ids()[0];
    wait_until("ordering state released after refusal", || {
        server.chunk_admission_gate.len() == 0
            && server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 88));
    assert!(scheduler.try_acquire(0).is_ok());

    drop(client);
    handler.await.expect("connection handler completes");
}

/// Disconnect while the first chunk is parked behind the route gate must
/// tear the ordering state and every chunk permit down (cancellation), and
/// must not leave the caller's request charged in the registry.
#[tokio::test]
async fn disconnect_cancels_parked_first_chunk_and_releases_permits() {
    let server = ordering_test_server("ipc://chunked_ordering_cancel", 3);
    let mut route = make_host_route(&server, "grid");
    route.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    ));
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // Park the first chunk on the route gate and two later chunks on its
    // admission; every chunk-processing permit is then held.
    let dispatcher_guard = server.dispatcher.write().await;
    for idx in 0..3usize {
        client
            .write_all(&chunked_call_frame(
                99,
                idx as u16,
                3,
                &[0x5au8; 32],
                "grid",
            ))
            .await
            .expect("write chunk frame");
    }
    wait_until("chunk frames parked behind the route gate", || {
        server.chunk_admission_gate.len() == 1
            && server.chunk_processing_permits.available_permits() == 0
    })
    .await;
    let conn_id = server.active_connection_ids()[0];

    // Client disconnect: the receive loop cancels the connection, which
    // cancels the parked first-chunk task behind the held gate.
    drop(client);
    handler.await.expect("connection handler completes");

    wait_until("disconnect released ordering state and permits", || {
        server.chunk_admission_gate.len() == 0
            && server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 99));
    drop(dispatcher_guard);
}

/// An incomplete chunked request that expires on the assembler timeout must
/// release its assembly charge and its stored route pending permit, leave
/// no ordering state behind, keep answering control frames, and still serve
/// a complete request afterwards with every byte delivered once.
#[tokio::test]
async fn assembler_timeout_releases_charge_and_route_capacity_then_serves_again() {
    use std::sync::atomic::AtomicUsize;

    let mut config = ServerIpcConfig::default();
    config.base.max_total_chunks = 3;
    config.base.chunk_assembler_timeout_secs = 0.05;
    let server = Arc::new(test_server_new("ipc://chunked_ordering_timeout", config).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // First chunk + middle chunk of a three-chunk request: the assembly
    // stays incomplete and keeps the route pending permit while it waits.
    for idx in 0..2usize {
        client
            .write_all(&chunked_call_frame(
                55,
                idx as u16,
                3,
                &[0x44u8; 32],
                "grid",
            ))
            .await
            .expect("write chunk frame");
    }
    wait_until(
        "incomplete assembly registered with route admission",
        || {
            server.chunk_registry.active_count() == 1
                && server.chunk_admission_gate.len() == 0
                && server.chunk_processing_permits.available_permits() == 3
        },
    )
    .await;
    let conn_id = server.active_connection_ids()[0];
    assert!(matches!(
        scheduler.try_acquire(0),
        Err(crate::scheduler::SchedulerAcquireError::Capacity {
            field: "max_pending",
            limit: 1,
        })
    ));

    // Control liveness while the incomplete assembly is still charged.
    client
        .write_all(&encode_frame(
            6,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .expect("write ping");
    let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
    assert_eq!(ping_rid, 6);
    assert_eq!(ping_payload, PONG_BYTES);

    // Let the assembler timeout elapse and run the same sweep the
    // background GC task runs.
    tokio::time::sleep(Duration::from_millis(90)).await;
    let stats = server.chunk_registry.gc_sweep();
    assert_eq!(stats.expired, 1);

    assert_eq!(server.chunk_registry.active_count(), 0);
    assert!(!server.chunk_registry.contains(conn_id, 55));
    assert_eq!(server.chunk_admission_gate.len(), 0);
    assert_eq!(server.chunk_processing_permits.available_permits(), 3);
    assert!(scheduler.try_acquire(0).is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Control liveness after the timeout, and the connection still serves
    // a complete request with every byte delivered once.
    client
        .write_all(&encode_frame(
            7,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .expect("write ping");
    let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
    assert_eq!(ping_rid, 7);
    assert_eq!(ping_payload, PONG_BYTES);

    let data: Vec<u8> = (0..90u16).map(|i| (i % 251) as u8).collect();
    for idx in 0..3usize {
        client
            .write_all(&chunked_call_frame(
                56,
                idx as u16,
                3,
                &data[idx * 30..(idx + 1) * 30],
                "grid",
            ))
            .await
            .expect("write chunk frame");
    }
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 56);
    let (control, consumed) =
        c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
    match control {
        ReplyControl::Success => {}
        other => panic!("expected successful reply after timeout cleanup, got {other:?}"),
    }
    assert_eq!(&reply_payload[consumed..], data.as_slice());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    handler.await.expect("connection handler completes");
}

// -- Reviewed server admission/ownership defects (recovery repair 2) --

/// Decode a reply payload's structured error message.
fn reply_error_message(payload: &[u8]) -> String {
    let (control, _) = c2_wire::control::decode_reply_control(payload, 0).expect("reply control");
    match control {
        ReplyControl::Error(err) => String::from_utf8_lossy(&err).to_string(),
        other => panic!("expected a structured error reply, got {other:?}"),
    }
}

/// A terminal chunk-processing capacity refusal while the first chunk is
/// still parked in route admission must wake and fence the owner: after the
/// stalled route gate is released there is no callback, no recreated
/// assembly, no stored route admission, and no leaked permit.
#[tokio::test]
async fn capacity_refusal_fences_stalled_first_admission_against_post_abort_publication() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_terminal_capacity", 2);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // Stall the first chunk's route admission and fill both chunk-processing
    // permits: the first chunk parks on the held dispatcher gate, one later
    // chunk parks on its admission.
    let dispatcher_guard = server.dispatcher.write().await;
    client
        .write_all(&chunked_call_frame(61, 0, 3, &[0x11u8; 32], "grid"))
        .await
        .expect("write first chunk");
    client
        .write_all(&chunked_call_frame(61, 1, 3, &[0x22u8; 32], "grid"))
        .await
        .expect("write later chunk");
    wait_until("chunk frames parked at capacity", || {
        server.chunk_admission_gate.len() == 1
            && server.chunk_processing_permits.available_permits() == 0
    })
    .await;

    // This frame cannot get a chunk-processing permit, so the request is
    // terminal: the refusal must fence the parked admission instead of
    // letting it publish when the route gate opens.
    client
        .write_all(&chunked_call_frame(61, 2, 3, &[0x33u8; 32], "grid"))
        .await
        .expect("write capacity-refused frame");
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 61);
    let message = reply_error_message(&reply_payload);
    assert!(
        message.contains("max_total_chunks=2"),
        "capacity refusal must name the chunk-processing bound: {message}"
    );

    // Release the stalled gate while the parked owner may still be inside
    // its route-admission await: even if the reservation completes, the
    // atomic admission commit must lose and the staged publication must
    // roll back.
    drop(dispatcher_guard);
    wait_until("terminal refusal released the parked admission", || {
        server.chunk_admission_gate.len() == 0
            && server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let conn_id = server.active_connection_ids()[0];
    assert!(!server.chunk_registry.contains(conn_id, 61));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no callback after refusal");
    assert!(
        scheduler.try_acquire(0).is_ok(),
        "the route pending permit must not stay charged"
    );
    assert_eq!(server.chunk_processing_permits.available_permits(), 2);

    // Control liveness on the same connection after the terminal refusal.
    client
        .write_all(&encode_frame(
            5,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .expect("write ping");
    let (ping_rid, _flags, ping_payload) = read_reply_frame(&mut client).await;
    assert_eq!(ping_rid, 5);
    assert_eq!(ping_payload, PONG_BYTES);

    drop(client);
    handler.await.expect("connection handler completes");
}

/// A malformed later chunk frame is a terminal request failure: it must
/// publish the abort, wake the first chunk parked on its admission, release
/// everything, and prevent publication when the route gate finally opens.
#[tokio::test]
async fn malformed_later_chunk_failure_fences_stalled_first_admission() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_terminal_malformed", 3);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    let dispatcher_guard = server.dispatcher.write().await;
    client
        .write_all(&chunked_call_frame(62, 0, 2, &[0x44u8; 32], "grid"))
        .await
        .expect("write first chunk");
    wait_until("first chunk parked on the held route gate", || {
        server.chunk_admission_gate.len() == 1
    })
    .await;

    // A chunked frame too short to hold a chunk header is terminal: its
    // dispatch reports the malformed frame and must tear the request down.
    client
        .write_all(&encode_frame(
            62,
            c2_wire::flags::FLAG_CALL_V2 | FLAG_CHUNKED,
            &[0x01, 0x02],
        ))
        .await
        .expect("write malformed later chunk");
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 62);
    let message = reply_error_message(&reply_payload);
    assert!(
        message.contains("chunk header decode failed"),
        "malformed chunk must be correlated: {message}"
    );

    drop(dispatcher_guard);
    wait_until(
        "malformed-frame teardown released the parked admission",
        || {
            server.chunk_admission_gate.len() == 0
                && server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let conn_id = server.active_connection_ids()[0];
    assert!(!server.chunk_registry.contains(conn_id, 62));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    drop(client);
    handler.await.expect("connection handler completes");
}

/// A duplicate first chunk over a pending admission refuses the duplicate
/// frame and tears the request down: the parked owner publishes nothing,
/// the caller gets exactly one correlated error, and every permit returns.
#[tokio::test]
async fn duplicate_first_chunk_refusal_aborts_pending_admission() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_duplicate_first", 3);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    let dispatcher_guard = server.dispatcher.write().await;
    client
        .write_all(&chunked_call_frame(81, 0, 2, &[0x66u8; 32], "grid"))
        .await
        .expect("write first chunk");
    wait_until("first admission pending", || {
        server.chunk_admission_gate.len() == 1
    })
    .await;

    client
        .write_all(&chunked_call_frame(81, 0, 2, &[0x77u8; 32], "grid"))
        .await
        .expect("write duplicate first chunk");
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 81);
    let message = reply_error_message(&reply_payload);
    assert!(
        message.contains("duplicate first chunk"),
        "duplicate first chunk must be refused: {message}"
    );

    drop(dispatcher_guard);
    wait_until("duplicate refusal released the request", || {
        server.chunk_admission_gate.len() == 0
            && server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let conn_id = server.active_connection_ids()[0];
    assert!(!server.chunk_registry.contains(conn_id, 81));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    drop(client);
    handler.await.expect("connection handler completes");
}

/// A duplicate first chunk over an already-admitted request refuses the
/// duplicate and tears the published generation down: the assembly is
/// released immediately (not left until its later chunks or the assembler
/// timeout) and its stored route admission returns.
#[tokio::test]
async fn duplicate_first_chunk_after_admission_aborts_the_published_assembly() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_duplicate_after_admission", 3);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    // The first chunk of a two-chunk request is admitted and its assembly
    // and stored route admission are published.
    client
        .write_all(&chunked_call_frame(82, 0, 2, &[0x21u8; 32], "grid"))
        .await
        .expect("write first chunk");
    wait_until("assembly published after admission", || {
        server.chunk_registry.active_count() == 1
    })
    .await;
    let conn_id = server.active_connection_ids()[0];
    assert!(server.chunk_registry.contains(conn_id, 82));

    // A duplicate first chunk for the same request id claims a fresh
    // admission entry (the previous one was released on admission), then
    // fails the registry duplicate check. The refusal must release the
    // published generation instead of stranding it.
    client
        .write_all(&chunked_call_frame(82, 0, 2, &[0x22u8; 32], "grid"))
        .await
        .expect("write duplicate first chunk");
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 82);
    let message = reply_error_message(&reply_payload);
    assert!(
        message.contains("duplicate assembly"),
        "duplicate admission must be correlated: {message}"
    );

    wait_until(
        "duplicate refusal released the published generation",
        || {
            server.chunk_registry.active_count() == 0
                && server.chunk_processing_permits.available_permits() == 3
                && server.chunk_admission_gate.len() == 0
        },
    )
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 82));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    drop(client);
    handler.await.expect("connection handler completes");
}

/// Cancellation after the assembly/route publication but before the
/// admission commit must roll the staged publication back exactly once.
///
/// The owner below is already torn down when its route wait completes (the
/// terminal abort won the atomic transition), which exercises the commit
/// fence directly rather than the wake-up path.
#[tokio::test]
async fn lost_admission_commit_rolls_back_staged_publication_exactly_once() {
    use c2_mem::budget::BudgetKind;
    use c2_wire::chunk::header::encode_chunk_header;
    use c2_wire::control::encode_call_control;

    const TOTAL_CHUNKS: usize = 2;
    const CHUNK_SIZE: usize = 512;
    let reassembly_bytes = (TOTAL_CHUNKS * (CHUNK_SIZE + std::mem::size_of::<bool>())) as u64;
    let budget = c2_mem::MemoryBudget::new(1 << 20, 1 << 20, 4096);
    let reassembly_pool = Arc::new(parking_lot::RwLock::new(
        MemPool::new_with_prefix_and_budget(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 2,
                buddy_idle_decay_secs: 0.0,
                spill_threshold: 1.0,
                spill_dir: std::env::temp_dir().join("c2_srv_commit_fence_test"),
                ..PoolConfig::default()
            },
            "/c2srv_commit_fence".to_string(),
            budget.clone(),
        )
        .unwrap(),
    ));
    let mut config = ServerIpcConfig::default();
    config.base.max_total_chunks = 4;
    let server = Arc::new(
        Server::with_reassembly_pool(
            test_parse_local_endpoint("ipc://chunk_commit_fence").unwrap(),
            config,
            ServerIdentity {
                server_id: "chunk_commit_fence".into(),
                server_instance_id: "chunk_commit_fence-instance".into(),
            },
            Arc::clone(&reassembly_pool),
        )
        .unwrap(),
    );
    let mut route = make_host_route(&server, "grid");
    let scheduler = Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    );
    route.scheduler = Arc::new(scheduler.clone());
    server.register_route(route).await.unwrap();

    let conn = Arc::new(Connection::new(321));
    let writer = closed_writer().await;
    let mut payload = Vec::new();
    payload.extend_from_slice(&encode_chunk_header(0, TOTAL_CHUNKS as u16));
    payload.extend_from_slice(&encode_call_control(&call_identity("grid"), 0).unwrap());
    payload.extend_from_slice(&[0x5au8; CHUNK_SIZE]);

    // Frame dispatch claimed this request's admission entry; a concurrent
    // capacity/feed/malformed refusal then tore the request down while the
    // owning task was still waiting for route admission.
    let owner = server.begin_chunk_admission(conn.conn_id(), 31).unwrap();
    server.abort_chunk_request(conn.conn_id(), 31);

    let chunk_permit = server.try_acquire_chunk_processing_permit().unwrap();
    dispatch_chunked_call(
        &server,
        &conn,
        31,
        FLAG_CHUNKED,
        &payload,
        &writer,
        chunk_permit,
        ChunkFrameOrdering::First(owner),
        None,
    )
    .await;

    // Route admission completed and the assembly + stored admission were
    // published, but the atomic admission commit lost: the staged
    // publication was rolled back exactly once.
    let budget_used = budget.snapshot().cell(BudgetKind::Reassembly).used_bytes;
    assert_eq!(
        budget_used, 0,
        "the rolled-back assembly must refund its reassembly charge"
    );
    assert!(!server.chunk_registry.contains(conn.conn_id(), 31));
    assert_eq!(server.chunk_registry.active_count(), 0);
    assert_eq!(server.chunk_registry.total_bytes(), 0);
    assert!(
        scheduler.try_acquire(0).is_ok(),
        "the route pending permit must be returned"
    );
    assert_eq!(server.chunk_admission_gate.len(), 0);
    assert_eq!(server.chunk_processing_permits.available_permits(), 4);

    // Observe the two release orders while the real pool prevents backing
    // release. Run synchronous teardown off the Tokio worker.
    for rollback in [false, true] {
        let admission = reserve_accepted_route_execution(&server, &call_identity("grid"), 0)
            .await
            .ok()
            .expect("route admission for release-order probe");
        let assembly = server
            .chunk_registry
            .insert(
                conn.conn_id(),
                31,
                TOTAL_CHUNKS,
                CHUNK_SIZE,
                ChunkRouteAdmission {
                    route: admission.route,
                    method_idx: 0,
                    pending_permit: admission.pending_permit,
                    _flight: crate::connection::FlightGuard::new(&conn),
                },
            )
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        let pool_guard = reassembly_pool.try_write().expect("idle reassembly pool");
        let cleanup = {
            let server = Arc::clone(&server);
            tokio::task::spawn_blocking(move || {
                if rollback {
                    drop(ChunkPublicationGuard {
                        server: &server,
                        assembly: Some(assembly),
                    });
                } else {
                    server.abort_chunk_request(assembly.conn_id(), 31);
                }
            })
        };
        wait_until("assembly detached before backing release", || {
            server.chunk_registry.active_count() == 0
        })
        .await;
        assert!(!cleanup.is_finished());
        assert_eq!(
            budget.snapshot().cell(BudgetKind::Reassembly).used_bytes,
            reassembly_bytes
        );
        if rollback {
            // Registry detachment precedes the lock-free permit drop.
            // Wait for that drop while backing release remains blocked.
            tokio::time::timeout(Duration::from_secs(5), scheduler.wait_drained_async())
                .await
                .expect("rollback releases its permit before the backing");
        }
        match scheduler.try_acquire(0) {
            Ok(guard) if rollback => drop(guard),
            Err(SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            }) if !rollback => {}
            other => panic!("unexpected capacity during rollback={rollback}: {other:?}"),
        }
        drop(pool_guard);
        tokio::time::timeout(Duration::from_secs(5), cleanup)
            .await
            .expect("teardown completes after pool unlock")
            .unwrap();
        assert_eq!(reassembly_pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().cell(BudgetKind::Reassembly).used_bytes, 0);
        assert!(scheduler.try_acquire(0).is_ok());
    }
}

#[tokio::test]
async fn ping_is_served_while_chunk_processing_capacity_is_exhausted() {
    use std::sync::atomic::AtomicUsize;

    let server = ordering_test_server("ipc://chunked_ping_at_capacity", 1);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    let dispatcher_guard = server.dispatcher.write().await;
    let data: Vec<u8> = (0..96u8).map(|i| i.wrapping_mul(7)).collect();
    client
        .write_all(&chunked_call_frame(71, 0, 1, &data, "grid"))
        .await
        .expect("write single chunk frame");
    wait_until("the only chunk-processing permit is held", || {
        server.chunk_processing_permits.available_permits() == 0
            && server.chunk_admission_gate.len() == 1
    })
    .await;
    assert_eq!(server.chunk_registry.active_count(), 0);

    // Control liveness at full chunk-processing capacity.
    client
        .write_all(&encode_frame(
            9,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .expect("write ping");
    let (ping_rid, ping_flags, ping_payload) = read_reply_frame(&mut client).await;
    assert_eq!(ping_rid, 9);
    assert!(ping_flags & FLAG_SIGNAL != 0);
    assert_eq!(ping_payload, PONG_BYTES);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Release the gate: the same request completes with exactly its bytes.
    drop(dispatcher_guard);
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 71);
    let (control, consumed) =
        c2_wire::control::decode_reply_control(&reply_payload, 0).expect("reply control");
    match control {
        ReplyControl::Success => {}
        other => panic!("expected successful reply after capacity stall, got {other:?}"),
    }
    assert_eq!(&reply_payload[consumed..], data.as_slice());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(server.chunk_processing_permits.available_permits(), 1);
    assert_eq!(server.chunk_admission_gate.len(), 0);
    assert_eq!(server.chunk_registry.active_count(), 0);

    drop(client);
    handler.await.expect("connection handler completes");
}

#[test]
fn buddy_response_wire_limit_rejects_oversized_payloads() {
    assert_eq!(
        crate::response::buddy_response_data_size(u32::MAX as usize),
        Some(u32::MAX)
    );
    assert_eq!(
        crate::response::buddy_response_data_size(u32::MAX as usize + 1),
        None
    );
}

#[test]
fn reply_chunk_count_rejects_unrepresentable_chunk_counts() {
    assert!(reply_chunk_count(0, 0).unwrap_err().contains("chunk_size"));
    assert_eq!(reply_chunk_count(0, 128).unwrap(), 0);
    assert_eq!(reply_chunk_count(1025, 512).unwrap(), 3);

    let err = reply_chunk_count(u32::MAX as usize + 1, 1).unwrap_err();
    assert!(err.contains("chunk count"));
}

#[test]
fn inline_reply_frame_len_rejects_unrepresentable_frames() {
    assert!(inline_reply_total_len(16).is_ok());

    let err = inline_reply_total_len(u32::MAX as usize).unwrap_err();
    assert!(err.contains("inline reply frame"));
}

#[tokio::test]
async fn smart_reply_without_shm_capacity_sends_complete_ordered_chunks() {
    let pool = small_response_pool("chunkfallback");
    let reservations: Vec<_> = (0..2)
        .map(|_| pool.inner.write().alloc_shm_handle(64 * 1024).unwrap())
        .collect();
    assert!(pool.inner.write().alloc_shm_handle(8193).is_err());
    let payload: Vec<_> = (0..8193).map(|i| (i % 251) as u8).collect();
    let (mut peer, stream) = isolated_pair().await;
    let (reader, half) = stream.into_split();
    let conn = Arc::new(Connection::new(7));
    conn.set_handshake_done(true).unwrap();
    let (writer, writer_task) = ConnectionWriter::spawn(
        half,
        Arc::clone(&conn),
        Duration::from_secs(30),
        Arc::new(Semaphore::new(1024)),
    );
    let receive = async {
        for (idx, expected) in payload.chunks(4096).enumerate() {
            let (request_id, flags, body) = read_reply_frame(&mut peer).await;
            assert_eq!(request_id, 7);
            let last = if idx == 2 { FLAG_CHUNK_LAST } else { 0 };
            assert_eq!(flags, FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_CHUNKED | last);
            assert_eq!(
                c2_wire::chunk::header::decode_reply_chunk_meta(&body, 0).unwrap(),
                (8193, 3, idx as u32, REPLY_CHUNK_META_SIZE)
            );
            assert_eq!(&body[REPLY_CHUNK_META_SIZE..], expected);
        }
    };
    let (sent, ()) = tokio::join!(
        smart_reply_with_data(&pool, &writer, 7, &payload, 1024, 4096, true),
        receive
    );
    sent.unwrap();
    conn.end(ConnectionEndReason::LocalClose, "test_done");
    drop((reader, writer));
    writer_task.await.unwrap();
    assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    assert_eq!(pool.read().stats().alloc_count, 2);
    for block in reservations {
        pool.inner.write().release_handle(block);
    }
    assert_eq!(pool.read().stats().alloc_count, 0);
}

#[tokio::test]
async fn inline_response_over_max_payload_is_rejected_before_transport() {
    let pool = small_response_pool("c");
    let writer = closed_writer().await;

    let err = send_response_meta(
        &pool,
        &writer,
        7,
        ResponseMeta::Inline(b"x".repeat(1025)),
        1024,
        4096,
        1024,
        true,
    )
    .await
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("response payload size 1025 exceeds max_payload_size 1024")
    );
    assert_eq!(pool.read().stats().alloc_count, 0);
}

#[test]
fn server_pools_and_prewarm_share_one_budget_context() {
    let base = c2_config::BaseIpcConfig {
        pool_segment_size: 64 * 1024,
        max_pool_segments: 2,
        pool_prewarm_segments: 1,
        ..c2_config::BaseIpcConfig::default()
    };
    let config = ServerIpcConfig {
        base,
        ..ServerIpcConfig::default()
    };
    let server = test_server_new("ipc://server_budget_share", config).unwrap();

    let snapshot = server.memory_budget_snapshot();
    assert_eq!(ServerMemorySnapshot::SCOPE, "server");
    assert_eq!(
        snapshot.limits.shm_backing_budget_bytes,
        c2_config::MemoryBudgetLimits::default().shm_backing_budget_bytes
    );
    assert!(
        snapshot.budget.shm.used_bytes > 0,
        "explicit prewarm must charge the server budget"
    );

    // Both owner pools report the same accounting object.
    let response_pool = server.response_pool_arc();
    assert_eq!(
        response_pool.read().budget().unwrap().snapshot(),
        snapshot.budget
    );
    assert_eq!(
        server
            .chunk_registry
            .pool()
            .read()
            .budget()
            .unwrap()
            .snapshot(),
        snapshot.budget
    );

    // Observing twice is stable and never resets usage.
    assert_eq!(server.memory_budget_snapshot(), snapshot);
}

/// A server configured with zero shm backing admits no server-side
/// mapping, while the snapshot still reports the resolved zero limits.
#[test]
fn server_zero_shm_budget_admits_no_backing_and_reports_zero_limits() {
    let base = c2_config::BaseIpcConfig {
        pool_segment_size: 64 * 1024,
        max_pool_segments: 2,
        shm_backing_budget_bytes: 0,
        file_backing_budget_bytes: 0,
        ..c2_config::BaseIpcConfig::default()
    };
    let server = test_server_new(
        "ipc://server_budget_zero",
        ServerIpcConfig {
            base,
            ..ServerIpcConfig::default()
        },
    )
    .unwrap();
    let snapshot = server.memory_budget_snapshot();
    assert_eq!(snapshot.limits.shm_backing_budget_bytes, 0);
    assert_eq!(snapshot.limits.file_backing_budget_bytes, 0);
    assert_eq!(snapshot.budget.shm.used_bytes, 0);
    assert_eq!(server.response_pool_arc().read().segment_count(), 0);
}

/// A retained server budget observer must outlive the Server and report
/// charges held by an outstanding response owner until that owner
/// releases them. This is the read-only half of retired observability:
/// the handle carries counters only, never the Server or its pools.
#[test]
fn server_budget_observer_outlives_the_server_and_tracks_retained_charges() {
    let base = c2_config::BaseIpcConfig {
        pool_segment_size: 64 * 1024,
        max_pool_segments: 2,
        ..c2_config::BaseIpcConfig::default()
    };
    let config = ServerIpcConfig {
        base,
        // Retire an idle segment at the next explicit GC so the release
        // is deterministic without sleeping.
        pool_decay_seconds: 0.0,
        ..ServerIpcConfig::default()
    };
    let server = test_server_new("ipc://server_budget_observer", config).unwrap();
    let observer = server.memory_budget_observer();
    assert_eq!(
        observer.limits(),
        server.memory_budget_snapshot().limits,
        "the observer must carry the server's resolved limits"
    );

    let response_pool = server.response_pool_arc();
    let handle = response_pool
        .inner
        .write()
        .alloc_handle(4096)
        .expect("response allocation");
    let charged = observer.used_bytes();
    assert!(
        charged > 0,
        "a mapped response segment must charge the observed budget"
    );
    assert_eq!(
        server.memory_budget_snapshot().budget.shm.used_bytes,
        observer.snapshot().shm.used_bytes,
        "observer and live snapshot must share one accounting object"
    );

    // The Server can be dropped while the outstanding response owner
    // keeps the mapping (and its observed charge) alive.
    drop(server);
    assert_eq!(observer.used_bytes(), charged);

    response_pool.inner.write().release_handle(handle);
    response_pool.inner.write().gc_buddy();
    assert_eq!(
        observer.used_bytes(),
        0,
        "the retained observer must see the charge return after release"
    );
}

// -- handshake extraction --

#[test]
fn handshake_extracts_client_info() {
    use c2_wire::handshake::{CAP_SHM_REQUEST, decode_handshake, encode_client_handshake};

    let hs_bytes = encode_client_handshake(CAP_SHM_REQUEST, "/cc3b_test").unwrap();
    let decoded = decode_handshake(&hs_bytes, false).unwrap();
    assert_eq!(decoded.prefix, "/cc3b_test");
    assert_eq!(decoded.capability_flags, CAP_SHM_REQUEST);
}

#[tokio::test]
async fn committed_route_waiting_for_admission_projects_closed_register_committed() {
    use c2_wire::route_catalog_control::{RouteStateReasonWire, RouteStateWire};

    let server = test_server_new(
        &unique_readiness_address("route_committed_pending"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let route = make_host_route(&server, "grid");
    let scheduler = Arc::clone(&route.scheduler);
    let token = commit_closed_test_route(&server, route).await;

    let record = route_records(&server).await.pop().unwrap();
    assert_eq!(record.state, RouteStateWire::Closed);
    assert_eq!(record.state_reason, RouteStateReasonWire::RegisterCommitted);
    assert!(scheduler.snapshot().closed);
    assert_eq!(
        scheduler.try_acquire(0).unwrap_err(),
        crate::scheduler::SchedulerAcquireError::Closed,
    );

    server.open_route_admission(token).await.unwrap();
    let record = route_records(&server).await.pop().unwrap();
    assert_eq!(record.state, RouteStateWire::Ready);
    assert_eq!(record.state_reason, RouteStateReasonWire::RegisterCommitted);
    drop(scheduler.try_acquire(0).unwrap());
    let route = server.dispatcher.read().await.resolve("grid").unwrap();
    assert!(server.unregister_route("grid").await);
    let record = route_record(&server, &route);
    assert_eq!(record.state, RouteStateWire::Closed);
    assert_eq!(
        record.state_reason,
        RouteStateReasonWire::ExplicitUnregister
    );
    assert!(scheduler.snapshot().closed);
    assert!(!scheduler.open_admission_for_registration());
}

#[tokio::test]
async fn host_close_retires_committed_route_before_admission() {
    use c2_wire::route_catalog_control::{RouteStateReasonWire, RouteStateWire};

    let server = test_server_new(
        &unique_readiness_address("route_pending_host_close"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let route = make_host_route(&server, "grid");
    let scheduler = Arc::clone(&route.scheduler);
    let token = commit_closed_test_route(&server, route).await;

    assert_eq!(server.close_business_admission("maintenance").await, 1);
    let record = route_records(&server).await.pop().unwrap();
    assert_eq!(record.state, RouteStateWire::Closed);
    assert_eq!(record.state_reason, RouteStateReasonWire::Shutdown);
    assert!(server.open_route_admission(token).await.is_err());
    assert!(scheduler.snapshot().closed);
    assert_eq!(server.close_business_admission("maintenance").await, 0);
}

#[tokio::test]
async fn aborted_and_dropped_reservations_terminally_close_their_schedulers() {
    let server = test_server_new(
        &unique_readiness_address("route_reservation_abort"),
        ServerIpcConfig::default(),
    )
    .unwrap();

    for explicit_abort in [true, false] {
        let route = make_host_route(&server, "grid");
        let scheduler = Arc::clone(&route.scheduler);
        let reservation = server
            .reserve_route(BuiltRoute::new(route).unwrap())
            .await
            .unwrap();
        if explicit_abort {
            server.abort_reserved_route(reservation).await;
        } else {
            drop(reservation);
        }
        assert!(scheduler.snapshot().closed);
        assert_eq!(
            scheduler.try_acquire(0).unwrap_err(),
            crate::scheduler::SchedulerAcquireError::Closed,
        );
        assert!(!server.contains_route("grid").await);
    }
}

struct ShutdownReplyFixture {
    server: Arc<Server>,
    peer: LocalStream,
    conn: Arc<Connection>,
    runner: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    completed: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}
impl ShutdownReplyFixture {
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
    async fn start(response: Vec<u8>) -> Self {
        Self::start_with_request(response, false).await
    }
    async fn start_with_request(response: Vec<u8>, chunked: bool) -> Self {
        Self::start_with_outcome(response, chunked, false).await
    }
    async fn start_with_outcome(response: Vec<u8>, chunked: bool, fail: bool) -> Self {
        Self::start_with_config(response, chunked, fail, ServerIpcConfig::default()).await
    }
    async fn start_with_config(
        response: Vec<u8>,
        chunked: bool,
        fail: bool,
        config: ServerIpcConfig,
    ) -> Self {
        struct Callback {
            calls: Arc<std::sync::atomic::AtomicUsize>,
            completed: Arc<std::sync::atomic::AtomicBool>,
            release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
            response: Vec<u8>,
            fail: bool,
        }
        impl CrmCallback for Callback {
            fn invoke(
                &self,
                _: &str,
                _: u16,
                _: Vec<u8>,
                _: Arc<crate::ResponsePool>,
            ) -> Result<ResponseMeta, CrmError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
                self.completed.store(true, Ordering::SeqCst);
                if self.fail {
                    Err(CrmError::InternalError("expected callback failure".into()))
                } else {
                    Ok(ResponseMeta::Inline(self.response.clone()))
                }
            }
        }
        let server =
            Arc::new(test_server_new(&unique_readiness_address("shutdown_reply"), config).unwrap());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut route = make_host_route(&server, "grid");
        route.callback = Arc::new(Callback {
            calls: calls.clone(),
            completed: completed.clone(),
            release: release.clone(),
            response,
            fail,
        });
        server.register_route(route).await.unwrap();
        let running = server.clone();
        let runner = tokio::spawn(async move { running.run().await });
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        let events = server.connection_events();
        let mut peer = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .unwrap();
        handshake_test_client(&mut peer).await;
        let observation = match events.recv().await.unwrap().unwrap() {
            c2_local::ConnectionEvent::Opened(observation) => observation,
            event => panic!("expected Opened: {event:?}"),
        };
        let conn = server
            .active_connection(observation.connection_id())
            .unwrap();
        let mut fixture = Self {
            server,
            peer,
            conn,
            runner: Some(runner),
            calls,
            completed,
            release,
        };
        let payload = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
        if chunked {
            for index in 0..2 {
                fixture
                    .peer
                    .write_all(&chunked_call_frame(1, index, 2, &[7, 8], "grid"))
                    .await
                    .unwrap();
            }
        } else {
            fixture
                .peer
                .write_all(&encode_frame(1, c2_wire::flags::FLAG_CALL_V2, &payload))
                .await
                .unwrap();
        }
        wait_until("callback entered before shutdown", || {
            fixture.calls.load(Ordering::SeqCst) == 1
        })
        .await;
        fixture
    }
}
impl Drop for ShutdownReplyFixture {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_preserves_active_reply_refuses_new_calls_and_fences_lifecycle_hook() {
    for (same_connection, chunked_request) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let mut f =
            ShutdownReplyFixture::start_with_request(b"done".to_vec(), chunked_request).await;
        let hook_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = {
            let server = f.server.clone();
            let completed = f.completed.clone();
            let hook_called = hook_called.clone();
            tokio::spawn(async move {
                server.wait_until_terminal().await.unwrap();
                assert!(
                    completed.load(Ordering::SeqCst),
                    "shutdown hook cannot precede the active callback"
                );
                hook_called.store(true, Ordering::SeqCst);
            })
        };
        let shutdown = encode_frame(
            99,
            FLAG_SIGNAL,
            &c2_wire::shutdown_control::encode_shutdown_initiate(),
        );
        if same_connection {
            tokio::io::AsyncWriteExt::write_all(&mut f.peer, &shutdown)
                .await
                .unwrap();
            assert_eq!(read_reply_frame(&mut f.peer).await.0, 99);
        } else {
            let mut admin =
                LocalStream::connect(f.server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
                    .await
                    .unwrap();
            // The following ACK read proves delivery. A separate native flush
            // can instead observe the admin peer's already completed close.
            tokio::io::AsyncWriteExt::write_all(&mut admin, &shutdown)
                .await
                .unwrap();
            assert_eq!(read_reply_frame(&mut admin).await.0, 99);
        }
        assert!(
            !f.conn.is_deactivated(),
            "active response must retain business write admission"
        );
        let payload = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
        f.peer
            .write_all(&encode_frame(2, c2_wire::flags::FLAG_CALL_V2, &payload))
            .await
            .unwrap();
        let (id, _, reply) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 2);
        assert!(matches!(
            c2_wire::control::decode_reply_control(&reply, 0).unwrap().0,
            ReplyControl::Error(_)
        ));
        assert_eq!(
            f.calls.load(Ordering::SeqCst),
            1,
            "shutdown admission must reject the new callback"
        );
        assert!(
            f.server
                .wait_until_stopped(Duration::from_millis(30))
                .await
                .is_err()
        );
        assert!(!hook_called.load(Ordering::SeqCst));
        f.release();
        let (id, _, reply) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 1);
        let (control, consumed) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
        assert!(matches!(control, ReplyControl::Success));
        assert_eq!(&reply[consumed..], b"done");
        f.runner.take().unwrap().await.unwrap().unwrap();
        hook.await.unwrap();
        assert!(hook_called.load(Ordering::SeqCst));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_keeps_response_flight_until_chunked_reply_flushes() {
    const SIZE: usize = 4 * 1024 * 1024;
    let mut f = ShutdownReplyFixture::start(vec![0x5b; SIZE]).await;
    f.server.request_shutdown_signal();
    f.release();
    wait_until("callback completed before blocked response drain", || {
        f.completed.load(Ordering::SeqCst)
    })
    .await;
    // The peer deliberately withholds reads. The response exceeds the native
    // stream buffer, so callback completion cannot be mistaken for reply flush.
    assert!(
        f.server
            .wait_until_stopped(Duration::from_millis(50))
            .await
            .is_err()
    );
    assert_eq!(f.conn.inflight_count(), 1);
    let mut received = 0;
    let mut index = 0;
    loop {
        let (id, flags, payload) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 1);
        assert!(c2_wire::flags::is_chunked(flags));
        let (size, total, chunk, meta) =
            c2_wire::chunk::header::decode_reply_chunk_meta(&payload, 0).unwrap();
        assert_eq!(size as usize, SIZE);
        assert!(total > 1);
        assert_eq!(chunk, index);
        let data = &payload[meta..];
        assert!(data.iter().all(|byte| *byte == 0x5b));
        received += data.len();
        index += 1;
        if c2_wire::flags::is_chunk_last(flags) {
            break;
        }
    }
    assert_eq!(received, SIZE);
    f.runner.take().unwrap().await.unwrap().unwrap();
    assert_eq!(f.conn.inflight_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_deadline_preserves_the_unfinished_call_and_its_reply() {
    let mut f = ShutdownReplyFixture::start(b"done".to_vec()).await;
    assert!(
        f.server
            .shutdown_and_wait(Duration::from_millis(30))
            .await
            .is_err()
    );
    assert!(f.conn.shutdown_requested());
    assert!(
        !f.conn.is_deactivated(),
        "the caller budget only bounds observation"
    );
    assert!(!f.completed.load(Ordering::SeqCst));
    f.release();
    let (id, _, reply) = read_reply_frame(&mut f.peer).await;
    assert_eq!(id, 1);
    let (control, used) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
    assert!(matches!(control, ReplyControl::Success));
    assert_eq!(&reply[used..], b"done");
    f.runner.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_flushes_the_active_call_error_before_deactivation() {
    let mut f = ShutdownReplyFixture::start_with_outcome(Vec::new(), false, true).await;
    f.server.request_shutdown_signal();
    f.release();
    let (id, _, reply) = read_reply_frame(&mut f.peer).await;
    assert_eq!(id, 1);
    match c2_wire::control::decode_reply_control(&reply, 0).unwrap().0 {
        ReplyControl::Error(error) => {
            assert!(String::from_utf8_lossy(&error).contains("expected callback failure"))
        }
        other => panic!("expected completed callback error, got {other:?}"),
    }
    f.runner.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_unregister_has_exactly_one_removal() {
    let server = test_server_new(
        &unique_readiness_address("unregister_race"),
        Default::default(),
    )
    .unwrap();
    let route = make_host_route(&server, "grid");
    let scheduler = route.scheduler.clone();
    server.register_route(route).await.unwrap();
    let active = scheduler.try_acquire(0).unwrap();
    let mut first = Box::pin(server.unregister_route("grid"));
    let mut second = Box::pin(server.unregister_route("grid"));
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    assert!(first.as_mut().poll(&mut context).is_pending());
    assert!(second.as_mut().poll(&mut context).is_pending());
    drop(active);
    let results = tokio::join!(first, second);
    assert_ne!(
        results.0, results.1,
        "only the invocation that removes the route succeeds"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_keeps_listener_owned_and_control_reachable_until_drain() {
    let mut f = ShutdownReplyFixture::start(b"done".to_vec()).await;
    let endpoint = f.server.local_endpoint().clone();
    f.server.request_shutdown_signal();
    assert!(
        f.server
            .wait_until_stopped(Duration::from_millis(30))
            .await
            .is_err()
    );
    assert!(f.server.endpoint_credential().is_some());
    assert!(
        LocalListener::bind_until(
            &endpoint,
            Some(std::time::Instant::now() + Duration::from_millis(50))
        )
        .is_err(),
        "another host must not bind the address before the active reply drains"
    );
    let mut admin = LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    admin
        .write_all(&encode_frame(
            99,
            FLAG_SIGNAL,
            &c2_wire::shutdown_control::encode_shutdown_initiate(),
        ))
        .await
        .unwrap();
    let (id, _, payload) = read_reply_frame(&mut admin).await;
    assert_eq!(id, 99);
    assert_eq!(payload, c2_wire::shutdown_control::encode_shutdown_ack());
    drop(admin);
    f.release();
    assert_eq!(read_reply_frame(&mut f.peer).await.0, 1);
    f.runner.take().unwrap().await.unwrap().unwrap();
    assert!(f.server.endpoint_credential().is_none());
    let replacement = LocalListener::bind(&endpoint)
        .expect("address available only after drain and listener close");
    assert!(matches!(
        replacement.close(),
        EndpointReapResult::Reaped | EndpointReapResult::NotApplicable
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn startup_budget_bounds_listener_namespace_gate_wait() {
    let directory = tempfile::Builder::new()
        .prefix("c2-start-gate-")
        .tempdir_in("/tmp")
        .unwrap();
    let context = c2_config::LocalEndpointContext::with_unix_root(directory.path()).unwrap();
    let first_endpoint = context
        .endpoint(&unique_readiness_address("gate_setup"))
        .unwrap();
    let first = LocalListener::bind(&first_endpoint).unwrap();
    assert!(matches!(first.close(), EndpointReapResult::Reaped));
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join(".gate"))
        .unwrap();
    gate.lock().unwrap();
    let endpoint = context
        .endpoint(&unique_readiness_address("gate_budget"))
        .unwrap();
    let server = Arc::new(Server::new_with_endpoint(endpoint, Default::default()).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_millis(30);
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        server.run_with_startup_deadline(deadline, Duration::from_millis(30)),
    )
    .await
    .expect("bind must obey the startup deadline")
    .unwrap_err();
    gate.unlock().unwrap();
    assert!(
        matches!(error, ServerError::StartupTimeout(ServerStartupTimeout { stage: ServerStartupStage::Readiness, timeout }) if timeout == Duration::from_millis(30)),
        "{error:?}"
    );
    assert!(matches!(
        server.lifecycle_state(),
        ServerLifecycleState::Failed(_)
    ));
    assert!(server.endpoint_credential().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_writer_preserves_business_errors_capacity_replies_and_other_calls() {
    use std::num::NonZeroUsize;
    use std::sync::atomic::AtomicUsize;
    const CALLS: u64 = 80;
    const BYTES: usize = 512 * 1024;
    struct Replies(Arc<AtomicUsize>);
    impl CrmCallback for Replies {
        fn invoke(
            &self,
            _: &str,
            method: u16,
            _: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if method == 1 {
                Err(CrmError::UserError(error_wire(
                    ErrorCode::ResourceFunctionExecuting,
                    "business failed",
                )))
            } else {
                Ok(ResponseMeta::Inline(vec![0x51; BYTES]))
            }
        }
    }
    let mut config = ServerIpcConfig::default();
    config.shm_threshold = 1024 * 1024;
    config.max_pending_requests = 128;
    let server =
        Arc::new(test_server_new(&unique_readiness_address("full_writer"), config).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits::default(),
        server.admission.clone(),
    ));
    route.callback = Arc::new(Replies(calls.clone()));
    server.register_route(route).await.unwrap();
    let mut limited = make_host_route(&server, "limited");
    limited.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        SchedulerLimits {
            max_pending: Some(NonZeroUsize::new(1).unwrap()),
            max_workers: None,
        },
        server.admission.clone(),
    ));
    let occupied = limited.scheduler.reserve_pending(0).unwrap();
    server.register_route(limited).await.unwrap();
    let running = server.clone();
    let runner = tokio::spawn(async move { running.run().await });
    server
        .wait_until_ready(Duration::from_secs(2))
        .await
        .unwrap();
    let mut peer = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    handshake_test_client(&mut peer).await;
    let conn = server
        .active_connections
        .lock()
        .values()
        .next()
        .unwrap()
        .clone();
    let success = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
    for id in 1..=CALLS {
        peer.write_all(&encode_frame(id, c2_wire::flags::FLAG_CALL_V2, &success))
            .await
            .unwrap();
    }
    wait_until(
        "all successful callbacks returned while peer withholds reads",
        || calls.load(Ordering::SeqCst) == CALLS as usize,
    )
    .await;
    assert!(
        conn.inflight_count() >= 65,
        "one blocked frame and a full 64-entry queue must retain drain ownership"
    );
    assert!(
        server.pending_requests.available_permits() <= 128 - 65,
        "pending admission covers replies waiting for the writer"
    );
    let business_error = c2_wire::control::encode_call_control(&call_identity("grid"), 1).unwrap();
    peer.write_all(&encode_frame(
        101,
        c2_wire::flags::FLAG_CALL_V2,
        &business_error,
    ))
    .await
    .unwrap();
    let capacity_error =
        c2_wire::control::encode_call_control(&call_identity("limited"), 0).unwrap();
    peer.write_all(&encode_frame(
        102,
        c2_wire::flags::FLAG_CALL_V2,
        &capacity_error,
    ))
    .await
    .unwrap();
    peer.write_all(&encode_frame(
        103,
        FLAG_SIGNAL,
        &c2_wire::msg_type::PING_BYTES,
    ))
    .await
    .unwrap();
    wait_until(
        "business error dispatched behind the full writer queue",
        || calls.load(Ordering::SeqCst) == CALLS as usize + 1,
    )
    .await;
    assert!(!conn.is_deactivated());
    let mut received = std::collections::HashSet::new();
    for _ in 0..CALLS + 3 {
        let (id, flags, payload) = read_reply_frame(&mut peer).await;
        assert!(received.insert(id), "one outcome per accepted call");
        if id == 103 {
            assert_eq!(flags, FLAG_RESPONSE | FLAG_SIGNAL);
            assert_eq!(payload, c2_wire::msg_type::PONG_BYTES);
        } else {
            let (control, consumed) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
            if id <= CALLS {
                assert!(matches!(control, ReplyControl::Success));
                assert_eq!(payload.len() - consumed, BYTES);
                assert!(payload[consumed..].iter().all(|byte| *byte == 0x51));
            } else {
                let ReplyControl::Error(wire) = control else {
                    panic!("expected call-specific error for {id}");
                };
                assert!(String::from_utf8_lossy(&wire).contains(if id == 101 {
                    "business failed"
                } else {
                    "max_pending"
                }));
            }
        }
    }
    assert!(
        !conn.is_deactivated(),
        "queue backpressure must preserve the physical connection"
    );
    peer.write_all(&encode_frame(
        104,
        FLAG_SIGNAL,
        &c2_wire::msg_type::PING_BYTES,
    ))
    .await
    .unwrap();
    assert_eq!(read_reply_frame(&mut peer).await.0, 104);
    wait_until("reply drain returns all pending permits", || {
        server.pending_requests.available_permits() == 128
    })
    .await;
    drop(occupied);
    drop(peer);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_release_deactivates_new_work_but_preserves_active_owned_replies() {
    for (invalid_id, size) in [(0, 4), (u64::MAX, 4 * 1024 * 1024)] {
        let mut f = ShutdownReplyFixture::start(vec![0x73; size]).await;
        let release = c2_wire::release::ReleaseBatch::new(&[invalid_id]).unwrap();
        f.peer
            .write_all(&c2_wire::release::encode_release_frame(&release, false))
            .await
            .unwrap();
        wait_until("invalid release deactivates business admission", || {
            f.conn.is_deactivated()
        })
        .await;
        assert!(!f.completed.load(Ordering::SeqCst));
        assert!(
            f.conn.connection_observation().snapshot().is_none(),
            "accepted callback retains response access"
        );
        f.release();
        let mut received = Vec::new();
        loop {
            let (id, flags, payload) = read_reply_frame(&mut f.peer).await;
            assert_eq!(id, 1);
            if c2_wire::flags::is_chunked(flags) {
                let (_, _, _, used) =
                    c2_wire::chunk::header::decode_reply_chunk_meta(&payload, 0).unwrap();
                received.extend_from_slice(&payload[used..]);
                if c2_wire::flags::is_chunk_last(flags) {
                    break;
                }
            } else {
                let (control, used) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
                assert!(matches!(control, ReplyControl::Success));
                received.extend_from_slice(&payload[used..]);
                break;
            }
        }
        assert_eq!(received, vec![0x73; size]);
        f.server.request_shutdown_signal();
        f.runner.take().unwrap().await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn rejected_acquire_releases_materialized_request_on_its_original_connection() {
    use c2_wire::connection::{ConnectionCore, ConnectionSide, WriteRequest};
    use std::num::NonZeroUsize;
    for len in [128, 65537] {
        let config = PoolConfig {
            segment_size: 65536,
            min_block_size: 128,
            max_segments: 1,
            max_dedicated_segments: 1,
            min_retained_segments: 0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            ..Default::default()
        };
        let owner = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix(config.clone(), unique_response_pool_prefix("rejected"))
                .unwrap(),
        ));
        let budget = owner.read().budget().unwrap().clone();
        let baseline = budget.snapshot();
        let mut handle = owner.write().alloc_shm_handle(len).unwrap();
        owner.read().handle_slice_mut(&mut handle).fill(0x41);
        let allocation = handle.shm_allocation().unwrap();
        assert_eq!(allocation.is_dedicated, len > 65536);
        let peer_core = ConnectionCore::new(ConnectionSide::Client, || {});
        peer_core.finish_handshake().unwrap();
        let loan = peer_core.prepare_loan(owner.clone(), handle).unwrap();
        let id = loan.id();
        let mut batch = peer_core
            .begin_write(WriteRequest::Business(vec![loan]))
            .unwrap()
            .unwrap();
        batch.begin_io().unwrap();
        batch.frame_completed();
        batch.flush_completed();
        let (stream, mut peer) = isolated_pair().await;
        let conn = Arc::new(Connection::new(71));
        conn.install_remote_read_guard(stream.retain_for_remote_read());
        conn.set_handshake_done(true).unwrap();
        conn.init_peer_shm(owner.read().prefix().into(), 0.0)
            .unwrap();
        let (reader, half) = stream.into_split();
        let (_writer, driver) = ConnectionWriter::spawn(
            half,
            conn.clone(),
            Duration::from_secs(30),
            Arc::new(Semaphore::new(16)),
        );
        let request = admit_peer_request(
            &conn,
            BuddyPayload {
                seg_idx: allocation.seg_idx as u16,
                generation: allocation.generation,
                offset: allocation.offset,
                data_size: len as u32,
                is_dedicated: allocation.is_dedicated,
                loan_id: id,
            },
            &budget,
        )
        .unwrap();
        let scheduler = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: None,
            },
        );
        let pending = scheduler.reserve_pending(0).unwrap();
        scheduler.close();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invoked = called.clone();
        let error = execute_route_request(
            Scheduler::new(ConcurrencyMode::Parallel, HashMap::new()),
            pending,
            &conn,
            request,
            move |_| {
                invoked.store(true, Ordering::SeqCst);
                Ok(ResponseMeta::Empty)
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            RouteExecutionError::Acquire(SchedulerAcquireError::Closed)
        ));
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(scheduler.snapshot().pending, 0);
        let (rid, flags, payload) = read_reply_frame(&mut peer).await;
        let frame = encode_frame(rid, flags, &payload);
        let (header, payload) = c2_wire::frame::decode_frame(&frame).unwrap();
        assert_eq!(
            c2_wire::release::decode_release_frame(&header, payload, true, true)
                .unwrap()
                .ids(),
            &[id]
        );
        let released = peer_core.receive_release(&header, payload);
        assert!(released.error.is_none());
        drop(released.reclaims);
        assert_eq!(owner.read().stats().alloc_count, 0);
        owner.write().gc_buddy();
        let after = budget.snapshot();
        assert_eq!(after.shm.used_bytes, baseline.shm.used_bytes);
        assert_eq!(after.file.used_bytes, baseline.file.used_bytes);
        assert_eq!(after.reassembly.used_bytes, baseline.reassembly.used_bytes);
        let cache = conn.peer_mappings().unwrap();
        if !allocation.is_dedicated {
            cache.gc_idle();
        }
        assert_eq!(cache.mapped_segments(), 0);
        conn.end(ConnectionEndReason::LocalClose, "test_done");
        driver.await.unwrap();
        drop(reader);
        conn.clear_remote_read_guard();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_overlapped_flush_peer_close_keeps_buddy_and_dedicated_allocations() {
    for len in [8192, 65537] {
        let pool = Arc::new(crate::ResponsePool::new(
            MemPool::new_with_prefix(
                PoolConfig {
                    segment_size: 65536,
                    max_segments: 1,
                    max_dedicated_segments: 2,
                    ..Default::default()
                },
                unique_response_pool_prefix("overlapped"),
            )
            .unwrap(),
        ));
        let prepared = try_prepare_shm_response(&pool, 1024, len, |dst| {
            dst.fill(7);
            Ok(())
        })
        .unwrap()
        .unwrap();
        let original = prepared.shm_allocation().unwrap();
        assert_eq!(original.is_dedicated, len > 65536);
        let (peer, stream) = isolated_pair().await;
        let conn = Arc::new(Connection::new(73));
        conn.set_handshake_done(true).unwrap();
        conn.install_remote_read_guard(stream.retain_for_remote_read());
        let (reader, mut half) = stream.into_split();
        // Fill Tokio's 64 KiB pipe plus Mio's posted 4 KiB read-ahead. This
        // completes natively before publishing the response's descriptor.
        tokio::time::timeout(
            Duration::from_secs(5),
            half.write_all(&vec![0; 65536 + 4096]),
        )
        .await
        .unwrap()
        .unwrap();
        let (writer, driver) = ConnectionWriter::spawn(
            half,
            conn.clone(),
            Duration::from_secs(30),
            Arc::new(Semaphore::new(16)),
        );
        let mut send = Box::pin(send_prepared_shm_response(&writer, 73, prepared, 64 * 1024));
        tokio::select! {
            result = &mut send => panic!("full non-reading Named Pipe must keep response pending: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
        drop(peer);
        let error = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, ResponseSendError::Transport(_)));
        assert_eq!(
            pool.read().stats().alloc_count,
            1,
            "write failure alone cannot reclaim a possibly read allocation"
        );
        let next = pool.inner.write().alloc_shm_handle(len).unwrap();
        let replacement = next.shm_allocation().unwrap();
        assert!(
            replacement.is_dedicated != original.is_dedicated
                || replacement.seg_idx != original.seg_idx
                || replacement.generation != original.generation
                || replacement.offset != original.offset,
            "unsettled peer-visible coordinates must not be reused"
        );
        pool.inner.write().release_handle(next);
        // Only the read side's native EOF authorizes the outstanding loan's reclaim.
        let mut reader = reader;
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), reader.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(reader.peer_ended());
        conn.peer_end();
        assert_eq!(pool.read().stats().alloc_count, 0);
        driver.await.unwrap();
        conn.clear_remote_read_guard();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_keeps_listener_until_timed_out_response_writer_finishes() {
    const SIZE: usize = 512 * 1024;
    let mut config = ServerIpcConfig::default();
    config.base.write_timeout_secs = 0.01;
    config.shm_threshold = 1024 * 1024;
    config.base.chunk_size = 1024 * 1024;
    let mut f =
        ShutdownReplyFixture::start_with_config(vec![0x46; SIZE], false, false, config).await;
    f.server.request_shutdown_signal();
    f.release();
    wait_until("configured response write timeout elapsed", || {
        f.conn.is_deactivated()
    })
    .await;
    assert!(
        f.server
            .wait_until_stopped(Duration::from_millis(30))
            .await
            .is_err(),
        "a timed-out waiter cannot end the native response flush or close the listener"
    );
    assert!(f.server.endpoint_credential().is_some());
    let (id, _, payload) = read_reply_frame(&mut f.peer).await;
    assert_eq!(id, 1);
    let (control, used) = c2_wire::control::decode_reply_control(&payload, 0).unwrap();
    assert!(matches!(control, ReplyControl::Success));
    assert_eq!(payload[used..], vec![0x46; SIZE]);
    f.runner.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn deactivated_connection_returns_unpublished_shm_response_as_owned_reply() {
    for len in [8192, 65537] {
        let pool = small_response_pool("deactivated");
        let prepared = try_prepare_shm_response(&pool, 1024, len, |dst| {
            dst.fill(0x67);
            Ok(())
        })
        .unwrap()
        .unwrap();
        let (stream, mut peer) = isolated_pair().await;
        let (reader, half) = stream.into_split();
        let conn = Arc::new(Connection::new(91));
        conn.set_handshake_done(true).unwrap();
        let flight = crate::connection::FlightGuard::new(&conn);
        conn.end(ConnectionEndReason::ProtocolError, "invalid_release");
        let (writer, driver) = ConnectionWriter::spawn(
            half,
            conn.clone(),
            Duration::from_secs(30),
            Arc::new(Semaphore::new(16)),
        );
        let send = send_prepared_shm_response(&writer, 91, prepared, 16384);
        let receive = async {
            let mut bytes = Vec::new();
            loop {
                let (id, flags, payload) = read_reply_frame(&mut peer).await;
                assert_eq!(id, 91);
                assert!(
                    !c2_wire::flags::is_buddy(flags),
                    "deactivation forbids a new SHM loan"
                );
                if c2_wire::flags::is_chunked(flags) {
                    let (_, _, _, used) =
                        c2_wire::chunk::header::decode_reply_chunk_meta(&payload, 0).unwrap();
                    bytes.extend_from_slice(&payload[used..]);
                    if c2_wire::flags::is_chunk_last(flags) {
                        break;
                    }
                } else {
                    let (reply, used) =
                        c2_wire::control::decode_reply_control(&payload, 0).unwrap();
                    assert!(matches!(reply, ReplyControl::Success));
                    bytes.extend_from_slice(&payload[used..]);
                    break;
                }
            }
            bytes
        };
        let (sent, bytes) = tokio::join!(send, receive);
        sent.unwrap();
        assert_eq!(bytes, vec![0x67; len]);
        assert_eq!(pool.read().stats().alloc_count, 0);
        drop(flight);
        driver.await.unwrap();
        drop(reader);
    }
}

#[tokio::test]
async fn idle_peer_request_views_retire_while_shutdown_keeps_an_unreleased_response() {
    for len in [8192, 65537] {
        let mut config = ServerIpcConfig::default();
        config.pool_decay_seconds = 0.03;
        config.base.chunk_gc_interval_secs = 0.005;
        config.base.pool_segment_size = 65536;
        config.base.max_pool_segments = 1;
        config.shm_threshold = 1024;
        let server =
            Arc::new(test_server_new(&unique_readiness_address("idle_peer_gc"), config).unwrap());
        let mut route = make_host_route(&server, "grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        server.register_route(route).await.unwrap();
        let running = server.clone();
        let runner = tokio::spawn(async move { running.run().await });
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        let mut pool = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 65536,
                max_segments: 1,
                max_dedicated_segments: 1,
                ..Default::default()
            },
            unique_response_pool_prefix("idlepeer"),
        )
        .unwrap();
        let mut input = pool.alloc_shm_handle(len).unwrap();
        pool.handle_slice_mut(&mut input).fill(0x23);
        let allocation = input.shm_allocation().unwrap();
        let descriptor = BuddyPayload {
            seg_idx: allocation.seg_idx as u16,
            generation: allocation.generation,
            offset: allocation.offset,
            data_size: len as u32,
            is_dedicated: allocation.is_dedicated,
            loan_id: 19,
        };
        let mut peer = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .unwrap();
        handshake_test_client_with(&mut peer, CAP_SHM_REQUEST | CAP_SHM_RESPONSE, pool.prefix())
            .await;
        let conn = server
            .active_connections
            .lock()
            .values()
            .next()
            .unwrap()
            .clone();
        let cache = conn.peer_mappings().unwrap();
        let mut payload = encode_buddy_payload(&descriptor).to_vec();
        payload.extend_from_slice(
            &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
        );
        peer.write_all(&encode_frame(
            1,
            c2_wire::flags::FLAG_CALL_V2 | FLAG_BUDDY,
            &payload,
        ))
        .await
        .unwrap();
        assert_eq!(read_release_id(&mut peer).await, 19);
        pool.release_handle(input);
        let (id, flags, response) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        assert!(c2_wire::flags::is_buddy(flags));
        let (response, _) = decode_buddy_payload(&response).unwrap();
        assert_eq!(response.data_size as usize, len);
        assert_eq!(response.is_dedicated, len > 65536);
        server.request_shutdown_signal();
        runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
        assert_eq!(
            server.active_connection_count(),
            1,
            "unreleased response keeps the original driver"
        );
        wait_until(
            "maintenance retires idle request views without another call",
            || cache.mapped_segments() == 0,
        )
        .await;
        let release = c2_wire::release::ReleaseBatch::new(&[response.loan_id]).unwrap();
        peer.write_all(&c2_wire::release::encode_release_frame(&release, false))
            .await
            .unwrap();
        drop(peer);
        tokio::time::timeout(Duration::from_secs(2), server.wait_connections_settled())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn zero_business_pending_budget_keeps_control_and_capacity_replies() {
    let mut config = ServerIpcConfig::default();
    config.max_pending_requests = 0;
    let server =
        Arc::new(test_server_new(&unique_readiness_address("zero_pending"), config).unwrap());
    server
        .register_route(make_host_route(&server, "grid"))
        .await
        .unwrap();
    let running = server.clone();
    let runner = tokio::spawn(async move { running.run().await });
    server
        .wait_until_responsive(Duration::from_millis(100))
        .await
        .unwrap();
    let mut peer = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    handshake_test_client(&mut peer).await;
    let call = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
    peer.write_all(&encode_frame(1, c2_wire::flags::FLAG_CALL_V2, &call))
        .await
        .unwrap();
    let (id, _, payload) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 1);
    let ReplyControl::Error(error) = c2_wire::control::decode_reply_control(&payload, 0)
        .unwrap()
        .0
    else {
        panic!("zero business budget must reject the call");
    };
    assert!(String::from_utf8_lossy(&error).contains("max_pending_requests=0"));
    peer.write_all(&encode_frame(
        2,
        FLAG_SIGNAL,
        &c2_wire::msg_type::PING_BYTES,
    ))
    .await
    .unwrap();
    assert_eq!(read_reply_frame(&mut peer).await.0, 2);
    let conn = server
        .active_connections
        .lock()
        .values()
        .find(|conn| conn.handshake_done())
        .unwrap()
        .clone();
    assert!(!conn.is_deactivated());
    drop(peer);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn deactivation_keeps_calls_waiting_for_route_or_global_execution() {
    for route_wait in [true, false] {
        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..Default::default()
        };
        let server = Arc::new(test_server_new("ipc://deactivation_waiter", config).unwrap());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut route = make_host_route(&server, "grid");
        route.scheduler = Arc::new(Scheduler::with_admission(
            ConcurrencyMode::Exclusive,
            HashMap::new(),
            SchedulerLimits::try_from_usize(None, Some(1)).unwrap(),
            server.admission.clone(),
        ));
        route.callback = Arc::new(EchoRequestBytes {
            calls: calls.clone(),
        });
        let scheduler = route.scheduler.clone();
        server.register_route(route).await.unwrap();
        let capacity = if route_wait {
            scheduler.try_acquire(0).unwrap()
        } else {
            server.execution_scheduler.try_acquire(0).unwrap()
        };
        let (mut peer, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(server.clone(), stream));
        handshake_test_client(&mut peer).await;
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        let mut payload = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
        payload.extend_from_slice(b"queued");
        peer.write_all(&encode_frame(1, c2_wire::flags::FLAG_CALL_V2, &payload))
            .await
            .unwrap();
        wait_until("accepted call queued", || {
            conn.inflight_count() == 1
                && scheduler.snapshot().pending == if route_wait { 2 } else { 1 }
        })
        .await;
        conn.end(ConnectionEndReason::LocalClose, "test_deactivation");
        peer.write_all(&encode_frame(2, c2_wire::flags::FLAG_CALL_V2, &payload))
            .await
            .unwrap();
        let (id, _, reply) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 2, "the accepted call has no premature error");
        assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
        assert!(reply_error_message(&reply).contains("not executed"));
        assert_eq!(conn.inflight_count(), 1);
        drop(capacity);
        let (id, _, reply) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        let (control, used) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
        assert!(matches!(control, ReplyControl::Success));
        assert_eq!(&reply[used..], b"queued");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(peer);
        handler.await.unwrap();
    }
}

#[tokio::test]
async fn deactivation_and_shutdown_preserve_started_chunked_calls() {
    for shutdown in [false, true] {
        for parked_admission in [false, true] {
            let server = ordering_test_server("ipc://deactivated_chunks", 8);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut route = make_host_route(&server, "grid");
            route.callback = Arc::new(EchoRequestBytes {
                calls: calls.clone(),
            });
            server.register_route(route).await.unwrap();
            let (mut peer, stream) = isolated_pair().await;
            let handler = tokio::spawn(handle_connection(server.clone(), stream));
            handshake_test_client(&mut peer).await;
            let conn = server
                .active_connection(server.active_connection_ids()[0])
                .unwrap();
            let gate = if parked_admission {
                Some(server.dispatcher.write().await)
            } else {
                None
            };
            peer.write_all(&chunked_call_frame(1, 0, 2, b"abcd", "grid"))
                .await
                .unwrap();
            wait_until("first chunk accepted", || {
                if parked_admission {
                    server.chunk_admission_gate.len() == 1
                } else {
                    server.chunk_registry.contains(conn.conn_id(), 1)
                }
            })
            .await;
            if shutdown {
                server.request_shutdown_signal();
            } else {
                conn.end(ConnectionEndReason::ProtocolError, "test_deactivation");
            }
            let inline = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
            peer.write_all(&encode_frame(2, c2_wire::flags::FLAG_CALL_V2, &inline))
                .await
                .unwrap();
            peer.write_all(&chunked_call_frame(3, 0, 2, b"new!", "grid"))
                .await
                .unwrap();
            let mut refused = HashSet::new();
            for _ in 0..2 {
                let (id, _, reply) = read_reply_frame(&mut peer).await;
                assert!([2, 3].contains(&id));
                assert!(refused.insert(id));
                assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
                assert!(reply_error_message(&reply).contains("not executed"));
            }
            peer.write_all(&chunked_call_frame(1, 1, 2, b"EFGH", "grid"))
                .await
                .unwrap();
            drop(gate);
            let (id, _, reply) = read_reply_frame(&mut peer).await;
            assert_eq!(id, 1);
            let (control, used) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
            assert!(matches!(control, ReplyControl::Success), "{control:?}");
            assert_eq!(&reply[used..], b"abcdEFGH");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(server.chunk_registry.active_count(), 0);
            drop(peer);
            handler.await.unwrap();
        }
    }
}

#[tokio::test]
async fn shutdown_preserves_accepted_calls_behind_dispatcher_route_and_global_gates() {
    for stage in ["dispatcher", "route", "global"] {
        let config = ServerIpcConfig {
            max_execution_workers: 1,
            ..Default::default()
        };
        let server = Arc::new(
            test_server_new(&unique_readiness_address("shutdown_waiter"), config).unwrap(),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut route = make_host_route(&server, "grid");
        route.scheduler = Arc::new(Scheduler::with_admission(
            ConcurrencyMode::Exclusive,
            HashMap::new(),
            SchedulerLimits::try_from_usize(None, Some(1)).unwrap(),
            server.admission.clone(),
        ));
        route.callback = Arc::new(EchoRequestBytes {
            calls: calls.clone(),
        });
        let scheduler = route.scheduler.clone();
        server.register_route(route).await.unwrap();
        let running = server.clone();
        let runner = tokio::spawn(async move { running.run().await });
        server
            .wait_until_ready(Duration::from_secs(2))
            .await
            .unwrap();
        let mut peer = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
            .await
            .unwrap();
        handshake_test_client(&mut peer).await;
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        let dispatcher = if stage == "dispatcher" {
            Some(server.dispatcher.write().await)
        } else {
            None
        };
        let capacity = match stage {
            "route" => Some(scheduler.try_acquire(0).unwrap()),
            "global" => Some(server.execution_scheduler.try_acquire(0).unwrap()),
            _ => None,
        };
        let payload = c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
        peer.write_all(&encode_frame(1, c2_wire::flags::FLAG_CALL_V2, &payload))
            .await
            .unwrap();
        wait_until("accepted call parks before execution", || {
            conn.inflight_count() == 1
                && (stage == "dispatcher"
                    || scheduler.snapshot().pending == if stage == "route" { 2 } else { 1 })
        })
        .await;
        assert!(
            server
                .shutdown_and_wait(Duration::from_millis(30))
                .await
                .is_err()
        );
        assert!(!runner.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            endpoint_connects(server.local_endpoint()).await,
            "listener stays until accepted work drains"
        );
        drop(dispatcher);
        drop(capacity);
        let (id, _, reply) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        assert!(matches!(
            c2_wire::control::decode_reply_control(&reply, 0).unwrap().0,
            ReplyControl::Success
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runner.await.unwrap().unwrap();
        assert_eq!(server.lifecycle_state(), ServerLifecycleState::Stopped);
    }
}

#[tokio::test]
async fn response_write_buffer_capacity_failure_reports_executed_output_error() {
    let mut f = ShutdownReplyFixture::start(b"executed".to_vec()).await;
    f.conn
        .fail_next_write_allocation
        .store(true, Ordering::Release);
    f.release();
    let (id, _, reply) = read_reply_frame(&mut f.peer).await;
    assert_eq!(id, 1);
    assert!(f.completed.load(Ordering::SeqCst));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reply_error_code(&reply),
        ErrorCode::ResourceOutputSerializing
    );
    assert!(reply_error_message(&reply).contains("write buffer allocation failure"));
    f.server.request_shutdown_signal();
    f.runner.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn buddy_chunk_continuations_survive_deactivation_and_shutdown() {
    for shutdown in [false, true] {
        for parked_admission in [false, true] {
            let server = ordering_test_server("ipc://deactivated_buddy_chunks", 8);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut route = make_host_route(&server, "grid");
            route.callback = Arc::new(EchoRequestBytes {
                calls: calls.clone(),
            });
            server.register_route(route).await.unwrap();
            let mut pool = MemPool::new_with_prefix(
                PoolConfig {
                    segment_size: 64 * 1024,
                    min_block_size: 4096,
                    max_segments: 1,
                    ..Default::default()
                },
                unique_response_pool_prefix("drainingchunks"),
            )
            .unwrap();
            let (mut peer, stream) = isolated_pair().await;
            let handler = tokio::spawn(handle_connection(server.clone(), stream));
            let ack = handshake_test_client_with(&mut peer, CAP_SHM_REQUEST, pool.prefix()).await;
            assert_ne!(ack.capability_flags & CAP_SHM_REQUEST, 0);
            let conn = server
                .active_connection(server.active_connection_ids()[0])
                .unwrap();
            let gate = if parked_admission {
                Some(server.dispatcher.write().await)
            } else {
                None
            };
            let mut gate = gate;
            for (index, bytes) in [b"abcd", b"EFGH"].into_iter().enumerate() {
                let mut handle = pool.alloc_shm_handle(bytes.len()).unwrap();
                pool.handle_slice_mut(&mut handle).copy_from_slice(bytes);
                let allocation = handle.shm_allocation().unwrap();
                let loan_id = 101 + index as u64;
                let mut payload = encode_buddy_payload(&BuddyPayload {
                    seg_idx: allocation.seg_idx as u16,
                    generation: allocation.generation,
                    offset: allocation.offset,
                    data_size: bytes.len() as u32,
                    is_dedicated: allocation.is_dedicated,
                    loan_id,
                })
                .to_vec();
                payload.extend_from_slice(&c2_wire::chunk::header::encode_chunk_header(
                    index as u16,
                    2,
                ));
                if index == 0 {
                    payload.extend_from_slice(
                        &c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap(),
                    );
                }
                let flags = c2_wire::flags::FLAG_CALL_V2
                    | FLAG_CHUNKED
                    | FLAG_BUDDY
                    | if index == 1 { FLAG_CHUNK_LAST } else { 0 };
                peer.write_all(&encode_frame(1, flags, &payload))
                    .await
                    .unwrap();
                if index == 1 {
                    drop(gate.take());
                }
                assert_eq!(read_release_id(&mut peer).await, loan_id);
                pool.release_handle(handle);
                assert_eq!(pool.stats().alloc_count, 0);
                if index == 0 {
                    wait_until("buddy first chunk accepted", || {
                        if parked_admission {
                            server.chunk_admission_gate.len() == 1
                        } else {
                            server.chunk_registry.contains(conn.conn_id(), 1)
                        }
                    })
                    .await;
                    if shutdown {
                        server.request_shutdown_signal();
                    } else {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "test_buddy_deactivation",
                        );
                    }
                    let new_call =
                        c2_wire::control::encode_call_control(&call_identity("grid"), 0).unwrap();
                    peer.write_all(&encode_frame(2, c2_wire::flags::FLAG_CALL_V2, &new_call))
                        .await
                        .unwrap();
                    let (id, _, reply) = read_reply_frame(&mut peer).await;
                    assert_eq!(id, 2);
                    assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
                }
            }
            let (id, _, reply) = read_reply_frame(&mut peer).await;
            assert_eq!(id, 1);
            let (control, used) = c2_wire::control::decode_reply_control(&reply, 0).unwrap();
            assert!(matches!(control, ReplyControl::Success), "{control:?}");
            assert_eq!(&reply[used..], b"abcdEFGH");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(peer);
            handler.await.unwrap();
        }
    }
}
