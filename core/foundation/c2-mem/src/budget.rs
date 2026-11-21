//! Capacity accounting for one client, Host, or relay domain.

use std::collections::HashMap;
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
    memory_waiters: AsyncWaiters,
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

    fn release_memory(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let waiters = {
            let mut state = self.lock();
            state.memory.release(bytes);
            state.memory_waiters.take()
        };
        self.memory_changed.notify_all();
        for waiter in waiters {
            waiter.wake();
        }
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
                    memory_waiters: AsyncWaiters::default(),
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

    pub fn try_reserve_memory(&self, bytes: u64) -> Result<MemoryReservation, PayloadBufferError> {
        self.shared
            .lock()
            .memory
            .try_admit_memory(bytes, bytes.into())?;
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
    pub fn close_operations(&self) {
        self.shared.lock().operations.closed = true;
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
    bytes: u64,
    waiter: Option<u64>,
}

impl Future for ReserveFuture<'_> {
    type Output = Result<MemoryReservation, PayloadBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let waker = cx.waker().clone();
        let mut state = this.budget.shared.lock();
        let memory = &mut state.memory;
        let result = match memory.check_memory_request(this.bytes.into()) {
            Err(error) => Err(error),
            Ok(()) if this.bytes <= memory.available() => {
                memory.admit(this.bytes);
                Ok(MemoryReservation {
                    shared: Arc::clone(&this.budget.shared),
                    bytes: this.bytes,
                })
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
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn try_grow(&mut self, additional: u64) -> Result<(), PayloadBufferError> {
        let requested = u128::from(self.bytes) + u128::from(additional);
        self.shared
            .lock()
            .memory
            .try_admit_memory(additional, requested)?;
        self.bytes += additional;
        Ok(())
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

/// Buffer and its charge for the actual allocated capacity. Suitable for
/// `Bytes::from_owner`, which retains this owner until the final slice drops.
#[derive(Debug)]
pub struct BudgetedBuffer {
    bytes: Vec<u8>,
    reservation: MemoryReservation,
}

impl BudgetedBuffer {
    pub fn new(bytes: Vec<u8>, reservation: MemoryReservation) -> Self {
        assert!(
            bytes.capacity() as u64 <= reservation.bytes(),
            "buffer allocation exceeds its memory reservation"
        );
        Self { bytes, reservation }
    }

    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn into_parts(self) -> (Vec<u8>, MemoryReservation) {
        (self.bytes, self.reservation)
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
