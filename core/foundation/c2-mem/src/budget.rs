//! Capacity accounting for one client, Host, or relay domain.

use std::collections::{HashMap, TryReserveError, VecDeque};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use c2_config::DomainLimits;

const WARM_BUFFER_MIN_BYTES: usize = 256 * 1024;
const WARM_BUFFER_MAX_COUNT: usize = 4;
const WARM_BUFFER_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetCellSnapshot {
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub peak_bytes: u64,
    pub rejected_allocations: u64,
    pub rejected_bytes: u64,
}

impl BudgetCellSnapshot {
    fn empty(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            used_bytes: 0,
            peak_bytes: 0,
            rejected_allocations: 0,
            rejected_bytes: 0,
        }
    }

    fn available(&self) -> u64 {
        self.limit_bytes - self.used_bytes
    }

    fn admit(&mut self, bytes: u64) {
        self.used_bytes += bytes;
        self.peak_bytes = self.peak_bytes.max(self.used_bytes);
    }

    fn release(&mut self, bytes: u64) {
        self.used_bytes = self
            .used_bytes
            .checked_sub(bytes)
            .expect("released more than reserved");
    }

    fn reject(&mut self, requested: u128) {
        self.rejected_allocations = self.rejected_allocations.saturating_add(1);
        self.rejected_bytes = self
            .rejected_bytes
            .saturating_add(u64::try_from(requested).unwrap_or(u64::MAX));
    }

    fn check_memory_request(&mut self, requested: u128) -> Result<(), PayloadBufferError> {
        if requested > u128::from(self.limit_bytes) {
            self.reject(requested);
            return Err(PayloadBufferError::TooLarge {
                requested,
                limit: self.limit_bytes,
            });
        }
        Ok(())
    }

    fn try_admit_memory(&mut self, bytes: u64, requested: u128) -> Result<(), PayloadBufferError> {
        self.check_memory_request(requested)?;
        let available = self.available();
        if bytes > available {
            self.reject(requested);
            return Err(PayloadBufferError::InsufficientCapacity {
                requested,
                available,
            });
        }
        self.admit(bytes);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainSnapshot {
    pub domain_id: u64,
    pub closed: bool,
    pub memory: BudgetCellSnapshot,
    pub file_spill: BudgetCellSnapshot,
    pub operations: RetentionSnapshot,
}

/// Rejected file-spill reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetError {
    pub requested: u64,
    pub used: u64,
    pub limit: u64,
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "file spill budget rejected a {}-byte reservation: {} of {} bytes already in use",
            self.requested, self.used, self.limit
        )
    }
}

impl Error for BudgetError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadBufferError {
    TooLarge { requested: u128, limit: u64 },
    InsufficientCapacity { requested: u128, available: u64 },
    DeadlineExceeded,
}

impl fmt::Display for PayloadBufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { requested, limit } => {
                write!(
                    f,
                    "{requested}-byte reservation exceeds {limit}-byte memory budget"
                )
            }
            Self::InsufficientCapacity {
                requested,
                available,
            } => {
                write!(
                    f,
                    "{requested}-byte memory reservation exceeds {available} available bytes"
                )
            }
            Self::DeadlineExceeded => f.write_str("memory reservation deadline exceeded"),
        }
    }
}

impl Error for PayloadBufferError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSnapshot {
    pub max_operations: u64,
    pub used_operations: u64,
    pub peak_operations: u64,
    pub rejected_reservations: u64,
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionRejectReason {
    LimitReached,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionError {
    pub reason: RetentionRejectReason,
    pub used_operations: u64,
    pub max_operations: u64,
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            RetentionRejectReason::Closed => f.write_str("call admission is closed"),
            RetentionRejectReason::LimitReached => {
                write!(f, "all {} operation slots are in use", self.max_operations)
            }
        }
    }
}
impl Error for RetentionError {}

/// Waiters are registered under the same lock as the capacity check. Waker
/// cloning, destruction and notification happen outside the accounting lock.
#[derive(Debug, Default)]
pub(crate) struct AsyncWaiters {
    next_id: u64,
    registered: HashMap<u64, Waker>,
}

