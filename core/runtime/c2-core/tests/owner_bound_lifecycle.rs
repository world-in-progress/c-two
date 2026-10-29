//! OwnerBound host lifecycle tests against the public Runtime/Host surface.
//!
//! In-process cases use a real Core Runtime, a real host thread, real direct
//! IPC clients, and the real owner control pair. The controller-kill case
//! runs a controller that owns the control session and a service that adopts
//! the inherited bootstrap, so SIGKILL of the controller
//! is observed by the service purely as a control-channel EOF.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Client, Connect, EncodedClient, EncodedService, Error, HostLifecyclePhase, HostOptions,
    MethodDefinition, OwnerControlErrorKind, OwnerPhase, OwnerRecoveryTicket,
    RouteConcurrencyError, Runtime, RuntimeOptions, ServiceConcurrencyMode, ServiceDefinition,
    owner_control_pair,
};
use c2_error::{C2Error, ErrorCode};
use c2_local::LocalStream;
use c2_wire::flags::{FLAG_CTRL, FLAG_RESPONSE};
use c2_wire::frame::{self, FrameHeader, encode_frame};
use c2_wire::owner_control::{
    MAX_OWNER_PAYLOAD_SIZE, OWNER_CONTROL_VERSION, OwnerRejection, OwnerRequest, OwnerResponse,
    decode_owner_response, encode_owner_request, encode_owner_response,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

const FIXTURE_ACTION: &str = "C2_CORE_OWNER_BOUND_FIXTURE";
#[cfg(unix)]
const FIXTURE_ROOT: &str = "C2_CORE_OWNER_BOUND_ROOT";
const FIXTURE_SERVER_ID: &str = "C2_CORE_OWNER_BOUND_SERVER_ID";
const CONTROL_TIMEOUT: Option<Duration> = Some(Duration::from_millis(500));

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request),
            _ => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                "method index escaped validated definition",
            )),
        }
    }
}

struct CountedEcho(Arc<AtomicU64>);

impl EncodedService for CountedEcho {
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        if index == 1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Echo.invoke(index, request)
    }
}

/// Blocks every `echo` invocation until the shared flag is set, proving
/// in-flight callbacks drain under the original rules across an owner EOF.
struct GatedEcho {
    gate: Arc<(StdMutex<bool>, Condvar)>,
    entered: std::sync::mpsc::Sender<()>,
}

impl EncodedService for GatedEcho {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        if method_index == 1 {
            self.entered.send(()).unwrap();
            let (lock, cvar) = &*self.gate;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
        }
        Ok(request)
    }
}

struct ReleaseGate(Arc<(StdMutex<bool>, Condvar)>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
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

fn methods() -> [MethodDefinition; 2] {
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
    ]
}

