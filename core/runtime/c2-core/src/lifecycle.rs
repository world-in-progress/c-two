//! Read-only Host lifecycle and retained transport charge observations.

use std::sync::Arc;

use parking_lot::Mutex;

/// Observable lifecycle phase of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLifecyclePhase {
    /// The host runs until explicit shutdown.
    Persistent,
    /// The host's shutdown transaction completed and produced one outcome.
    Finished,
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
    pub phase: HostLifecyclePhase,
    /// No listener is open: either the server never bound, or the shutdown
    /// transaction closed its listener.
    pub listener_closed: bool,
    /// Whether every recorded route close drained its active work. `None`
    /// until a shutdown transaction produced route outcomes.
    pub work_drained: Option<bool>,
    pub client_held_leases: HostClientHeldLeases,
}

#[derive(Clone)]
pub(crate) struct HostLifecycleCell {
    phase: Arc<Mutex<HostLifecyclePhase>>,
}

impl HostLifecycleCell {
    pub(crate) fn new() -> Self {
        Self {
            phase: Arc::new(Mutex::new(HostLifecyclePhase::Persistent)),
        }
    }

    pub(crate) fn phase(&self) -> HostLifecyclePhase {
        self.phase.lock().clone()
    }

    pub(crate) fn finish_from_outcome(&self, outcome: &crate::ShutdownOutcome) {
        if outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none() {
            *self.phase.lock() = HostLifecyclePhase::Finished;
        }
    }
}
