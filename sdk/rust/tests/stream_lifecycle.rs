//! Design section 9 lifecycle acceptance through public Rust SDK typed owners.
#[path = "fixtures/stream_lifecycle.rs"]
mod fixture;

use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use c_two::generated::{C2Error, EncodedClient, ErrorCode};
use c_two::{
    CallOptions, CallTimeout, Client, Connect, Error, Held, ResponseStream, StreamCloseReceipt,
};
use fastdb::Payload;
use fixture::{
    Fixture, Settings, WAIT, observe_until, open_held_stream, open_stream, payload, text,
};

fn assert_semantic(error: Error, code: ErrorCode, cause: &str) {
    let Error::Semantic(error) = error else {
        panic!("expected {code:?}, got {error:?}");
    };
    assert_eq!(error.code, code);
    assert!(error.message.contains(cause), "{error:?}");
}

fn assert_cancelled(receipt: StreamCloseReceipt, cause: &str) {
    assert!(receipt.completed, "original call has not finished");
    assert_semantic(
        receipt.terminal.unwrap().expect_err("cancelled stream"),
        ErrorCode::CallCancelled,
        cause,
    );
}

fn next_payload(stream: &mut ResponseStream<Payload>, index: usize) {
    assert_eq!(
        text(&stream.next().expect("item").expect("typed payload")),
        format!("{index:08}")
    );
}

fn next_hold(stream: &mut ResponseStream<Held<Payload>>, index: usize) -> Held<Payload> {
    let held = stream.next().expect("item").expect("typed hold");
    assert_eq!(text(held.value().unwrap()), format!("{index:08}"));
    held
}

fn two_full_holds(
    fixture: &Fixture,
) -> (ResponseStream<Held<Payload>>, Held<Payload>, Held<Payload>) {
    let mut stream = open_held_stream(&fixture.client).unwrap();
    let first = next_hold(&mut stream, 1);
    let second = next_hold(&mut stream, 2);
    fixture.wait_for_step(3);
    let limit = fixture
        .client_runtime
        .memory_stats()
        .payload_buffer
        .unwrap()
        .limit_bytes;
    assert_eq!(fixture.used_bytes(), limit, "two typed holds fill B");
    (stream, first, second)
}

fn two_item_budget() -> u64 {
    2 * payload(1).binary_bytes().unwrap().len() as u64
}

// Requirement 1: application `?` drops the typed stream immediately;
// only its real terminal, after gated iterator cleanup, refunds the call slot.
#[test]
fn typed_question_mark_does_not_wait_and_refunds_only_after_cleanup() {
    fn application(client: Client) -> Result<(), Error> {
        let mut stream = open_stream(&client)?;
        next_payload(&mut stream, 1);
        Err::<(), Error>(Error::Semantic(C2Error::new(
            ErrorCode::ResourceUnavailable,
            "application stopped",
        )))?;
        Ok(())
    }
    let fixture = Fixture::new(Settings {
        gated_cleanup: true,
        client_slots: Some(1),
        ..Default::default()
    });
    let client = fixture.client.clone();
    let (returned, observed) = mpsc::channel();
    let application = std::thread::spawn(move || returned.send(application(client)).unwrap());
    assert_semantic(
        observed.recv_timeout(WAIT).unwrap().unwrap_err(),
        ErrorCode::ResourceUnavailable,
        "application stopped",
    );
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    assert_eq!(fixture.used_slots(), 1);
    let refusal = open_stream(&fixture.client)
        .err()
        .expect("slot still occupied");
    assert_semantic(refusal, ErrorCode::CallCapacityExceeded, "operation slot");
    assert_eq!(fixture.probe.opens.load(Ordering::SeqCst), 1);
    fixture.probe.cleanup_gate.release();
    observe_until(|| fixture.used_slots() == 0);
    let mut recovered = open_stream(&fixture.client).unwrap();
    next_payload(&mut recovered, 1);
    assert_cancelled(recovered.close_with_timeout(Some(WAIT)), "consumer");
    assert_eq!(fixture.probe.cleanups.load(Ordering::SeqCst), 2);
    application.join().unwrap();
}

