use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};

use tokio::sync::Notify;

/// Method access level (from CRM contract `@cc.read` / `@cc.write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    Read,
    Write,
}

/// Concurrency mode for a CRM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyMode {
    /// No locking — all methods run concurrently.
    Parallel,
    /// Single-threaded — one method at a time.
    Exclusive,
    /// Reader-writer lock — reads concurrent, writes exclusive.
    ReadParallel,
}

impl ConcurrencyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Exclusive => "exclusive",
            Self::ReadParallel => "read_parallel",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SchedulerLimits {
    pub max_pending: Option<NonZeroUsize>,
    pub max_workers: Option<NonZeroUsize>,
}

impl SchedulerLimits {
    pub fn try_from_usize(
        max_pending: Option<usize>,
        max_workers: Option<usize>,
    ) -> Result<Self, String> {
        let max_pending = max_pending
            .map(|value| {
                NonZeroUsize::new(value).ok_or_else(|| "max_pending must be at least 1".to_string())
            })
            .transpose()?;
        let max_workers = max_workers
            .map(|value| {
                NonZeroUsize::new(value).ok_or_else(|| "max_workers must be at least 1".to_string())
            })
            .transpose()?;
        Ok(Self {
            max_pending,
            max_workers,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerSnapshot {
    pub mode: ConcurrencyMode,
    pub max_pending: Option<usize>,
    pub max_workers: Option<usize>,
    pub pending: usize,
    pub active_workers: usize,
    pub closed: bool,
    pub is_unconstrained: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerAcquireError {
    Closed,
    Capacity { field: &'static str, limit: usize },
}

impl std::fmt::Display for SchedulerAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "route closed"),
            Self::Capacity { field, limit } => {
                write!(f, "route concurrency capacity exceeded: {field}={limit}")
            }
        }
    }
}

impl std::error::Error for SchedulerAcquireError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldAccess {
    Parallel,
    Read,
    Write,
}

#[derive(Debug, Default)]
struct SchedulerState {
    closed: bool,
    pending: usize,
    active_workers: usize,
    active_readers: usize,
    writer_active: bool,
    waiting_writers: usize,
    next_ticket: u64,
    queue: VecDeque<QueuedPermit>,
}

#[derive(Debug)]
struct SchedulerInner {
    mode: ConcurrencyMode,
    access_map: HashMap<u16, AccessLevel>,
    limits: SchedulerLimits,
    state: Mutex<SchedulerState>,
    cvar: Condvar,
    notify: Arc<Notify>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedPermit {
    ticket: u64,
    access: HeldAccess,
}

/// Per-CRM scheduler controlling method execution concurrency.
///
/// This is the Rust-owned route concurrency authority. Clones share the same
/// state and can be projected into language SDKs for direct same-process calls.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

#[derive(Clone)]
pub struct RouteConcurrencyHandle {
    scheduler: Scheduler,
}

pub struct SchedulerGuard {
    inner: Arc<SchedulerInner>,
    access: HeldAccess,
}

#[derive(Debug)]
pub(crate) struct SchedulerPendingPermit {
    inner: Arc<SchedulerInner>,
    access: HeldAccess,
    ticket: u64,
    writer_wait_registered: bool,
    completed: bool,
}

impl std::fmt::Debug for SchedulerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerGuard")
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

impl SchedulerInner {
    fn notify_all(&self) {
        self.cvar.notify_all();
        self.notify.notify_waiters();
    }

    fn worker_slots_available(&self, state: &SchedulerState) -> usize {
        self.limits.max_workers.map_or(usize::MAX, |limit| {
            limit.get().saturating_sub(state.active_workers)
        })
    }

    fn remove_queued(&self, state: &mut SchedulerState, ticket: u64) {
        if let Some(pos) = state.queue.iter().position(|entry| entry.ticket == ticket) {
            state.queue.remove(pos);
        }
    }

    fn can_activate(&self, state: &SchedulerState, ticket: u64, access: HeldAccess) -> bool {
        if state.closed {
            return false;
        }
        let worker_slots = self.worker_slots_available(state);
        if worker_slots == 0 {
            return false;
        }

        match access {
            HeldAccess::Parallel => state
                .queue
                .iter()
                .position(|entry| entry.ticket == ticket)
                .is_some_and(|pos| pos < worker_slots),
            HeldAccess::Read => {
                if state.writer_active {
                    return false;
                }
                for (earlier_readers, entry) in state.queue.iter().enumerate() {
                    if entry.ticket == ticket {
                        return earlier_readers < worker_slots;
                    }
                    if entry.access == HeldAccess::Write {
                        return false;
                    }
                }
                false
            }
            HeldAccess::Write => {
                state
                    .queue
                    .front()
                    .is_some_and(|entry| entry.ticket == ticket)
                    && !state.writer_active
                    && state.active_readers == 0
            }
        }
    }
}

impl Scheduler {
    #[cfg(test)]
    pub(crate) fn new(mode: ConcurrencyMode, access_map: HashMap<u16, AccessLevel>) -> Self {
        Self::with_limits(mode, access_map, SchedulerLimits::default())
    }

    pub fn with_limits(
        mode: ConcurrencyMode,
        access_map: HashMap<u16, AccessLevel>,
        limits: SchedulerLimits,
    ) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                mode,
                access_map,
                limits,
                state: Mutex::new(SchedulerState::default()),
                cvar: Condvar::new(),
                notify: Arc::new(Notify::new()),
            }),
        }
    }

    pub fn snapshot(&self) -> SchedulerSnapshot {
        let state = self.inner.state.lock().unwrap();
        SchedulerSnapshot {
            mode: self.inner.mode,
            max_pending: self.inner.limits.max_pending.map(NonZeroUsize::get),
            max_workers: self.inner.limits.max_workers.map(NonZeroUsize::get),
            pending: state.pending,
            active_workers: state.active_workers,
            closed: state.closed,
            is_unconstrained: self.is_unconstrained_with_state(&state),
        }
    }

    pub fn access_map_snapshot(&self) -> HashMap<u16, AccessLevel> {
        self.inner.access_map.clone()
    }

    pub fn is_unconstrained(&self) -> bool {
        let state = self.inner.state.lock().unwrap();
        self.is_unconstrained_with_state(&state)
    }

    fn is_unconstrained_with_state(&self, state: &SchedulerState) -> bool {
        self.inner.mode == ConcurrencyMode::Parallel
            && self.inner.limits.max_pending.is_none()
            && self.inner.limits.max_workers.is_none()
            && !state.closed
    }

    pub fn close(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.notify_all();
    }

    pub(crate) async fn wait_drained_async(&self) {
        if self.is_drained() {
            return;
        }
        loop {
            let notified = self.inner.notify.notified();
            if self.is_drained() {
                return;
            }
            notified.await;
            if self.is_drained() {
                return;
            }
        }
    }

    fn is_drained(&self) -> bool {
        let state = self.inner.state.lock().unwrap();
        state.pending == 0 && state.active_workers == 0
    }

    pub(crate) fn open_admission_for_registration(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closed = false;
        self.inner.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn try_acquire(
        &self,
        method_idx: u16,
    ) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.reserve_pending(method_idx)?.try_activate()
    }

    pub fn blocking_acquire(
        &self,
        method_idx: u16,
    ) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.reserve_pending(method_idx)?.blocking_activate()
    }

    pub(crate) async fn async_acquire(
        &self,
        method_idx: u16,
    ) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.reserve_pending(method_idx)?.async_activate().await
    }

    pub(crate) fn reserve_pending(
        &self,
        method_idx: u16,
    ) -> Result<SchedulerPendingPermit, SchedulerAcquireError> {
        let access = self.held_access(method_idx);
        let mut state = self.inner.state.lock().unwrap();

        if state.closed {
            return Err(SchedulerAcquireError::Closed);
        }
        if let Some(limit) = self.inner.limits.max_pending
            && state.pending >= limit.get()
        {
            return Err(SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: limit.get(),
            });
        }

        state.pending += 1;
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.queue.push_back(QueuedPermit { ticket, access });
        let mut writer_wait_registered = false;
        if access == HeldAccess::Write {
            state.waiting_writers += 1;
            writer_wait_registered = true;
        }

        Ok(SchedulerPendingPermit {
            inner: Arc::clone(&self.inner),
            access,
            ticket,
            writer_wait_registered,
            completed: false,
        })
    }

    fn held_access(&self, method_idx: u16) -> HeldAccess {
        match self.inner.mode {
            ConcurrencyMode::Parallel => HeldAccess::Parallel,
            ConcurrencyMode::Exclusive => HeldAccess::Write,
            ConcurrencyMode::ReadParallel => match self
                .inner
                .access_map
                .get(&method_idx)
                .copied()
                .unwrap_or(AccessLevel::Write)
            {
                AccessLevel::Read => HeldAccess::Read,
                AccessLevel::Write => HeldAccess::Write,
            },
        }
    }

    /// Execute a CRM method under the appropriate concurrency guard.
    ///
    /// `f` runs inside `spawn_blocking`; language-runtime entry happens there.
    /// The guard is acquired and held in the blocking task until `f` returns.
    #[cfg(test)]
    async fn execute<F, R>(&self, method_idx: u16, f: F) -> Result<R, SchedulerAcquireError>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let sched = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = sched.blocking_acquire(method_idx)?;
            Ok(f())
        })
        .await
        .expect("scheduler task panicked")
    }
}