fn test_runtime() -> Runtime {
    Runtime::new(RuntimeOptions {
        server_id: Some(unique_name("owner-bound")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .expect("runtime")
}

fn owner_host(runtime: &Runtime, grace: Duration) -> (c2_core::OwnerControl, c2_core::Host) {
    let (owner, bootstrap) = owner_control_pair().unwrap();
    runtime.attach_owner_control(bootstrap).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(grace),
        )
        .unwrap();
    (owner, host)
}

fn register_echo(host: &c2_core::Host, route_name: &str) -> c2_core::Registration {
    let release = release();
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        route_name,
        methods(),
        Arc::new(Echo),
    )
    .expect("definition");
    host.register(definition).expect("register route")
}

fn direct_client(runtime: &Runtime, route_name: &str) -> Result<Client, Error> {
    let release = release();
    runtime.connect(
        release.expected_route(route_name).expect("expected route"),
        Connect::DirectIpc {
            address: runtime.server_address().expect("server address"),
        },
    )
}

fn wait_for_phase<F>(host: &c2_core::Host, predicate: F, timeout: Duration) -> HostLifecyclePhase
where
    F: Fn(&HostLifecyclePhase) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = host.lifecycle_snapshot();
        if predicate(&snapshot.phase) {
            return snapshot.phase;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for lifecycle phase, still {:?}",
            snapshot.phase
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Only the Unix directory-specific cases are platform-gated. The capability,
// admission, payload and process cases also run on Windows Named Pipes.
#[cfg(unix)]
type PrivateRoot = tempfile::TempDir;
#[cfg(windows)]
type PrivateRoot = ();

#[cfg(unix)]
fn private_root() -> PrivateRoot {
    tempfile::Builder::new()
        .prefix("c2o-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[cfg(windows)]
fn private_root() -> PrivateRoot {}

fn runtime_in(root: &PrivateRoot) -> Runtime {
    let runtime = test_runtime();
    #[cfg(unix)]
    let options = c2_config::LocalEndpointOptions {
        unix_root: Some(root.path().into()),
    };
    #[cfg(windows)]
    let options = {
        let _ = root;
        c2_config::LocalEndpointOptions::default()
    };
    runtime
        .set_local_endpoint_with_sources(options, c2_config::ConfigSources::empty())
        .unwrap();
    runtime
}

fn assert_endpoint_gone(endpoint: &c2_config::LocalEndpoint) {
    let inspection = c2_local::inspect_endpoint(endpoint);
    assert!(
        matches!(
            inspection,
            c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
        ),
        "{inspection:?}"
    );
    #[cfg(unix)]
    {
        let socket = std::path::Path::new(endpoint.os_name());
        assert!(!socket.exists(), "native close left this test's socket");
        let root = endpoint.context().unix_root().unwrap();
        assert!(
            std::fs::read_dir(root).unwrap().all(|entry| {
                entry.unwrap().path().extension() != Some(std::ffi::OsStr::new("lease"))
            }),
            "native close left an endpoint lease in this test's private root"
        );
    }
}

async fn send_owner_frame(stream: &mut LocalStream, request_id: u64, request: &OwnerRequest) {
    let payload = encode_owner_request(request).unwrap();
    tokio::time::timeout(
        Duration::from_millis(500),
        stream.write_all(&encode_frame(request_id, FLAG_CTRL, &payload)),
    )
    .await
    .expect("bounded owner request write")
    .unwrap();
}

async fn read_owner_frame(stream: &mut LocalStream) -> (FrameHeader, Vec<u8>) {
    tokio::time::timeout(Duration::from_millis(500), async {
        let mut bytes = [0; frame::HEADER_SIZE];
        stream.read_exact(&mut bytes).await.unwrap();
        let (length, body) = frame::decode_total_len(&bytes).unwrap();
        let (header, _) = frame::decode_frame_body(body, length).unwrap();
        assert!(header.payload_len() <= MAX_OWNER_PAYLOAD_SIZE);
        let mut payload = vec![0; header.payload_len()];
        stream.read_exact(&mut payload).await.unwrap();
        (header, payload)
    })
    .await
    .expect("bounded owner reply read")
}

async fn read_owner_reply(stream: &mut LocalStream, request_id: u64) -> OwnerResponse {
    let (header, payload) = read_owner_frame(stream).await;
    assert_eq!(header.request_id, request_id);
    assert_eq!(header.flags, FLAG_CTRL | FLAG_RESPONSE);
    decode_owner_response(&payload).unwrap()
}

async fn expect_owner_peer_close(stream: &mut LocalStream, deadline: Instant) {
    let result = tokio::time::timeout_at(deadline.into(), stream.read(&mut [0])).await;
    match result {
        Ok(Ok(0)) => {}
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("owner peer must close before its original deadline: {other:?}"),
    }
}

#[test]
fn owner_protocol_delayed_shutdown_ack_survives_bootstrap_and_listener_drain() {
    for takeover in [false, true] {
        let root = private_root();
        let runtime = runtime_in(&root);
        let (mut controller, bootstrap) = c2_local::owner_endpoint_pair().unwrap();
        runtime.attach_owner_control(bootstrap).unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_owner_bound(Duration::from_secs(3)),
            )
            .unwrap();
        let endpoint = host.local_endpoint().clone();
        let registration = register_echo(&host, &unique_name("delayed-owner-ack"));
        let io = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut stream = io.block_on(controller.activate()).unwrap();
            if takeover {
                io.block_on(send_owner_frame(
                    &mut stream,
                    1,
                    &OwnerRequest::GetRecoveryTicket,
                ));
                let OwnerResponse::RecoveryTicket(ticket) =
                    io.block_on(read_owner_reply(&mut stream, 1))
                else {
                    panic!("bootstrap must issue a recovery ticket");
                };
                drop(stream);
                wait_for_phase(
                    &host,
                    |p| *p == HostLifecyclePhase::OwnerMissing,
                    Duration::from_secs(2),
                );
                stream = io
                    .block_on(LocalStream::connect(&endpoint, Duration::from_millis(500)))
                    .unwrap();
                io.block_on(send_owner_frame(&mut stream, 2, &OwnerRequest::Hello));
                assert!(
                    matches!(io.block_on(read_owner_reply(&mut stream, 2)), OwnerResponse::Status(status) if status.phase == OwnerPhase::OwnerMissing)
                );
                io.block_on(send_owner_frame(
                    &mut stream,
                    3,
                    &OwnerRequest::Takeover {
                        ticket,
                        successor_secret: [0x35; 32],
                    },
                ));
                assert!(
                    matches!(io.block_on(read_owner_reply(&mut stream, 3)), OwnerResponse::TakenOver(status) if status.phase == OwnerPhase::Armed)
                );
            }
            io.block_on(send_owner_frame(
                &mut stream,
                91,
                &OwnerRequest::RequestShutdown,
            ));
            // Read late within the existing 50ms courtesy window, with no active
            // business flight to keep a missing owner reply guard alive for us.
            std::thread::sleep(Duration::from_millis(20));
            let ack = io.block_on(read_owner_reply(&mut stream, 91));
            drop(stream); // The terminal reply waits for this control-session EOF.
            assert_eq!(ack, OwnerResponse::ShutdownInitiated);
            // The correlated reply confirms initiation only. Observe actual
            // completion through the native lifecycle and shutdown journal.
            wait_for_phase(
                &host,
                |p| *p == HostLifecyclePhase::Finished,
                Duration::from_secs(2),
            );
            assert!(host.shutdown_outcome().is_some());
        }));
        controller.close();
        let shutdown = host.shutdown();
        let stopped = host.lifecycle_snapshot();
        drop(registration);
        drop(host);
        drop(runtime);
        assert_endpoint_gone(&endpoint);
        assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
        assert!(shutdown.route_close_error.is_none(), "{shutdown:?}");
        assert!(stopped.listener_closed);
        assert_eq!(stopped.work_drained, Some(true));
        observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
}

#[test]
fn owner_protocol_lost_takeover_ack_recovers_with_saved_successor_past_old_deadline() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let controller_runtime = runtime_in(&root);
    let (owner, bootstrap) = owner_control_pair().unwrap();
    runtime.attach_owner_control(bootstrap).unwrap();
    let grace = Duration::from_secs(2);
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(grace),
        )
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("lost-owner-ack");
    let registration = register_echo(&host, &route);
    let io = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let current = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        let OwnerResponse::RecoveryTicket(ticket) = decode_owner_response(&current).unwrap() else {
            panic!("explicit export must contain the recovery ticket");
        };
        // The application saves exactly its proposed successor before sending.
        // No guessed old/new fallback or SDK timeout server is involved.
        let successor_secret = [0xa7; 32];
        let mut successor = ticket.clone();
        successor.epoch += 1;
        successor.secret = successor_secret;
        let saved = OwnerRecoveryTicket::import(
            &encode_owner_response(&OwnerResponse::RecoveryTicket(successor)).unwrap(),
        )
        .unwrap()
        .export();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let old_deadline_upper = Instant::now() + grace;
        let mut stream = io
            .block_on(LocalStream::connect(&endpoint, Duration::from_millis(500)))
            .unwrap();
        io.block_on(send_owner_frame(&mut stream, 1, &OwnerRequest::Hello));
        assert!(
            matches!(io.block_on(read_owner_reply(&mut stream, 1)), OwnerResponse::Status(status) if status.epoch == ticket.epoch && status.phase == OwnerPhase::OwnerMissing)
        );
        let epoch = ticket.epoch;
        io.block_on(send_owner_frame(
            &mut stream,
            2,
            &OwnerRequest::Takeover {
                ticket,
                successor_secret,
            },
        ));
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::Armed,
            Duration::from_secs(2),
        );
        // Deliberately never read request 2's ACK, even though Core committed.
        drop(stream);
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let replay = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        let status = replay.inspect(CONTROL_TIMEOUT).unwrap();
        assert_eq!(status.epoch, epoch + 1);
        assert_eq!(status.phase, OwnerPhase::OwnerMissing);
        assert_eq!(
            replay
                .commit(CONTROL_TIMEOUT)
                .err()
                .expect("consumed old grant")
                .kind(),
            OwnerControlErrorKind::InvalidGrant
        );
        let recovery = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&saved).unwrap())
            .unwrap();
        let installed = recovery.successor_ticket().export();
        let replacement = recovery.commit(CONTROL_TIMEOUT).unwrap();
        assert!(
            replacement
                .recovery_ticket(CONTROL_TIMEOUT)
                .unwrap()
                .export()
                == installed,
            "only the explicitly proposed grant is installed"
        );
        // Cross the original grace for real. Stale timers must not close a
        // still-open owner from either later epoch.
        std::thread::sleep(
            old_deadline_upper.saturating_duration_since(Instant::now())
                + Duration::from_millis(100),
        );
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
        assert!(host.shutdown_outcome().is_none());
        assert_eq!(
            direct_client(&runtime, &route)
                .unwrap()
                .call_owned("echo", b"after-old-grace")
                .unwrap(),
            b"after-old-grace"
        );
        replacement.close();
    }));
    owner.close();
    let shutdown = host.shutdown();
    drop(registration);
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_protocol_oversized_tag_is_rejected_before_body_on_first_and_later_frames() {
    use c2_wire::flags::FLAG_SIGNAL;
    use c2_wire::msg_type::{MsgType, PING_BYTES, PONG_BYTES};

    let root = private_root();
    let runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let io = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let grant = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        for ordinary in [false, true] {
            io.block_on(async {
                let mut stream = LocalStream::connect(&endpoint, Duration::from_millis(500))
                    .await
                    .unwrap();
                if ordinary {
                    stream
                        .write_all(&encode_frame(11, FLAG_SIGNAL, &PING_BYTES))
                        .await
                        .unwrap();
                    let (header, payload) = read_owner_frame(&mut stream).await;
                    assert_eq!(header.request_id, 11);
                    assert_eq!(header.flags, FLAG_SIGNAL | FLAG_RESPONSE);
                    assert_eq!(payload, PONG_BYTES);
                }
                // Send only the header and tag, declaring 4097 bytes. Waiting
                // for the unsent body (or allocating 2GiB) cannot pass this case.
                let mut prefix = encode_frame(12, FLAG_CTRL, &[MsgType::OwnerControl.as_byte()]);
                prefix[..4]
                    .copy_from_slice(&(12 + MAX_OWNER_PAYLOAD_SIZE as u32 + 1).to_le_bytes());
                // The deliberately invalid frame closes the peer immediately;
                // observe that rejection instead of flushing a closed pipe.
                AsyncWriteExt::write_all(&mut stream, &prefix)
                    .await
                    .unwrap();
                expect_owner_peer_close(&mut stream, Instant::now() + Duration::from_millis(500))
                    .await;
            });
            assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
            assert!(
                owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export() == grant,
                "rejected role/size input must not change the grant"
            );
        }
    }));
    let shutdown = host.shutdown();
    owner.close();
    drop(host);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_protocol_owner_role_rejects_business_header_old_version_and_unknown_operation() {
    use c2_wire::flags::FLAG_CALL_V2;
    use c2_wire::msg_type::MsgType;

    let root = private_root();
    let runtime = runtime_in(&root);
    let controller_runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let io = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let current = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        for invalid in 0..4 {
            io.block_on(async {
                let mut stream = LocalStream::connect(&endpoint, Duration::from_millis(500)).await.unwrap();
                if invalid != 3 {
                    send_owner_frame(&mut stream, 1, &OwnerRequest::Hello).await;
                    assert!(matches!(read_owner_reply(&mut stream, 1).await, OwnerResponse::Status(status) if status.phase == OwnerPhase::OwnerMissing));
                }
                let (frame, rejection) = match invalid {
                    0 => {
                        // A business-call header alone must fail the fixed
                        // owner role before reading any business payload.
                        let mut frame = encode_frame(8, FLAG_CALL_V2, &[0; 3]);
                        frame.truncate(frame::HEADER_SIZE);
                        (frame, OwnerRejection::Malformed)
                    }
                    1 => {
                        let mut frame = encode_frame(8, FLAG_CTRL, &[MsgType::OwnerControl.as_byte(), 0, 1]);
                        frame[..4].copy_from_slice(&(12 + MAX_OWNER_PAYLOAD_SIZE as u32).to_le_bytes());
                        // Unsupported version is rejected before the absent
                        // remainder of this otherwise permitted 4KiB body.
                        (frame, OwnerRejection::UnsupportedVersion)
                    }
                    2 => (encode_frame(8, FLAG_CTRL, &[MsgType::OwnerControl.as_byte(), OWNER_CONTROL_VERSION, 255]), OwnerRejection::Malformed),
                    _ => (encode_frame(8, FLAG_CTRL, &encode_owner_request(&OwnerRequest::GetRecoveryTicket).unwrap()), OwnerRejection::Malformed),
                };
                AsyncWriteExt::write_all(&mut stream, &frame).await.unwrap();
                let response = read_owner_reply(&mut stream, 8).await;
                expect_owner_peer_close(&mut stream, Instant::now() + Duration::from_millis(500)).await;
                drop(stream);
                assert_eq!(response, OwnerResponse::Rejected(rejection));
            });
            assert_eq!(
                host.lifecycle_snapshot().phase,
                HostLifecyclePhase::OwnerMissing
            );
        }
        let prepared = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        let replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
        replacement.close();
    }));
    owner.close();
    let shutdown = host.shutdown();
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_protocol_fragmented_and_silent_setup_share_one_deadline_without_heartbeat() {
    let root = private_root();
    let runtime = runtime_in(&root);
    runtime
        .set_server_options(
            None,
            Some(c2_config::ServerIpcConfigOverrides {
                heartbeat_interval_secs: Some(0.0),
                heartbeat_timeout_secs: Some(0.0),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    let controller_runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(8));
    let endpoint = host.local_endpoint().clone();
    let io = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let current = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        let OwnerResponse::RecoveryTicket(ticket) = decode_owner_response(&current).unwrap() else {
            panic!("explicit recovery ticket export");
        };
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        io.block_on(async {
            let mut fragmented = LocalStream::connect(&endpoint, Duration::from_millis(500)).await.unwrap();
            let mut silent = LocalStream::connect(&endpoint, Duration::from_millis(500)).await.unwrap();
            let hello = encode_frame(1, FLAG_CTRL, &encode_owner_request(&OwnerRequest::Hello).unwrap());
            let started = Instant::now();
            fragmented.write_all(&hello[..frame::HEADER_SIZE + 1]).await.unwrap();
            send_owner_frame(&mut silent, 2, &OwnerRequest::Hello).await;
            assert!(matches!(read_owner_reply(&mut silent, 2).await, OwnerResponse::Status(status) if status.phase == OwnerPhase::OwnerMissing && status.epoch == ticket.epoch));
            tokio::time::sleep(Duration::from_secs(2)).await;
            fragmented.write_all(&hello[frame::HEADER_SIZE + 1..]).await.unwrap();
            assert!(matches!(read_owner_reply(&mut fragmented, 1).await, OwnerResponse::Status(status) if status.phase == OwnerPhase::OwnerMissing && status.epoch == ticket.epoch));
            let takeover = encode_frame(3, FLAG_CTRL, &encode_owner_request(&OwnerRequest::Takeover { ticket, successor_secret: [0x61; 32] }).unwrap());
            fragmented.write_all(&takeover[..frame::HEADER_SIZE + 3]).await.unwrap();
            // Completing Hello and starting a second half-frame must not reset
            // setup to five more seconds. The other candidate stays silent.
            let deadline = started + c2_local::DEFAULT_CONNECT_TIMEOUT + Duration::from_millis(500);
            expect_owner_peer_close(&mut fragmented, deadline).await;
            expect_owner_peer_close(&mut silent, deadline).await;
            assert!(started.elapsed() >= c2_local::DEFAULT_CONNECT_TIMEOUT);
        });
        assert_eq!(
            host.lifecycle_snapshot().phase,
            HostLifecyclePhase::OwnerMissing
        );
        assert!(!host.lifecycle_snapshot().listener_closed);
        // Successful use of the original grant proves failed partial/silent
        // candidates neither consumed nor replaced it or advanced the epoch.
        let prepared = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        let replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
        replacement.close();
    }));
    owner.close();
    let shutdown = host.shutdown();
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_protocol_late_candidates_cannot_extend_the_original_short_grace() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let (owner, bootstrap) = owner_control_pair().unwrap();
    runtime.attach_owner_control(bootstrap).unwrap();
    let grace = Duration::from_secs(2);
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(grace),
        )
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let io = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owner.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let original_deadline_upper = Instant::now() + grace;
        // Late classification must use the existing grace, not now + grace.
        std::thread::sleep(Duration::from_secs(1));
        io.block_on(async {
            let mut fragmented = LocalStream::connect(&endpoint, Duration::from_millis(500)).await.unwrap();
            let mut silent = LocalStream::connect(&endpoint, Duration::from_millis(500)).await.unwrap();
            let hello = encode_frame(1, FLAG_CTRL, &encode_owner_request(&OwnerRequest::Hello).unwrap());
            fragmented.write_all(&hello[..frame::HEADER_SIZE + 1]).await.unwrap();
            send_owner_frame(&mut silent, 2, &OwnerRequest::Hello).await;
            assert!(matches!(read_owner_reply(&mut silent, 2).await, OwnerResponse::Status(status) if status.phase == OwnerPhase::OwnerMissing));
            let deadline = original_deadline_upper + Duration::from_millis(500);
            expect_owner_peer_close(&mut fragmented, deadline).await;
            expect_owner_peer_close(&mut silent, deadline).await;
        });
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::Finished,
            (original_deadline_upper + Duration::from_millis(500))
                .saturating_duration_since(Instant::now()),
        );
        assert!(
            Instant::now() <= original_deadline_upper + Duration::from_millis(500),
            "candidate setup extended the original grace"
        );
    }));
    let shutdown = host.shutdown();
    owner.close();
    drop(host);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn persistent_host_keeps_serving_after_all_business_clients_disconnect() {
    let runtime = test_runtime();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let route_name = unique_name("persistent");
    let _registration = register_echo(&host, &route_name);

    let snapshot = host.lifecycle_snapshot();
    assert_eq!(snapshot.phase, HostLifecyclePhase::Persistent);
    assert!(!snapshot.listener_closed);

    for _ in 0..2 {
        let client = direct_client(&runtime, &route_name).expect("client");
        assert_eq!(
            client.call_owned("echo", b"payload").expect("call"),
            b"payload"
        );
        drop(client);
    }
    // Every business client is gone; the persistent host still accepts.
    let client = direct_client(&runtime, &route_name).expect("client after disconnects");
    assert_eq!(client.call_owned("echo", b"again").expect("call"), b"again");

    let outcome = host.shutdown();
    assert!(outcome.removed_routes.contains(&route_name));
    let snapshot = host.lifecycle_snapshot();
    assert_eq!(snapshot.phase, HostLifecyclePhase::Finished);
    assert!(snapshot.listener_closed);
    assert_eq!(snapshot.work_drained, Some(true));
    // Idempotent explicit shutdown keeps one result.
    assert_eq!(host.shutdown(), outcome);
}