impl AsyncWaiters {
    pub(crate) fn register(&mut self, waiter: &mut Option<u64>, waker: Waker) -> Option<Waker> {
        let id = *waiter.get_or_insert_with(|| {
            let id = self.next_id;
            self.next_id = id.checked_add(1).expect("reservation waiter id exhausted");
            id
        });
        self.registered.insert(id, waker)
    }

    pub(crate) fn remove(&mut self, waiter: &mut Option<u64>) -> Option<Waker> {
        waiter.take().and_then(|id| self.registered.remove(&id))
    }

    pub(crate) fn take(&mut self) -> impl IntoIterator<Item = Waker> + use<> {
        std::mem::take(&mut self.registered).into_values()
    }
}

#[derive(Debug)]
struct DomainState {
    memory: BudgetCellSnapshot,
    file_spill: BudgetCellSnapshot,
    operations: RetentionSnapshot,
    active_subscriptions: u64,
    active_control_operations: u64,
    memory_waiters: AsyncWaiters,
    warm_buffers: VecDeque<Vec<u8>>,
    warm_idle_bytes: u64,
}

impl DomainState {
    // Return evictions to the caller so allocation storage is freed after
    // unlocking the domain.
    fn evict_warm_to(&mut self, bytes: u64, count: usize) -> Vec<Vec<u8>> {
        let mut evicted = Vec::new();
        while self.warm_idle_bytes > bytes || self.warm_buffers.len() > count {
            let oldest = self.warm_buffers.pop_front().expect("warm buffer to evict");
            self.warm_idle_bytes -= oldest.capacity() as u64;
            evicted.push(oldest);
        }
        evicted
    }

    fn evict_for_admission(&mut self) -> Vec<Vec<u8>> {
        self.evict_warm_to(self.memory.available(), WARM_BUFFER_MAX_COUNT)
    }

    fn warm_capacity(&self) -> u64 {
        self.memory.available().min(WARM_BUFFER_MAX_BYTES)
    }

    fn recycle_warm_vec(&mut self, bytes: &mut Vec<u8>) -> Vec<Vec<u8>> {
        if self.operations.closed {
            return Vec::new();
        }
        let cap = bytes.capacity() as u64;
        let limit = self.warm_capacity();
        if cap < WARM_BUFFER_MIN_BYTES as u64 || cap > limit {
            return Vec::new();
        }
        let evicted = self.evict_warm_to(limit - cap, WARM_BUFFER_MAX_COUNT - 1);
        bytes.clear();
        self.warm_buffers.push_back(std::mem::take(bytes));
        self.warm_idle_bytes += cap;
        evicted
    }

    fn take_warm_vec(&mut self, min_len: usize, max_cap: usize) -> Option<Vec<u8>> {
        let (index, cap) = self
            .warm_buffers
            .iter()
            .map(Vec::capacity)
            .enumerate()
            .filter(|&(_, cap)| (min_len..=max_cap).contains(&cap))
            .min_by_key(|&(_, cap)| cap)?;
        self.warm_idle_bytes -= cap as u64;
        self.warm_buffers.remove(index)
    }
}

fn allocate_exact_vec(len: usize) -> Result<Vec<u8>, TryReserveError> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len)?;
    Ok(bytes)
}

#[derive(Debug)]
struct Shared {
    domain_id: u64,
    state: Mutex<DomainState>,
    memory_changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, DomainState> {
        self.state.lock().expect("domain accounting mutex poisoned")
    }

    fn snapshot(&self) -> DomainSnapshot {
        let state = self.lock();
        DomainSnapshot {
            domain_id: self.domain_id,
            closed: state.operations.closed,
            memory: state.memory,
            file_spill: state.file_spill,
            operations: state.operations,
        }
    }

    fn take_warm_vec(&self, len: usize) -> Option<Vec<u8>> {
        if len < WARM_BUFFER_MIN_BYTES {
            return None;
        }
        self.lock().take_warm_vec(len, len.saturating_add(len / 4))
    }

