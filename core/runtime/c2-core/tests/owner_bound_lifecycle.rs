//! OwnerBound host lifecycle tests against the public Runtime/Host surface.
//!
//! In-process cases use a real Core Runtime, a real host thread, real direct
//! IPC clients, and the real owner control pair. The controller-kill case
//! runs two real child processes: a controller that owns the keepalive and a
//! service that adopts the inherited receiver, so SIGKILL of the controller
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
    MethodDefinition, Runtime, RuntimeOptions, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_local::owner_control_pair;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

const FIXTURE_ACTION: &str = "C2_CORE_OWNER_BOUND_FIXTURE";

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request.to_vec()),
            _ => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                "method index escaped validated definition",
            )),
        }
    }
}

/// Blocks every `echo` invocation until the shared flag is set, proving
/// in-flight callbacks drain under the original rules across an owner EOF.
struct GatedEcho {
    gate: Arc<(StdMutex<bool>, Condvar)>,
    entered: std::sync::mpsc::Sender<()>,
}

impl EncodedService for GatedEcho {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        if method_index == 1 {
            self.entered.send(()).unwrap();
            let (lock, cvar) = &*self.gate;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
        }
        Ok(request.to_vec())
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
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).expect("attach");
    // The controller exits before the host arms its watcher.
    keepalive.shutdown();
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
            .contains("exactly one owner control receiver"),
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

#[test]
fn owner_eof_closes_admission_immediately_and_stops_after_grace() {
    let runtime = test_runtime();
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
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
    keepalive.shutdown();
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

#[test]
fn inflight_callback_completes_across_owner_eof() {
    let runtime = test_runtime();
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
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
    keepalive.shutdown();
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
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
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

    keepalive.shutdown();
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
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
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

    keepalive.shutdown();
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
        "controller" => {
            let (keepalive, mut receiver) = owner_control_pair().unwrap();
            let mut service = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "owner_bound_fixture", "--nocapture"])
                .env(FIXTURE_ACTION, "service")
                .stdin(receiver.take_stdio().expect("receiver as stdin"))
                .stdout(Stdio::inherit())
                .spawn()
                .expect("spawn service fixture");
            write_fixture_stdout(b"CONTROLLER_SPAWNED\n");
            // Hold the keepalive until the controller is killed.
            std::thread::sleep(Duration::from_secs(60));
            let _ = keepalive;
            let _ = service.wait();
        }
        "service" => {
            #[cfg(unix)]
            // SAFETY: fd 0 is the receiver endpoint this fixture explicitly
            // configured as its stdin.
            let receiver = unsafe { c2_local::OwnerControlReceiver::from_inherited_fd(0) }
                .expect("adopt the inherited owner control receiver endpoint");
            #[cfg(windows)]
            // SAFETY: the inherited stdin handle was explicitly configured
            // from the receiver endpoint by the controller fixture.
            let receiver = unsafe {
                use std::os::windows::io::{AsHandle, AsRawHandle};
                let stdin = std::io::stdin();
                c2_local::OwnerControlReceiver::from_inherited_handle(
                    stdin.as_handle().as_raw_handle(),
                )
                .expect("adopt the inherited owner control receiver handle")
            };

            let runtime = Runtime::new(RuntimeOptions {
                server_id: Some(format!("owner-kill-{}", std::process::id())),
                use_process_relay_anchor: false,
                ..RuntimeOptions::default()
            })
            .expect("service runtime");
            runtime.attach_owner_control(receiver).expect("attach");
            let host = runtime
                .host(
                    HostOptions::default()
                        .without_relay()
                        .with_startup_timeout(Duration::from_secs(5))
                        .with_shutdown_timeout(Duration::from_secs(2))
                        .with_owner_bound(Duration::from_secs(1)),
                )
                .expect("owner-bound host");
            write_fixture_stdout(b"SERVICE_READY\n");
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if host.shutdown_outcome().is_some_and(|outcome| {
                    outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none()
                }) {
                    write_fixture_stdout(b"SERVICE_FINISHED\n");
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "service never finished after EOF"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
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
    let mut controller = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owner_bound_fixture", "--nocapture"])
            .env(FIXTURE_ACTION, "controller")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn controller fixture"),
    );
    let stdout = controller.0.stdout.take().expect("controller stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    // Blocking line reads live only in this reader. Every test-side receive has a deadline.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line)
                    if matches!(
                        line.as_str(),
                        "CONTROLLER_SPAWNED" | "SERVICE_READY" | "SERVICE_FINISHED"
                    ) =>
                {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
                Ok(_) => {} // libtest output is not a fixture protocol marker.
                Err(_) => return,
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut controller_spawned = false;
    let mut service_ready = false;
    while !controller_spawned || !service_ready {
        let marker = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("bounded fixture readiness");
        controller_spawned |= marker == "CONTROLLER_SPAWNED";
        service_ready |= marker == "SERVICE_READY";
    }
    controller.0.kill().expect("kill controller");
    controller.0.wait().expect("reap controller");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let marker = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("bounded service shutdown marker");
        if marker == "SERVICE_FINISHED" {
            break;
        }
    }
}

#[test]
fn shutdown_deadline_does_not_cancel_pending_native_drain() {
    let runtime = test_runtime();
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::ZERO),
        )
        .unwrap();
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
    keepalive.shutdown();
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
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
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
    keepalive.shutdown();
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
    let runtime = test_runtime();
    let (mut keepalive, receiver) = owner_control_pair().unwrap();
    runtime.attach_owner_control(receiver).unwrap();
    let host = runtime
        .host(
            HostOptions::default()
                .without_relay()
                .with_owner_bound(Duration::ZERO),
        )
        .unwrap();
    let route_name = unique_name("held-after-stop");
    let _registration = register_echo(&host, &route_name);
    let client_runtime = test_runtime();
    let client = client_runtime
        .connect(
            release().expected_route(&route_name).unwrap(),
            Connect::DirectIpc {
                address: runtime.server_address().unwrap(),
            },
        )
        .unwrap();
    let payload = vec![91u8; 128 * 1024];
    let mut held = client.call_held("echo", &payload).unwrap();
    let observer = host.server_memory_observer();
    let before = observer.snapshot();
    assert!(
        before.shm.used_bytes > 0,
        "test must retain actual SHM backing"
    );
    keepalive.shutdown();
    wait_for_phase(
        &host,
        |p| *p == HostLifecyclePhase::Finished,
        Duration::from_secs(3),
    );
    assert_eq!(
        observer.snapshot().shm.used_bytes,
        before.shm.used_bytes,
        "stopping endpoint cannot release actual retained response backing"
    );
    assert_eq!(held.bytes(), payload);
    held.invalidate_then_release(|| Ok(())).unwrap();
    assert!(held.is_released());
    // The pool's reserved backing remains charged while the Server still owns it.
    drop(client);
    drop(client_runtime);
    drop(_registration);
    drop(host);
    drop(runtime);
    assert_eq!(observer.snapshot().shm.used_bytes, 0);
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