#[test]
fn owner_bound_policy_without_capability_is_refused_before_readiness() {
    let runtime = test_runtime();
    let error = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::from_millis(200)),
        )
        .expect_err("policy name without capability must be refused");
    let message = error.to_string();
    assert!(
        message.contains("requires an attached native owner control capability"),
        "unexpected error: {message}"
    );
    assert!(!runtime.owner_control_attached());
}

#[test]
fn owner_bound_grace_is_validated_by_rust_against_the_bounded_range() {
    let runtime = test_runtime();
    let (_keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let error = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(c2_config::MAX_OWNER_MISSING_GRACE + Duration::from_millis(1)),
        )
        .expect_err("unbounded grace must be rejected");
    assert!(
        error.to_string().contains("exceeds the maximum"),
        "unexpected error: {error}"
    );
}

fn assert_already_gone_is_refused() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    // The controller exits before the host arms its watcher.
    keepalive.close();
    let error = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::from_millis(200)),
        )
        .expect_err("already-gone capability must refuse the start");
    let message = error.to_string();
    assert!(
        message.contains("already closed before the host became ready"),
        "unexpected error: {message}"
    );
    assert!(!runtime.owner_control_attached());
}

#[test]
fn already_gone_capability_is_refused_before_readiness() {
    // Pipe creation happens inside an exact post-exec fixture, outside the
    // other libtest cases' concurrent fork/exec windows.
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owner_bound_fixture", "--nocapture"])
            .env(FIXTURE_ACTION, "preclosed")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = BufReader::new(stdout)
            .lines()
            .collect::<std::io::Result<Vec<_>>>();
        let _ = tx.send(result);
    });
    let lines = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("isolated preclosed Host deadline")
        .unwrap();
    assert!(
        lines.iter().any(|line| line == "PRE_READY_REFUSED"),
        "{lines:?}"
    );
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn duplicate_and_late_capability_attaches_are_rejected() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime
        .attach_owner_control(receiver)
        .expect("first attach");
    let (_keepalive2, receiver2) = owner_control_pair().unwrap();
    let duplicate = runtime
        .attach_owner_control(receiver2)
        .expect_err("duplicate attach must be rejected");
    assert!(
        duplicate
            .to_string()
            .contains("exactly one owner bootstrap"),
        "unexpected error: {duplicate}"
    );

    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::from_secs(30)),
        )
        .expect("host");
    assert!(!runtime.owner_control_attached());
    let (_keepalive3, receiver3) = owner_control_pair().unwrap();
    let late = runtime
        .attach_owner_control(receiver3)
        .expect_err("late attach after a host consumed the capability must be rejected");
    assert!(
        late.to_string().contains("already consumed"),
        "unexpected error: {late}"
    );
    drop(keepalive);
    let _ = host.shutdown();
}

fn check_live_owner_refuses_ordinary_shutdown(handshaken: bool) {
    let root = private_root();
    let runtime = runtime_in(&root);
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_secs(5))
                .with_owner_bound(Duration::from_secs(3)),
        )
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("owner-admin-refusal");
    let registration = register_echo(&host, &route);
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let client = direct_client(&runtime, &route).unwrap().with_call_options(
            c2_config::CallOptions::with_timeout(c2_config::CallTimeout::After(
                Duration::from_millis(500),
            )),
        );
        let before = client.call_owned("echo", b"before");
        let ack = if handshaken {
            use c2_local::LocalStream;
            use c2_wire::flags::{FLAG_HANDSHAKE, FLAG_RESPONSE, FLAG_SIGNAL};
            use c2_wire::frame::{self, FrameHeader, encode_frame};
            use c2_wire::handshake::{
                CAP_CALL_V2, VersionPreambleKind, decode_handshake, encode_client_handshake,
                encode_version_preamble,
            };
            use c2_wire::shutdown_control::{decode_shutdown_ack, encode_shutdown_initiate};
            use tokio::io::AsyncReadExt;

            async fn read_reply(stream: &mut LocalStream) -> (FrameHeader, Vec<u8>) {
                let mut header_bytes = [0; frame::HEADER_SIZE];
                stream.read_exact(&mut header_bytes).await.unwrap();
                let (length, body) = frame::decode_total_len(&header_bytes).unwrap();
                let (header, _) = frame::decode_frame_body(body, length).unwrap();
                // Match the existing IPC control reader's bounded reply allocation.
                assert!(header.payload_len() <= 1024 * 1024);
                let mut payload = vec![0; header.payload_len()];
                stream.read_exact(&mut payload).await.unwrap();
                (header, payload)
            }

            let io = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            io.block_on(async {
                tokio::time::timeout(Duration::from_millis(500), async {
                    let mut stream = LocalStream::connect(&endpoint, Duration::from_millis(500))
                        .await
                        .unwrap();
                    stream
                        .write_all(&encode_version_preamble(VersionPreambleKind::Hello))
                        .await
                        .unwrap();
                    let mut accepted = [0; 25];
                    stream.read_exact(&mut accepted).await.unwrap();
                    assert_eq!(
                        accepted,
                        encode_version_preamble(VersionPreambleKind::Accepted)
                    );
                    let handshake = encode_client_handshake(&[], CAP_CALL_V2, "").unwrap();
                    stream
                        .write_all(&encode_frame(0, FLAG_HANDSHAKE, &handshake))
                        .await
                        .unwrap();
                    let (header, payload) = read_reply(&mut stream).await;
                    assert_eq!(header.request_id, 0);
                    assert_eq!(header.flags, FLAG_HANDSHAKE | FLAG_RESPONSE);
                    assert!(
                        decode_handshake(&payload)
                            .unwrap()
                            .server_identity
                            .is_some()
                    );
                    stream
                        .write_all(&encode_frame(1, FLAG_SIGNAL, &encode_shutdown_initiate()))
                        .await
                        .unwrap();
                    let (header, payload) = read_reply(&mut stream).await;
                    assert_eq!(header.request_id, 1);
                    assert_eq!(header.flags, FLAG_SIGNAL | FLAG_RESPONSE);
                    c2_core::DirectIpcShutdownOutcome::from(decode_shutdown_ack(&payload).unwrap())
                })
                .await
                .expect("bounded handshaken shutdown acknowledgement")
            })
        } else {
            // This existing admin helper sends shutdown as the first frame.
            runtime
                .shutdown_direct_ipc(endpoint.address(), Duration::from_millis(500))
                .unwrap()
        };
        let snapshot = host.lifecycle_snapshot();
        let after = client.call_owned("echo", b"after");
        (before, ack, snapshot, after)
    }));

    // Keep the real owner alive until trusted in-process native shutdown has
    // fenced the server. Even a red assertion or wire/setup panic is deferred.
    let shutdown = host.shutdown();
    let stopped = host.lifecycle_snapshot();
    drop(registration);
    keepalive.close();
    drop(host);
    drop(runtime);
    let endpoint_after_stop = c2_local::inspect_endpoint(&endpoint);

    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    assert!(shutdown.route_close_error.is_none(), "{shutdown:?}");
    assert!(shutdown.ipc_client_close_error.is_none(), "{shutdown:?}");
    assert!(stopped.listener_closed);
    assert_eq!(stopped.work_drained, Some(true));
    assert!(
        matches!(
            endpoint_after_stop,
            c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
        ),
        "{endpoint_after_stop:?}"
    );
    let (before, ack, snapshot, after) =
        observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_eq!(before.unwrap(), b"before");
    assert!(
        !ack.acknowledged && !ack.shutdown_started && !ack.server_stopped,
        "OwnerBound must negatively acknowledge ordinary shutdown (handshaken={handshaken}): {ack:?}"
    );
    assert!(ack.route_outcomes.is_empty(), "{ack:?}");
    assert_eq!(snapshot.phase, HostLifecyclePhase::Armed);
    assert!(!snapshot.listener_closed);
    assert_eq!(after.unwrap(), b"after");
}

#[test]
fn live_owner_refuses_pre_handshake_ipc_shutdown() {
    check_live_owner_refuses_ordinary_shutdown(false);
}

#[test]
fn live_owner_refuses_handshaken_ipc_shutdown() {
    check_live_owner_refuses_ordinary_shutdown(true);
}