    fn take_warm_vec_for_reservation(&self, reserved: &mut u64, len: usize) -> Option<Vec<u8>> {
        if len < WARM_BUFFER_MIN_BYTES {
            return None;
        }
        let slack_cap = len.saturating_add(len / 4);
        let (bytes, evicted) = {
            let mut state = self.lock();
            let fit_cap = usize::try_from(reserved.saturating_add(state.memory.available()))
                .unwrap_or(usize::MAX);
            let max_cap = slack_cap.min(fit_cap);
            let Some(bytes) = state.take_warm_vec(len, max_cap) else {
                return None;
            };
            let additional = (bytes.capacity() as u64).saturating_sub(*reserved);
            let evicted = if additional > 0 {
                state.memory.admit(additional);
                *reserved += additional;
                state.evict_for_admission()
            } else {
                Vec::new()
            };
            (bytes, evicted)
        };
        drop(evicted);
        Some(bytes)
    }

    fn recycle_vec(&self, mut bytes: Vec<u8>) {
        if bytes.capacity() >= WARM_BUFFER_MIN_BYTES {
            let evicted = self.lock().recycle_warm_vec(&mut bytes);
            drop(evicted);
        }
    }

    fn release_memory(&self, bytes: u64) {
        self.release_memory_and_recycle(bytes, Vec::new());
    }

    fn release_memory_and_recycle(&self, bytes: u64, mut buffer: Vec<u8>) {
        if bytes == 0 {
            return;
        }
        let (waiters, evicted) = {
            let mut state = self.lock();
            state.memory.release(bytes);
            let evicted = state.recycle_warm_vec(&mut buffer);
            (state.memory_waiters.take(), evicted)
        };
        self.memory_changed.notify_all();
        for waiter in waiters {
            waiter.wake();
        }
        drop(evicted);
    }

    fn try_admit_memory(&self, bytes: u64, requested: u128) -> Result<(), PayloadBufferError> {
        let evicted = {
            let mut state = self.lock();
            state.memory.try_admit_memory(bytes, requested)?;
            state.evict_for_admission()
        };
        drop(evicted);
        Ok(())
    }
}

/// Shared capacity for one role. Reservations follow their storage or call
/// owners and may survive the pool or runtime that created the domain.
#[derive(Debug, Clone)]
pub struct DomainBudget {
    shared: Arc<Shared>,
}

