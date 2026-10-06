//! Native OwnerBound host lifecycle for explicitly managed service processes.
//!
//! A controller creates the private owner control pair
//! ([`c2_local::owner_control_pair`]), keeps the keepalive endpoint, and hands
//! the receiver to one Runtime through [`Runtime::attach_owner_control`](crate::Runtime::attach_owner_control).
//! Starting a host with the [`OwnerBound`](c2_config::ServerLifecyclePolicy::OwnerBound)
//! policy consumes that capability before the host can publish readiness, so
//! a policy name alone never creates an owner-bound server.
//!
//! The watcher is armed on the host's own runtime thread before the accept
//! loop runs, which is before readiness can be published. When the controller
//! closes (EOF) or the watcher itself fails, new business admission closes
//! immediately — never after the grace window — and after the bounded grace
//! the existing native drain, route journal, and shutdown barrier take over.
//! The first version does not reconnect and does not support takeover.

use std::sync::Arc;
use std::time::Duration;

use c2_config::ServerLifecyclePolicy;
use c2_local::OwnerControlReceiver;
use c2_server::Server;
use parking_lot::Mutex;

/// Observable lifecycle phase of one host.
///
/// Phases never expose OS handles or the capability itself. `WatcherIoError`
/// is sticky: it records why owner observation ended and is not overwritten
/// by the later `Finished` transition of the shutdown transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLifecyclePhase {
    /// The host runs with the `Persistent` policy; no owner watcher exists
    /// and only explicit shutdown ends it.
    Persistent,
    /// The owner control watcher is armed and the controller end is still
    /// open. This is the phase from before readiness is published.
    Armed,
    /// The controller relationship ended (control EOF or watcher failure).
    /// New business admission is already suspended; the bounded grace window
    /// is running and in-flight work continues under the original rules.
    OwnerMissing,
    /// The grace window ended and the native drain/shutdown transaction is
    /// running.
    Draining,
    /// The host's shutdown transaction completed and produced one outcome.
    Finished,
    /// The owner watcher itself failed, so the owner relationship can no
    /// longer be observed. Admission is closed and the same bounded shutdown
    /// sequence runs; this is not a business-stream failure and not an owner
    /// EOF.
    WatcherIoError(String),
}

/// Server-direction transport charges still retained by payload owners.
///
/// These are held payload references (response and reassembly backing), not
/// process memory. A stopped server does not zero them, a completed shutdown
/// transaction does not release them, and endpoint cleanup never releases or
/// falsifies them: they drop only when the actual payload owners release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostClientHeldLeases {
    /// Retained shared-memory charges in the server response direction.
    pub response_shm_bytes: u64,
    /// Retained file-spill charges in the server response direction.
    pub response_file_bytes: u64,
    /// Retained chunk-reassembly charges.
    pub reassembly_bytes: u64,
}

/// Read-only host lifecycle observation.
///
/// `listener_closed`, `work_drained`, and `client_held_leases` are separate
/// observations: a closed listener does not prove drained work, and drained
/// server work does not prove client-held payload references were released.
/// This snapshot does not report endpoint (socket/lock) cleanup results;
/// that projection belongs to the later endpoint-reclamation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLifecycleSnapshot {
    pub policy: ServerLifecyclePolicy,
    pub phase: HostLifecyclePhase,
    /// No listener is open: either the server never bound, or the shutdown
    /// transaction closed its listener.
    pub listener_closed: bool,
    /// Whether every recorded route close drained its active work. `None`
    /// until a shutdown transaction produced route outcomes.
    pub work_drained: Option<bool>,
    pub client_held_leases: HostClientHeldLeases,
}

/// Shared phase cell between the host handle and the host's runtime thread.
#[derive(Clone)]
pub(crate) struct HostLifecycleCell {
    phase: Arc<Mutex<HostLifecyclePhase>>,
}

impl HostLifecycleCell {
    pub(crate) fn new(policy: &ServerLifecyclePolicy) -> Self {
        let phase = if policy.is_owner_bound() {
            HostLifecyclePhase::Armed
        } else {
            HostLifecyclePhase::Persistent
        };
        Self {
            phase: Arc::new(Mutex::new(phase)),
        }
    }

    pub(crate) fn phase(&self) -> HostLifecyclePhase {
        self.phase.lock().clone()
    }

    pub(crate) fn set_phase(&self, phase: HostLifecyclePhase) {
        *self.phase.lock() = phase;
    }

    /// Record the terminal phase from a completed shutdown transaction,
    /// preserving a sticky watcher IO-error diagnosis.
    ///
    /// A transaction whose native barrier or route close reported an error is
    /// structured-incomplete: the phase stays `Draining` instead of claiming
    /// `Finished`, because nothing may mark undrained work as done.
    pub(crate) fn finish_from_outcome(&self, outcome: &crate::ShutdownOutcome) {
        let mut phase = self.phase.lock();
        if matches!(&*phase, HostLifecyclePhase::WatcherIoError(_)) {
            return;
        }
        let completed =
            outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none();
        if completed {
            *phase = HostLifecyclePhase::Finished;
        }
    }
}