#[test]
fn owner_missing_grace_keeps_route_ready_and_returns_resource_unavailable() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_secs(5))
                .with_owner_bound(Duration::from_secs(3)),
        )
        .unwrap();
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("owner-missing-ready");
    let calls = Arc::new(AtomicU64::new(0));
    let contract = release();
    let registration = host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                &route,
                methods(),
                Arc::new(CountedEcho(calls.clone())),
            )
            .unwrap(),
        )
        .unwrap();
    let original = registration.outcome().clone();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let client = direct_client(&runtime, &route).unwrap().with_call_options(
            c2_config::CallOptions::with_timeout(c2_config::CallTimeout::After(
                Duration::from_millis(500),
            )),
        );
        let before = client.call_owned("echo", b"before");
        let before_count = calls.load(Ordering::SeqCst);
        keepalive.close();
        wait_for_phase(
            &host,
            |phase| *phase == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let call = client.call_owned("echo", b"must-not-execute");
        // Core acquisition performs a live contract-scoped catalog lookup,
        // even on a pooled connection, and only binds a Ready route. This
        // checks current authority rather than the old client's identity cache.
        let acquired = runtime
            .connect_with_options(
                contract.expected_route(&route).unwrap(),
                Connect::DirectIpc {
                    address: endpoint.address().to_string(),
                },
                c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(500)),
            )
            .map(|client| client.observed_route().clone());
        (
            before,
            before_count,
            call,
            acquired,
            calls.load(Ordering::SeqCst),
            host.lifecycle_snapshot(),
        )
    }));

    let shutdown = host.shutdown();
    let stopped = host.lifecycle_snapshot();
    let drained_count = calls.load(Ordering::SeqCst);
    drop(registration);
    keepalive.close();
    drop(host);
    drop(runtime);
    let endpoint_after_stop = c2_local::inspect_endpoint(&endpoint);

    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    assert!(shutdown.route_close_error.is_none(), "{shutdown:?}");
    assert!(shutdown.ipc_client_close_error.is_none(), "{shutdown:?}");
    assert!(stopped.listener_closed);
    assert_eq!(stopped.work_drained, Some(true));
    assert!(
        matches!(
            endpoint_after_stop,
            c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
        ),
        "{endpoint_after_stop:?}"
    );
    let (before, before_count, call, acquired, after_count, snapshot) =
        observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_eq!(before.unwrap(), b"before");
    assert_eq!(
        before_count, 1,
        "positive control must execute one callback"
    );
    assert_eq!(snapshot.phase, HostLifecyclePhase::OwnerMissing);
    assert!(!snapshot.listener_closed);
    assert_eq!(
        after_count, before_count,
        "paused work reached the callback"
    );
    assert_eq!(
        drained_count, before_count,
        "native drain observed a late callback after refusal"
    );
    assert!(
        matches!(&call, Err(Error::Semantic(error)) if error.code == ErrorCode::ResourceUnavailable),
        "grace must refuse calls with ResourceUnavailable and retain Ready: call={call:?}, acquisition={acquired:?}"
    );
    let acquired = acquired.expect("grace must retain an authoritatively Ready route");
    assert_eq!(acquired.route_uid, original.route_uid);
    assert_eq!(acquired.route_revision, original.route_revision);
}

#[test]
fn owner_recovery_rotates_grants_and_resumes_the_same_route_and_clients() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let controller_runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("takeover-identity");
    let registration = register_echo(&host, &route);
    let original = registration.outcome().clone();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let client = direct_client(&runtime, &route).unwrap();
        assert_eq!(client.call_owned("echo", b"before").unwrap(), b"before");
        let revoked = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        let repeated = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        // Never put exported capability bytes in an assertion's diagnostic.
        assert!(revoked == repeated, "get must return the current grant");
        owner.revoke_recovery(CONTROL_TIMEOUT).unwrap();
        owner.revoke_recovery(CONTROL_TIMEOUT).unwrap();
        let current = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        assert!(
            current != revoked,
            "reissue after revoke must rotate the grant"
        );

        let armed = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        let initial = armed.inspect(CONTROL_TIMEOUT).unwrap();
        assert_eq!(initial.phase, OwnerPhase::Armed);
        let error = armed
            .commit(CONTROL_TIMEOUT)
            .err()
            .expect("live owner must refuse takeover");
        assert_eq!(error.kind(), OwnerControlErrorKind::OwnerPresent);
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);

        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let paused = client.call_owned("echo", b"paused");
        assert!(
            matches!(paused, Err(Error::Semantic(e)) if e.code == ErrorCode::ResourceUnavailable)
        );
        let invalid = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&revoked).unwrap())
            .unwrap();
        let error = invalid
            .commit(CONTROL_TIMEOUT)
            .err()
            .expect("revoked grant must be refused");
        assert_eq!(error.kind(), OwnerControlErrorKind::InvalidGrant);
        assert_eq!(
            host.lifecycle_snapshot().phase,
            HostLifecyclePhase::OwnerMissing
        );

        let prepared = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        // Application-side persistence happens before the one commit attempt.
        let successor = prepared.successor_ticket().export();
        let missing = prepared.inspect(CONTROL_TIMEOUT).unwrap();
        assert_eq!(missing.phase, OwnerPhase::OwnerMissing);
        assert_eq!(missing.epoch, initial.epoch);
        let replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::Armed,
            Duration::from_secs(2),
        );
        assert!(
            replacement
                .recovery_ticket(CONTROL_TIMEOUT)
                .unwrap()
                .export()
                == successor,
            "committed successor must be the installed grant"
        );
        assert_eq!(
            client.call_owned("echo", b"old-client").unwrap(),
            b"old-client"
        );
        let new_client = controller_runtime
            .connect(
                release().expected_route(&route).unwrap(),
                Connect::DirectIpc {
                    address: endpoint.address().into(),
                },
            )
            .unwrap();
        assert_eq!(
            new_client.call_owned("echo", b"new-client").unwrap(),
            b"new-client"
        );
        for observed in [client.observed_route(), new_client.observed_route()] {
            assert_eq!(observed.route_uid, original.route_uid);
            assert_eq!(observed.route_revision, original.route_revision);
        }
        assert!(owner.revoke_recovery(CONTROL_TIMEOUT).is_err());
        assert!(owner.request_shutdown(CONTROL_TIMEOUT).is_err());
        owner.close(); // A stale local close cannot affect the replacement.
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
        assert_eq!(
            new_client.call_owned("echo", b"after-stale-owner").unwrap(),
            b"after-stale-owner"
        );

        replacement.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        // Replay is checked in Missing, so OwnerPresent cannot mask a consumed grant.
        let replay = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&current).unwrap())
            .unwrap();
        assert_eq!(
            replay
                .commit(CONTROL_TIMEOUT)
                .err()
                .expect("consumed grant")
                .kind(),
            OwnerControlErrorKind::InvalidGrant
        );
        let next = controller_runtime
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&successor).unwrap())
            .unwrap();
        let status = next.inspect(CONTROL_TIMEOUT).unwrap();
        assert_eq!(status.phase, OwnerPhase::OwnerMissing);
        assert_eq!(status.epoch, initial.epoch + 1);
        let _next_owner = next.commit(CONTROL_TIMEOUT).unwrap();
        assert_eq!(
            client.call_owned("echo", b"successor").unwrap(),
            b"successor"
        );
    }));
    let shutdown = host.shutdown();
    drop(registration);
    owner.close();
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn competing_owner_takeovers_commit_once_and_install_only_the_winners_successor() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let contenders = [runtime_in(&root), runtime_in(&root)];
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("takeover-race");
    let registration = register_echo(&host, &route);
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bytes = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        let first = contenders[0]
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&bytes).unwrap())
            .unwrap();
        let second = contenders[1]
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&bytes).unwrap())
            .unwrap();
        let successors = [
            first.successor_ticket().export(),
            second.successor_ticket().export(),
        ];
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                first.commit(CONTROL_TIMEOUT)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                second.commit(CONTROL_TIMEOUT)
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let mut winner = None;
        let mut winning_index = 0;
        for (index, result) in results.into_iter().enumerate() {
            match result {
                Ok(control) => {
                    winner = Some(control);
                    winning_index = index;
                }
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        OwnerControlErrorKind::OwnerPresent | OwnerControlErrorKind::InvalidGrant
                    ),
                    "contender rejection: {error:?}"
                ),
            }
        }
        let winner = winner.unwrap();
        assert!(
            winner.recovery_ticket(CONTROL_TIMEOUT).unwrap().export() == successors[winning_index],
            "losing contender must not install its successor"
        );
        assert_eq!(
            direct_client(&runtime, &route)
                .unwrap()
                .call_owned("echo", b"one-winner")
                .unwrap(),
            b"one-winner"
        );
        winner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let losing_successor = contenders[0]
            .prepare_owner_takeover(
                OwnerRecoveryTicket::import(&successors[1 - winning_index]).unwrap(),
            )
            .unwrap();
        let rejection = losing_successor
            .commit(CONTROL_TIMEOUT)
            .err()
            .expect("uninstalled successor");
        assert_eq!(
            rejection.kind(),
            OwnerControlErrorKind::InvalidGrant,
            "uninstalled successor rejection: {rejection:?}"
        );
        let winning_successor = contenders[1]
            .prepare_owner_takeover(
                OwnerRecoveryTicket::import(&successors[winning_index]).unwrap(),
            )
            .unwrap();
        let _next_owner = winning_successor.commit(CONTROL_TIMEOUT).unwrap();
        assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
    }));
    let shutdown = host.shutdown();
    drop(registration);
    drop(host);
    drop(contenders);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_takeover_resumes_local_concurrency_without_reopening_unregistered_routes() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let controller_runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("takeover-concurrency");
    let contract = release();
    let registration = host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                &route,
                methods(),
                Arc::new(Echo),
            )
            .unwrap()
            .with_concurrency(ServiceConcurrencyMode::Parallel, None, None)
            .unwrap(),
        )
        .unwrap();
    let mut closed = register_echo(&host, &unique_name("takeover-close"));
    let mut removed = Some(register_echo(&host, &unique_name("takeover-unregister")));
    let closed_handle = closed.route_concurrency();
    let removed_handle = removed.as_ref().unwrap().route_concurrency();
    let closed_name = closed.route_name().to_owned();
    let removed_name = removed.as_ref().unwrap().route_name().to_owned();
    let concurrency = registration.route_concurrency();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let before = concurrency.snapshot();
        assert!(!before.closed);
        let mut stale_permit = concurrency.blocking_acquire(1).unwrap();
        let ticket = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        assert_eq!(
            concurrency.blocking_acquire(1).unwrap_err(),
            RouteConcurrencyError::Unavailable
        );
        assert!(concurrency.snapshot().closed);
        assert!(!concurrency.snapshot().is_unconstrained);
        assert!(!concurrency.is_unconstrained());
        closed.close().unwrap();
        drop(removed.take());
        let prepared = controller_runtime.prepare_owner_takeover(ticket).unwrap();
        let _replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        // A permit acquired before the pause never becomes valid after resume.
        assert_eq!(
            stale_permit.commit().unwrap_err(),
            RouteConcurrencyError::Unavailable
        );
        drop(stale_permit);
        let snapshot = concurrency.snapshot();
        assert!(!snapshot.closed);
        assert_eq!(snapshot.is_unconstrained, before.is_unconstrained);
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.active_workers, 0);
        let mut fresh_permit = concurrency.blocking_acquire(1).unwrap();
        fresh_permit.commit().unwrap();
        drop(fresh_permit);
        for handle in [&closed_handle, &removed_handle] {
            assert!(handle.snapshot().closed);
            assert_eq!(
                handle.blocking_acquire(1).unwrap_err(),
                RouteConcurrencyError::Closed
            );
        }
        for name in [&closed_name, &removed_name] {
            assert!(
                direct_client(&runtime, name).is_err(),
                "unregistered route was resurrected"
            );
        }
        assert_eq!(
            direct_client(&runtime, &route)
                .unwrap()
                .call_owned("echo", b"resumed")
                .unwrap(),
            b"resumed"
        );
    }));
    let shutdown = host.shutdown();
    drop(removed);
    drop(closed);
    drop(registration);
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
#[cfg(unix)]
fn owner_ticket_rejects_another_private_root_with_the_same_logical_address() {
    let roots = [private_root(), private_root()];
    let runtimes = [runtime_in(&roots[0]), runtime_in(&roots[1])];
    let server_id = unique_name("takeover-same-address");
    for runtime in &runtimes {
        runtime
            .set_server_options(Some(server_id.clone()), None, None)
            .unwrap();
    }
    let (owner_a, bootstrap_a) = owner_control_pair().unwrap();
    let (owner_b, bootstrap_b) = owner_control_pair().unwrap();
    runtimes[0].attach_owner_control(bootstrap_a).unwrap();
    runtimes[1].attach_owner_control(bootstrap_b).unwrap();
    let hosts = runtimes.each_ref().map(|runtime| {
        runtime
            .host(
                HostOptions::default()
                    .without_relay()
                    .with_owner_bound(Duration::from_secs(3)),
            )
            .unwrap()
    });
    let endpoints = hosts.each_ref().map(|host| host.local_endpoint().clone());
    let route = unique_name("takeover-two-roots");
    let registrations = hosts.each_ref().map(|host| register_echo(host, &route));
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(endpoints[0].address(), endpoints[1].address());
        let ticket_a = owner_a.recovery_ticket(CONTROL_TIMEOUT).unwrap().export();
        let ticket_b = owner_b.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        owner_a.close();
        owner_b.close();
        for host in &hosts {
            wait_for_phase(
                host,
                |p| *p == HostLifecyclePhase::OwnerMissing,
                Duration::from_secs(2),
            );
        }
        let rejection = match runtimes[1]
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&ticket_a).unwrap())
        {
            Err(error) => error.kind(),
            Ok(prepared) => prepared
                .commit(CONTROL_TIMEOUT)
                .err()
                .expect("wrong root")
                .kind(),
        };
        assert_eq!(rejection, OwnerControlErrorKind::IdentityMismatch);
        for host in &hosts {
            assert_eq!(
                host.lifecycle_snapshot().phase,
                HostLifecyclePhase::OwnerMissing
            );
        }
        let prepared_a = runtimes[0]
            .prepare_owner_takeover(OwnerRecoveryTicket::import(&ticket_a).unwrap())
            .unwrap();
        let prepared_b = runtimes[1].prepare_owner_takeover(ticket_b).unwrap();
        let _owner_a = prepared_a.commit(CONTROL_TIMEOUT).unwrap();
        let _owner_b = prepared_b.commit(CONTROL_TIMEOUT).unwrap();
        for runtime in &runtimes {
            assert_eq!(
                direct_client(runtime, &route)
                    .unwrap()
                    .call_owned("echo", b"own-root")
                    .unwrap(),
                b"own-root"
            );
        }
    }));
    let shutdowns = hosts.each_ref().map(|host| host.shutdown());
    drop(registrations);
    drop(hosts);
    drop(runtimes);
    for endpoint in &endpoints {
        assert_endpoint_gone(endpoint);
    }
    for shutdown in shutdowns {
        assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    }
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[cfg(unix)]
fn runtime_at(root: &std::path::Path) -> Runtime {
    let runtime = test_runtime();
    runtime
        .set_local_endpoint_with_sources(
            c2_config::LocalEndpointOptions {
                unix_root: Some(root.into()),
            },
            c2_config::ConfigSources::empty(),
        )
        .unwrap();
    runtime
}