impl DomainBudget {
    pub fn from_limits(limits: &DomainLimits) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let domain_id = NEXT_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("domain identity space exhausted");
        Self {
            shared: Arc::new(Shared {
                domain_id,
                state: Mutex::new(DomainState {
                    memory: BudgetCellSnapshot::empty(limits.memory_budget_bytes),
                    file_spill: BudgetCellSnapshot::empty(limits.file_spill_budget_bytes),
                    operations: RetentionSnapshot {
                        max_operations: limits.max_outstanding_calls,
                        used_operations: 0,
                        peak_operations: 0,
                        rejected_reservations: 0,
                        closed: false,
                    },
                    active_subscriptions: 0,
                    active_control_operations: 0,
                    memory_waiters: AsyncWaiters::default(),
                    warm_buffers: VecDeque::new(),
                    warm_idle_bytes: 0,
                }),
                memory_changed: Condvar::new(),
            }),
        }
    }

    pub fn domain_id(&self) -> u64 {
        self.shared.domain_id
    }

    pub fn snapshot(&self) -> DomainSnapshot {
        self.shared.snapshot()
    }

    pub fn observe(&self) -> DomainObserver {
        DomainObserver {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Acquire an empty local buffer without taking a memory reservation.
    pub fn allocate_vec(&self, len: usize) -> Result<Vec<u8>, TryReserveError> {
        match self.shared.take_warm_vec(len) {
            Some(bytes) => Ok(bytes),
            None => allocate_exact_vec(len),
        }
    }

    pub fn recycle_vec(&self, bytes: Vec<u8>) {
        self.shared.recycle_vec(bytes);
    }

    pub fn try_reserve_memory(&self, bytes: u64) -> Result<MemoryReservation, PayloadBufferError> {
        self.shared.try_admit_memory(bytes, bytes.into())?;
        Ok(MemoryReservation {
            shared: Arc::clone(&self.shared),
            bytes,
        })
    }

    /// Waits for capacity. Dropping the future cancels its wait; async callers
    /// enforce their own deadline.
    pub fn reserve_memory(&self, bytes: u64) -> ReserveFuture<'_> {
        ReserveFuture {
            budget: self,
            fixed_bytes: bytes,
            min_window: 0,
            max_window: 0,
            waiter: None,
        }
    }

    /// Waits for `fixed_bytes + min_window` of capacity and atomically claims
    /// `fixed_bytes` plus a fair share of the remaining available capacity
    /// within `min_window..=max_window`.
    ///
    /// When the remaining available capacity is at least `2 * max_window`,
    /// claims the full `max_window`; under contention it claims
    /// `(remaining / 2).clamp(min_window, max_window)` so long-lived streams,
    /// topic hubs and concurrent calls cannot starve one another.
    pub fn reserve_memory_with_window(
        &self,
        fixed_bytes: u64,
        min_window: u64,
        max_window: u64,
    ) -> ReserveFuture<'_> {
        ReserveFuture {
            budget: self,
            fixed_bytes,
            min_window,
            max_window: max_window.max(min_window),
            waiter: None,
        }
    }

    /// Waits for capacity and atomically claims a matching warm local buffer
    /// before evicting other idle buffers.
    pub fn reserve_vec(&self, bytes: u64) -> ReserveVecFuture<'_> {
        ReserveVecFuture {
            budget: self,
            bytes,
            waiter: None,
        }
    }

    pub fn reserve_memory_blocking(
        &self,
        bytes: u64,
        deadline: Option<Instant>,
    ) -> Result<MemoryReservation, PayloadBufferError> {
        let mut state = self.shared.lock();
        state.memory.check_memory_request(bytes.into())?;
        loop {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Err(PayloadBufferError::DeadlineExceeded);
            }
            if bytes <= state.memory.available() {
                state.memory.admit(bytes);
                let evicted = state.evict_for_admission();
                drop(state);
                drop(evicted);
                return Ok(MemoryReservation {
                    shared: Arc::clone(&self.shared),
                    bytes,
                });
            }
            state = if let Some(end) = deadline {
                self.shared
                    .memory_changed
                    .wait_timeout(state, end.saturating_duration_since(Instant::now()))
                    .expect("domain accounting mutex poisoned")
                    .0
            } else {
                self.shared
                    .memory_changed
                    .wait(state)
                    .expect("domain accounting mutex poisoned")
            };
        }
    }

    pub fn try_reserve_file_spill(&self, bytes: u64) -> Result<FileSpillReservation, BudgetError> {
        let mut state = self.shared.lock();
        let file = &mut state.file_spill;
        if bytes > file.available() {
            file.reject(bytes.into());
            return Err(BudgetError {
                requested: bytes,
                used: file.used_bytes,
                limit: file.limit_bytes,
            });
        }
        file.admit(bytes);
        Ok(FileSpillReservation {
            shared: Arc::clone(&self.shared),
            bytes,
        })
    }

    pub fn reserve_operation(&self) -> Result<OperationPermit, RetentionError> {
        let mut state = self.shared.lock();
        let operations = &mut state.operations;
        let rejection = if operations.closed {
            Some(RetentionRejectReason::Closed)
        } else if operations.used_operations == operations.max_operations {
            Some(RetentionRejectReason::LimitReached)
        } else {
            None
        };
        if let Some(reason) = rejection {
            operations.rejected_reservations = operations.rejected_reservations.saturating_add(1);
            return Err(RetentionError {
                reason,
                used_operations: operations.used_operations,
                max_operations: operations.max_operations,
            });
        }
        operations.used_operations += 1;
        operations.peak_operations = operations.peak_operations.max(operations.used_operations);
        Ok(OperationPermit {
            shared: Arc::clone(&self.shared),
            is_subscription: false,
        })
    }

    /// Admit a bounded control operation independently of the business slots.
    pub fn reserve_control_operation(&self) -> Result<ControlOperationPermit, RetentionError> {
        let mut state = self.shared.lock();
        let max_operations = state.operations.max_operations.clamp(1, 64);
        let used_operations = state.active_control_operations;
        let reason = if state.operations.closed {
            Some(RetentionRejectReason::Closed)
        } else if used_operations == max_operations {
            Some(RetentionRejectReason::LimitReached)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(RetentionError {
                reason,
                used_operations,
                max_operations,
            });
        }
        state.active_control_operations += 1;
        Ok(ControlOperationPermit {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Reserve one business slot for a long-lived topic subscription.
    ///
    /// When `max_operations >= 2`, long-lived subscriptions may occupy at most
    /// `max_operations - 1` slots so at least one ordinary control/unary call
    /// can always enter once transient calls complete. `max_operations == 1`
    /// permits one total operation; `max_operations == 0` rejects all.
    pub fn reserve_subscription_operation(&self) -> Result<OperationPermit, RetentionError> {
        let mut state = self.shared.lock();
        let max_ops = state.operations.max_operations;
        let max_subs = if max_ops >= 2 { max_ops - 1 } else { max_ops };
        let rejection = if state.operations.closed {
            Some(RetentionRejectReason::Closed)
        } else if state.operations.used_operations == max_ops
            || state.active_subscriptions >= max_subs
        {
            Some(RetentionRejectReason::LimitReached)
        } else {
            None
        };
        if let Some(reason) = rejection {
            state.operations.rejected_reservations =
                state.operations.rejected_reservations.saturating_add(1);
            return Err(RetentionError {
                reason,
                used_operations: state.operations.used_operations,
                max_operations: max_ops,
            });
        }
        state.operations.used_operations += 1;
        state.operations.peak_operations = state
            .operations
            .peak_operations
            .max(state.operations.used_operations);
        state.active_subscriptions += 1;
        Ok(OperationPermit {
            shared: Arc::clone(&self.shared),
            is_subscription: true,
        })
    }

    /// Stop new calls; admitted work can still reserve memory and file spill.
    ///
    /// Drains idle warm buffers so a retiring domain does not retain cached
    /// heap capacity behind surviving observers or holds.
    pub fn close_operations(&self) {
        let evicted = {
            let mut state = self.shared.lock();
            state.operations.closed = true;
            state.evict_warm_to(0, 0)
        };
        drop(evicted);
    }
}

#[derive(Debug, Clone)]
pub struct DomainObserver {
    shared: Arc<Shared>,
}

impl DomainObserver {
    pub fn snapshot(&self) -> DomainSnapshot {
        self.shared.snapshot()
    }

    pub fn downgrade(&self) -> DomainObserverWeak {
        DomainObserverWeak {
            shared: Arc::downgrade(&self.shared),
        }
    }
}

/// Observes active or retired accounting without keeping it alive.
#[derive(Debug, Clone)]
pub struct DomainObserverWeak {
    shared: Weak<Shared>,
}

impl DomainObserverWeak {
    pub fn is_alive(&self) -> bool {
        self.shared.strong_count() > 0
    }

    pub fn snapshot(&self) -> Option<DomainSnapshot> {
        self.upgrade().map(|observer| observer.snapshot())
    }

    pub fn upgrade(&self) -> Option<DomainObserver> {
        self.shared
            .upgrade()
            .map(|shared| DomainObserver { shared })
    }
}

#[must_use = "poll or await the reservation before allocating storage"]
pub struct ReserveFuture<'a> {
    budget: &'a DomainBudget,
    fixed_bytes: u64,
    min_window: u64,
    max_window: u64,
    waiter: Option<u64>,
}

impl Future for ReserveFuture<'_> {
    type Output = Result<MemoryReservation, PayloadBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let waker = cx.waker().clone();
        let mut state = this.budget.shared.lock();
        let min_total = u128::from(this.fixed_bytes) + u128::from(this.min_window);
        let (result, evicted) = match state.memory.check_memory_request(min_total) {
            Err(error) => (Err(error), Vec::new()),
            Ok(()) if min_total <= u128::from(state.memory.available()) => {
                let avail_window = state.memory.available() - this.fixed_bytes;
                let window = if this.min_window == this.max_window {
                    this.min_window
                } else if avail_window >= this.max_window.saturating_mul(2) {
                    this.max_window
                } else {
                    (avail_window / 2).clamp(this.min_window, this.max_window)
                };
                let claim = this.fixed_bytes + window;
                state.memory.admit(claim);
                (
                    Ok(MemoryReservation {
                        shared: Arc::clone(&this.budget.shared),
                        bytes: claim,
                    }),
                    state.evict_for_admission(),
                )
            }
            Ok(()) => {
                let old_waker = state.memory_waiters.register(&mut this.waiter, waker);
                drop(state);
                drop(old_waker);
                return Poll::Pending;
            }
        };
        let old_waker = state.memory_waiters.remove(&mut this.waiter);
        drop(state);
        drop(old_waker);
        drop(evicted);
        Poll::Ready(result)
    }
}

