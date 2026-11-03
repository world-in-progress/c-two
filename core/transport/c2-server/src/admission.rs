use std::collections::HashMap;

use parking_lot::{Mutex, MutexGuard};
use tokio::sync::Notify;

use crate::scheduler::SchedulerAcquireError;
use crate::server::PendingRouteInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionPhase {
    Open,
    Draining,
    Terminal,
}

/// Registration publication and callback admission share this commit lock and
/// generation. Never await or invoke user code with this lock held. Whenever a
/// scheduler lock is also needed, acquire this lock first.
pub(crate) struct AdmissionState {
    pub(crate) pending: HashMap<String, PendingRouteInfo>,
    pub(crate) generation: u64,
    phase: AdmissionPhase,
}

impl AdmissionState {
    pub(crate) fn is_closed(&self) -> bool {
        self.phase != AdmissionPhase::Open
    }

    pub(crate) fn check(&self, generation: u64) -> Result<(), SchedulerAcquireError> {
        if self.phase == AdmissionPhase::Terminal {
            Err(SchedulerAcquireError::Closed)
        } else if self.generation != generation {
            Err(SchedulerAcquireError::Unavailable)
        } else if self.phase == AdmissionPhase::Draining {
            Err(SchedulerAcquireError::Closed)
        } else {
            Ok(())
        }
    }

    /// Continue work admitted in this host generation while shutdown drains it.
    pub(crate) fn check_accepted(&self, generation: u64) -> Result<(), SchedulerAcquireError> {
        if self.phase == AdmissionPhase::Terminal {
            Err(SchedulerAcquireError::Closed)
        } else if self.generation != generation {
            Err(SchedulerAcquireError::Unavailable)
        } else {
            Ok(())
        }
    }

    pub(crate) fn begin_drain(&mut self) {
        if self.phase == AdmissionPhase::Open {
            self.phase = AdmissionPhase::Draining;
        }
    }

    pub(crate) fn terminate(&mut self) {
        if self.phase != AdmissionPhase::Terminal {
            self.generation = self
                .generation
                .checked_add(1)
                .expect("host admission generation exhausted");
            self.phase = AdmissionPhase::Terminal;
        }
    }

    pub(crate) fn begin_start(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("host admission generation exhausted");
        self.phase = AdmissionPhase::Open;
    }
}

pub(crate) struct AdmissionGate {
    state: Mutex<AdmissionState>,
    // All route and global scheduler transitions wake the same wait source.
    // Waiters register before checking state, and release both locks to wait.
    pub(crate) notify: Notify,
}

impl AdmissionGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                pending: HashMap::new(),
                generation: 0,
                phase: AdmissionPhase::Open,
            }),
            notify: Notify::new(),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, AdmissionState> {
        self.state.lock()
    }
}