#[test]
#[cfg(unix)]
fn owner_takeover_accepts_a_symlink_alias_of_the_captured_directory() {
    let root = private_root();
    let parent = root.path().join("p");
    let actual = parent.join("r");
    let alias_parent = root.path().join("a");
    std::fs::create_dir_all(&actual).unwrap();
    // The original endpoint policy permits parent-path aliases (such as
    // /tmp and /private/tmp), while the final directory remains no-follow.
    std::os::unix::fs::symlink(&parent, &alias_parent).unwrap();
    let alias = alias_parent.join("r");
    let runtime = runtime_at(&actual);
    let controller_runtime = runtime_at(&alias);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("takeover-alias");
    let registration = register_echo(&host, &route);
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ticket = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let prepared = controller_runtime.prepare_owner_takeover(ticket).unwrap();
        assert_eq!(
            prepared.inspect(CONTROL_TIMEOUT).unwrap().phase,
            OwnerPhase::OwnerMissing
        );
        let _replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        let client = controller_runtime
            .connect(
                release().expected_route(&route).unwrap(),
                Connect::DirectIpc {
                    address: endpoint.address().into(),
                },
            )
            .unwrap();
        assert_eq!(
            client.call_owned("echo", b"same-directory").unwrap(),
            b"same-directory"
        );
    }));
    let shutdown = host.shutdown();
    drop(registration);
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
#[cfg(unix)]
fn prepared_owner_takeover_refuses_a_replaced_directory_before_connecting() {
    let root = private_root();
    let active = root.path().join("r");
    let parked = root.path().join("old");
    let replacement = root.path().join("new");
    std::fs::create_dir(&active).unwrap();
    let runtime = runtime_at(&active);
    let controller_runtime = runtime_at(&active);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let registration = register_echo(&host, &unique_name("takeover-replaced-root"));
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ticket = owner.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        let prepared = controller_runtime.prepare_owner_takeover(ticket).unwrap();
        std::fs::rename(&active, &parked).unwrap();
        std::fs::create_dir(&active).unwrap();
        owner.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        let error = prepared
            .commit(CONTROL_TIMEOUT)
            .err()
            .expect("replaced captured directory");
        assert_eq!(error.kind(), OwnerControlErrorKind::NotSent);
        assert_eq!(
            error.cause_kind(),
            Some(OwnerControlErrorKind::IdentityMismatch)
        );
        assert_eq!(
            std::fs::read_dir(&active).unwrap().count(),
            0,
            "a captured ticket must not initialize the replacement namespace"
        );
        assert_eq!(
            host.lifecycle_snapshot().phase,
            HostLifecyclePhase::OwnerMissing
        );
    }));
    // Restore the exact original directory before native close. Keep the
    // replacement for inspection rather than hiding endpoint leaks by deletion.
    let restored = (|| -> std::io::Result<()> {
        if parked.exists() {
            if active.exists() {
                std::fs::rename(&active, &replacement)?;
            }
            std::fs::rename(&parked, &active)?;
        }
        Ok(())
    })();
    let shutdown = host.shutdown();
    drop(registration);
    drop(host);
    drop(controller_runtime);
    drop(runtime);
    restored.unwrap();
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_eof_closes_admission_immediately_and_stops_after_grace() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_secs(5))
                .with_owner_bound(Duration::from_secs(3)),
        )
        .expect("host");
    let route_name = unique_name("owner-eof");
    let _registration = register_echo(&host, &route_name);
    assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);

    let client = direct_client(&runtime, &route_name).expect("client");
    assert_eq!(
        client.call_owned("echo", b"before").expect("call"),
        b"before"
    );

    let started = Instant::now();
    keepalive.close();
    // Admission closes immediately, long before the 3s grace expires.
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::OwnerMissing,
        Duration::from_secs(2),
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "admission must close immediately, not after the grace window"
    );
    // New business is refused while the grace window is still running.
    let rejected = direct_client(&runtime, &route_name)
        .and_then(|client| client.call_owned("echo", b"rejected"));
    assert!(
        rejected.is_err(),
        "new business must be refused during OwnerMissing"
    );

    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(10),
    );
    let snapshot = host.lifecycle_snapshot();
    assert!(snapshot.listener_closed);
    assert_eq!(snapshot.work_drained, Some(true));
    let outcome = host.shutdown();
    assert!(outcome.removed_routes.contains(&route_name));
    assert!(
        outcome
            .route_outcomes
            .iter()
            .any(|close| close.closed_reason == "owner_bound_grace_expired"),
        "route journal must record the owner-bound reason: {outcome:?}"
    );
    // Explicit shutdown after the owner-driven transaction is idempotent.
    assert_eq!(host.shutdown(), outcome);
}

mod relay_final_admission {
    use super::*;
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use std::sync::mpsc;
    use tokio::sync::{Notify, oneshot};

    #[derive(Clone)]
    struct FinalGate {
        seen: mpsc::Sender<serde_json::Value>,
        release: Arc<Notify>,
        status: StatusCode,
    }

    async fn register(
        State(gate): State<FinalGate>,
        Json(body): Json<serde_json::Value>,
    ) -> StatusCode {
        if body["prepare_only"] == true {
            return StatusCode::ACCEPTED;
        }
        let _ = gate.seen.send(body);
        gate.release.notified().await;
        gate.status
    }