impl RouteConcurrencyHandle {
    pub(crate) fn new(scheduler: Scheduler) -> Self {
        Self { scheduler }
    }

    pub fn snapshot(&self) -> SchedulerSnapshot {
        self.scheduler.snapshot()
    }

    pub fn blocking_acquire(
        &self,
        method_idx: u16,
    ) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.scheduler.blocking_acquire(method_idx)
    }

    pub fn is_unconstrained(&self) -> bool {
        self.scheduler.is_unconstrained()
    }
}

impl SchedulerPendingPermit {
    #[cfg(test)]
    pub(crate) fn try_activate(self) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.activate(false)
    }

    pub(crate) fn blocking_activate(self) -> Result<SchedulerGuard, SchedulerAcquireError> {
        self.activate(true)
    }

    pub(crate) async fn async_activate(mut self) -> Result<SchedulerGuard, SchedulerAcquireError> {
        loop {
            let notified = {
                let mut state = self.inner.state.lock().unwrap();
                if state.closed {
                    state.pending -= 1;
                    self.inner.remove_queued(&mut state, self.ticket);
                    if self.writer_wait_registered {
                        state.waiting_writers -= 1;
                        self.writer_wait_registered = false;
                    }
                    self.completed = true;
                    self.inner.notify_all();
                    return Err(SchedulerAcquireError::Closed);
                }

                if self.inner.can_activate(&state, self.ticket, self.access) {
                    self.inner.remove_queued(&mut state, self.ticket);
                    if self.writer_wait_registered {
                        state.waiting_writers -= 1;
                        self.writer_wait_registered = false;
                    }
                    match self.access {
                        HeldAccess::Parallel => {}
                        HeldAccess::Read => state.active_readers += 1,
                        HeldAccess::Write => state.writer_active = true,
                    }
                    state.active_workers += 1;
                    self.completed = true;

                    return Ok(SchedulerGuard {
                        inner: Arc::clone(&self.inner),
                        access: self.access,
                    });
                }

                Arc::clone(&self.inner.notify).notified_owned()
            };
            notified.await;
        }
    }

    fn activate(mut self, wait_for_mode: bool) -> Result<SchedulerGuard, SchedulerAcquireError> {
        let inner = Arc::clone(&self.inner);
        let mut state = inner.state.lock().unwrap();
        if state.closed {
            state.pending -= 1;
            inner.remove_queued(&mut state, self.ticket);
            if self.writer_wait_registered {
                state.waiting_writers -= 1;
                self.writer_wait_registered = false;
            }
            self.completed = true;
            inner.notify_all();
            return Err(SchedulerAcquireError::Closed);
        }

        loop {
            let workers_available = inner.worker_slots_available(&state) > 0;
            let can_activate = inner.can_activate(&state, self.ticket, self.access);

            if can_activate {
                break;
            }
            if !wait_for_mode {
                let field = if workers_available {
                    "mode"
                } else {
                    "max_workers"
                };
                let limit = self.inner.limits.max_workers.map_or(1, NonZeroUsize::get);
                state.pending -= 1;
                inner.remove_queued(&mut state, self.ticket);
                if self.writer_wait_registered {
                    state.waiting_writers -= 1;
                    self.writer_wait_registered = false;
                }
                self.completed = true;
                inner.notify_all();
                return Err(SchedulerAcquireError::Capacity { field, limit });
            }
            state = inner.cvar.wait(state).unwrap();
            if state.closed {
                state.pending -= 1;
                inner.remove_queued(&mut state, self.ticket);
                if self.writer_wait_registered {
                    state.waiting_writers -= 1;
                    self.writer_wait_registered = false;
                }
                self.completed = true;
                inner.notify_all();
                return Err(SchedulerAcquireError::Closed);
            }
        }

        inner.remove_queued(&mut state, self.ticket);
        if self.writer_wait_registered {
            state.waiting_writers -= 1;
            self.writer_wait_registered = false;
        }
        match self.access {
            HeldAccess::Parallel => {}
            HeldAccess::Read => state.active_readers += 1,
            HeldAccess::Write => state.writer_active = true,
        }
        state.active_workers += 1;
        self.completed = true;

        Ok(SchedulerGuard {
            inner: Arc::clone(&self.inner),
            access: self.access,
        })
    }
}