impl Drop for ReserveFuture<'_> {
    fn drop(&mut self) {
        let waker = self
            .budget
            .shared
            .lock()
            .memory_waiters
            .remove(&mut self.waiter);
        drop(waker);
    }
}

#[must_use = "poll or await the reservation before using storage"]
pub struct ReserveVecFuture<'a> {
    budget: &'a DomainBudget,
    bytes: u64,
    waiter: Option<u64>,
}

impl Future for ReserveVecFuture<'_> {
    type Output = Result<(Option<Vec<u8>>, MemoryReservation), PayloadBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let waker = cx.waker().clone();
        let mut state = this.budget.shared.lock();
        let (result, evicted) = match state.memory.check_memory_request(this.bytes.into()) {
            Err(error) => (Err(error), Vec::new()),
            Ok(()) if this.bytes <= state.memory.available() => {
                state.memory.admit(this.bytes);
                let mut reserved = this.bytes;
                let warm = usize::try_from(this.bytes)
                    .ok()
                    .filter(|&len| len >= WARM_BUFFER_MIN_BYTES)
                    .and_then(|len| {
                        let avail = usize::try_from(state.memory.available()).unwrap_or(usize::MAX);
                        let max_cap = len.saturating_add((len / 4).min(avail));
                        let bytes = state.take_warm_vec(len, max_cap)?;
                        let extra = (bytes.capacity() as u64).saturating_sub(this.bytes);
                        if extra > 0 {
                            state.memory.admit(extra);
                            reserved += extra;
                        }
                        Some(bytes)
                    });
                (
                    Ok((
                        warm,
                        MemoryReservation {
                            shared: Arc::clone(&this.budget.shared),
                            bytes: reserved,
                        },
                    )),
                    state.evict_for_admission(),
                )
            }
            Ok(()) => {
                let old_waker = state.memory_waiters.register(&mut this.waiter, waker);
                drop(state);
                drop(old_waker);
                return Poll::Pending;
            }
        };
        let old_waker = state.memory_waiters.remove(&mut this.waiter);
        drop(state);
        drop(old_waker);
        drop(evicted);
        Poll::Ready(result)
    }
}