    struct DelayedFinalRelay {
        url: String,
        seen: mpsc::Receiver<serde_json::Value>,
        release: Arc<Notify>,
        stop: Option<oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl DelayedFinalRelay {
        fn start(status: StatusCode) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (seen_tx, seen) = mpsc::channel();
            let release = Arc::new(Notify::new());
            let gate = FinalGate {
                seen: seen_tx,
                release: release.clone(),
                status,
            };
            let (stop_tx, stop_rx) = oneshot::channel();
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let app = Router::new()
                        .route("/_register", post(register))
                        .route("/_unregister", post(|| async { StatusCode::OK }))
                        .with_state(gate);
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stop_rx.await;
                        })
                        .await
                        .unwrap();
                });
            });
            Self {
                url,
                seen,
                release,
                stop: Some(stop_tx),
                thread: Some(thread),
            }
        }

        fn stop(&mut self) -> std::thread::Result<()> {
            // Also release a pending final handler when a setup assertion panics.
            self.release.notify_one();
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            self.thread.take().map_or(Ok(()), |thread| thread.join())
        }
    }

    impl Drop for DelayedFinalRelay {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn check_final_response(lose_owner: bool, status: StatusCode) {
        let mut relay = DelayedFinalRelay::start(status);
        let runtime = test_runtime();
        #[cfg(unix)]
        let directory = tempfile::Builder::new()
            .prefix("c2-owner-final-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        runtime
            .set_local_endpoint(c2_config::LocalEndpointOptions {
                unix_root: Some(directory.path().to_path_buf()),
            })
            .unwrap();
        let (keepalive, receiver) = owner_control_pair().unwrap();
        runtime.attach_owner_control(receiver).unwrap();
        let host = runtime
            .host(
                HostOptions::default()
                    .with_relay_anchor_address(&relay.url)
                    .with_relay_use_proxy(false)
                    .with_owner_bound(Duration::from_secs(30)),
            )
            .unwrap();
        let endpoint = host.local_endpoint().clone();
        let route = unique_name("owner-relay-final");
        let calls = Arc::new(AtomicU64::new(0));
        let contract = release();
        let definition = ServiceDefinition::new(
            &contract,
            contract.reference(),
            &route,
            methods(),
            Arc::new(CountedEcho(calls.clone())),
        )
        .unwrap();
        let registering_host = host.clone();
        let registration_thread = std::thread::spawn(move || registering_host.register(definition));

        // Preserve a setup failure until after final response, native shutdown,
        // and both threads have completed. The expected red assertions are below.
        let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let request = relay.seen.recv_timeout(Duration::from_secs(5)).unwrap();
            let refused_before_final = direct_client(&runtime, &route)
                .and_then(|client| client.call_owned("echo", b"before-final"))
                .is_err();
            if lose_owner {
                keepalive.close();
                wait_for_phase(
                    &host,
                    |phase| *phase == HostLifecyclePhase::OwnerMissing,
                    Duration::from_secs(3),
                );
            }
            (
                request,
                refused_before_final,
                host.lifecycle_snapshot().phase,
            )
        }));
        relay.release.notify_one();
        let registration = registration_thread.join();
        let phase_after_final = host.lifecycle_snapshot().phase;
        let call = direct_client(&runtime, &route)
            .and_then(|client| client.call_owned("echo", b"after-final"));
        let callback_count = calls.load(Ordering::SeqCst);
        let shutdown = host.shutdown();
        let stopped = host.lifecycle_snapshot();
        // Drop Registration while Host and the relay still exist, after the
        // explicit native drain. Keep only the outcome for assertions.
        let registration = registration.map(|result| result.map(|r| r.outcome().clone()));
        keepalive.close();
        drop(host);
        drop(runtime);
        let relay_result = relay.stop();
        let endpoint_after_stop = c2_local::inspect_endpoint(&endpoint);

        relay_result.unwrap();
        assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
        assert!(shutdown.route_close_error.is_none(), "{shutdown:?}");
        assert!(shutdown.relay_errors.is_empty(), "{shutdown:?}");
        assert!(shutdown.ipc_client_close_error.is_none(), "{shutdown:?}");
        assert!(stopped.listener_closed);
        assert_eq!(stopped.work_drained, Some(true));
        assert!(
            matches!(
                endpoint_after_stop,
                c2_local::EndpointInspection::Absent | c2_local::EndpointInspection::KernelManaged
            ),
            "{endpoint_after_stop:?}"
        );
        let (request, refused_before_final, phase_before_final) =
            observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let registration = registration.unwrap();
        assert_ne!(request["prepare_only"], true);
        assert_eq!(request["name"], route);
        assert!(
            refused_before_final,
            "pending final publish cannot admit IPC work"
        );
        let expected_phase = if lose_owner {
            HostLifecyclePhase::OwnerMissing
        } else {
            HostLifecyclePhase::Armed
        };
        assert_eq!(phase_before_final, expected_phase);
        assert_eq!(phase_after_final, expected_phase);
        if !lose_owner && status == StatusCode::OK {
            assert!(registration.is_ok(), "{registration:?}");
            assert_eq!(call.unwrap(), b"after-final");
            assert_eq!(callback_count, 1);
        } else {
            assert!(
                call.is_err(),
                "late final response admitted business work: {call:?}"
            );
            assert_eq!(
                callback_count, 0,
                "refused business work must never reach the callback"
            );
            let Err(Error::Lifecycle(c2_core::LifecycleError::RegisterFailure(failure))) =
                registration
            else {
                panic!("expected structured registration failure: {registration:?}");
            };
            assert_eq!(
                failure.failure_source,
                if lose_owner {
                    "route_open"
                } else {
                    "relay_register"
                }
            );
            assert_eq!(
                failure.status_code,
                if lose_owner { None } else { Some(409) }
            );
        }
    }

    #[test]
    fn owner_missing_relay_final_200_cannot_reopen_ipc_admission() {
        check_final_response(true, StatusCode::OK);
    }

    #[test]
    fn live_owner_relay_final_200_opens_ipc_admission() {
        check_final_response(false, StatusCode::OK);
    }

    #[test]
    fn live_owner_relay_final_409_keeps_ipc_admission_closed() {
        check_final_response(false, StatusCode::CONFLICT);
    }
}

#[test]
fn inflight_callback_completes_across_owner_eof() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_secs(5))
                .with_owner_bound(Duration::ZERO),
        )
        .expect("host");
    let route_name = unique_name("inflight");
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let release = release();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        &route_name,
        methods(),
        Arc::new(GatedEcho {
            gate: gate.clone(),
            entered: entered_tx,
        }),
    )
    .expect("definition");
    let _registration = host.register(definition).expect("register route");

    let worker_runtime = runtime.clone();
    let worker_route = route_name.clone();
    let worker = std::thread::spawn(move || {
        let client = direct_client(&worker_runtime, &worker_route).expect("client");
        client
            .call_owned("echo", b"inflight")
            .map(|bytes| bytes.to_vec())
            .expect("in-flight call must complete")
    });

    let _release_on_panic = ReleaseGate(gate.clone());
    entered_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("callback entered barrier");
    keepalive.close();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Draining || *p == HostLifecyclePhase::Finished,
        Duration::from_secs(5),
    );

    // Release the gated callback; the in-flight request must complete under
    // the original drain rules, not be cancelled by the owner EOF.
    {
        let (lock, cvar) = &*gate;
        let mut ready = lock.lock().unwrap();
        *ready = true;
        cvar.notify_all();
    }
    let response = worker.join().expect("worker");
    assert_eq!(response, b"inflight");
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(5),
    );
}

#[test]
fn explicit_shutdown_racing_owner_eof_produces_one_outcome() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_secs(5))
                .with_owner_bound(Duration::from_secs(30)),
        )
        .expect("host");
    let route_name = unique_name("race");
    let _registration = register_echo(&host, &route_name);

    keepalive.close();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::OwnerMissing,
        Duration::from_secs(2),
    );
    // Explicit shutdown must not wait out the 30s grace window.
    let started = Instant::now();
    let outcome = host.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "explicit shutdown must win the race against the grace window"
    );
    assert!(outcome.removed_routes.contains(&route_name));
    assert_eq!(host.shutdown(), outcome);
    assert_eq!(
        host.lifecycle_snapshot().phase,
        HostLifecyclePhase::Finished
    );
}

#[test]
fn dropping_host_during_grace_is_bounded_and_not_sustained_by_the_watcher() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_startup_timeout(Duration::from_secs(5))
                .with_shutdown_timeout(Duration::from_millis(500))
                .with_owner_bound(Duration::from_secs(30)),
        )
        .expect("host");

    keepalive.close();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::OwnerMissing,
        Duration::from_secs(2),
    );
    let started = Instant::now();
    drop(host);
    // The watcher must not keep the dropped host's thread alive through the
    // 30s grace window; Drop runs its own bounded shutdown transaction.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "host drop must be bounded by the shutdown timeout, not the grace window"
    );
}

#[test]
fn business_client_disconnects_do_not_trigger_owner_bound_shutdown() {
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::from_secs(1)),
        )
        .expect("host");
    let route_name = unique_name("idle");
    let _registration = register_echo(&host, &route_name);

    for _ in 0..2 {
        let client = direct_client(&runtime, &route_name).expect("client");
        assert_eq!(client.call_owned("echo", b"work").expect("call"), b"work");
        drop(client);
    }
    // All business streams are gone and the grace window (1s) has elapsed;
    // the owner-bound host stays armed and keeps serving.
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(host.lifecycle_snapshot().phase, HostLifecyclePhase::Armed);
    let client = direct_client(&runtime, &route_name).expect("client still served");
    assert_eq!(
        client.call_owned("echo", b"still-up").expect("call"),
        b"still-up"
    );

    drop(keepalive);
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(5),
    );
}

// ---------------------------------------------------------------------------
// Real child-process fixtures: controller kill reaches the service only as a
// control-channel EOF.
// ---------------------------------------------------------------------------

fn write_fixture_stdout(bytes: &[u8]) {
    use std::io::Write;
    std::io::stdout().write_all(bytes).unwrap();
    std::io::stdout().flush().unwrap();
}

fn inherited_bootstrap(from_stderr: bool) -> c2_local::OwnerBootstrap {
    #[cfg(unix)]
    {
        // SAFETY: the fixture launcher deliberately installed its bootstrap as
        // stdin (service) or stderr (guardian), never a business connection.
        unsafe { c2_local::OwnerBootstrap::from_inherited_fd(if from_stderr { 2 } else { 0 }) }
            .expect("adopt explicitly inherited owner bootstrap")
    }
    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        // SAFETY: same explicit launcher contract, using the corresponding
        // inherited standard handle before any IOCP registration.
        let handle = if from_stderr {
            std::io::stderr().as_handle().as_raw_handle()
        } else {
            std::io::stdin().as_handle().as_raw_handle()
        };
        c2_local::OwnerBootstrap::from_inherited_handle(handle)
            .expect("adopt explicitly inherited owner bootstrap")
    }
}

fn fixture_runtime() -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(std::env::var(FIXTURE_SERVER_ID).expect("fixture server identity")),
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
    .unwrap();
    #[cfg(unix)]
    {
        let root = std::env::var_os(FIXTURE_ROOT).expect("fixture private endpoint root");
        runtime
            .set_local_endpoint_with_sources(
                c2_config::LocalEndpointOptions {
                    unix_root: Some(root.into()),
                },
                c2_config::ConfigSources::empty(),
            )
            .unwrap();
    }
    #[cfg(windows)]
    runtime
        .set_local_endpoint_with_sources(
            c2_config::LocalEndpointOptions::default(),
            c2_config::ConfigSources::empty(),
        )
        .unwrap();
    runtime
}

