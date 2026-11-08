//! Failure-only connection diagnostics for the unlimited-stream slot test.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::time::Duration;

use c2_core::{ConnectionEvent, ConnectionEventWait, recv_connection_event};
use c2_local::{ConnectionClosed, ConnectionEvents, ConnectionObservation, ConnectionSide};

const CLOSE_WAIT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Observations {
    events: Option<ConnectionEvents>,
    outgoing: Option<ConnectionObservation>,
    incoming: Vec<ConnectionObservation>,
}

pub(super) struct StreamDiagnostics {
    stage: Cell<&'static str>,
    observations: RefCell<Observations>,
}

impl StreamDiagnostics {
    pub(super) fn new() -> Self {
        Self {
            stage: Cell::new("fixture"),
            observations: RefCell::new(Observations::default()),
        }
    }

    pub(super) fn attach(&self, events: ConnectionEvents, outgoing: ConnectionObservation) {
        let mut observations = self.observations.borrow_mut();
        observations.events = Some(events);
        observations.outgoing = Some(outgoing);
    }

    pub(super) fn stage(&self, stage: &'static str) {
        self.stage.set(stage);
    }

    /// Construct after the fixture so this guard inspects connections before
    /// fixture cleanup can create additional close events during unwinding.
    pub(super) fn failure_guard(&self) -> FailureGuard<'_> {
        FailureGuard(self)
    }

    pub(super) fn run(&self, body: impl FnOnce()) {
        if let Err(original) = catch_unwind(AssertUnwindSafe(body)) {
            // Diagnostics must never replace the test's original failure.
            let _ = catch_unwind(AssertUnwindSafe(|| self.dump_after_unwind()));
            resume_unwind(original);
        }
    }

    fn dump(&self, when: &str) {
        eprintln!(
            "unlimited stream diagnostics: stage={} {when}",
            self.stage.get()
        );
        let mut observations = self.observations.borrow_mut();
        drain_events(&mut observations, Duration::ZERO);
        if let Some(outgoing) = &observations.outgoing {
            print_snapshot("outgoing", outgoing);
        } else {
            eprintln!("  outgoing: unavailable");
        }
        if observations.incoming.is_empty() {
            eprintln!("  incoming: no opened event captured");
        }
        for incoming in &observations.incoming {
            print_snapshot("incoming", incoming);
        }
    }

    fn dump_after_unwind(&self) {
        self.dump("after unwind");
        {
            let mut observations = self.observations.borrow_mut();
            // An accept event can race the first nonblocking drain.
            if observations.incoming.is_empty() {
                drain_events(&mut observations, CLOSE_WAIT);
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build();
            match runtime {
                Ok(runtime) => {
                    if let Some(outgoing) = &observations.outgoing {
                        print_wait("outgoing", outgoing, &runtime);
                    }
                    for incoming in &observations.incoming {
                        print_wait("incoming", incoming, &runtime);
                    }
                }
                Err(error) => eprintln!("  close wait runtime: {error}"),
            }
            drain_events(&mut observations, Duration::ZERO);
        }
        self.dump("after bounded close waits");
    }
}

pub(super) struct FailureGuard<'a>(&'a StreamDiagnostics);

impl Drop for FailureGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = catch_unwind(AssertUnwindSafe(|| self.0.dump("before fixture drop")));
        }
    }
}

fn drain_events(observations: &mut Observations, first_timeout: Duration) {
    let Some(events) = &observations.events else {
        eprintln!("  server events: unavailable");
        return;
    };
    let mut timeout = first_timeout;
    loop {
        let received = recv_connection_event(events, Some(timeout));
        timeout = Duration::ZERO;
        match received {
            Ok(Some(ConnectionEventWait::Event(ConnectionEvent::Opened(opened)))) => {
                eprintln!(
                    "  opened: side={} id={}",
                    opened.side().as_str(),
                    opened.connection_id()
                );
                if opened.side() == ConnectionSide::Server {
                    observations.incoming.push(opened);
                }
            }
            Ok(Some(ConnectionEventWait::Event(ConnectionEvent::Closed(closed)))) => {
                print_closed("closed event", closed);
            }
            Ok(Some(ConnectionEventWait::Lagged(missed))) => {
                eprintln!("  server events: lagged by {missed}");
                break;
            }
            Ok(Some(ConnectionEventWait::ConcurrentReceive)) => {
                eprintln!("  server events: concurrent receiver");
                break;
            }
            Ok(Some(ConnectionEventWait::Ended)) => break,
            Ok(None) => break,
            Err(error) => {
                eprintln!("  server events: {error}");
                break;
            }
        }
    }
}

fn print_snapshot(label: &str, observation: &ConnectionObservation) {
    eprint!(
        "  {label} snapshot: side={} id={} ",
        observation.side().as_str(),
        observation.connection_id()
    );
    match observation.snapshot() {
        Some(closed) => print_end(closed),
        None => eprintln!("end=none"),
    }
}

fn print_wait(label: &str, observation: &ConnectionObservation, runtime: &tokio::runtime::Runtime) {
    eprint!(
        "  {label} close wait: side={} id={} ",
        observation.side().as_str(),
        observation.connection_id()
    );
    match runtime.block_on(observation.wait_closed(Some(CLOSE_WAIT))) {
        Some(closed) => print_end(closed),
        None => eprintln!("end=none within {CLOSE_WAIT:?}"),
    }
}

fn print_closed(label: &str, closed: ConnectionClosed) {
    eprint!(
        "  {label}: side={} id={} ",
        closed.side().as_str(),
        closed.connection_id()
    );
    print_end(closed);
}

fn print_end(closed: ConnectionClosed) {
    eprintln!(
        "end={} operation={} os_error={:?}",
        closed.reason().as_str(),
        closed.operation(),
        closed.os_error()
    );
}