// Requirement 2: delivery to the capacity-one local consumer grants demand,
// independently of application next(). After the application takes k items,
// item k+1 can be buffered there and item k+2 is authorized; step k+3 cannot run.
#[test]
fn typed_iteration_stopped_after_k_items_bounds_buffered_and_authorized_items() {
    let fixture = Fixture::new(Settings::default());
    let mut stream = open_stream(&fixture.client).unwrap();
    let taken_by_application = 3;
    for index in 1..=taken_by_application {
        next_payload(&mut stream, index);
    }
    let buffered_item = taken_by_application + 1;
    let authorized_item = buffered_item + 1;
    fixture.wait_for_step(authorized_item);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    // Both complete local copies now exist: k+1 is in the consumer interface,
    // and k+2 is awaiting its occupied slot. B has ample room; it is demand
    // that bounds further steps, while the route can still serve unary calls.
    observe_until(|| {
        fixture.used_bytes() == two_item_budget() && route.snapshot().active_workers == 0
    });
    fixture.client.call_owned("ping", &[]).unwrap();
    drop(stream);
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    observe_until(|| fixture.used_slots() == 0 && fixture.used_bytes() == 0);
    assert!(
        fixture.probe.steps.load(Ordering::SeqCst) <= authorized_item,
        "item {buffered_item} was buffered and item {authorized_item} authorized; step {} must not start",
        authorized_item + 1,
    );
    assert_eq!(fixture.probe.opens.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.cleanups.load(Ordering::SeqCst), 1);
}

// Requirement 3: typed Held<Payload> owners fill B independently, and
// releasing exactly one wakes the waiting iteration without a capacity error.
#[test]
fn typed_holds_fill_b_and_releasing_one_resumes_the_next_item() {
    let fixture = Fixture::new(Settings {
        gated_step: Some(3),
        client_bytes: Some(two_item_budget()),
        ..Default::default()
    });
    let (mut stream, mut first, mut second) = two_full_holds(&fixture);
    let first_view = first.value().unwrap().entry_view(0).unwrap();
    let (started, starting) = mpsc::channel();
    let (returned, observed) = mpsc::channel();
    let waiting = std::thread::spawn(move || {
        started.send(()).unwrap();
        returned.send(stream.next()).unwrap();
        stream
    });
    starting.recv_timeout(WAIT).unwrap();
    fixture.probe.step_gate.release();
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    observe_until(|| route.snapshot().active_workers == 0);
    assert!(matches!(
        observed.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(fixture.used_bytes(), two_item_budget());
    first.release().unwrap();
    assert_eq!(first_view.kind().unwrap_err().symbol(), "VIEW_INVALIDATED");
    let mut third = observed.recv_timeout(WAIT).unwrap().unwrap().unwrap();
    assert_eq!(text(third.value().unwrap()), "00000003");
    assert_eq!(text(second.value().unwrap()), "00000002");
    let stream = waiting.join().unwrap();
    assert_cancelled(stream.close_with_timeout(Some(WAIT)), "consumer");
    second.release().unwrap();
    third.release().unwrap();
    observe_until(|| fixture.used_bytes() == 0);
}

// Requirement 3: a finite deadline while B is full is an uncertain caller
// timeout; it neither cancels cleanup nor refunds the original call slot.
#[test]
fn typed_full_b_deadline_reports_deadline_and_preserves_the_original_call() {
    let fixture = Fixture::new(Settings {
        gated_cleanup: true,
        client_slots: Some(1),
        client_bytes: Some(two_item_budget()),
        ..Default::default()
    });
    let client = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(
            Duration::from_secs(2),
        )));
    let mut stream = open_held_stream(&client).unwrap();
    let mut first = next_hold(&mut stream, 1);
    let mut second = next_hold(&mut stream, 2);
    fixture.wait_for_step(3);
    assert_eq!(fixture.used_bytes(), two_item_budget());
    let error = stream.next().expect("deadline result").unwrap_err();
    let Error::Semantic(ref semantic) = error else {
        panic!("expected CallDeadlineExceeded, got {error:?}");
    };
    assert_eq!(semantic.details["transport_phase"], "dispatch_uncertain");
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "deadline");
    assert!(stream.next().is_none());
    assert_eq!(fixture.probe.cleanups.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.used_slots(), 1);
    assert!(!stream.close_with_timeout(Some(Duration::ZERO)).completed);
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    assert!(!stream.close_with_timeout(Some(Duration::ZERO)).completed);
    fixture.probe.cleanup_gate.release();
    assert_cancelled(stream.close_with_timeout(Some(WAIT)), "consumer");
    assert_eq!(text(first.value().unwrap()), "00000001");
    assert_eq!(text(second.value().unwrap()), "00000002");
    first.release().unwrap();
    second.release().unwrap();
    observe_until(|| fixture.used_bytes() == 0 && fixture.used_slots() == 0);
}

// Requirement 3: close can finish the call while every data byte in B remains
// retained by the application's typed holds.
#[test]
fn typed_close_bypasses_a_full_hold_budget() {
    let fixture = Fixture::new(Settings {
        client_bytes: Some(two_item_budget()),
        ..Default::default()
    });
    let (stream, mut first, mut second) = two_full_holds(&fixture);
    assert_cancelled(stream.close_with_timeout(Some(WAIT)), "consumer");
    assert_eq!(fixture.used_bytes(), two_item_budget());
    assert_eq!(text(first.value().unwrap()), "00000001");
    assert_eq!(text(second.value().unwrap()), "00000002");
    first.release().unwrap();
    second.release().unwrap();
    observe_until(|| fixture.used_bytes() == 0 && fixture.used_slots() == 0);
}