// The guardian is still the service's OS parent after the real controller
// dies. Its stdin is an anonymous cleanup pipe owned by the outer test; EOF
// makes it kill + wait, then reap only the credential captured at readiness.
// Its stderr transports the bootstrap once; stdout goes to a separate captured
// log pipe, so libtest output cannot corrupt the bootstrap or exported ticket.
// This avoids orphan cleanup based merely on a PID or a marker.
fn takeover_service_guardian(recovery: bool) {
    use std::io::Read;
    let mut bootstrap = inherited_bootstrap(true);
    let runtime = fixture_runtime();
    let address = c2_core::ipc_address_for_server_id(&std::env::var(FIXTURE_SERVER_ID).unwrap());
    let endpoint = runtime.local_endpoint(&address).unwrap();
    let mut service = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owner_bound_fixture", "--nocapture"])
            .env(
                FIXTURE_ACTION,
                if recovery {
                    "takeover-service"
                } else {
                    "service"
                },
            )
            .stdin(bootstrap.take_stdio().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = service.0.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line == "SERVICE_READY" {
                let _ = ready_tx.send(());
            }
            if matches!(
                line.as_str(),
                "SERVICE_READY" | "SERVICE_MISSING" | "SERVICE_FINISHED"
            ) {
                write_fixture_stdout(format!("{line}\n").as_bytes());
            }
        }
    });
    #[cfg(unix)]
    let mut credential = None;
    #[cfg(windows)]
    let credential = None;
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match c2_local::inspect_endpoint(&endpoint) {
                    c2_local::EndpointInspection::Present(value) => {
                        credential = Some(value);
                        break;
                    }
                    c2_local::EndpointInspection::IoError(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    other => panic!("cannot capture this test's ready endpoint: {other:?}"),
                }
            }
        }
        write_fixture_stdout(b"SERVICE_GUARDED\n");
        let mut byte = [0];
        assert_eq!(std::io::stdin().read(&mut byte).unwrap(), 0);
    }));
    let _ = service.0.kill();
    let waited = service.0.wait();
    drop(service);
    let reaped = if waited.is_ok() {
        credential
            .as_ref()
            .map(|value: &c2_local::EndpointCredential| c2_local::reap_endpoint(&endpoint, value))
    } else {
        None
    };
    drop(runtime);
    reader.join().unwrap();
    waited.unwrap();
    if let Some(reaped) = reaped {
        assert!(
            matches!(
                reaped,
                c2_local::EndpointReapResult::Reaped | c2_local::EndpointReapResult::AlreadyAbsent
            ),
            "{reaped:?}"
        );
    }
    assert_endpoint_gone(&endpoint);
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    write_fixture_stdout(b"SERVICE_REAPED\n");
}

#[test]
fn owner_bound_fixture() {
    let Ok(action) = std::env::var(FIXTURE_ACTION) else {
        return;
    };
    match action.as_str() {
        "preclosed" => {
            assert_already_gone_is_refused();
            write_fixture_stdout(b"PRE_READY_REFUSED\n");
        }
        "controller" | "takeover-controller" => {
            let (owner, mut bootstrap) = owner_control_pair().unwrap();
            #[cfg(unix)]
            let log = {
                use std::os::fd::AsFd;
                Stdio::from(std::io::stderr().as_fd().try_clone_to_owned().unwrap())
            };
            #[cfg(windows)]
            let log = {
                use std::os::windows::io::AsHandle;
                Stdio::from(std::io::stderr().as_handle().try_clone_to_owned().unwrap())
            };
            // The outer cleanup pipe also governs a controller panic. A
            // controller-side kill guard would wrongly kill the guardian
            // before it could wait for and reap its service child.
            let _guardian = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "owner_bound_fixture", "--nocapture"])
                .env(
                    FIXTURE_ACTION,
                    if action == "takeover-controller" {
                        "takeover-guardian"
                    } else {
                        "service-guardian"
                    },
                )
                .stdin(Stdio::inherit())
                .stdout(log)
                .stderr(bootstrap.take_stdio().unwrap())
                .spawn()
                .unwrap();
            if action == "takeover-controller" {
                let bytes = owner
                    .recovery_ticket(Some(Duration::from_secs(5)))
                    .unwrap()
                    .export();
                // Explicit application export only: no argv/env, files or test
                // diagnostics contain these bytes. The outer test parses its pipe.
                let line = format!(
                    "RECOVERY_TICKET:{}\n",
                    serde_json::to_string(&bytes).unwrap()
                );
                write_fixture_stdout(line.as_bytes());
            }
            std::thread::sleep(Duration::from_secs(60));
            drop(owner);
        }
        "service-guardian" | "takeover-guardian" => {
            takeover_service_guardian(action == "takeover-guardian")
        }
        "service" | "takeover-service" => {
            let receiver = inherited_bootstrap(false);
            let runtime = fixture_runtime();
            runtime.attach_owner_control(receiver).expect("attach");
            let host =
                runtime
                    .host(
                        HostOptions::default()
                            .without_relay()
                            .with_startup_timeout(Duration::from_secs(5))
                            .with_shutdown_timeout(Duration::from_secs(2))
                            .with_owner_bound(Duration::from_secs(
                                if action == "takeover-service" { 3 } else { 1 },
                            )),
                    )
                    .expect("owner-bound host");
            let endpoint = host.local_endpoint().clone();
            let registration =
                (action == "takeover-service").then(|| register_echo(&host, "fixture-echo"));
            let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_fixture_stdout(b"SERVICE_READY\n");
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut missing_reported = false;
                loop {
                    if !missing_reported
                        && host.lifecycle_snapshot().phase == HostLifecyclePhase::OwnerMissing
                    {
                        write_fixture_stdout(b"SERVICE_MISSING\n");
                        missing_reported = true;
                    }
                    if host.shutdown_outcome().is_some_and(|outcome| {
                        outcome.runtime_barrier_error.is_none()
                            && outcome.route_close_error.is_none()
                    }) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "service never finished after EOF"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
            }));
            let shutdown = host.shutdown();
            drop(registration);
            drop(host);
            drop(runtime);
            assert_endpoint_gone(&endpoint);
            assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
            observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            write_fixture_stdout(b"SERVICE_FINISHED\n");
        }
        unknown => panic!("unknown child fixture action: {unknown}"),
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn controller_kill_triggers_owner_bound_shutdown_in_the_service_process() {
    check_controller_exit(false);
}

#[test]
fn killed_controller_can_be_replaced_using_only_its_explicitly_exported_ticket() {
    check_controller_exit(true);
}

fn check_controller_exit(recovery: bool) {
    enum Event {
        Marker(String),
        Ticket(Vec<u8>),
    }
    let root = private_root();
    let runtime = runtime_in(&root);
    let server_id = unique_name("takeover-controller-kill");
    let address = c2_core::ipc_address_for_server_id(&server_id);
    let endpoint = runtime.local_endpoint(&address).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "owner_bound_fixture", "--nocapture"])
        .env(
            FIXTURE_ACTION,
            if recovery {
                "takeover-controller"
            } else {
                "controller"
            },
        )
        .env(FIXTURE_SERVER_ID, server_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.env(FIXTURE_ROOT, root.path());
    let mut controller = ChildGuard(command.spawn().unwrap());
    let cleanup = controller.0.stdin.take().unwrap();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(controller.0.stdout.take().unwrap()),
        Box::new(controller.0.stderr.take().unwrap()),
    ];
    let (tx, rx) = std::sync::mpsc::channel();
    let readers = streams.map(|stream| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let event = if let Some(json) = line.strip_prefix("RECOVERY_TICKET:") {
                    // Do not forward, debug-format or include this line in a panic.
                    let Ok(bytes) = serde_json::from_str::<Vec<u8>>(json) else {
                        return;
                    };
                    Event::Ticket(bytes)
                } else if matches!(
                    line.as_str(),
                    "SERVICE_GUARDED" | "SERVICE_MISSING" | "SERVICE_FINISHED" | "SERVICE_REAPED"
                ) {
                    Event::Marker(line)
                } else {
                    continue;
                };
                if tx.send(event).is_err() {
                    return;
                }
            }
        })
    });
    drop(tx);
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut ticket = None;
        let mut guarded = false;
        while (recovery && ticket.is_none()) || !guarded {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Ticket(bytes) => ticket = Some(bytes),
                Event::Marker(marker) => guarded |= marker == "SERVICE_GUARDED",
            }
        }
        controller.0.kill().unwrap(); // SIGKILL on Unix, TerminateProcess on Windows.
        controller.0.wait().unwrap(); // Confirm actual controller exit before takeover.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(),
                Event::Marker(marker) if marker == "SERVICE_MISSING")
            {
                break;
            }
        }
        if recovery {
            let prepared = runtime
                .prepare_owner_takeover(OwnerRecoveryTicket::import(&ticket.unwrap()).unwrap())
                .unwrap();
            let _saved_successor = prepared.successor_ticket().export();
            assert_eq!(
                prepared.inspect(CONTROL_TIMEOUT).unwrap().phase,
                OwnerPhase::OwnerMissing
            );
            let replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
            let client = runtime
                .connect(
                    release().expected_route("fixture-echo").unwrap(),
                    Connect::DirectIpc { address },
                )
                .unwrap();
            assert_eq!(
                client.call_owned("echo", b"controller-replaced").unwrap(),
                b"controller-replaced"
            );
            replacement.request_shutdown(CONTROL_TIMEOUT).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if matches!(rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(),
                Event::Marker(marker) if marker == "SERVICE_FINISHED")
            {
                break;
            }
        }
    }));
    let _ = controller.0.kill();
    let waited = controller.0.wait();
    // Closing this nonsecret pipe also handles a red assertion: the service's
    // still-live parent kills + waits and performs exact native reap.
    drop(cleanup);
    let deadline = Instant::now() + Duration::from_secs(5);
    let reaped = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Marker(marker)) if marker == "SERVICE_REAPED" => break true,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    drop(controller);
    drop(runtime);
    for reader in readers {
        reader.join().unwrap();
    }
    waited.unwrap();
    assert!(
        reaped,
        "service guardian did not confirm OS wait and exact endpoint cleanup"
    );
    assert_endpoint_gone(&endpoint);
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn owner_request_shutdown_initiates_the_existing_drain_without_cancelling_active_work() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let client_runtime = runtime_in(&root);
    let (owner, host) = owner_host(&runtime, Duration::from_secs(3));
    let endpoint = host.local_endpoint().clone();
    let route = unique_name("owner-request-drain");
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let contract = release();
    let registration = host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                &route,
                methods(),
                Arc::new(GatedEcho {
                    gate: gate.clone(),
                    entered: entered_tx,
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let address = endpoint.address().to_owned();
    let worker_route = route.clone();
    let worker = std::thread::spawn(move || {
        let client = client_runtime
            .connect(
                release().expected_route(worker_route).unwrap(),
                Connect::DirectIpc { address },
            )
            .unwrap();
        client.call_owned("echo", b"owner-drain")
    });
    let release_gate = ReleaseGate(gate);
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        owner.request_shutdown(CONTROL_TIMEOUT).unwrap();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::Draining,
            Duration::from_secs(3),
        );
        assert!(
            host.shutdown_outcome().is_none(),
            "initiate ACK cannot prove callback drain"
        );
        let incomplete = host.shutdown_with_timeout(Duration::from_millis(100));
        assert!(incomplete.runtime_barrier_error.is_some());
        assert_ne!(host.lifecycle_snapshot().work_drained, Some(true));
    }));
    // Release before completing native shutdown, also on an assertion panic.
    drop(release_gate);
    let response = worker.join();
    let shutdown = host.shutdown();
    drop(registration);
    drop(host);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    assert!(
        shutdown
            .route_outcomes
            .iter()
            .any(|r| r.route_name == route && r.active_drained)
    );
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_eq!(response.unwrap().unwrap(), b"owner-drain");
}

