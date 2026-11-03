//! Read-only Host shutdown observations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Listener closure and business drain for one Host's shutdown transaction.
/// In-flight transport storage is observed separately through memory statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLifecycleSnapshot {
    /// The original shutdown transaction completed successfully.
    pub finished: bool,
    /// No listener is open: the server never bound or closed its listener.
    pub listener_closed: bool,
    /// Whether every recorded route close drained its active work. `None`
    /// until a shutdown transaction produced route outcomes.
    pub work_drained: Option<bool>,
}

#[derive(Clone)]
pub(crate) struct HostLifecycleCell {
    finished: Arc<AtomicBool>,
}

impl HostLifecycleCell {
    pub(crate) fn new() -> Self {
        Self {
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub(crate) fn finish_from_outcome(&self, outcome: &crate::ShutdownOutcome) {
        if outcome.runtime_barrier_error.is_none() && outcome.route_close_error.is_none() {
            self.finished.store(true, Ordering::Release);
        }
    }
}