// Requirement 3: Drop submits cancellation despite full B and does not wait
// for the resource's cleanup barrier or invalidate independently held items.
#[test]
fn typed_drop_cancels_through_full_b_without_waiting_for_cleanup() {
    let fixture = Fixture::new(Settings {
        gated_cleanup: true,
        client_bytes: Some(two_item_budget()),
        ..Default::default()
    });
    let (stream, first, second) = two_full_holds(&fixture);
    let (returned, observed) = mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(stream);
        returned.send(()).unwrap();
    });
    observed.recv_timeout(WAIT).unwrap();
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    assert_eq!(fixture.used_slots(), 1);
    fixture.probe.cleanup_gate.release();
    observe_until(|| fixture.used_slots() == 0);
    assert_eq!(fixture.used_bytes(), two_item_budget());
    assert_eq!(text(first.value().unwrap()), "00000001");
    assert_eq!(text(second.value().unwrap()), "00000002");
    drop((first, second));
    observe_until(|| fixture.used_bytes() == 0);
    dropping.join().unwrap();
}

fn lifecycle_fixture() -> Fixture {
    Fixture::new(Settings {
        gated_step: Some(1),
        gated_cleanup: true,
        gated_unary: true,
        ..Default::default()
    })
}

// Requirement 4, Runtime shutdown: the already-running typed item precedes
// the named cancellation; the original retirement receipt waits for cleanup
// and an already-entered unary on the same route.
#[test]
fn typed_runtime_shutdown_delivers_running_item_and_preserves_entered_unary() {
    let fixture = lifecycle_fixture();
    let mut stream = open_stream(&fixture.client).unwrap();
    fixture.wait_for_step(1);
    let client = fixture.client.clone();
    let unary = std::thread::spawn(move || client.call_owned("ping", &[]));
    fixture.unary_events.recv_timeout(WAIT).unwrap();
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    // The shutdown target type accompanies Runtime's public retirement method;
    // all resource dispatch and stream decoding use the SDK seams above.
    let receipt = fixture
        .server
        .begin_session_retirement(
            c2_core::RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    observe_until(|| route.snapshot().closed);
    assert!(!receipt.observe_until(Instant::now()).1);
    fixture.probe.step_gate.release();
    next_payload(&mut stream, 1);
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    assert!(!receipt.observe_until(Instant::now()).1);
    assert!(receipt.terminal_outcome().is_none());
    fixture.probe.cleanup_gate.release();
    assert_semantic(
        stream.next().expect("terminal").unwrap_err(),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    assert!(stream.next().is_none());
    let terminal = stream.close_with_timeout(Some(WAIT));
    assert_eq!(terminal.complete_items, Some(1));
    assert_cancelled(terminal, "shutdown");
    assert!(!receipt.observe_until(Instant::now()).1, "unary still runs");
    fixture.probe.unary_gate.release();
    unary.join().unwrap().unwrap();
    let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
    assert!(completed, "{outcome:?}");
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert!(outcome.route_close_error.is_none(), "{outcome:?}");
    assert!(fixture.host.lifecycle_snapshot().finished);
    assert_eq!(fixture.probe.steps.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.unary_calls.load(Ordering::SeqCst), 1);
}

// Requirement 4, unregistration: the one original Registration::close waits
// through iterator cleanup and the same route's entered unary, preserving the
// current item and the unregistration cause on the typed stream.
#[test]
fn typed_unregister_delivers_running_item_and_preserves_entered_unary() {
    let mut fixture = lifecycle_fixture();
    let mut stream = open_stream(&fixture.client).unwrap();
    fixture.wait_for_step(1);
    let client = fixture.client.clone();
    let unary = std::thread::spawn(move || client.call_owned("ping", &[]));
    fixture.unary_events.recv_timeout(WAIT).unwrap();
    let mut registration = fixture.registration.take().unwrap();
    let route = registration.route_concurrency();
    let (returned, observed) = mpsc::channel();
    let closing = std::thread::spawn(move || returned.send(registration.close()).unwrap());
    observe_until(|| route.snapshot().closed);
    fixture.probe.step_gate.release();
    next_payload(&mut stream, 1);
    fixture.cleanup_events.recv_timeout(WAIT).unwrap();
    assert!(matches!(
        observed.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    fixture.probe.cleanup_gate.release();
    assert_semantic(
        stream.next().expect("terminal").unwrap_err(),
        ErrorCode::CallCancelled,
        "unregister",
    );
    assert!(stream.next().is_none());
    let terminal = stream.close_with_timeout(Some(WAIT));
    assert_eq!(terminal.complete_items, Some(1));
    assert_cancelled(terminal, "unregister");
    assert!(matches!(
        observed.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    fixture.probe.unary_gate.release();
    unary.join().unwrap().unwrap();
    let outcome = observed.recv_timeout(WAIT).unwrap().unwrap();
    assert!(outcome.close.active_drained, "{outcome:?}");
    assert!(outcome.close.close_error.is_none(), "{outcome:?}");
    assert_eq!(fixture.probe.steps.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.unary_calls.load(Ordering::SeqCst), 1);
    closing.join().unwrap();
}

struct ResourceProcess(Child);

impl Drop for ResourceProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn assert_dispatch_uncertain(error: Error) {
    let Error::Transport(error) = error else {
        panic!("peer end must be a dispatch-uncertain transport failure: {error:?}");
    };
    assert!(
        !error.is_fallback_eligible(),
        "dispatched work cannot be replayed"
    );
    assert_eq!(error.phase(), c2_core::TransportPhase::DispatchUncertain);
}

// Requirement 5, all platforms: Child::kill ends a real resource process
// (SIGKILL on Unix, TerminateProcess on Windows), not a mock transport. A held
// delivered item survives; the unfinished step is uncertain, with one OPEN
// and no replay in the resource's flushed process audit.
#[test]
fn typed_peer_end_preserves_delivered_hold_and_never_replays_the_stream() {
    const CHILD: &str = "C2_TEST_TYPED_STREAM_PEER_END_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use std::io::Write;
        let fixture = Fixture::new(Settings {
            gated_step: Some(2),
            process_events: true,
            ..Default::default()
        });
        println!(
            "\nC2_STREAM_READY {}",
            serde_json::json!({
                "address": fixture.host.local_endpoint().address(),
                "route": fixture.route,
                "root": fixture.root,
            })
        );
        std::io::stdout().flush().unwrap();
        let mut control = String::new();
        std::io::stdin().read_line(&mut control).unwrap();
        panic!("resource must be killed while its second step is gated");
    }
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "typed_peer_end_preserves_delivered_hold_and_never_replays_the_stream",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = ResourceProcess(child);
    let stdout = child.0.stdout.take().unwrap();
    let (sent, events) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            sent.send(line.unwrap()).unwrap();
        }
    });
    let mut audit = Vec::new();
    let ready = loop {
        let line = events.recv_timeout(WAIT).unwrap();
        let ready = line.strip_prefix("C2_STREAM_READY ").map(str::to_owned);
        audit.push(line);
        if let Some(ready) = ready {
            break serde_json::from_str::<serde_json::Value>(&ready).unwrap();
        }
    };
    let client_runtime = fixture::runtime(
        std::path::Path::new(ready["root"].as_str().unwrap()),
        fixture::unique("peer-end-client"),
    );
    let client = client_runtime
        .connect(
            fixture::release()
                .expected_route(ready["route"].as_str().unwrap())
                .unwrap(),
            Connect::DirectIpc {
                address: ready["address"].as_str().unwrap().into(),
            },
        )
        .unwrap();
    let mut stream = open_held_stream(&client).unwrap();
    let mut first = next_hold(&mut stream, 1);
    loop {
        let line = events.recv_timeout(WAIT).unwrap();
        let step_entered = line == "C2_STREAM_STEP 2";
        audit.push(line);
        if step_entered {
            break;
        }
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert_dispatch_uncertain(stream.next().expect("uncertain terminal").unwrap_err());
    assert!(stream.next().is_none());
    let terminal = stream.close_with_timeout(Some(WAIT));
    assert!(terminal.completed);
    assert!(
        terminal.complete_items.is_none(),
        "no CALL_END was received"
    );
    assert_dispatch_uncertain(terminal.terminal.unwrap().unwrap_err());
    assert_eq!(text(first.value().unwrap()), "00000001");
    reader.join().unwrap();
    audit.extend(events.try_iter());
    assert_eq!(
        audit
            .iter()
            .filter(|line| *line == "C2_STREAM_OPEN")
            .count(),
        1
    );
    assert_eq!(
        audit
            .iter()
            .filter(|line| line.starts_with("C2_STREAM_STEP "))
            .count(),
        2
    );
    observe_until(|| {
        client_runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
    first.release().unwrap();
    assert_eq!(
        client_runtime
            .memory_stats()
            .payload_buffer
            .unwrap()
            .used_bytes,
        0
    );
}