#[test]
fn shutdown_deadline_does_not_cancel_pending_native_drain() {
    let runtime = test_runtime();
    let (keepalive, host) = owner_host(&runtime, Duration::ZERO);
    let route_name = unique_name("slow-drain");
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let contract = release();
    let _registration = host
        .register(
            ServiceDefinition::new(
                &contract,
                contract.reference(),
                &route_name,
                methods(),
                Arc::new(GatedEcho {
                    gate: gate.clone(),
                    entered: entered_tx,
                }),
            )
            .unwrap(),
        )
        .unwrap();
    // The business caller has its own Runtime so Host close does not close this in-flight client.
    let client_runtime = test_runtime();
    let address = runtime.server_address().unwrap();
    let route = route_name.clone();
    let worker = std::thread::spawn(move || {
        let client = client_runtime
            .connect(
                release().expected_route(route).unwrap(),
                Connect::DirectIpc { address },
            )
            .unwrap();
        client.call_owned("echo", b"retained-inflight").unwrap()
    });
    let _release_on_panic = ReleaseGate(gate.clone());
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    keepalive.close();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Draining,
        Duration::from_secs(3),
    );
    let started = Instant::now();
    let incomplete = host.shutdown_with_timeout(Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(incomplete.runtime_barrier_error.is_some());
    assert!(host.shutdown_outcome().is_none());
    assert_ne!(host.lifecycle_snapshot().work_drained, Some(true));
    let started = Instant::now();
    let again = host.shutdown_with_timeout(Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(again.runtime_barrier_error.is_some());
    {
        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    assert_eq!(worker.join().unwrap(), b"retained-inflight");
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(3),
    );
    let completed = host.shutdown();
    assert!(completed.runtime_barrier_error.is_none(), "{completed:?}");
    assert!(
        completed
            .route_outcomes
            .iter()
            .any(|r| r.route_name == route_name && r.active_drained)
    );
    assert_eq!(host.shutdown(), completed);
}

#[test]
fn runtime_policy_is_native_frozen_and_capability_is_consumed_once() {
    let runtime = test_runtime();
    runtime
        .set_lifecycle_policy(
            c2_config::ServerLifecyclePolicy::owner_bound(Duration::ZERO).unwrap(),
        )
        .unwrap();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    assert!(host.lifecycle_snapshot().policy.is_owner_bound());
    assert!(runtime.set_lifecycle_policy(Default::default()).is_err());
    drop(keepalive);
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(3),
    );
}

#[test]
fn owner_shutdown_withdraws_the_actual_registered_relay_route() {
    use c2_http::relay::{RelayConfig, RelayServer};
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let url = format!("http://{address}");
    let mut relay = RelayServer::start(RelayConfig {
        bind: address.to_string(),
        advertise_url: url.clone(),
        idle_timeout_secs: 1,
        anti_entropy_interval: Duration::ZERO,
        heartbeat_interval: Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let runtime = test_runtime();
    let (keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(url)
                .with_relay_use_proxy(false)
                .with_owner_bound(Duration::ZERO),
        )
        .unwrap();
    let route_name = unique_name("relay-owner");
    let _registration = register_echo(&host, &route_name);
    assert!(
        relay
            .list_routes()
            .unwrap()
            .iter()
            .any(|(name, _)| name == &route_name)
    );
    keepalive.close();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(5),
    );
    assert!(
        !relay
            .list_routes()
            .unwrap()
            .iter()
            .any(|(name, _)| name == &route_name),
        "relay must withdraw owner journal routes"
    );
    assert!(host.shutdown().relay_errors.is_empty());
    relay.stop().unwrap();
}

#[test]
fn owner_stop_does_not_force_free_client_retained_response_backing() {
    let root = private_root();
    let runtime = runtime_in(&root);
    let (keepalive, host) = owner_host(&runtime, Duration::from_secs(3));
    let route_name = unique_name("held-after-stop");
    let _registration = register_echo(&host, &route_name);
    let endpoint = host.local_endpoint().clone();
    let client_runtime = runtime_in(&root);
    let client = client_runtime
        .connect(
            release().expected_route(&route_name).unwrap(),
            Connect::DirectIpc {
                address: runtime.server_address().unwrap(),
            },
        )
        .unwrap();
    let observer = host.server_memory_observer();
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let payload = vec![91u8; 128 * 1024];
        let mut held = client.call_held("echo", &payload).unwrap();
        let before = observer.snapshot();
        assert!(
            before.shm.used_bytes > 0,
            "test must retain actual SHM backing"
        );
        let ticket = keepalive.recovery_ticket(CONTROL_TIMEOUT).unwrap();
        keepalive.close();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::OwnerMissing,
            Duration::from_secs(2),
        );
        assert_eq!(held.bytes(), payload);
        assert_eq!(observer.snapshot().shm.used_bytes, before.shm.used_bytes);
        let prepared = client_runtime.prepare_owner_takeover(ticket).unwrap();
        let replacement = prepared.commit(CONTROL_TIMEOUT).unwrap();
        assert_eq!(held.bytes(), payload);
        assert_eq!(client.call_owned("echo", &payload).unwrap(), payload);
        replacement.request_shutdown(CONTROL_TIMEOUT).unwrap();
        wait_for_phase(
            &host,
            |p| *p == HostLifecyclePhase::Finished,
            Duration::from_secs(3),
        );
        assert_eq!(
            observer.snapshot().shm.used_bytes,
            before.shm.used_bytes,
            "pause, takeover and stopping cannot release retained response backing"
        );
        assert_eq!(held.bytes(), payload);
        held.invalidate_then_release(|| Ok(())).unwrap();
        assert!(held.is_released());
    }));
    let shutdown = host.shutdown();
    // The pool's reserved backing remains charged while the Server still owns it.
    drop(client);
    drop(client_runtime);
    drop(_registration);
    drop(host);
    drop(runtime);
    assert_endpoint_gone(&endpoint);
    assert!(shutdown.runtime_barrier_error.is_none(), "{shutdown:?}");
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
    observation.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[test]
fn persistent_runtime_new_host_does_not_inherit_a_previous_shutdown_journal() {
    let runtime = test_runtime();
    for index in 0..2 {
        let host = runtime
            .host(HostOptions::default().without_relay())
            .unwrap();
        assert!(host.is_running());
        assert!(host.shutdown_outcome().is_none());
        let route = unique_name(&format!("persistent-restart-{index}"));
        let registration = register_echo(&host, &route);
        let client = direct_client(&runtime, &route).unwrap();
        assert_eq!(
            client.call_owned("echo", b"new-instance").unwrap(),
            b"new-instance"
        );
        let outcome = host.shutdown();
        assert_eq!(outcome.removed_routes, vec![route]);
        drop(client);
        drop(registration);
        drop(host);
    }
}

#[test]
fn delayed_old_registration_withdrawal_preserves_new_runtime_with_same_server_id() {
    use c2_http::client::{RelayControlClient, RelayRegistrationScope};
    use c2_http::relay::{RelayConfig, RelayServer};
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let url = format!("http://{address}");
    let mut relay = RelayServer::start(RelayConfig {
        bind: address.to_string(),
        advertise_url: url.clone(),
        anti_entropy_interval: Duration::ZERO,
        heartbeat_interval: Duration::ZERO,
        ..Default::default()
    })
    .unwrap();
    let server_id = unique_name("scope-same-server");
    let options = RuntimeOptions {
        server_id: Some(server_id),
        use_process_relay_anchor: false,
        ..Default::default()
    };
    let old_runtime = Runtime::new(options.clone()).unwrap();
    let old_host = old_runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(&url)
                .with_relay_use_proxy(false),
        )
        .unwrap();
    let route = unique_name("scope-same-route");
    let old_registration = register_echo(&old_host, &route);
    let original = old_registration.outcome();
    let scope = RelayRegistrationScope {
        name: route.clone(),
        server_id: original.server_id.clone(),
        server_instance_id: original.server_instance_id.clone(),
        route_uid: original.route_uid.clone(),
        route_revision: original.route_revision,
    };
    assert!(old_host.shutdown().relay_errors.is_empty());
    let new_runtime = Runtime::new(options).unwrap();
    let new_host = new_runtime
        .host(
            HostOptions::default()
                .with_relay_anchor_address(&url)
                .with_relay_use_proxy(false),
        )
        .unwrap();
    let new_registration = register_echo(&new_host, &route);
    assert_ne!(
        new_registration.outcome().server_instance_id,
        scope.server_instance_id
    );
    // The original wire target arrives after the replacement registration has committed.
    let control = RelayControlClient::new(&url, false).unwrap();
    control.unregister_registration(&scope).unwrap();
    assert!(
        relay
            .list_routes()
            .unwrap()
            .iter()
            .any(|(name, _)| name == &route)
    );
    let client = new_runtime
        .connect(
            release().expected_route(&route).unwrap(),
            Connect::ExplicitRelay { relay_url: url },
        )
        .unwrap();
    assert_eq!(
        client.call_owned("echo", b"new-resource").unwrap(),
        b"new-resource"
    );
    assert!(new_host.shutdown().relay_errors.is_empty());
    drop(new_registration);
    drop(new_host);
    drop(old_registration);
    drop(old_host);
    relay.stop().unwrap();
}
