use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, MutexGuard};
use tokio::sync::Notify;

use crate::scheduler::SchedulerAcquireError;
use crate::server::PendingRouteInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionPhase {
    Open,
    Paused,
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
        } else if self.is_closed() || self.generation != generation {
            Err(SchedulerAcquireError::Unavailable)
        } else {
            Ok(())
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

    pub(crate) fn begin_start(&mut self, owner_bound: bool) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("host admission generation exhausted");
        // An old owner authority can never revive a terminal host. Core must
        // construct a new owner-bound server with a fresh bootstrap instead.
        if !owner_bound {
            self.phase = AdmissionPhase::Open;
        }
    }
}

pub(crate) struct AdmissionGate {
    state: Mutex<AdmissionState>,
    pub(crate) owner_bound: bool,
    // All route and global scheduler transitions wake the same wait source.
    // Waiters register before checking state, and release both locks to wait.
    pub(crate) notify: Notify,
}

impl AdmissionGate {
    pub(crate) fn new(owner_bound: bool) -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                pending: HashMap::new(),
                generation: 0,
                phase: AdmissionPhase::Open,
            }),
            owner_bound,
            notify: Notify::new(),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, AdmissionState> {
        self.state.lock()
    }
}

/// Sole recovery authority for the gate of one owner-bound server.
///
/// Issued exactly once by `Server::new_owner_bound_with_identity_and_endpoint`.
/// It cannot be cloned or recovered from a server or scheduler handle. Core
/// owns owner-session authorization and calls these synchronous operations in
/// its own owner-state commit boundary; this object stores no owner protocol
/// state. Dropping it does not change host lifecycle or admission.
pub struct OwnerAdmissionControl {
    gate: Arc<AdmissionGate>,
}

impl OwnerAdmissionControl {
    pub(crate) fn new(gate: Arc<AdmissionGate>) -> Self {
        Self { gate }
    }

    /// Pause admission and invalidate all permits and registration tokens
    /// minted before this pause. Returns false if the host is terminal.
    pub fn pause(&self) -> bool {
        let mut state = self.gate.lock();
        match state.phase {
            AdmissionPhase::Terminal => return false,
            AdmissionPhase::Open => {
                state.generation = state
                    .generation
                    .checked_add(1)
                    .expect("host admission generation exhausted");
                state.phase = AdmissionPhase::Paused;
            }
            AdmissionPhase::Paused => {}
        }
        self.gate.notify.notify_waiters();
        true
    }

    /// Open only this host gate. Explicit route closure is unaffected and old
    /// permits remain invalid. The deadline is checked under the same commit
    /// lock, so waiting for that lock cannot reopen an expired owner window.
    pub fn resume(&self, deadline: Instant) -> bool {
        let mut state = self.gate.lock();
        if state.phase == AdmissionPhase::Terminal || Instant::now() >= deadline {
            return false;
        }
        state.phase = AdmissionPhase::Open;
        self.gate.notify.notify_waiters();
        true
    }
}