/// Result of the single arming poll performed before readiness.
pub(crate) enum OwnerArmOutcome {
    /// The watcher registration is live and the controller end is still open.
    Alive,
    /// The controller end was already closed before the host armed.
    Closed,
    /// The watcher itself failed before the host armed.
    IoError(String),
}

/// Establish the watcher and probe the native endpoint before polling the accept loop.
pub(crate) async fn arm_owner_watch(receiver: &mut OwnerControlReceiver) -> OwnerArmOutcome {
    match receiver.prepare().await {
        Ok(true) => OwnerArmOutcome::Alive,
        Ok(false) => OwnerArmOutcome::Closed,
        Err(error) => OwnerArmOutcome::IoError(error.to_string()),
    }
}

/// Why the owner supervision ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerWatchEvent {
    /// The controller endpoint closed (EOF). This is the control channel
    /// only: business stream EOF, idle eviction, ping failure, cancellation,
    /// and relay reconnects never produce this event.
    OwnerEof,
    /// The watcher itself failed and owner observation is gone.
    WatcherError(String),
}

/// Supervise the owner control channel after arming and drive the
/// OwnerMissing → Draining sequence.
///
/// Completes after requesting shutdown. The coordinator continues polling the actual run
/// future until it finishes; caller deadlines never cancel that drain.
pub(crate) async fn supervise_owner(
    receiver: &mut OwnerControlReceiver,
    server: &Arc<Server>,
    lifecycle: &HostLifecycleCell,
    owner_missing_grace: Duration,
) -> OwnerWatchEvent {
    let event = match receiver.wait_closed().await {
        Ok(()) => OwnerWatchEvent::OwnerEof,
        Err(error) => OwnerWatchEvent::WatcherError(error.to_string()),
    };
    let (admission_reason, shutdown_reason, phase) = match &event {
        OwnerWatchEvent::OwnerEof => (
            c2_server::OWNER_MISSING_ADMISSION_REASON,
            c2_server::OWNER_BOUND_SHUTDOWN_REASON,
            HostLifecyclePhase::OwnerMissing,
        ),
        OwnerWatchEvent::WatcherError(message) => (
            c2_server::OWNER_WATCHER_ERROR_ADMISSION_REASON,
            c2_server::OWNER_WATCHER_ERROR_SHUTDOWN_REASON,
            HostLifecyclePhase::WatcherIoError(message.clone()),
        ),
    };
    // Close new business admission immediately. The grace window only delays
    // the drain transaction; it never delays admission closure.
    server.close_business_admission(admission_reason).await;
    lifecycle.set_phase(phase);
    tokio::time::sleep(owner_missing_grace).await;
    if !matches!(lifecycle.phase(), HostLifecyclePhase::WatcherIoError(_)) {
        lifecycle.set_phase(HostLifecyclePhase::Draining);
    }
    // Hand the shutdown to the run loop with an owner-bound journal reason so
    // the recorded route closes stay distinguishable from an explicit direct
    // IPC stop, then observe the native transaction with a bounded wait.
    server.request_shutdown_signal_with_reason(shutdown_reason);
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preclosed_probe_does_not_depend_on_reactor_delivery() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (keepalive, mut receiver) = c2_local::owner_control_pair().unwrap();
        drop(keepalive);
        assert!(matches!(
            runtime.block_on(arm_owner_watch(&mut receiver)),
            OwnerArmOutcome::Closed
        ));
    }
    #[test]
    fn watcher_error_diagnosis_survives_draining_and_completion() {
        let cell = HostLifecycleCell::new(&ServerLifecyclePolicy::Persistent);
        cell.set_phase(HostLifecyclePhase::WatcherIoError("invalid control".into()));
        cell.finish_from_outcome(&crate::ShutdownOutcome::default());
        assert_eq!(
            cell.phase(),
            HostLifecyclePhase::WatcherIoError("invalid control".into())
        );
    }
    #[test]
    fn actual_watcher_io_failure_closes_admission_and_is_sticky() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_keepalive, mut receiver) = c2_local::owner_control_pair().unwrap();
        receiver.shutdown();
        let server = Arc::new(
            Server::new(
                &format!("ipc://watch-error-{}", uuid::Uuid::new_v4().simple()),
                c2_server::config::ServerIpcConfig::default(),
            )
            .unwrap(),
        );
        let policy = ServerLifecyclePolicy::owner_bound(Duration::ZERO).unwrap();
        let cell = HostLifecycleCell::new(&policy);
        let event = runtime.block_on(supervise_owner(
            &mut receiver,
            &server,
            &cell,
            Duration::ZERO,
        ));
        assert!(matches!(event, OwnerWatchEvent::WatcherError(_)));
        assert!(server.business_admission_closed());
        assert!(matches!(
            cell.phase(),
            HostLifecyclePhase::WatcherIoError(_)
        ));
        cell.finish_from_outcome(&crate::ShutdownOutcome::default());
        assert!(matches!(
            cell.phase(),
            HostLifecyclePhase::WatcherIoError(_)
        ));
    }
}