impl Drop for SchedulerPendingPermit {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let mut state = inner.state.lock().unwrap();
        state.pending -= 1;
        inner.remove_queued(&mut state, self.ticket);
        if self.writer_wait_registered {
            state.waiting_writers -= 1;
            self.writer_wait_registered = false;
        }
        self.completed = true;
        inner.notify_all();
    }
}

impl SchedulerGuard {
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.state.lock().unwrap().closed
    }

    pub(crate) async fn wait_closed(&self) {
        if self.is_closed() {
            return;
        }
        loop {
            let notified = self.inner.notify.notified();
            if self.is_closed() {
                return;
            }
            notified.await;
            if self.is_closed() {
                return;
            }
        }
    }
}

impl Drop for SchedulerGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.pending -= 1;
        state.active_workers -= 1;
        match self.access {
            HeldAccess::Parallel => {}
            HeldAccess::Read => state.active_readers -= 1,
            HeldAccess::Write => state.writer_active = false,
        }
        self.inner.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn empty_map() -> HashMap<u16, AccessLevel> {
        HashMap::new()
    }

    fn read_write_map() -> HashMap<u16, AccessLevel> {
        let mut m = HashMap::new();
        m.insert(0, AccessLevel::Read);
        m.insert(1, AccessLevel::Write);
        m
    }

    #[test]
    fn unconstrained_snapshot_reports_fast_path() {
        let sched = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits::default(),
        );
        let snap = sched.snapshot();
        assert!(snap.is_unconstrained);
        assert!(!snap.closed);
        assert_eq!(snap.max_pending, None);
        assert_eq!(snap.max_workers, None);
    }

    #[test]
    fn close_rejects_new_acquires_but_keeps_snapshot() {
        let sched = Scheduler::with_limits(
            ConcurrencyMode::Exclusive,
            HashMap::new(),
            SchedulerLimits::default(),
        );
        sched.close();
        let snap = sched.snapshot();
        assert!(snap.closed);
        assert_eq!(
            sched.try_acquire(0).unwrap_err(),
            SchedulerAcquireError::Closed
        );
    }

    #[test]
    fn close_rejects_waiting_acquires_after_wakeup() {
        use std::sync::mpsc;

        let sched = Arc::new(Scheduler::with_limits(
            ConcurrencyMode::Exclusive,
            HashMap::new(),
            SchedulerLimits::default(),
        ));
        let first = sched.try_acquire(0).expect("first acquire should enter");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();

        let waiter = {
            let s = Arc::clone(&sched);
            std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let result = s.blocking_acquire(0).map(|_| ());
                result_tx.send(result).unwrap();
            })
        };

        ready_rx.recv().unwrap();
        sched.close();
        drop(first);

        let result = result_rx.recv().unwrap();
        assert_eq!(result.unwrap_err(), SchedulerAcquireError::Closed);
        waiter.join().unwrap();
    }

    #[test]
    fn route_limits_fail_fast_when_capacity_is_exhausted() {
        let sched = Arc::new(Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        ));

        let first = sched.try_acquire(0).expect("first acquire should enter");
        let err = sched.try_acquire(0).unwrap_err();
        assert_eq!(
            err,
            SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            },
        );

        drop(first);
        let second = sched.try_acquire(0).expect("capacity releases on drop");
        drop(second);
        assert_eq!(sched.snapshot().pending, 0);
        assert_eq!(sched.snapshot().active_workers, 0);
    }

    #[test]
    fn pending_reservation_drop_restores_capacity() {
        let sched = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(1).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );

        let reserved = sched
            .reserve_pending(0)
            .expect("first reservation should succeed");
        assert_eq!(
            sched.reserve_pending(0).unwrap_err(),
            SchedulerAcquireError::Capacity {
                field: "max_pending",
                limit: 1,
            }
        );

        drop(reserved);

        let second = sched
            .reserve_pending(0)
            .expect("reservation capacity should release on drop");
        drop(second);
        assert_eq!(sched.snapshot().pending, 0);
    }

    #[test]
    fn write_pending_reservation_blocks_new_readers_until_released() {
        let sched = Scheduler::with_limits(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(2).unwrap()),
                max_workers: Some(NonZeroUsize::new(2).unwrap()),
            },
        );

        let reserved_write = sched
            .reserve_pending(1)
            .expect("write reservation should succeed");
        assert_eq!(
            sched.try_acquire(0).unwrap_err(),
            SchedulerAcquireError::Capacity {
                field: "mode",
                limit: 2,
            }
        );

        drop(reserved_write);

        let read = sched
            .try_acquire(0)
            .expect("read should acquire once write reservation is gone");
        drop(read);
    }

    #[test]
    fn exclusive_waits_without_consuming_extra_worker_permits() {
        use std::sync::{Barrier, mpsc};
        use std::time::Duration;

        let sched = Arc::new(Scheduler::with_limits(
            ConcurrencyMode::Exclusive,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(2).unwrap()),
                max_workers: Some(NonZeroUsize::new(2).unwrap()),
            },
        ));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();

        let s1 = Arc::clone(&sched);
        let entered1 = Arc::clone(&entered);
        let release1 = Arc::clone(&release);
        let first = std::thread::spawn(move || {
            let _guard = s1.blocking_acquire(0).expect("first enters");
            entered1.wait();
            release1.wait();
        });

        entered.wait();

        let s2 = Arc::clone(&sched);
        let second = std::thread::spawn(move || {
            let _guard = s2.blocking_acquire(0).expect("second waits then enters");
            tx.send(()).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        release.wait();
        first.join().unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        second.join().unwrap();
    }

    #[test]
    fn parallel_blocking_acquire_waits_for_worker_capacity() {
        use std::sync::{Barrier, mpsc};
        use std::time::Duration;

        let sched = Arc::new(Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(2).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        ));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();

        let s1 = Arc::clone(&sched);
        let entered1 = Arc::clone(&entered);
        let release1 = Arc::clone(&release);
        let first = std::thread::spawn(move || {
            let _guard = s1.blocking_acquire(0).expect("first enters");
            entered1.wait();
            release1.wait();
        });

        entered.wait();

        let s2 = Arc::clone(&sched);
        let second = std::thread::spawn(move || {
            let _guard = s2.blocking_acquire(0).expect("second waits then enters");
            tx.send(()).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        release.wait();
        first.join().unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        second.join().unwrap();
    }

    #[tokio::test]
    async fn parallel_async_activation_respects_reservation_fifo() {
        use std::num::NonZeroUsize;
        use tokio::time::{Duration, sleep, timeout};

        let sched = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(3).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        let first = sched.try_acquire(0).expect("first enters");
        let earlier = sched
            .reserve_pending(0)
            .expect("earlier reservation succeeds");
        let later = sched
            .reserve_pending(0)
            .expect("later reservation succeeds");
        let later_task = tokio::spawn(async move { later.async_activate().await });

        drop(first);
        sleep(Duration::from_millis(30)).await;
        assert!(
            !later_task.is_finished(),
            "later reservation must not overtake earlier pending permit"
        );

        drop(earlier);
        let later_guard = timeout(Duration::from_secs(1), later_task)
            .await
            .expect("later reservation should proceed after earlier drops")
            .expect("later task should not panic")
            .expect("later should acquire");
        drop(later_guard);
    }

    #[test]
    fn parallel_blocking_activation_respects_reservation_fifo() {
        use std::num::NonZeroUsize;
        use std::sync::mpsc;
        use std::time::Duration;

        let sched = Scheduler::with_limits(
            ConcurrencyMode::Parallel,
            HashMap::new(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(3).unwrap()),
                max_workers: Some(NonZeroUsize::new(1).unwrap()),
            },
        );
        let first = sched.try_acquire(0).expect("first enters");
        let earlier = sched
            .reserve_pending(0)
            .expect("earlier reservation succeeds");
        let later = sched
            .reserve_pending(0)
            .expect("later reservation succeeds");
        let (tx, rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let result = later.blocking_activate();
            tx.send(result.map(|_| ())).unwrap();
        });

        drop(first);
        assert!(
            rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "later blocking reservation must not overtake earlier pending permit"
        );

        drop(earlier);
        rx.recv_timeout(Duration::from_secs(1))
            .expect("later reservation should proceed after earlier drops")
            .expect("later should acquire");
        waiter.join().unwrap();
    }

    #[tokio::test]
    async fn read_parallel_allows_earlier_read_before_later_writer() {
        use std::num::NonZeroUsize;
        use tokio::time::{Duration, timeout};

        let sched = Scheduler::with_limits(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
            SchedulerLimits {
                max_pending: Some(NonZeroUsize::new(2).unwrap()),
                max_workers: Some(NonZeroUsize::new(2).unwrap()),
            },
        );
        let read = sched.reserve_pending(0).expect("read reservation succeeds");
        let write = sched
            .reserve_pending(1)
            .expect("writer reservation succeeds behind read");

        let read_guard = timeout(Duration::from_millis(100), read.async_activate())
            .await
            .expect("earlier read should not wait behind later writer")
            .expect("read should acquire");

        drop(read_guard);
        drop(write);
    }

    #[tokio::test]
    async fn parallel_allows_concurrent_execution() {
        let sched = Arc::new(Scheduler::new(ConcurrencyMode::Parallel, empty_map()));
        let counter = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = Arc::clone(&sched);
            let c = Arc::clone(&counter);
            let b = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(b.wait());
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exclusive_serializes_execution() {
        let sched = Arc::new(Scheduler::new(ConcurrencyMode::Exclusive, empty_map()));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn read_parallel_allows_concurrent_reads() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let counter = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = Arc::clone(&sched);
            let c = Arc::clone(&counter);
            let b = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                s.execute(0, move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(b.wait());
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn read_parallel_serializes_writes() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                s.execute(1, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_method_idx_defaults_to_write() {
        let sched = Arc::new(Scheduler::new(
            ConcurrencyMode::ReadParallel,
            read_write_map(),
        ));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let active = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let s = Arc::clone(&sched);
            let mc = Arc::clone(&max_concurrent);
            let ac = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                s.execute(99, move || {
                    let cur = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }
}