impl Drop for ReserveVecFuture<'_> {
    fn drop(&mut self) {
        let waker = self
            .budget
            .shared
            .lock()
            .memory_waiters
            .remove(&mut self.waiter);
        drop(waker);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReservationError {
    pub requested: u64,
    pub reserved: u64,
}

impl fmt::Display for MemoryReservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} bytes requested from a {}-byte reservation",
            self.requested, self.reserved
        )
    }
}

impl Error for MemoryReservationError {}

#[must_use = "keep the reservation with its storage"]
#[derive(Debug)]
pub struct MemoryReservation {
    shared: Arc<Shared>,
    bytes: u64,
}

impl MemoryReservation {
    pub fn domain_budget(&self) -> DomainBudget {
        DomainBudget {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn try_grow(&mut self, additional: u64) -> Result<(), PayloadBufferError> {
        let requested = u128::from(self.bytes) + u128::from(additional);
        self.shared.try_admit_memory(additional, requested)?;
        self.bytes += additional;
        Ok(())
    }

    /// Acquire an empty vector for a length already covered by this reservation.
    pub fn allocate_vec(&mut self, len: usize) -> Result<Vec<u8>, TryReserveError> {
        assert!(
            len as u64 <= self.bytes,
            "length exceeds memory reservation"
        );
        if let Some(bytes) = self
            .shared
            .take_warm_vec_for_reservation(&mut self.bytes, len)
        {
            return Ok(bytes);
        }
        allocate_exact_vec(len)
    }

    fn release_and_recycle(&mut self, bytes: Vec<u8>) {
        let reserved = std::mem::take(&mut self.bytes);
        self.shared.release_memory_and_recycle(reserved, bytes);
    }

    /// Return unused bytes after shrinking or freeing their storage.
    pub fn shrink_to(&mut self, new_bytes: u64) -> Result<(), MemoryReservationError> {
        let released = self.remaining_after(new_bytes)?;
        self.bytes = new_bytes;
        self.shared.release_memory(released);
        Ok(())
    }

    /// Transfer part of the existing charge without altering the domain counters.
    pub fn split_off(&mut self, bytes: u64) -> Result<Self, MemoryReservationError> {
        self.bytes = self.remaining_after(bytes)?;
        Ok(Self {
            shared: Arc::clone(&self.shared),
            bytes,
        })
    }

    fn remaining_after(&self, bytes: u64) -> Result<u64, MemoryReservationError> {
        self.bytes.checked_sub(bytes).ok_or(MemoryReservationError {
            requested: bytes,
            reserved: self.bytes,
        })
    }

    pub fn into_buffer(self, bytes: Vec<u8>) -> BudgetedBuffer {
        BudgetedBuffer::new(bytes, self)
    }

    pub fn into_reusable_window(self) -> crate::ReusableWindow {
        crate::ReusableWindow::new(self)
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.shared.release_memory(self.bytes);
    }
}

/// Local bytes that can return their allocation to a domain's warm pool.
///
/// Holds only a weak reference to the domain so a detached local buffer never
/// keeps a retired domain alive after shutdown.
#[derive(Debug)]
pub struct RecycledBytes {
    bytes: Vec<u8>,
    budget: Option<Weak<Shared>>,
}

impl RecycledBytes {
    pub fn new(bytes: Vec<u8>, budget: DomainBudget) -> Self {
        Self {
            bytes,
            budget: Some(Arc::downgrade(&budget.shared)),
        }
    }

    pub fn owned(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            budget: None,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl Deref for RecycledBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for RecycledBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for RecycledBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::owned(bytes)
    }
}

impl Drop for RecycledBytes {
    fn drop(&mut self) {
        if let Some(shared) = self.budget.as_ref().and_then(Weak::upgrade) {
            shared.recycle_vec(std::mem::take(&mut self.bytes));
        }
    }
}

/// Buffer and its charge for the actual allocated capacity. Suitable for
/// `Bytes::from_owner`, which retains this owner until the final slice drops.
#[derive(Debug)]
pub struct BudgetedBuffer {
    bytes: Vec<u8>,
    reservation: Option<MemoryReservation>,
}

impl BudgetedBuffer {
    pub fn new(bytes: Vec<u8>, reservation: MemoryReservation) -> Self {
        assert!(
            bytes.capacity() as u64 <= reservation.bytes(),
            "buffer allocation exceeds its memory reservation"
        );
        Self {
            bytes,
            reservation: Some(reservation),
        }
    }

    /// A buffer that carries no domain charge: delivery-only owners whose
    /// bytes are transient send copies, never retained outcome storage.
    pub fn unreserved(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reservation: None,
        }
    }

    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn into_parts(mut self) -> (Vec<u8>, MemoryReservation) {
        (
            std::mem::take(&mut self.bytes),
            self.reservation.take().expect("buffer reservation"),
        )
    }
}

impl Drop for BudgetedBuffer {
    fn drop(&mut self) {
        if let Some(mut reservation) = self.reservation.take() {
            reservation.release_and_recycle(std::mem::take(&mut self.bytes));
        }
    }
}

impl AsRef<[u8]> for BudgetedBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Deref for BudgetedBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

#[must_use = "keep this reservation with its file backing"]
#[derive(Debug)]
pub struct FileSpillReservation {
    shared: Arc<Shared>,
    bytes: u64,
}

impl FileSpillReservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for FileSpillReservation {
    fn drop(&mut self) {
        self.shared.lock().file_spill.release(self.bytes);
    }
}

#[must_use = "keep the permit until the control operation finishes"]
#[derive(Debug)]
pub struct ControlOperationPermit {
    shared: Arc<Shared>,
}

impl Drop for ControlOperationPermit {
    fn drop(&mut self) {
        self.shared.lock().active_control_operations -= 1;
    }
}

#[must_use = "keep the permit until the call finishes"]
#[derive(Debug)]
pub struct OperationPermit {
    shared: Arc<Shared>,
    is_subscription: bool,
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.operations.used_operations = state
            .operations
            .used_operations
            .checked_sub(1)
            .expect("operation permit released more than once");
        if self.is_subscription {
            state.active_subscriptions = state
                .active_subscriptions
                .checked_sub(1)
                .expect("subscription permit released more than once");
        }
    }
}

#[cfg(test)]
mod tests;
