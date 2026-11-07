use super::*;
use crate::test_support::isolated_pair;
use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};
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
async fn false_shutdown_updates_cannot_truncate_chunked_handshake() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("chunked_start"),
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
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
    assert_eq!(server.response_pool.read().segment_count(), 0);
    let handle = reassembly.write().alloc_shm_handle(4096).unwrap();
    reassembly.write().release_handle(handle);
    assert_eq!(observer.snapshot().shm.used_bytes, 64 * 1024);
    let runner = {
        let server = server.clone();
        tokio::spawn(async move { server.run().await })
    };
    server
        .wait_until_ready(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(observer.snapshot().shm.used_bytes, 2 * 64 * 1024);
    let generation = server.response_pool.read().segment_generation(0).unwrap();
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
        stream_callback: None,
        method_modes: vec![CallMode::Unary; 2],
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
    server.dispatcher.read().resolve(name).unwrap()
}

async fn admission_poll_once<F: std::future::Future>(
    mut future: std::pin::Pin<&mut F>,
) -> std::task::Poll<F::Output> {
    std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test]
async fn reserved_route_pending_cannot_activate_after_generation_change() {
    let fixture = admission_fixture();
    let server = &fixture.server;
    let route = admission_route(server, "grid", ConcurrencyMode::Parallel).await;
    let mut identity = call_identity("grid");
    identity.route_uid = route.route_uid.clone();
    let request = reserve_accepted_route_execution(server, &identity, 0, CallMode::Unary)
        .ok()
        .expect("route reservation");
    assert_eq!(route.scheduler.snapshot().pending, 1);
    {
        let mut admission = server.admission.lock();
        admission.terminate();
        admission.begin_start();
    }
    server.admission.notify.notify_waiters();
    assert!(matches!(
        request.pending_permit.async_activate().await,
        Err(SchedulerAcquireError::Unavailable)
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
    let old_call = conn.calls.lock().open(1, ServerCall::default()).unwrap();
    assert!(old_call.queue());
    let pending = route.scheduler.reserve_pending(0).unwrap();
    let mut request = Box::pin(execute_route_request(
        server.execution_scheduler.clone(),
        pending,
        &conn,
        1,
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
    let next_call = conn.calls.lock().open(2, ServerCall::default()).unwrap();
    assert!(next_call.queue());
    let pending = route.scheduler.reserve_pending(0).unwrap();
    execute_route_request(
        server.execution_scheduler.clone(),
        pending,
        &conn,
        2,
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
        reserve_accepted_route_execution(server, &identity, 0, CallMode::Unary),
        Err(RouteAdmissionError::Acquire(SchedulerAcquireError::Closed))
    ));
    assert!(matches!(
        reserve_accepted_route_execution(server, &identity, 99, CallMode::Unary),
        Err(RouteAdmissionError::UnknownMethod { .. })
    ));
    let mut wrong = identity.clone();
    wrong.route_uid.push_str("-stale");
    assert!(matches!(
        reserve_accepted_route_execution(server, &wrong, 99, CallMode::Unary),
        Err(RouteAdmissionError::RouteStale { .. })
    ));
    wrong = identity;
    wrong.abi_hash = "0".repeat(64);
    assert!(matches!(
        reserve_accepted_route_execution(server, &wrong, 99, CallMode::Unary),
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

fn call_identity(name: &str) -> c2_wire::rpc::RouteCallIdentity {
    c2_wire::rpc::RouteCallIdentity {
        route_name: name.into(),
        route_uid: format!("{name}-uid-0001"),
        crm_ns: "test.grid".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
    }
}

#[derive(Debug)]
enum TestReply {
    Success,
    Error(Vec<u8>),
}

/// Inspect the IPC 16 unary terminal body after the frame envelope has been read.
fn decode_test_call_end(payload: &[u8], offset: usize) -> Result<(TestReply, usize), String> {
    let body = payload
        .get(offset..)
        .ok_or_else(|| "CALL_END offset exceeds body".to_string())?;
    if body.len() < 2 || body[0] != c2_wire::rpc::RpcRecordKind::CallEnd as u8 {
        return Err("expected CALL_END kind and status".into());
    }
    match c2_wire::rpc::CallStatus::try_from(body[1]).map_err(|e| e.to_string())? {
        c2_wire::rpc::CallStatus::Success => Ok((TestReply::Success, offset + 2)),
        c2_wire::rpc::CallStatus::Failed | c2_wire::rpc::CallStatus::NotExecuted => {
            let bytes = &body[2..];
            let error = C2Error::from_wire_bytes(bytes)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "empty CALL_END error".to_string())?;
            let _ = error;
            Ok((TestReply::Error(bytes.to_vec()), offset + 2))
        }
    }
}

fn assert_call_end_status(payload: &[u8], expected: c2_wire::rpc::CallStatus) {
    assert_eq!(
        payload.first().copied(),
        Some(c2_wire::rpc::RpcRecordKind::CallEnd as u8)
    );
    assert_eq!(payload.get(1).copied(), Some(expected as u8));
}

fn inline_success_part(flags: u32, payload: &[u8]) -> (Option<u64>, &[u8]) {
    use c2_wire::flags::{FLAG_CHUNKED, FLAG_FIRST_CHUNK, FLAG_RPC_RECORD};
    assert_eq!(
        flags & (FLAG_RPC_RECORD | FLAG_RESPONSE | FLAG_BUDDY),
        FLAG_RPC_RECORD | FLAG_RESPONSE
    );
    if flags & FLAG_CHUNKED == 0 {
        assert_call_end_status(payload, c2_wire::rpc::CallStatus::Success);
        return (Some((payload.len() - 2) as u64), &payload[2..]);
    }
    if flags & FLAG_FIRST_CHUNK != 0 {
        assert_call_end_status(payload, c2_wire::rpc::CallStatus::Success);
        assert!(payload.len() >= 11);
        let total = u64::from_le_bytes(payload[2..10].try_into().unwrap());
        (Some(total), &payload[10..])
    } else {
        assert_eq!(
            payload.first().copied(),
            Some(c2_wire::rpc::RpcRecordKind::CallEnd as u8)
        );
        (None, &payload[1..])
    }
}

#[tokio::test]
async fn register_unregister_route() {
    let s = Arc::new(test_server_new("ipc://reg_test", ServerIpcConfig::default()).unwrap());

    let route = make_host_route(&s, "grid");
    let scheduler = route.scheduler.as_ref().clone();
    s.register_route(route).await.unwrap();
    assert!(s.dispatcher.read().resolve("grid").is_some());
    assert!(s.contains_route("grid").await);

    assert!(s.unregister_route("grid").await);
    assert!(s.dispatcher.read().resolve("grid").is_none());
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

async fn check_route_open_interrupted_at_registration(close_admission: bool) {
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

    // Poll the actual open operation with registration held after its
    // initial lifecycle check.
    let registration = server.route_registration.lock().await;
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
    drop(registration);

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
        "open must be parked at route registration"
    );
    assert!(
        interrupted,
        "the real close/signal must precede registration release"
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
async fn route_open_rechecks_admission_closure_after_registration_wait() {
    check_route_open_interrupted_at_registration(true).await;
}

#[tokio::test]
async fn route_open_rechecks_shutdown_after_registration_wait() {
    check_route_open_interrupted_at_registration(false).await;
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
    assert!(s.dispatcher.read().resolve("overflow").is_none());
}

#[tokio::test]
async fn inline_dispatch_rejects_unknown_method_index_before_callback() {
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

    let frame = c2_wire::rpc::encode_open_frame(
        42,
        &call_identity("grid"),
        99,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(&[])),
    )
    .unwrap();
    let (request_id, _flags, reply_payload) = call_over_connection(&server, &frame).await;

    assert_eq!(request_id, 42);
    assert_call_end_status(&reply_payload, c2_wire::rpc::CallStatus::NotExecuted);
    match decode_test_call_end(&reply_payload, 0).unwrap().0 {
        TestReply::Error(err) => {
            let message = String::from_utf8_lossy(&err);
            assert!(message.contains("unknown method index 99"));
        }
        other => panic!("expected method-index error reply, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn same_error_code_distinguishes_admission_from_resource_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingResource(Arc<AtomicUsize>);
    impl CrmCallback for FailingResource {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(CrmError::UserError(error_wire(
                ErrorCode::ResourceFunctionExecuting,
                "resource failed after entry",
            )))
        }
    }

    let server = Arc::new(
        test_server_new("ipc://same_error_code_status", ServerIpcConfig::default()).unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(FailingResource(Arc::clone(&calls)));
    server.register_route(route).await.unwrap();

    let refused = c2_wire::rpc::encode_open_frame(
        1,
        &call_identity("grid"),
        99,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(&[])),
    )
    .unwrap();
    let (_, _, refused_reply) = call_over_connection(&server, &refused).await;
    assert_call_end_status(&refused_reply, c2_wire::rpc::CallStatus::NotExecuted);
    assert_eq!(
        reply_error_code(&refused_reply),
        ErrorCode::ResourceFunctionExecuting
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let valid = c2_wire::rpc::encode_open_frame(
        2,
        &call_identity("grid"),
        0,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(&[])),
    )
    .unwrap();
    let (_, _, failed_reply) = call_over_connection(&server, &valid).await;
    assert_call_end_status(&failed_reply, c2_wire::rpc::CallStatus::Failed);
    assert_eq!(
        reply_error_code(&failed_reply),
        ErrorCode::ResourceFunctionExecuting
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn inline_dispatch_rejects_stale_route_token_before_callback() {
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

    let frame = c2_wire::rpc::encode_open_frame(
        42,
        &identity,
        0,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(&[])),
    )
    .unwrap();
    let (request_id, _flags, reply_payload) = call_over_connection(&server, &frame).await;

    assert_eq!(request_id, 42);
    match decode_test_call_end(&reply_payload, 0).unwrap().0 {
        TestReply::Error(err) => {
            let decoded = C2Error::from_wire_bytes(&err)
                .unwrap()
                .expect("route stale must be encoded as a C2 error");
            assert_eq!(decoded.code, ErrorCode::RouteStale);
            assert!(decoded.message.contains("stale route token"));
            assert!(decoded.message.contains("retired-grid-uid"));
            assert_eq!(
                decoded.details.get("route").map(String::as_str),
                Some("grid")
            );
            assert_eq!(
                decoded.details.get("route_uid").map(String::as_str),
                Some("retired-grid-uid")
            );
            assert_eq!(
                decoded.details.get("current_route_uid").map(String::as_str),
                Some(call_identity("grid").route_uid.as_str())
            );
        }
        other => panic!("expected route-stale error reply, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_callbacks_respect_server_wide_execution_limit_across_routes() {
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

    let payload_a = inline_call_frame(1, "grid_a", &[]);
    let payload_b = inline_call_frame(2, "grid_b", &[]);

    let first = {
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            call_over_connection(&server, &payload_a).await;
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
            call_over_connection(&server, &payload_b).await;
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

        let payload_a = inline_call_frame(1, "grid_a", &[]);
        let payload_b = inline_call_frame(3, "grid_b", &[]);

        let first_a = {
            let server = Arc::clone(&server);
            let payload = payload_a.clone();
            tokio::spawn(async move {
                call_over_connection(&server, &payload).await;
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
            let payload = inline_call_frame(2, "grid_a", &[]);
            tokio::spawn(async move {
                call_over_connection(&server, &payload).await;
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
                call_over_connection(&server, &payload_b).await;
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

        let payload_a = inline_call_frame(1, "grid_a", &[]);
        let payload_b = inline_call_frame(2, "grid_b", &[]);

        let first = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                call_over_connection(&server, &payload_a).await;
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
                call_over_connection(&server, &payload_b).await;
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
    let frame = inline_call_frame(1, "grid", &[]);
    client.write_all(&frame).await.unwrap();

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

    let payload_a = inline_call_frame(1, "grid_a", &[]);
    let payload_b = inline_call_frame(2, "grid_b", &[]);

    let first = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            call_over_connection(&server, &payload_a).await;
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
            call_over_connection(&server, &payload_b).await;
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
            let frame = inline_call_frame(rid, route, &[]);
            peer.as_mut().unwrap().write_all(&frame).await.unwrap();
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

    let payload_a = inline_call_frame(1, "grid_a", &[]);
    let payload_b = inline_call_frame(2, "grid_b", &[]);

    let first = {
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            call_over_connection(&server, &payload_a).await;
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
            let (_, _, reply_payload) = call_over_connection(&server, &payload_b).await;
            let message = match decode_test_call_end(&reply_payload, 0).unwrap().0 {
                TestReply::Error(err) => String::from_utf8_lossy(&err).to_string(),
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
    drop(client);
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
fn chunk_processing_permit_is_bounded_by_max_inflight_chunk_frames() {
    let config = ServerIpcConfig {
        base: c2_config::BaseIpcConfig {
            max_inflight_chunk_frames: 1,
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

// -- OPEN metadata precedes SHM storage --

#[test]
fn open_metadata_precedes_shm_descriptor() {
    use c2_wire::rpc::{RpcRecord, ValuePart, ValueStorage};

    let bp = BuddyPayload {
        seg_idx: 0,
        generation: 1,
        offset: 4096,
        data_size: 256,
        is_dedicated: false,
        loan_id: 1,
    };
    let frame = c2_wire::rpc::encode_open_frame(
        1,
        &call_identity("grid"),
        1,
        c2_wire::rpc::CallMode::Unary,
        ValuePart::whole(ValueStorage::Shm(bp)),
    )
    .unwrap();
    let (header, payload) = c2_wire::frame::decode_frame(&frame).unwrap();
    let (_, _, _, storage_offset) = c2_wire::rpc::decode_open_metadata(payload).unwrap();
    assert_eq!(&payload[storage_offset..], &encode_buddy_payload(&bp));
    match c2_wire::rpc::decode_rpc_record(&header, payload, None).unwrap() {
        RpcRecord::Open {
            identity,
            method_idx,
            value,
            ..
        } => {
            assert_eq!(identity.route_name, "grid");
            assert_eq!(method_idx, 1);
            assert_eq!(value.storage, ValueStorage::Shm(bp));
        }
        other => panic!("expected OPEN, got {other:?}"),
    }
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
pub(super) async fn read_reply_frame(client: &mut LocalStream) -> (u64, u32, Vec<u8>) {
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
pub(super) async fn handshake_test_client(client: &mut LocalStream) {
    let ack = handshake_test_client_with(client, 0, "").await;
    assert!(ack.server_identity.is_some());
}

pub(super) async fn handshake_test_client_with(
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
async fn standalone_shutdown_ack_survives_delayed_read_until_peer_closes() {
    for already_stopping in [false, true] {
        let server = Arc::new(
            test_server_new(
                &unique_readiness_address("shutdown_delayed_ack"),
                ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        if already_stopping {
            server.request_shutdown_signal();
        }
        let (mut client, stream) = isolated_pair().await;
        let mut handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        client
            .write_all(&encode_frame(
                17,
                FLAG_SIGNAL,
                &c2_wire::shutdown_control::encode_shutdown_initiate(),
            ))
            .await
            .unwrap();
        // Keep the client read parked beyond the retired 50 ms courtesy window.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut handler)
                .await
                .is_err(),
            "the ACK stream must survive until its control peer consumes it and closes"
        );
        let (id, flags, payload) = read_reply_frame(&mut client).await;
        assert_eq!(id, 17);
        assert_eq!(flags, FLAG_SIGNAL | FLAG_RESPONSE);
        assert_eq!(payload, c2_wire::shutdown_control::encode_shutdown_ack());
        assert!(
            !handler.is_finished(),
            "reading the ACK alone is not peer close"
        );
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server.active_connection_count(), 0);
    }
}

#[tokio::test]
async fn standalone_shutdown_ack_peer_wait_uses_configured_write_budget() {
    for already_stopping in [false, true] {
        let server = Arc::new(
            test_server_new(
                &unique_readiness_address("shutdown_ack_budget"),
                ServerIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        write_timeout_secs: 0.05,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        if already_stopping {
            server.request_shutdown_signal();
        }
        let (mut client, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        client
            .write_all(&encode_frame(
                18,
                FLAG_SIGNAL,
                &c2_wire::shutdown_control::encode_shutdown_initiate(),
            ))
            .await
            .unwrap();
        assert_eq!(read_reply_frame(&mut client).await.0, 18);
        // A control peer retaining its stream cannot extend the configured
        // ACK budget, and receipt of the ACK alone does not prove shutdown.
        tokio::time::timeout(Duration::from_secs(2), handler)
            .await
            .expect("control peer wait respects the existing write budget")
            .unwrap();
        drop(client);
    }
}

#[tokio::test]
async fn disconnect_cause_precedes_failed_ack_when_receive_loop_is_paused() {
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("disconnect_failed_ack"),
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
    let observation = conn.connection_observation();
    let (entered, paused) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    *conn.signal_reply_pause.lock() = Some((entered, resumed));
    client
        .write_all(&encode_frame(
            0,
            FLAG_SIGNAL,
            &c2_wire::msg_type::DISCONNECT_BYTES,
        ))
        .await
        .unwrap();
    paused.await.unwrap();
    // The receive loop has decoded DISCONNECT. Its ACK must now fail against
    // a closed peer, and that writer result is observed before the loop resumes.
    drop(client);
    resume.send(()).unwrap();
    let closed = observation
        .wait_closed(Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::PeerCloseMessage);
    assert_eq!(closed.operation(), "disconnect");
    handler.await.unwrap();
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
    assert_eq!(flags, c2_wire::flags::FLAG_RPC_RECORD | FLAG_RESPONSE);
    let frame = encode_frame(request_id, flags, &payload);
    let (header, body) = c2_wire::frame::decode_frame(&frame).unwrap();
    let batch = c2_wire::release::decode_release_frame(&header, body, true, true).unwrap();
    assert_eq!(batch.ids().len(), 1);
    batch.ids()[0]
}

#[tokio::test]
async fn raw_buddy_request_releases_before_blocked_callback_completes() {
    use c2_wire::buddy::BuddyPayload;

    struct BlockingEcho {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }
    // Lets a failed assertion end the test instead of leaving the callback
    // blocked, which would make runtime shutdown wait forever.
    struct ReleaseOnDrop(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let (lock, changed) = &*self.0;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
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
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
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
    let (started, callback_started) = tokio::sync::oneshot::channel();
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release_callback = ReleaseOnDrop(Arc::clone(&release));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(BlockingEcho {
        started: std::sync::Mutex::new(Some(started)),
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
    client
        .write_all(&shm_call_frame(41, "grid", descriptor))
        .await
        .unwrap();

    assert_eq!(read_release_id(&mut client).await, loan_id);
    // The release is written before the resource method runs, so wait for
    // the callback to start instead of assuming it already has.
    tokio::time::timeout(Duration::from_secs(5), callback_started)
        .await
        .expect("callback did not start")
        .unwrap();
    assert_eq!(pool.stats().alloc_count, 1);
    pool.release_handle(handle);
    assert_eq!(pool.stats().alloc_count, 0);
    drop(release_callback);
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 41);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    let (control, consumed) = decode_test_call_end(&payload, 0).unwrap();
    assert!(matches!(control, TestReply::Success));
    assert_eq!(&payload[consumed..], b"callback done");
    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn raw_buddy_mapping_failure_still_queues_release() {
    use c2_wire::buddy::BuddyPayload;
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
    client
        .write_all(&shm_call_frame(42, "grid", descriptor))
        .await
        .unwrap();

    assert_eq!(read_release_id(&mut client).await, loan_id);
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 42);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    let (control, _) = decode_test_call_end(&payload, 0).unwrap();
    assert!(matches!(control, TestReply::Error(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    pool.release_handle(handle);
    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

async fn raw_response_loan_settlement(release_frame: bool, shutdown_before_release: bool) {
    use c2_wire::buddy::decode_buddy_payload;
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
    client
        .write_all(&inline_call_frame(91, "grid", &expected))
        .await
        .unwrap();
    let (request_id, flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(request_id, 91);
    assert_ne!(flags & FLAG_RESPONSE, 0);
    assert_ne!(flags & FLAG_BUDDY, 0);
    let (control, consumed) = decode_test_call_end(&payload, 0).unwrap();
    assert!(matches!(control, TestReply::Success));
    let (descriptor, _) = decode_buddy_payload(&payload[consumed..]).unwrap();

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
    let call = conn.calls.lock().open(93, ServerCall::default()).unwrap();
    assert!(call.queue());
    assert!(call.enter_resource());
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

async fn call_over_connection(server: &Arc<Server>, frame: &[u8]) -> (u64, u32, Vec<u8>) {
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(server), stream));
    handshake_test_client(&mut client).await;
    client.write_all(frame).await.unwrap();
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
    let good = inline_call_frame(41, "grid", b"accepted");
    client.write_all(&good).await.unwrap();
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
        if flags == c2_wire::flags::FLAG_RPC_RECORD | FLAG_RESPONSE && id == 0 {
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
    assert_call_end_status(&reply, c2_wire::rpc::CallStatus::NotExecuted);
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
        .write_all(&inline_call_frame(43, "grid", b"accepted"))
        .await
        .unwrap();
    if deactivates {
        let (id, _, reply) = read_reply_frame(&mut client).await;
        assert_eq!(id, 43);
        assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
        assert_call_end_status(&reply, c2_wire::rpc::CallStatus::NotExecuted);
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
        let (control, consumed) = decode_test_call_end(&reply, 0).unwrap();
        assert!(matches!(control, TestReply::Success));
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
    let TestReply::Error(error) = decode_test_call_end(payload, 0).unwrap().0 else {
        panic!("expected a correlated error reply");
    };
    C2Error::from_wire_bytes(&error).unwrap().unwrap().code
}

fn bad_call_buddy_payload(loan_id: u64) -> Vec<u8> {
    let mut payload = inline_call_frame(42, "grid", &[])[c2_wire::frame::HEADER_SIZE..].to_vec();
    payload.extend_from_slice(&encode_buddy_payload(&BuddyPayload {
        seg_idx: 0,
        generation: 1,
        offset: 4096,
        data_size: 16,
        is_dedicated: false,
        loan_id,
    }));
    payload
}

#[tokio::test]
async fn malformed_inline_call_control_only_fails_its_call() {
    let mut payload = inline_call_frame(42, "grid", &[])[c2_wire::frame::HEADER_SIZE..].to_vec();
    payload[2] = 0xff; // invalid UTF-8 in the first identity field
    assert_bad_call_isolated(c2_wire::flags::FLAG_RPC_RECORD, &payload, false).await;
}

#[tokio::test]
async fn malformed_buddy_payload_deactivates_but_preserves_accepted_call() {
    let mut payload = bad_call_buddy_payload(1);
    let descriptor_at = payload.len() - c2_wire::buddy::BUDDY_PAYLOAD_SIZE;
    payload[descriptor_at + 14] = 0x80;
    assert_bad_call_isolated(c2_wire::flags::FLAG_RPC_RECORD | FLAG_BUDDY, &payload, true).await;
}

#[tokio::test]
async fn zero_buddy_loan_deactivates_but_preserves_accepted_call() {
    let payload = bad_call_buddy_payload(0);
    assert_bad_call_isolated(c2_wire::flags::FLAG_RPC_RECORD | FLAG_BUDDY, &payload, true).await;
}

#[tokio::test]
async fn buddy_mapping_open_failure_only_fails_its_call() {
    let payload = bad_call_buddy_payload(1);
    assert_bad_call_isolated(
        c2_wire::flags::FLAG_RPC_RECORD | FLAG_BUDDY,
        &payload,
        false,
    )
    .await;
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
    value_chunk_call_frame(
        request_id,
        chunk_idx,
        u64::from(total_chunks) * data.len() as u64,
        data,
        identity,
    )
}

fn value_chunk_call_frame(
    request_id: u64,
    chunk_idx: u16,
    total_bytes: u64,
    data: &[u8],
    identity: &str,
) -> Vec<u8> {
    use c2_wire::rpc::{
        RpcRecordKind, ValuePart, ValueStorage, encode_open_frame, encode_value_continuation_frame,
    };
    if chunk_idx == 0 {
        let value = if total_bytes == data.len() as u64 {
            ValuePart::whole(ValueStorage::Inline(data))
        } else {
            ValuePart::first(total_bytes, ValueStorage::Inline(data))
        };
        encode_open_frame(
            request_id,
            &call_identity(identity),
            0,
            c2_wire::rpc::CallMode::Unary,
            value,
        )
        .unwrap()
    } else {
        encode_value_continuation_frame(request_id, RpcRecordKind::Open, ValueStorage::Inline(data))
            .unwrap()
    }
}

fn chunk_test_server(address: &str, max_inflight_chunk_frames: u32) -> Arc<Server> {
    let mut config = ServerIpcConfig::default();
    config.base.max_inflight_chunk_frames = max_inflight_chunk_frames;
    Arc::new(test_server_new(address, config).unwrap())
}

async fn assert_early_chunk_refusal_preserves_connection(processing_capacity: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut config = ServerIpcConfig::default();
    config.base.max_inflight_chunk_frames = 2;
    config.base.max_reassembly_bytes = if processing_capacity { 1024 } else { 64 };
    config.base.chunk_assembler_timeout_secs = 0.02;
    let server = Arc::new(test_server_new("ipc://early_chunk_refusal", config).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: Arc::clone(&calls),
    });
    server.register_route(route).await.unwrap();

    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    let occupied = if processing_capacity {
        Some(
            server
                .chunk_processing_permits
                .clone()
                .try_acquire_many_owned(2)
                .unwrap(),
        )
    } else {
        None
    };
    client
        .write_all(&chunked_call_frame(61, 0, 6, &[0x11; 32], "grid"))
        .await
        .unwrap();
    let (id, flags, reply) = read_reply_frame(&mut client).await;
    assert_eq!(id, 61);
    assert_eq!(flags, c2_wire::flags::FLAG_RPC_RECORD | FLAG_RESPONSE);
    assert_call_end_status(&reply, c2_wire::rpc::CallStatus::NotExecuted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(occupied);

    // The refused record has no retained tombstone. High-water rejects its
    // later chunks even after the old assembler timeout and GC pass.
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.chunk_registry.gc_sweep();
    let mut frames = chunked_call_frame(61, 1, 6, &[0x22; 32], "grid");
    frames.extend(chunked_call_frame(61, 5, 6, &[0x55; 32], "grid"));
    frames.extend(encode_frame(7, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES));
    client.write_all(&frames).await.unwrap();
    let (barrier_id, _, barrier) = read_reply_frame(&mut client).await;
    assert_eq!(
        barrier_id, 7,
        "late parts must not claim another terminal reply"
    );
    assert_eq!(barrier, PONG_BYTES);

    client
        .write_all(&inline_call_frame(62, "grid", b"next"))
        .await
        .unwrap();
    let (next_id, _, next) = read_reply_frame(&mut client).await;
    assert_eq!(next_id, 62);
    assert_call_end_status(&next, c2_wire::rpc::CallStatus::Success);
    assert_eq!(&next[2..], b"next");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(server.chunk_registry.active_count(), 0);
    assert_eq!(server.chunk_processing_permits.available_permits(), 2);
    drop(client);
    handler.await.unwrap();
}

#[tokio::test]
async fn reassembly_size_refusal_discards_late_chunks_and_preserves_connection() {
    assert_early_chunk_refusal_preserves_connection(false).await;
}

#[tokio::test]
async fn chunk_processing_refusal_discards_late_chunks_and_preserves_connection() {
    assert_early_chunk_refusal_preserves_connection(true).await;
}

#[tokio::test]
async fn gc_late_chunk_still_reports_to_the_waiting_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut config = ServerIpcConfig::default();
    config.base.chunk_assembler_timeout_secs = 0.05;
    config.base.chunk_gc_interval_secs = 0.005;
    let server =
        Arc::new(test_server_new(&unique_readiness_address("gc_late_call_probe"), config).unwrap());
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
    let mut client = LocalStream::connect(server.local_endpoint(), DEFAULT_CONNECT_TIMEOUT)
        .await
        .unwrap();
    handshake_test_client(&mut client).await;

    client
        .write_all(&chunked_call_frame(61, 0, 4, &[7; 32], "grid"))
        .await
        .unwrap();
    wait_until("first chunk registered", || {
        server.chunk_registry.active_count() == 1
    })
    .await;
    let (id, _, reply) = read_reply_frame(&mut client).await;
    assert_eq!(
        id, 61,
        "GC must settle the original call without waiting for a late part"
    );
    assert_call_end_status(&reply, c2_wire::rpc::CallStatus::NotExecuted);
    assert_eq!(server.chunk_registry.active_count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut frames = chunked_call_frame(61, 1, 4, &[8; 32], "grid");
    frames.extend(chunked_call_frame(61, 3, 4, &[10; 32], "grid"));
    frames.extend(encode_frame(9, FLAG_SIGNAL, &c2_wire::msg_type::PING_BYTES));
    client.write_all(&frames).await.unwrap();
    let (barrier_id, _, barrier) = read_reply_frame(&mut client).await;
    assert_eq!(
        barrier_id, 9,
        "late parts must not produce a second terminal"
    );
    assert_eq!(barrier, PONG_BYTES);

    client
        .write_all(&inline_call_frame(62, "grid", b"after-gc"))
        .await
        .unwrap();
    let (next_id, _, next) = read_reply_frame(&mut client).await;
    assert_eq!(next_id, 62);
    assert_call_end_status(&next, c2_wire::rpc::CallStatus::Success);
    assert_eq!(&next[2..], b"after-gc");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn future_continuation_is_rejected_at_protocol_boundary() {
    use c2_wire::rpc::{RpcRecordKind, ValueStorage, encode_value_continuation_frame};
    let server = chunk_test_server("ipc://future_chunk_refuse", 2);
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    let conn = server
        .active_connection(server.active_connection_ids()[0])
        .unwrap();
    let future =
        encode_value_continuation_frame(9, RpcRecordKind::Open, ValueStorage::Inline(b"orphan"))
            .unwrap();
    client.write_all(&future).await.unwrap();
    wait_until("future continuation ends admission", || {
        conn.is_deactivated()
    })
    .await;
    assert_eq!(server.chunk_registry.active_count(), 0);
    drop(client);
    handler.await.unwrap();
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
    assert!(u32::from(TOTAL_CHUNKS) > config.max_inflight_chunk_frames);
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
                    == server.config.max_inflight_chunk_frames as usize
            })
            .await;
        }
    }
    let (rid, _, reply) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 42);
    let (control, consumed) = decode_test_call_end(&reply, 0).unwrap();
    assert!(matches!(control, TestReply::Success), "{control:?}");
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
            .write_all(&value_chunk_call_frame(42, chunk as u16, 9, data, "grid"))
            .await
            .unwrap();
    }
    let (_, _, reply) = read_reply_frame(&mut client).await;
    let (control, _) = decode_test_call_end(&reply, 0).unwrap();
    let TestReply::Error(error) = control else {
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

/// A first chunk reserves its route before a later unary frame can reserve it.
/// Holding the real reassembly pool pauses only backing allocation; PING proves
/// the receive loop still reaches the later unary admission.
#[tokio::test]
async fn first_chunk_write_precedes_later_unary_on_same_connection() {
    use std::sync::Mutex;

    struct RecordOrder(Arc<Mutex<Vec<u16>>>);
    impl CrmCallback for RecordOrder {
        fn invoke(
            &self,
            _: &str,
            method_idx: u16,
            request: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.0.lock().unwrap().push(method_idx);
            Ok(ResponseMeta::Inline(request))
        }
    }

    for mode in [ConcurrencyMode::Exclusive, ConcurrencyMode::ReadParallel] {
        let server = chunk_test_server("ipc://first_chunk_before_unary", 3);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut route = make_host_route(&server, "grid");
        route.scheduler = Arc::new(Scheduler::with_admission(
            mode,
            HashMap::from([
                (0, crate::scheduler::AccessLevel::Write),
                (1, crate::scheduler::AccessLevel::Read),
            ]),
            SchedulerLimits::default(),
            Arc::clone(&server.admission),
        ));
        route.callback = Arc::new(RecordOrder(Arc::clone(&order)));
        let scheduler = Arc::clone(&route.scheduler);
        server.register_route(route).await.unwrap();
        let (mut client, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
        handshake_test_client(&mut client).await;
        let pool = server.chunk_registry.pool();
        let backing = pool.write();
        client
            .write_all(&value_chunk_call_frame(1, 0, 11, b"first-", "grid"))
            .await
            .unwrap();
        let unary = c2_wire::rpc::encode_open_frame(
            2,
            &call_identity("grid"),
            1,
            c2_wire::rpc::CallMode::Unary,
            c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(b"read")),
        )
        .unwrap();
        client.write_all(&unary).await.unwrap();
        client
            .write_all(&encode_frame(
                3,
                FLAG_SIGNAL,
                &c2_wire::msg_type::PING_BYTES,
            ))
            .await
            .unwrap();
        let (id, _, payload) = read_reply_frame(&mut client).await;
        assert_eq!((id, payload.as_slice()), (3, PONG_BYTES.as_slice()));
        assert_eq!(scheduler.snapshot().pending, 2);
        assert!(order.lock().unwrap().is_empty());
        drop(backing);
        client
            .write_all(&chunked_call_frame(1, 1, 2, b"write", "grid"))
            .await
            .unwrap();
        let mut replies = HashMap::new();
        for _ in 0..2 {
            let (id, _, payload) = read_reply_frame(&mut client).await;
            replies.insert(id, payload);
        }
        for (id, bytes) in [(1, b"first-write".as_slice()), (2, b"read".as_slice())] {
            let (control, used) = decode_test_call_end(&replies[&id], 0).unwrap();
            assert!(
                matches!(control, TestReply::Success),
                "request {id}: {control:?}"
            );
            assert_eq!(&replies[&id][used..], bytes);
        }
        assert_eq!(*order.lock().unwrap(), [0, 1]);
        assert_eq!(server.chunk_registry.active_count(), 0);
        drop(client);
        handler.await.unwrap();
    }
}

#[tokio::test]
async fn incomplete_first_chunk_holds_server_pending_capacity() {
    let mut config = ServerIpcConfig::default();
    config.max_pending_requests = 1;
    let server = Arc::new(test_server_new("ipc://chunk_pending_capacity", config).unwrap());
    server
        .register_route(make_host_route(&server, "grid"))
        .await
        .unwrap();
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;
    let pool = server.chunk_registry.pool();
    let backing = pool.write();
    client
        .write_all(&chunked_call_frame(1, 0, 2, b"first", "grid"))
        .await
        .unwrap();
    client
        .write_all(&encode_frame(
            3,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .unwrap();
    assert_eq!(read_reply_frame(&mut client).await.0, 3);
    assert_eq!(server.pending_requests.available_permits(), 0);
    client
        .write_all(&inline_call_frame(2, "grid", b"second"))
        .await
        .unwrap();
    let (id, _, reply) = read_reply_frame(&mut client).await;
    assert_eq!(id, 2);
    assert!(reply_error_message(&reply).contains("max_pending_requests=1"));
    drop(backing);
    client
        .write_all(&chunked_call_frame(1, 1, 2, b"-done", "grid"))
        .await
        .unwrap();
    let (id, _, reply) = read_reply_frame(&mut client).await;
    assert_eq!(id, 1);
    assert!(matches!(
        decode_test_call_end(&reply, 0).unwrap().0,
        TestReply::Success
    ));
    wait_until("chunk pending slot returned", || {
        server.pending_requests.available_permits() == 1
    })
    .await;
    drop(client);
    handler.await.unwrap();
}

#[tokio::test]
async fn pending_registration_does_not_delay_reader_route_reservations() {
    let server = chunk_test_server("ipc://registration_and_reader", 3);
    let route = make_host_route(&server, "grid");
    let scheduler = Arc::clone(&route.scheduler);
    server.register_route(route).await.unwrap();
    let (mut client, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(Arc::clone(&server), stream));
    handshake_test_client(&mut client).await;

    let registration = server.route_registration.lock().await;
    let mut pending_registration =
        Box::pin(server.register_route(make_host_route(&server, "other")));
    assert!(
        admission_poll_once(pending_registration.as_mut())
            .await
            .is_pending()
    );
    let pool = server.chunk_registry.pool();
    let backing = pool.write();
    client
        .write_all(&chunked_call_frame(1, 0, 2, b"first", "grid"))
        .await
        .unwrap();
    client
        .write_all(&inline_call_frame(2, "grid", b"unary"))
        .await
        .unwrap();
    client
        .write_all(&encode_frame(
            3,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .unwrap();
    let (id, _, payload) = read_reply_frame(&mut client).await;
    assert_eq!((id, payload.as_slice()), (3, PONG_BYTES.as_slice()));
    assert_eq!(scheduler.snapshot().pending, 2);
    drop(backing);
    drop(registration);
    pending_registration.await.unwrap();
    client
        .write_all(&chunked_call_frame(1, 1, 2, b"-done", "grid"))
        .await
        .unwrap();
    let mut replies = HashSet::new();
    for _ in 0..2 {
        let (id, _, reply) = read_reply_frame(&mut client).await;
        assert!(replies.insert(id));
        assert!(matches!(
            decode_test_call_end(&reply, 0).unwrap().0,
            TestReply::Success
        ));
    }
    assert_eq!(replies, HashSet::from([1, 2]));
    drop(client);
    handler.await.unwrap();
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
        let server = chunk_test_server("ipc://shm_ack_intersection", 8);
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
    let server = chunk_test_server("ipc://shm_reachability_error", 8);
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
    shm_call_frame(
        request_id,
        "grid",
        BuddyPayload {
            seg_idx: 0,
            generation: 7,
            offset: 4096,
            data_size: 4096,
            is_dedicated: false,
            loan_id: 4242,
        },
    )
}

/// An inbound buddy request frame on a connection that never negotiated
/// the SHM request direction is refused with a correlated protocol error
/// before any admission, lazy-open, or cleanup could touch a mapping, and
/// the connection itself stays healthy.
#[tokio::test]
async fn unnegotiated_buddy_request_is_refused_before_any_mapping() {
    let server = chunk_test_server("ipc://shm_buddy_refuse", 8);
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
    let (control, _) = decode_test_call_end(&payload, 0).unwrap();
    match control {
        TestReply::Error(bytes) => {
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
    let (control, consumed) = decode_test_call_end(&payload, 0).unwrap();
    match control {
        TestReply::Success => {}
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
    let server = chunk_test_server("ipc://shm_buddy_chunk_refuse", 8);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut client, handler) = unnegotiated_shm_connection(&server, Arc::clone(&calls)).await;

    // A first OPEN chunk carries metadata, total value length, then SHM storage.
    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 3,
        offset: 0,
        data_size: 128,
        is_dedicated: true,
        loan_id: 1,
    };
    client
        .write_all(
            &c2_wire::rpc::encode_open_frame(
                44,
                &call_identity("grid"),
                0,
                c2_wire::rpc::CallMode::Unary,
                c2_wire::rpc::ValuePart::first(256, c2_wire::rpc::ValueStorage::Shm(descriptor)),
            )
            .unwrap(),
        )
        .await
        .expect("write un-negotiated buddy chunk frame");

    let (rid, flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 44);
    assert!(flags & c2_wire::flags::FLAG_BUDDY == 0);
    let (control, _) = decode_test_call_end(&reply_payload, 0).unwrap();
    match control {
        TestReply::Error(bytes) => {
            let error = C2Error::from_wire_bytes(&bytes)
                .unwrap()
                .expect("error payload");
            assert_eq!(error.code, ErrorCode::ProtocolViolation);
        }
        other => panic!("expected a protocol violation, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // No chunk admission was published for the refused request id.
    assert_eq!(server.chunk_registry.active_count(), 0);

    // A fully inline chunked request still completes on this connection.
    let data = b"inline-chunked-payload";
    client
        .write_all(&chunked_call_frame(45, 0, 1, data, "grid"))
        .await
        .expect("write inline chunked call");
    let (rid, _flags, payload) = read_reply_frame(&mut client).await;
    assert_eq!(rid, 45);
    let (control, consumed) = decode_test_call_end(&payload, 0).unwrap();
    match control {
        TestReply::Success => {}
        other => panic!("expected successful reply, got {other:?}"),
    }
    assert_eq!(&payload[consumed..], data);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    handler.await.expect("connection handler completes");
}

fn inline_call_frame(request_id: u64, identity: &str, data: &[u8]) -> Vec<u8> {
    c2_wire::rpc::encode_open_frame(
        request_id,
        &call_identity(identity),
        0,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(data)),
    )
    .unwrap()
}

fn shm_call_frame(request_id: u64, identity: &str, descriptor: BuddyPayload) -> Vec<u8> {
    c2_wire::rpc::encode_open_frame(
        request_id,
        &call_identity(identity),
        0,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Shm(descriptor)),
    )
    .unwrap()
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
            let call = conn.calls.lock().open(7, ServerCall::default()).unwrap();
            assert!(call.queue());
            assert!(call.enter_resource());
            let (writer, writer_task) = ConnectionWriter::spawn(
                half,
                Arc::clone(&conn),
                Duration::from_secs(30),
                Arc::new(Semaphore::new(1024)),
            );
            let receive = async {
                let mut assembled = Vec::new();
                let chunks = if size > 1024 { size.div_ceil(4096) } else { 1 };
                let mut expected_total = None;
                for _ in 0..chunks {
                    let (rid, flags, body) = read_reply_frame(&mut peer).await;
                    assert_eq!(rid, 7);
                    assert_eq!(flags & FLAG_BUDDY, 0, "no SHM coordinates");
                    assert_ne!(flags & FLAG_RESPONSE, 0);
                    let (total, data) = inline_success_part(flags, &body);
                    if let Some(total) = total {
                        expected_total = Some(total);
                    }
                    assembled.extend_from_slice(data);
                }
                assert_eq!(expected_total, Some(size as u64));
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

/// Route admission refusal settles a first chunk and its later continuation
/// with one correlated reply and no retained call record or permit.
#[tokio::test]
async fn route_refusal_settles_later_chunks_once_and_releases_state() {
    let server = chunk_test_server("ipc://chunked_ordering_refusal", 3);
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
    let (control, _) = decode_test_call_end(&reply_payload, 0).expect("CALL_END");
    match control {
        TestReply::Error(bytes) => {
            let error = C2Error::from_wire_bytes(&bytes).unwrap().unwrap();
            assert_eq!(error.code, ErrorCode::ResourceNotFound);
            assert!(error.message.contains("missing"));
        }
        other => panic!("expected structured route-not-found reply, got {other:?}"),
    }

    // The refused call record leaves no assembly or permit.
    let conn_id = server.active_connection_ids()[0];
    wait_until("call record released after refusal", || {
        server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 88));
    assert!(scheduler.try_acquire(0).is_ok());

    drop(client);
    handler.await.expect("connection handler completes");
}

/// Disconnect while the first chunk awaits real backing allocation must
/// release its call record and every chunk permit.
#[tokio::test]
async fn disconnect_cancels_parked_first_chunk_and_releases_permits() {
    let server = chunk_test_server("ipc://chunked_ordering_cancel", 3);
    let mut route = make_host_route(&server, "grid");
    route.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::Parallel,
        HashMap::new(),
        crate::scheduler::SchedulerLimits::try_from_usize(Some(1), Some(1)).unwrap(),
        Arc::clone(&server.admission),
    ));
    let scheduler = Arc::clone(&route.scheduler);
    server.register_route(route).await.unwrap();

    let (mut client, server_stream) = isolated_pair().await;
    let handler = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { handle_connection(server, server_stream).await })
    };
    handshake_test_client(&mut client).await;

    let pool = server.chunk_registry.pool();
    let backing = pool.write();
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
    wait_until("chunk frames parked behind backing allocation", || {
        server.chunk_processing_permits.available_permits() == 0
    })
    .await;
    let conn_id = server.active_connection_ids()[0];

    // Client disconnect cancels the call record while allocation is parked.
    drop(client);
    tokio::task::yield_now().await;
    assert_eq!(
        scheduler.snapshot().pending,
        1,
        "blocked original copy retains route admission"
    );
    assert_eq!(
        server.pending_requests.available_permits(),
        server.config.max_pending_requests as usize - 1,
        "blocked original copy retains its server call slot"
    );
    drop(backing);
    handler.await.expect("connection handler completes");

    wait_until("disconnect released call record and permits", || {
        server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 99));
    assert_eq!(scheduler.snapshot().pending, 0);
    assert_eq!(
        server.pending_requests.available_permits(),
        server.config.max_pending_requests as usize
    );
}

/// An incomplete chunked request that expires on the assembler timeout must
/// release its assembly charge and its stored route pending permit, leave
/// no call record behind, keep answering control frames, and still serve
/// a complete request afterwards with every byte delivered once.
#[tokio::test]
async fn assembler_timeout_releases_charge_and_route_capacity_then_serves_again() {
    use std::sync::atomic::AtomicUsize;

    let mut config = ServerIpcConfig::default();
    config.base.max_inflight_chunk_frames = 3;
    config.base.chunk_assembler_timeout_secs = 0.05;
    config.base.chunk_gc_interval_secs = 0.01;
    let server = Arc::new(
        test_server_new(
            &unique_readiness_address("chunked_ordering_timeout"),
            config,
        )
        .unwrap(),
    );
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

    // The real server GC loop publishes the original call's terminal receipt.
    let (expired_id, _, expired_reply) = read_reply_frame(&mut client).await;
    assert_eq!(expired_id, 55);
    assert_call_end_status(&expired_reply, c2_wire::rpc::CallStatus::NotExecuted);

    assert_eq!(server.chunk_registry.active_count(), 0);
    assert!(!server.chunk_registry.contains(conn_id, 55));
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
    let (control, consumed) = decode_test_call_end(&reply_payload, 0).expect("CALL_END");
    match control {
        TestReply::Success => {}
        other => panic!("expected successful reply after timeout cleanup, got {other:?}"),
    }
    assert_eq!(&reply_payload[consumed..], data.as_slice());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    server.request_shutdown_signal();
    runner.await.unwrap().unwrap();
}

// -- Reviewed server admission/ownership defects (recovery repair 2) --

/// Decode a reply payload's structured error message.
fn reply_error_message(payload: &[u8]) -> String {
    let (control, _) = decode_test_call_end(payload, 0).expect("CALL_END");
    match control {
        TestReply::Error(err) => String::from_utf8_lossy(&err).to_string(),
        other => panic!("expected a structured error reply, got {other:?}"),
    }
}

/// A repeated OPEN deactivates new admission while the first call keeps its
/// original input assembly and sole terminal outcome.
#[tokio::test]
async fn duplicate_open_preserves_original_assembly_and_only_terminal() {
    use std::sync::atomic::AtomicUsize;

    let server = chunk_test_server("ipc://chunked_duplicate_after_admission", 3);
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
    let conn = server.active_connection(conn_id).unwrap();
    assert!(
        conn.inflight_count() >= 1,
        "first OPEN retains connection flight"
    );
    assert!(
        conn.calls
            .lock()
            .get(82)
            .unwrap()
            .data
            .admission
            .lock()
            .is_some()
    );

    // Repeating OPEN cannot publish another call or terminal owner.
    client
        .write_all(&chunked_call_frame(82, 0, 2, &[0x22u8; 32], "grid"))
        .await
        .expect("write duplicate first chunk");
    wait_until("duplicate OPEN deactivates new admission", || {
        conn.is_deactivated()
    })
    .await;
    assert!(
        conn.inflight_count() >= 1,
        "duplicate must retain first OPEN flight"
    );
    assert!(
        conn.calls
            .lock()
            .get(82)
            .unwrap()
            .data
            .admission
            .lock()
            .is_some()
    );
    assert_eq!(
        server.chunk_registry.active_count(),
        1,
        "original assembly retained"
    );

    client
        .write_all(&encode_frame(
            9,
            FLAG_SIGNAL,
            &c2_wire::msg_type::PING_BYTES,
        ))
        .await
        .unwrap();
    let (barrier_id, _, barrier) = read_reply_frame(&mut client).await;
    assert_eq!(
        barrier_id, 9,
        "duplicate OPEN must not mint a terminal owner"
    );
    assert_eq!(barrier, PONG_BYTES);

    client
        .write_all(&chunked_call_frame(82, 1, 2, &[0x33u8; 32], "grid"))
        .await
        .expect("complete original input after duplicate");
    let (reply_rid, _, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 82);
    assert_call_end_status(&reply_payload, c2_wire::rpc::CallStatus::Success);
    assert_eq!(&reply_payload[2..32 + 2], &[0x21; 32]);
    assert_eq!(&reply_payload[32 + 2..], &[0x33; 32]);

    wait_until("original assembly released after completion", || {
        server.chunk_registry.active_count() == 0
            && server.chunk_processing_permits.available_permits() == 3
    })
    .await;
    assert!(!server.chunk_registry.contains(conn_id, 82));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(calls.load(Ordering::SeqCst), 1);

    drop(client);
    handler.await.expect("connection handler completes");
}

#[tokio::test]
async fn ping_is_served_while_chunk_processing_capacity_is_exhausted() {
    use std::sync::atomic::AtomicUsize;

    let server = chunk_test_server("ipc://chunked_ping_at_capacity", 1);
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

    let pool = server.chunk_registry.pool();
    let backing = pool.write();
    let data: Vec<u8> = (0..96u8).map(|i| i.wrapping_mul(7)).collect();
    client
        .write_all(&value_chunk_call_frame(71, 0, 96, &data[..48], "grid"))
        .await
        .expect("write first input chunk");
    wait_until("the only chunk-processing permit is held", || {
        server.chunk_processing_permits.available_permits() == 0
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

    // Release backing allocation: the same request completes with its bytes.
    drop(backing);
    wait_until("first input chunk finishes copying", || {
        server.chunk_processing_permits.available_permits() == 1
    })
    .await;
    client
        .write_all(&value_chunk_call_frame(71, 1, 96, &data[48..], "grid"))
        .await
        .unwrap();
    let (reply_rid, _flags, reply_payload) = read_reply_frame(&mut client).await;
    assert_eq!(reply_rid, 71);
    let (control, consumed) = decode_test_call_end(&reply_payload, 0).expect("CALL_END");
    match control {
        TestReply::Success => {}
        other => panic!("expected successful reply after capacity stall, got {other:?}"),
    }
    assert_eq!(&reply_payload[consumed..], data.as_slice());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(server.chunk_processing_permits.available_permits(), 1);
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
    let call = conn.calls.lock().open(7, ServerCall::default()).unwrap();
    assert!(call.queue());
    assert!(call.enter_resource());
    let (writer, writer_task) = ConnectionWriter::spawn(
        half,
        Arc::clone(&conn),
        Duration::from_secs(30),
        Arc::new(Semaphore::new(1024)),
    );
    let receive = async {
        let mut received = 0usize;
        for (idx, expected) in payload.chunks(4096).enumerate() {
            let (request_id, flags, body) = read_reply_frame(&mut peer).await;
            assert_eq!(request_id, 7);
            let (total, bytes) = inline_success_part(flags, &body);
            if idx == 0 {
                assert_eq!(total, Some(8193));
            } else {
                assert_eq!(total, None);
            }
            assert_eq!(bytes, expected);
            received += bytes.len();
        }
        assert_eq!(received, 8193);
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
    assert_eq!(snapshot.budget.shm.used_bytes, 0);
    assert_eq!(server.response_pool.read().segment_count(), 0);
    server.begin_start_attempt().unwrap();
    let snapshot = server.memory_budget_snapshot();
    assert_eq!(snapshot.budget.shm.used_bytes, 64 * 1024);

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
    let route = server.dispatcher.read().resolve("grid").unwrap();
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
async fn route_list_projects_host_admission_closed_while_route_is_open() {
    use c2_wire::route_catalog_control::{
        RouteListRequest, RouteSelector, RouteStateReasonWire, RouteStateWire,
        decode_route_list_response, encode_route_list_request,
    };

    let server = test_server_new(
        &unique_readiness_address("route_list_host_admission_closed"),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let route = make_host_route(&server, "grid");
    let expected_uid = route.route_uid.clone();
    let reservation = server
        .reserve_route(BuiltRoute::new(route).unwrap())
        .await
        .unwrap();
    server.commit_reserved_route(reservation).await.unwrap();

    assert_eq!(
        server
            .dispatcher
            .read()
            .resolve("grid")
            .unwrap()
            .scheduler
            .route_admission_state(),
        RouteAdmissionState::Opened
    );
    assert!(server.business_admission_closed() == false);

    // Close only the Host admission gate so this projection is checked before
    // per-route scheduler closure catches up.
    server.admission.lock().terminate();
    let request = encode_route_list_request(&RouteListRequest {
        selector: RouteSelector::RouteName {
            route_name: "grid".into(),
        },
    })
    .unwrap();
    let payload = route_list_payload(&server, &request).await;
    let response = decode_route_list_response(&payload).unwrap();
    assert_eq!(response.routes.len(), 1);
    assert_eq!(response.routes[0].route_uid, expected_uid);
    assert_eq!(response.routes[0].state, RouteStateWire::Closed);
    assert_eq!(
        response.routes[0].state_reason,
        RouteStateReasonWire::Shutdown
    );
    assert_eq!(
        server
            .dispatcher
            .read()
            .resolve("grid")
            .unwrap()
            .scheduler
            .route_admission_state(),
        RouteAdmissionState::Opened,
        "RouteList must report the Host gate without needing route mutation"
    );
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
                .write_all(&inline_call_frame(1, "grid", &[]))
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
        f.peer
            .write_all(&inline_call_frame(2, "grid", &[]))
            .await
            .unwrap();
        let (id, _, reply) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 2);
        assert!(matches!(
            decode_test_call_end(&reply, 0).unwrap().0,
            TestReply::Error(_)
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
        let (control, consumed) = decode_test_call_end(&reply, 0).unwrap();
        assert!(matches!(control, TestReply::Success));
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
    let mut total = None;
    loop {
        let (id, flags, payload) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 1);
        let (part_total, data) = inline_success_part(flags, &payload);
        if let Some(part_total) = part_total {
            total = Some(part_total);
        }
        assert!(data.iter().all(|byte| *byte == 0x5b));
        received += data.len();
        if received == SIZE {
            break;
        }
    }
    assert_eq!(received, SIZE);
    assert_eq!(total, Some(SIZE as u64));
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
    let (control, used) = decode_test_call_end(&reply, 0).unwrap();
    assert!(matches!(control, TestReply::Success));
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
    match decode_test_call_end(&reply, 0).unwrap().0 {
        TestReply::Error(error) => {
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
    for id in 1..=CALLS {
        peer.write_all(&inline_call_frame(id, "grid", &[]))
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
    let business_error = c2_wire::rpc::encode_open_frame(
        101,
        &call_identity("grid"),
        1,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::whole(c2_wire::rpc::ValueStorage::Inline(&[])),
    )
    .unwrap();
    peer.write_all(&business_error).await.unwrap();
    let capacity_error = inline_call_frame(102, "limited", &[]);
    peer.write_all(&capacity_error).await.unwrap();
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
            let (control, consumed) = decode_test_call_end(&payload, 0).unwrap();
            if id <= CALLS {
                assert!(matches!(control, TestReply::Success));
                assert_eq!(payload.len() - consumed, BYTES);
                assert!(payload[consumed..].iter().all(|byte| *byte == 0x51));
            } else {
                let TestReply::Error(wire) = control else {
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
        let mut total = None;
        loop {
            let (id, flags, payload) = read_reply_frame(&mut f.peer).await;
            assert_eq!(id, 1);
            let (part_total, data) = inline_success_part(flags, &payload);
            if let Some(part_total) = part_total {
                total = Some(part_total);
            }
            received.extend_from_slice(data);
            if received.len() == size {
                break;
            }
        }
        assert_eq!(total, Some(size as u64));
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
        let call = conn.calls.lock().open(1, ServerCall::default()).unwrap();
        assert!(call.queue());
        let invoked = called.clone();
        let error = execute_route_request(
            Scheduler::new(ConcurrencyMode::Parallel, HashMap::new()),
            pending,
            &conn,
            1,
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
        let call = conn.calls.lock().open(73, ServerCall::default()).unwrap();
        assert!(call.queue());
        assert!(call.enter_resource());
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
    let (control, used) = decode_test_call_end(&payload, 0).unwrap();
    assert!(matches!(control, TestReply::Success));
    assert_eq!(payload[used..], vec![0x46; SIZE]);
    f.runner.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_timeout_preserves_remaining_unary_call_end_chunks() {
    const SIZE: usize = 8 * 1024 * 1024;
    let mut config = ServerIpcConfig::default();
    config.base.write_timeout_secs = 0.01;
    config.shm_threshold = 1024;
    config.base.chunk_size = 256 * 1024;
    let mut f =
        ShutdownReplyFixture::start_with_config(vec![0x46; SIZE], false, false, config).await;
    f.server.request_shutdown_signal();
    f.release();
    // Withhold all reads until a submitted chunk exceeds the write deadline.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !f.conn.is_deactivated() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!f.conn.is_work_cancelled());
    let mut received = 0;
    let mut total = None;
    while received < SIZE {
        let (id, flags, payload) = read_reply_frame(&mut f.peer).await;
        assert_eq!(id, 1);
        let (part_total, data) = inline_success_part(flags, &payload);
        if let Some(part_total) = part_total {
            total = Some(part_total);
        }
        assert!(data.iter().all(|byte| *byte == 0x46));
        received += data.len();
    }
    assert_eq!(received, SIZE);
    assert_eq!(total, Some(SIZE as u64));
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
        let call = conn.calls.lock().open(91, ServerCall::default()).unwrap();
        assert!(call.queue());
        assert!(call.enter_resource());
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
            let mut total = None;
            loop {
                let (id, flags, payload) = read_reply_frame(&mut peer).await;
                assert_eq!(id, 91);
                assert!(
                    !c2_wire::flags::is_buddy(flags),
                    "deactivation forbids a new SHM loan"
                );
                let (part_total, data) = inline_success_part(flags, &payload);
                if let Some(part_total) = part_total {
                    total = Some(part_total);
                }
                bytes.extend_from_slice(data);
                if bytes.len() == len {
                    break;
                }
            }
            assert_eq!(total, Some(len as u64));
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
        peer.write_all(&shm_call_frame(1, "grid", descriptor))
            .await
            .unwrap();
        assert_eq!(read_release_id(&mut peer).await, 19);
        pool.release_handle(input);
        let (id, flags, response) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        assert!(c2_wire::flags::is_buddy(flags));
        assert_call_end_status(&response, c2_wire::rpc::CallStatus::Success);
        let (response, _) = c2_wire::buddy::decode_buddy_payload(&response[2..]).unwrap();
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
    let call = inline_call_frame(1, "grid", &[]);
    peer.write_all(&call).await.unwrap();
    let (id, _, payload) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 1);
    let TestReply::Error(error) = decode_test_call_end(&payload, 0).unwrap().0 else {
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
        let frame = inline_call_frame(1, "grid", b"queued");
        peer.write_all(&frame).await.unwrap();
        wait_until("accepted call queued", || {
            conn.inflight_count() == 1
                && scheduler.snapshot().pending == if route_wait { 2 } else { 1 }
        })
        .await;
        conn.end(ConnectionEndReason::LocalClose, "test_deactivation");
        peer.write_all(&inline_call_frame(2, "grid", b"queued"))
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
        let (control, used) = decode_test_call_end(&reply, 0).unwrap();
        assert!(matches!(control, TestReply::Success));
        assert_eq!(&reply[used..], b"queued");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(peer);
        handler.await.unwrap();
    }
}

#[tokio::test]
async fn deactivation_and_shutdown_preserve_started_chunked_calls() {
    for shutdown in [false, true] {
        for parked_allocation in [false, true] {
            let server = chunk_test_server("ipc://deactivated_chunks", 8);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut route = make_host_route(&server, "grid");
            route.callback = Arc::new(EchoRequestBytes {
                calls: calls.clone(),
            });
            let scheduler = Arc::clone(&route.scheduler);
            server.register_route(route).await.unwrap();
            let (mut peer, stream) = isolated_pair().await;
            let handler = tokio::spawn(handle_connection(server.clone(), stream));
            handshake_test_client(&mut peer).await;
            let conn = server
                .active_connection(server.active_connection_ids()[0])
                .unwrap();
            let reassembly_pool = server.chunk_registry.pool();
            let backing = parked_allocation.then(|| reassembly_pool.write());
            peer.write_all(&chunked_call_frame(1, 0, 2, b"abcd", "grid"))
                .await
                .unwrap();
            wait_until("first chunk accepted", || {
                if parked_allocation {
                    scheduler.snapshot().pending == 1
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
            let inline = inline_call_frame(2, "grid", &[]);
            peer.write_all(&inline).await.unwrap();
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
            drop(backing);
            let (id, _, reply) = read_reply_frame(&mut peer).await;
            assert_eq!(id, 1);
            let (control, used) = decode_test_call_end(&reply, 0).unwrap();
            assert!(matches!(control, TestReply::Success), "{control:?}");
            assert_eq!(&reply[used..], b"abcdEFGH");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(server.chunk_registry.active_count(), 0);
            drop(peer);
            handler.await.unwrap();
        }
    }
}

#[tokio::test]
async fn shutdown_preserves_accepted_calls_behind_route_and_global_gates() {
    for stage in ["route", "global"] {
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
        let capacity = match stage {
            "route" => Some(scheduler.try_acquire(0).unwrap()),
            "global" => Some(server.execution_scheduler.try_acquire(0).unwrap()),
            _ => None,
        };
        let frame = inline_call_frame(1, "grid", &[]);
        peer.write_all(&frame).await.unwrap();
        wait_until("accepted call parks before execution", || {
            conn.inflight_count() == 1
                && scheduler.snapshot().pending == if stage == "route" { 2 } else { 1 }
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
        drop(capacity);
        let (id, _, reply) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        assert!(matches!(
            decode_test_call_end(&reply, 0).unwrap().0,
            TestReply::Success
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
        for parked_allocation in [false, true] {
            let server = chunk_test_server("ipc://deactivated_buddy_chunks", 8);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut route = make_host_route(&server, "grid");
            route.callback = Arc::new(EchoRequestBytes {
                calls: calls.clone(),
            });
            let scheduler = Arc::clone(&route.scheduler);
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
            let reassembly_pool = server.chunk_registry.pool();
            let mut backing = parked_allocation.then(|| reassembly_pool.write());
            for (index, bytes) in [b"abcd", b"EFGH"].into_iter().enumerate() {
                let mut handle = pool.alloc_shm_handle(bytes.len()).unwrap();
                pool.handle_slice_mut(&mut handle).copy_from_slice(bytes);
                let allocation = handle.shm_allocation().unwrap();
                let loan_id = 101 + index as u64;
                let descriptor = BuddyPayload {
                    seg_idx: allocation.seg_idx as u16,
                    generation: allocation.generation,
                    offset: allocation.offset,
                    data_size: bytes.len() as u32,
                    is_dedicated: allocation.is_dedicated,
                    loan_id,
                };
                let frame = if index == 0 {
                    c2_wire::rpc::encode_open_frame(
                        1,
                        &call_identity("grid"),
                        0,
                        c2_wire::rpc::CallMode::Unary,
                        c2_wire::rpc::ValuePart::first(
                            8,
                            c2_wire::rpc::ValueStorage::Shm(descriptor),
                        ),
                    )
                    .unwrap()
                } else {
                    c2_wire::rpc::encode_value_continuation_frame(
                        1,
                        c2_wire::rpc::RpcRecordKind::Open,
                        c2_wire::rpc::ValueStorage::Shm(descriptor),
                    )
                    .unwrap()
                };
                peer.write_all(&frame).await.unwrap();
                if index == 0 {
                    if parked_allocation {
                        peer.write_all(&encode_frame(
                            99,
                            FLAG_SIGNAL,
                            &c2_wire::msg_type::PING_BYTES,
                        ))
                        .await
                        .unwrap();
                        let (id, _, payload) = read_reply_frame(&mut peer).await;
                        assert_eq!((id, payload.as_slice()), (99, PONG_BYTES.as_slice()));
                        assert_eq!(scheduler.snapshot().pending, 1);
                    } else {
                        assert_eq!(read_release_id(&mut peer).await, loan_id);
                        wait_until("buddy first chunk assembled", || {
                            server.chunk_registry.contains(conn.conn_id(), 1)
                        })
                        .await;
                    }
                    if shutdown {
                        server.request_shutdown_signal();
                    } else {
                        conn.end(
                            ConnectionEndReason::ProtocolError,
                            "test_buddy_deactivation",
                        );
                    }
                    let new_call = inline_call_frame(2, "grid", &[]);
                    peer.write_all(&new_call).await.unwrap();
                    let (id, _, reply) = read_reply_frame(&mut peer).await;
                    assert_eq!(id, 2);
                    assert_eq!(reply_error_code(&reply), ErrorCode::ResourceClosed);
                    drop(backing.take());
                    if parked_allocation {
                        assert_eq!(read_release_id(&mut peer).await, loan_id);
                    }
                } else {
                    assert_eq!(read_release_id(&mut peer).await, loan_id);
                }
                pool.release_handle(handle);
                assert_eq!(pool.stats().alloc_count, 0);
            }
            let (id, _, reply) = read_reply_frame(&mut peer).await;
            assert_eq!(id, 1);
            let (control, used) = decode_test_call_end(&reply, 0).unwrap();
            assert!(matches!(control, TestReply::Success), "{control:?}");
            assert_eq!(&reply[used..], b"abcdEFGH");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(peer);
            handler.await.unwrap();
        }
    }
}

#[tokio::test]
async fn whole_open_over_payload_limit_is_not_executed() {
    let mut config = ServerIpcConfig::default();
    config.max_payload_size = 64;
    let server = Arc::new(test_server_new("ipc://b5a-whole-limit", config).unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(EchoRequestBytes {
        calls: calls.clone(),
    });
    server.register_route(route).await.unwrap();
    let (mut peer, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(server.clone(), stream));
    handshake_test_client(&mut peer).await;
    peer.write_all(&inline_call_frame(1, "grid", &[7; 128]))
        .await
        .unwrap();
    let (id, _, payload) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "payload admission precedes resource callback"
    );
    assert_call_end_status(&payload, c2_wire::rpc::CallStatus::NotExecuted);
    let first = c2_wire::rpc::encode_open_frame(
        2,
        &call_identity("grid"),
        0,
        c2_wire::rpc::CallMode::Unary,
        c2_wire::rpc::ValuePart::first(128, c2_wire::rpc::ValueStorage::Inline(&[7; 64])),
    )
    .unwrap();
    peer.write_all(&first).await.unwrap();
    let (id, _, payload) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 2);
    assert_call_end_status(&payload, c2_wire::rpc::CallStatus::NotExecuted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let late = c2_wire::rpc::encode_value_continuation_frame(
        2,
        c2_wire::rpc::RpcRecordKind::Open,
        c2_wire::rpc::ValueStorage::Inline(&[7; 64]),
    )
    .unwrap();
    peer.write_all(&late).await.unwrap();
    peer.write_all(&inline_call_frame(3, "grid", b"next"))
        .await
        .unwrap();
    let (id, _, payload) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 3);
    assert_call_end_status(&payload, c2_wire::rpc::CallStatus::Success);
    assert_eq!(&payload[2..], b"next");
    drop(peer);
    handler.await.unwrap();
}

#[test]
fn peer_end_retires_queued_call_before_worker_enters_resource() {
    let conn = Connection::new(121);
    let call = conn.calls.lock().open(1, ServerCall::default()).unwrap();
    assert!(call.queue());
    let worker_ready = Arc::new(std::sync::Barrier::new(2));
    let worker_resume = Arc::new(std::sync::Barrier::new(2));
    let worker = {
        let ready = worker_ready.clone();
        let resume = worker_resume.clone();
        std::thread::spawn(move || {
            // The executor already captured the original call before peer end.
            ready.wait();
            resume.wait();
            call.enter_resource()
        })
    };
    worker_ready.wait();
    conn.cancel_queued_work();
    worker_resume.wait();
    assert!(!worker.join().unwrap());
}

#[tokio::test]
async fn unknown_frame_bits_stop_admission_and_preserve_original_input() {
    for flags in [
        0x0100,
        FLAG_SIGNAL | 0x0400,
        FLAG_CTRL | 0x0400,
        c2_wire::flags::FLAG_RPC_RECORD | FLAG_HANDSHAKE,
    ] {
        let server = chunk_test_server("ipc://b5a-invalid-frame-flags", 3);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut route = make_host_route(&server, "grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: calls.clone(),
        });
        server.register_route(route).await.unwrap();
        let (mut peer, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(server.clone(), stream));
        handshake_test_client(&mut peer).await;
        peer.write_all(&chunked_call_frame(1, 0, 2, b"abcd", "grid"))
            .await
            .unwrap();
        wait_until("original input is admitted", || {
            server.chunk_registry.active_count() == 1
        })
        .await;
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        peer.write_all(&encode_frame(2, flags, &[0x01]))
            .await
            .unwrap();
        wait_until("unknown bits stop new admission", || conn.is_deactivated()).await;
        if flags & c2_wire::flags::FLAG_RPC_RECORD != 0 {
            let (id, _, payload) = read_reply_frame(&mut peer).await;
            assert_eq!(id, 2);
            assert_call_end_status(&payload, c2_wire::rpc::CallStatus::NotExecuted);
        }
        peer.write_all(&chunked_call_frame(1, 1, 2, b"efgh", "grid"))
            .await
            .unwrap();
        let (id, _, payload) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 1);
        assert_call_end_status(&payload, c2_wire::rpc::CallStatus::Success);
        assert_eq!(&payload[2..], b"abcdefgh");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(peer);
        handler.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_input_refusal_cannot_end_a_resource_that_entered_after_the_check() {
    struct Callback {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
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
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let _ = self.resume.lock().unwrap().recv();
            Ok(ResponseMeta::Inline(b"actual result".to_vec()))
        }
    }
    let server = chunk_test_server("ipc://stale-input-refusal", 4);
    let (started, entered) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.callback = Arc::new(Callback {
        started: std::sync::Mutex::new(Some(started)),
        resume: std::sync::Mutex::new(resumed),
        calls: calls.clone(),
    });
    server.register_route(route).await.unwrap();
    let (mut peer, stream) = isolated_pair().await;
    let handler = tokio::spawn(handle_connection(server.clone(), stream));
    handshake_test_client(&mut peer).await;
    peer.write_all(&chunked_call_frame(1, 0, 2, b"abcd", "grid"))
        .await
        .unwrap();
    wait_until("original OPEN admitted", || {
        server.chunk_registry.active_count() == 1
    })
    .await;
    let conn = server
        .active_connection(server.active_connection_ids()[0])
        .unwrap();
    let writer = conn.rpc_writer.lock().as_ref().unwrap().upgrade().unwrap();
    let call = conn.calls.lock().get(1).unwrap();
    // Capture the old receive-side phase check, then let the final input copy
    // cross the real resource-entry boundary before attempting that refusal.
    assert!(call.is_preparing_input());
    peer.write_all(&chunked_call_frame(1, 1, 2, b"efgh", "grid"))
        .await
        .unwrap();
    entered.await.unwrap();
    queue_input_error(&writer, 1, "extra input after final part".into()).await;
    assert!(
        conn.calls.lock().get(1).is_some(),
        "stale input refusal must not claim an executing call"
    );
    // The actual extra continuation stops admission, but cannot replace the
    // callback's terminal outcome while it is held at the test barrier.
    peer.write_all(&chunked_call_frame(1, 1, 2, b"ijkl", "grid"))
        .await
        .unwrap();
    wait_until("extra input ends admission", || conn.is_deactivated()).await;
    resume.send(()).unwrap();
    let (id, _, body) = read_reply_frame(&mut peer).await;
    assert_eq!(id, 1);
    assert_call_end_status(&body, c2_wire::rpc::CallStatus::Success);
    assert_eq!(&body[2..], b"actual result");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(peer);
    handler.await.unwrap();
}

struct FirstFlushBarrier {
    stream: tokio::io::DuplexStream,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    resume: tokio::sync::oneshot::Receiver<()>,
    released: bool,
}
impl tokio::io::AsyncWrite for FirstFlushBarrier {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.released {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            match std::future::Future::poll(std::pin::Pin::new(&mut self.resume), cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(_) => self.released = true,
            }
        }
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_call_end_preserves_running_queued_calls_and_releases() {
    struct Callback {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl CrmCallback for Callback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            data: Vec<u8>,
            _: Arc<crate::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if data == b"executing" {
                let _ = self.entered.lock().unwrap().take().unwrap().send(());
                let _ = self.resume.lock().unwrap().recv();
            }
            Ok(ResponseMeta::Inline(data))
        }
    }
    let mut config = ServerIpcConfig::default();
    config.shm_threshold = 0;
    config.base.chunk_size = 256;
    let server = Arc::new(test_server_new("ipc://partial-call-end", config).unwrap());
    let conn = Arc::new(Connection::new(151));
    conn.set_handshake_done(true).unwrap();
    let record = conn.calls.lock().open(1, ServerCall::default()).unwrap();
    assert!(record.queue());
    assert!(record.enter_resource());
    let flight = crate::connection::FlightGuard::new(&conn);

    // An earlier published loan must still be settled by a real RELEASE frame
    // after this call's reply fails. Its original owner also prevents early EOF.
    let pool = server.response_pool.inner.clone();
    let allocations = pool.read().stats().alloc_count;
    let handle = pool.write().alloc_shm_handle(128).unwrap();
    let loan = conn.core.prepare_loan(pool.clone(), handle).unwrap();
    let loan_id = loan.id();
    let mut batch = conn
        .core
        .begin_write(c2_wire::connection::WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    batch.begin_io().unwrap();
    batch.frame_completed();
    batch.flush_completed();

    let (callback_entered, callback_running) = tokio::sync::oneshot::channel();
    let mut callback_running = Some(callback_running);
    let (resume_callback, callback_resumed) = std::sync::mpsc::channel();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut route = make_host_route(&server, "grid");
    route.scheduler = Arc::new(Scheduler::with_admission(
        ConcurrencyMode::Exclusive,
        HashMap::new(),
        SchedulerLimits::default(),
        server.admission.clone(),
    ));
    route.callback = Arc::new(Callback {
        entered: std::sync::Mutex::new(Some(callback_entered)),
        resume: std::sync::Mutex::new(callback_resumed),
        calls: calls.clone(),
    });
    let scheduler = route.scheduler.clone();
    server.register_route(route).await.unwrap();
    let (stream, mut peer) = tokio::io::duplex(4096);
    let (entered, first_written) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let (writer, driver) = ConnectionWriter::spawn_test_stream(
        FirstFlushBarrier {
            stream,
            entered: Some(entered),
            resume: resumed,
            released: false,
        },
        conn.clone(),
    );

    for (id, input) in [(2, b"executing".as_slice()), (3, b"queued".as_slice())] {
        let bytes = rpc::encode_open_frame(
            id,
            &call_identity("grid"),
            0,
            c2_wire::rpc::CallMode::Unary,
            ValuePart::whole(ValueStorage::Inline(input)),
        )
        .unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let (header, payload) = decode_frame_body(&bytes[4..], len).unwrap();
        handle_rpc_record(&server, &conn, &writer, &header, payload).await;
        if id == 2 {
            callback_running.take().unwrap().await.unwrap();
        }
    }
    assert_eq!(scheduler.snapshot().active_workers, 1);
    assert_eq!(scheduler.snapshot().pending, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let sending = tokio::spawn({
        let server = server.clone();
        let conn = conn.clone();
        let writer = writer.clone();
        async move {
            let _flight = flight;
            send_route_execution_result(
                &server,
                &conn,
                &writer,
                1,
                Ok(ResponseMeta::Inline(vec![42; 1024])),
            )
            .await;
        }
    });
    first_written.await.unwrap();
    let mut header = [0; 16];
    peer.read_exact(&mut header).await.unwrap();
    let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let mut body = vec![0; length - 12];
    peer.read_exact(&mut body).await.unwrap();
    assert_eq!(&body[..2], &[6, 0]);
    assert_ne!(
        u32::from_le_bytes(header[12..].try_into().unwrap()) & c2_wire::flags::FLAG_FIRST_CHUNK,
        0
    );
    conn.fail_next_write_allocation
        .store(true, Ordering::Release);
    resume.send(()).unwrap();
    sending.await.unwrap();
    assert!(conn.is_deactivated());
    assert!(!conn.is_work_cancelled());
    assert!(!conn.try_close());

    // Exercise the production read boundary while the incomplete reply has
    // deactivated admission. The old stop_reading path returned before reading.
    let (mut input_peer, input_stream) = isolated_pair().await;
    let (mut reader, input_writer) = input_stream.into_split();
    let release = c2_wire::release::ReleaseBatch::new(&[loan_id]).unwrap();
    let bytes = c2_wire::release::encode_release_frame(&release, false);
    input_peer.write_all(&bytes).await.unwrap();
    let (_shutdown, mut shutdown) = watch::channel(false);
    let mut received = vec![0; bytes.len()];
    assert!(
        read_connection_exact(&mut reader, &mut received, &conn, &mut shutdown)
            .await
            .unwrap(),
        "deactivation must keep reading RELEASE and already-sent calls"
    );
    let len = u32::from_le_bytes(received[..4].try_into().unwrap());
    let (header, payload) = decode_frame_body(&received[4..], len).unwrap();
    let outcome = conn.core.receive_release(&header, payload);
    assert!(outcome.error.is_none());
    drop(outcome.reclaims);
    assert_eq!(pool.read().stats().alloc_count, allocations);
    assert!(!conn.try_close(), "accepted calls still own their work");

    resume_callback.send(()).unwrap();
    let mut results = HashMap::new();
    for _ in 0..2 {
        let mut header = [0; 16];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let mut body = vec![0; length - 12];
        peer.read_exact(&mut body).await.unwrap();
        let id = u64::from_le_bytes(header[4..12].try_into().unwrap());
        assert!(results.insert(id, body[2..].to_vec()).is_none());
        assert_call_end_status(&body, CallStatus::Success);
    }
    assert_eq!(results.remove(&2).unwrap(), b"executing");
    assert_eq!(results.remove(&3).unwrap(), b"queued");
    assert!(results.is_empty());
    tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .unwrap()
        .unwrap();
    let mut remaining = Vec::new();
    peer.read_to_end(&mut remaining).await.unwrap();
    assert!(
        remaining.is_empty(),
        "an incomplete Success value cannot be followed by another CALL_END: {remaining:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(conn.calls.lock().active().next().is_none());
    assert!(conn.try_close());
    drop((writer, reader, input_writer, input_peer));
}

#[tokio::test]
async fn mode_refused_shm_open_and_retired_continuation_release_original_loans() {
    for chunked in [false, true] {
        let server = chunk_test_server("ipc://mode_refused_shm", 8);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut route = make_host_route(&server, "grid");
        route.callback = Arc::new(EchoRequestBytes {
            calls: calls.clone(),
        });
        let scheduler = route.scheduler.clone();
        server.register_route(route).await.unwrap();
        let mut pool = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                ..Default::default()
            },
            unique_response_pool_prefix("mode-refused"),
        )
        .unwrap();
        let (mut peer, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(server.clone(), stream));
        handshake_test_client_with(&mut peer, CAP_SHM_REQUEST, pool.prefix()).await;
        let conn = server
            .active_connection(server.active_connection_ids()[0])
            .unwrap();
        for index in 0..if chunked { 2 } else { 1 } {
            let mut handle = pool.alloc_shm_handle(4).unwrap();
            pool.handle_slice_mut(&mut handle).copy_from_slice(b"data");
            let alloc = handle.shm_allocation().unwrap();
            let descriptor = BuddyPayload {
                seg_idx: alloc.seg_idx as u16,
                generation: alloc.generation,
                offset: alloc.offset,
                data_size: 4,
                is_dedicated: alloc.is_dedicated,
                loan_id: 101 + index,
            };
            let frame = if index == 0 {
                rpc::encode_open_frame(
                    1,
                    &call_identity("grid"),
                    0,
                    CallMode::ServerStream,
                    if chunked {
                        ValuePart::first(8, ValueStorage::Shm(descriptor))
                    } else {
                        ValuePart::whole(ValueStorage::Shm(descriptor))
                    },
                )
                .unwrap()
            } else {
                rpc::encode_value_continuation_frame(
                    1,
                    RpcRecordKind::Open,
                    ValueStorage::Shm(descriptor),
                )
                .unwrap()
            };
            peer.write_all(&frame).await.unwrap();
            let mut released = false;
            let mut refused = index != 0;
            while !released || !refused {
                let (id, flags, payload) = read_reply_frame(&mut peer).await;
                let bytes = encode_frame(id, flags, &payload);
                let (header, payload) = c2_wire::frame::decode_frame(&bytes).unwrap();
                match rpc::decode_rpc_record(&header, payload, Some(CallMode::ServerStream))
                    .unwrap()
                {
                    RpcRecord::Release(batch) => {
                        assert!(!released);
                        assert_eq!(batch.ids(), &[descriptor.loan_id]);
                        released = true;
                    }
                    RpcRecord::CallEnd {
                        status: CallStatus::NotExecuted,
                        error: Some(error),
                        ..
                    } => {
                        assert!(!refused, "retired chunks cannot produce another terminal");
                        assert_eq!(error.code, ErrorCode::UnsupportedCallMode);
                        refused = true;
                    }
                    other => panic!("unexpected record: {other:?}"),
                }
            }
            pool.release_handle(handle);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(scheduler.snapshot().pending, 0);
        assert_eq!(server.chunk_registry.active_count(), 0);
        assert!(!conn.is_deactivated());
        peer.write_all(
            &rpc::encode_open_frame(
                2,
                &call_identity("grid"),
                0,
                CallMode::Unary,
                ValuePart::whole(ValueStorage::Inline(b"next")),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let (id, _, payload) = read_reply_frame(&mut peer).await;
        assert_eq!(id, 2);
        assert_call_end_status(&payload, CallStatus::Success);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!conn.is_deactivated());
        drop(peer);
        handler.await.unwrap();
    }
}
