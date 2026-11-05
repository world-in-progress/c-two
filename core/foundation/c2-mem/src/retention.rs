//! One Runtime or relay payload-buffer budget and call-operation slots.
//!
//! A reservation charges C-Two-owned buffer storage until the last real owner
//! drops it. Byte waits never hold an operation lock while copying payloads,
//! doing I/O, or running a callback. The byte domain is independent of IPC
//! SHM, file and reassembly backing, and is not a process RSS bound.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadBufferSnapshot {
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub peak_bytes: u64,
    pub rejected_allocations: u64,
    pub rejected_bytes: u64,
}

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
                    "{requested}-byte payload exceeds {limit}-byte buffer budget"
                )
            }
            Self::InsufficientCapacity {
                requested,
                available,
            } => write!(
                f,
                "{requested}-byte payload cannot be reserved with {available} bytes currently available"
            ),
            Self::DeadlineExceeded => f.write_str("payload-buffer reservation deadline exceeded"),
        }
    }
}

impl Error for PayloadBufferError {}

#[derive(Debug)]
struct Inner {
    max_operations: u64,
    used_operations: u64,
    peak_operations: u64,
    rejected_operations: u64,
    operations_closed: bool,
    limit_bytes: u64,
    used_bytes: u64,
    peak_bytes: u64,
    rejected_allocations: u64,
    rejected_bytes: u64,
    next_waiter: u64,
    waiters: HashMap<u64, Waker>,
}

impl Inner {
    fn payload_snapshot(&self) -> PayloadBufferSnapshot {
        PayloadBufferSnapshot {
            limit_bytes: self.limit_bytes,
            used_bytes: self.used_bytes,
            peak_bytes: self.peak_bytes,
            rejected_allocations: self.rejected_allocations,
            rejected_bytes: self.rejected_bytes,
        }
    }

    fn check_request(&mut self, requested: u64) -> Result<(), PayloadBufferError> {
        if requested > self.limit_bytes {
            self.reject(u128::from(requested));
            return Err(PayloadBufferError::TooLarge {
                requested: u128::from(requested),
                limit: self.limit_bytes,
            });
        }
        Ok(())
    }

    fn reject(&mut self, requested: u128) {
        self.rejected_allocations = self.rejected_allocations.saturating_add(1);
        self.rejected_bytes = self
            .rejected_bytes
            .saturating_add(u64::try_from(requested).unwrap_or(u64::MAX));
    }

    fn can_admit(&self, bytes: u64) -> bool {
        // `bytes <= limit_bytes` was checked before this call. Subtraction
        // handles the complete u64 domain without a wrapping addition.
        bytes <= self.limit_bytes - self.used_bytes
    }

    fn admit(&mut self, bytes: u64) {
        self.used_bytes += bytes;
        self.peak_bytes = self.peak_bytes.max(self.used_bytes);
    }
}

#[derive(Debug)]
struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

fn locked(shared: &Shared) -> MutexGuard<'_, Inner> {
    shared
        .inner
        .lock()
        .expect("payload-buffer accounting mutex poisoned")
}

fn new_shared(max_operations: u64, limit_bytes: u64) -> Arc<Shared> {
    Arc::new(Shared {
        inner: Mutex::new(Inner {
            max_operations,
            used_operations: 0,
            peak_operations: 0,
            rejected_operations: 0,
            operations_closed: false,
            limit_bytes,
            used_bytes: 0,
            peak_bytes: 0,
            rejected_allocations: 0,
            rejected_bytes: 0,
            next_waiter: 1,
            waiters: HashMap::new(),
        }),
        changed: Condvar::new(),
    })
}

/// Shared B domain. Clone shares counters and releases, including across
/// connections and SDK holds. It owns no payload bytes.
#[derive(Debug, Clone)]
pub struct PayloadBufferBudget {
    shared: Arc<Shared>,
}

impl PayloadBufferBudget {
    pub fn new(limit_bytes: u64) -> Self {
        Self {
            shared: new_shared(u64::MAX, limit_bytes),
        }
    }

    /// The returned future waits without allocating bytes; dropping it
    /// deregisters its wakeup. Deadline and cancellation belong to the caller.
    pub fn reserve(&self, bytes: u64) -> ReserveFuture<'_> {
        ReserveFuture {
            budget: self,
            bytes,
            waiter: None,
        }
    }

    /// Immediate admission for storage that cannot safely wait while holding
    /// another positive-byte reservation in this same domain.
    pub fn try_reserve(&self, bytes: u64) -> Result<PayloadBufferReservation, PayloadBufferError> {
        let mut inner = locked(&self.shared);
        inner.check_request(bytes)?;
        if !inner.can_admit(bytes) {
            inner.reject(u128::from(bytes));
            return Err(PayloadBufferError::InsufficientCapacity {
                requested: u128::from(bytes),
                available: inner.limit_bytes - inner.used_bytes,
            });
        }
        inner.admit(bytes);
        Ok(PayloadBufferReservation {
            shared: self.shared.clone(),
            bytes,
        })
    }

    /// Wait on a caller thread outside language locks until the original
    /// absolute deadline or a real release.
    pub fn reserve_blocking(
        &self,
        bytes: u64,
        deadline: Option<Instant>,
    ) -> Result<PayloadBufferReservation, PayloadBufferError> {
        let mut inner = locked(&self.shared);
        inner.check_request(bytes)?;
        loop {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Err(PayloadBufferError::DeadlineExceeded);
            }
            if inner.can_admit(bytes) {
                inner.admit(bytes);
                return Ok(PayloadBufferReservation {
                    shared: self.shared.clone(),
                    bytes,
                });
            }
            inner = if let Some(end) = deadline {
                let duration = end.saturating_duration_since(Instant::now());
                self.shared
                    .changed
                    .wait_timeout(inner, duration)
                    .expect("payload-buffer accounting mutex poisoned")
                    .0
            } else {
                self.shared
                    .changed
                    .wait(inner)
                    .expect("payload-buffer accounting mutex poisoned")
            };
        }
    }

    pub fn snapshot(&self) -> PayloadBufferSnapshot {
        locked(&self.shared).payload_snapshot()
    }

    pub fn downgrade(&self) -> PayloadBufferObserverWeak {
        PayloadBufferObserverWeak {
            shared: Arc::downgrade(&self.shared),
        }
    }
}

/// Weak accounting observation. It cannot grant new reservations or retain B.
#[derive(Debug, Clone)]
pub struct PayloadBufferObserverWeak {
    shared: Weak<Shared>,
}

impl PayloadBufferObserverWeak {
    pub fn is_alive(&self) -> bool {
        self.shared.strong_count() > 0
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.shared, &other.shared)
    }
    pub fn snapshot(&self) -> Option<PayloadBufferSnapshot> {
        self.shared
            .upgrade()
            .map(|shared| locked(&shared).payload_snapshot())
    }
}

/// Cancel-safe byte admission. The waiter is registered under the same mutex
/// used for releases, so no release can be lost between checking and parking.
#[must_use = "poll or await the byte reservation before allocating storage"]
pub struct ReserveFuture<'a> {
    budget: &'a PayloadBufferBudget,
    bytes: u64,
    waiter: Option<u64>,
}

impl Future for ReserveFuture<'_> {
    type Output = Result<PayloadBufferReservation, PayloadBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut inner = locked(&this.budget.shared);
        if let Err(error) = inner.check_request(this.bytes) {
            if let Some(id) = this.waiter.take() {
                inner.waiters.remove(&id);
            }
            return Poll::Ready(Err(error));
        }
        if inner.can_admit(this.bytes) {
            inner.admit(this.bytes);
            if let Some(id) = this.waiter.take() {
                inner.waiters.remove(&id);
            }
            return Poll::Ready(Ok(PayloadBufferReservation {
                shared: this.budget.shared.clone(),
                bytes: this.bytes,
            }));
        }
        match this.waiter {
            Some(id) => {
                inner.waiters.insert(id, cx.waker().clone());
            }
            None => {
                let id = inner.next_waiter;
                inner.next_waiter = id
                    .checked_add(1)
                    .expect("payload-buffer waiter id exhausted");
                inner.waiters.insert(id, cx.waker().clone());
                this.waiter = Some(id);
            }
        }
        Poll::Pending
    }
}

impl Drop for ReserveFuture<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.waiter.take() {
            locked(&self.budget.shared).waiters.remove(&id);
        }
    }
}

/// One move-only charge. Moving it with a Vec transfers ownership; dropping
/// it refunds exactly once and wakes waiting calls.
#[must_use = "keep this reservation with its actual buffer storage"]
#[derive(Debug)]
pub struct PayloadBufferReservation {
    shared: Arc<Shared>,
    bytes: u64,
}

impl PayloadBufferReservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Grow the same storage before its next allocation or copy. A positive
    /// reservation never waits for more B while still owning its first bytes.
    pub fn try_grow(&mut self, additional: u64) -> Result<(), PayloadBufferError> {
        let mut inner = locked(&self.shared);
        let requested = u128::from(self.bytes) + u128::from(additional);
        if requested > u128::from(inner.limit_bytes) {
            inner.reject(requested);
            return Err(PayloadBufferError::TooLarge {
                requested,
                limit: inner.limit_bytes,
            });
        }
        if !inner.can_admit(additional) {
            inner.reject(requested);
            return Err(PayloadBufferError::InsufficientCapacity {
                requested,
                available: inner.limit_bytes - inner.used_bytes,
            });
        }
        inner.admit(additional);
        self.bytes += additional;
        Ok(())
    }
}

impl Drop for PayloadBufferReservation {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let waiters = {
            let mut inner = locked(&self.shared);
            inner.used_bytes = inner
                .used_bytes
                .checked_sub(self.bytes)
                .expect("payload-buffer reservation released more bytes than admitted");
            inner.waiters.values().cloned().collect::<Vec<_>>()
        };
        self.shared.changed.notify_all();
        for waiter in waiters {
            waiter.wake();
        }
    }
}

/// Move-only C-Two storage owner, usable as a `Bytes::from_owner` carrier.
/// Bytes slices and clones then keep this same reservation until their final
/// reference disappears.
#[derive(Debug)]
pub struct BudgetedBuffer {
    bytes: Vec<u8>,
    reservation: PayloadBufferReservation,
}

impl BudgetedBuffer {
    pub fn new(bytes: Vec<u8>, reservation: PayloadBufferReservation) -> Self {
        assert!(
            u64::try_from(bytes.capacity()).is_ok_and(|capacity| capacity <= reservation.bytes),
            "buffer allocation exceeds its payload reservation"
        );
        Self { bytes, reservation }
    }

    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
    pub fn into_parts(self) -> (Vec<u8>, PayloadBufferReservation) {
        (self.bytes, self.reservation)
    }
    pub fn into_vec(self) -> Vec<u8> {
        let (bytes, reservation) = self.into_parts();
        drop(reservation);
        bytes
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

/// Read-only call-domain counters. The retained-byte field names are the
/// established call-execution observation shape; they now describe all B
/// storage in the Runtime or relay, including HTTP buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSnapshot {
    pub max_operations: u64,
    pub max_retained_bytes: u64,
    pub used_operations: u64,
    pub used_retained_bytes: u64,
    pub peak_operations: u64,
    pub peak_retained_bytes: u64,
    pub rejected_reservations: u64,
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionRejectReason {
    OperationsExhausted,
    Closed,
}

impl fmt::Display for RetentionRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperationsExhausted => f.write_str("no operation slot available"),
            Self::Closed => f.write_str("call admission is closed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionError {
    pub reason: RetentionRejectReason,
    pub requested_bytes: u64,
    pub used_operations: u64,
    pub used_retained_bytes: u64,
    pub max_operations: u64,
    pub max_retained_bytes: u64,
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "call admission rejected ({}): {} of {} operation slots in use",
            self.reason, self.used_operations, self.max_operations
        )
    }
}
impl Error for RetentionError {}

/// Finite call operation slots and the same B domain used by payload owners.
#[derive(Debug, Clone)]
pub struct RetentionBudget {
    shared: Arc<Shared>,
}

impl RetentionBudget {
    pub fn new(max_operations: u64, max_payload_buffer_bytes: u64) -> Self {
        Self {
            shared: new_shared(max_operations, max_payload_buffer_bytes),
        }
    }

    pub fn from_call_limits(limits: &c2_config::CallExecutionLimits) -> Self {
        Self::new(
            limits.max_outstanding_calls,
            limits.payload_buffer_budget_bytes,
        )
    }

    pub fn payload_budget(&self) -> PayloadBufferBudget {
        PayloadBufferBudget {
            shared: self.shared.clone(),
        }
    }

    pub fn reserve(&self) -> Result<RetentionPermit, RetentionError> {
        let mut inner = locked(&self.shared);
        let reason = if inner.operations_closed {
            Some(RetentionRejectReason::Closed)
        } else if inner.used_operations >= inner.max_operations {
            Some(RetentionRejectReason::OperationsExhausted)
        } else {
            None
        };
        if let Some(reason) = reason {
            inner.rejected_operations = inner.rejected_operations.saturating_add(1);
            return Err(RetentionError {
                reason,
                requested_bytes: 0,
                used_operations: inner.used_operations,
                used_retained_bytes: inner.used_bytes,
                max_operations: inner.max_operations,
                max_retained_bytes: inner.limit_bytes,
            });
        }
        inner.used_operations += 1;
        inner.peak_operations = inner.peak_operations.max(inner.used_operations);
        Ok(RetentionPermit {
            shared: self.shared.clone(),
        })
    }

    /// Stop new operation slots; keep B open for already dispatched work.
    pub fn close(&self) {
        locked(&self.shared).operations_closed = true;
    }

    pub fn snapshot(&self) -> RetentionSnapshot {
        let inner = locked(&self.shared);
        RetentionSnapshot {
            max_operations: inner.max_operations,
            max_retained_bytes: inner.limit_bytes,
            used_operations: inner.used_operations,
            used_retained_bytes: inner.used_bytes,
            peak_operations: inner.peak_operations,
            peak_retained_bytes: inner.peak_bytes,
            rejected_reservations: inner
                .rejected_operations
                .saturating_add(inner.rejected_allocations),
            closed: inner.operations_closed,
        }
    }
}

#[must_use = "keep the slot until the call actually finishes"]
#[derive(Debug)]
pub struct RetentionPermit {
    shared: Arc<Shared>,
}

impl Drop for RetentionPermit {
    fn drop(&mut self) {
        let mut inner = locked(&self.shared);
        inner.used_operations = inner
            .used_operations
            .checked_sub(1)
            .expect("call operation slot released more than once");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, mpsc};
    use std::task::{Wake, Waker};
    use std::thread;
    use std::time::Duration;

    struct CountWake(AtomicUsize);
    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn buffer_wait_and_last_owner_refund() {
        let budget = PayloadBufferBudget::new(8);
        let first = Arc::new(BudgetedBuffer::new(
            vec![1; 8],
            budget.reserve_blocking(8, None).unwrap(),
        ));
        let alias = Arc::clone(&first);
        let mut waiting = Box::pin(budget.reserve(1));
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        assert!(matches!(
            waiting.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ));
        assert_eq!(budget.snapshot().rejected_allocations, 0);
        drop(first);
        assert_eq!(budget.snapshot().used_bytes, 8);
        drop(alias);
        assert!(wake.0.load(Ordering::SeqCst) > 0);
        match waiting.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(Ok(reservation)) => assert_eq!(reservation.bytes(), 1),
            other => panic!("release must wake and admit the waiter: {other:?}"),
        }
        drop(waiting);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[test]
    fn deadline_wait_is_not_a_rejection() {
        let budget = PayloadBufferBudget::new(4);
        let first = budget.reserve_blocking(4, None).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            let budget = budget.clone();
            let child_barrier = barrier.clone();
            let join = scope.spawn(move || {
                child_barrier.wait();
                budget.reserve_blocking(1, Some(Instant::now() + Duration::from_millis(20)))
            });
            barrier.wait();
            assert_eq!(
                join.join().unwrap().unwrap_err(),
                PayloadBufferError::DeadlineExceeded
            );
        });
        assert_eq!(budget.snapshot().rejected_allocations, 0);
        drop(first);
        assert_eq!(budget.reserve_blocking(4, None).unwrap().bytes(), 4);
    }

    #[test]
    fn oversize_zero_and_u64_boundary() {
        let budget = PayloadBufferBudget::new(0);
        assert_eq!(budget.reserve_blocking(0, None).unwrap().bytes(), 0);
        assert_eq!(
            budget.reserve_blocking(1, None).unwrap_err(),
            PayloadBufferError::TooLarge {
                requested: 1,
                limit: 0
            }
        );
        assert_eq!(budget.snapshot().rejected_allocations, 1);
        assert_eq!(budget.snapshot().rejected_bytes, 1);
        let budget = PayloadBufferBudget::new(u64::MAX);
        let all = budget.reserve_blocking(u64::MAX, None).unwrap();
        assert_eq!(budget.snapshot().used_bytes, u64::MAX);
        drop(all);
        assert_eq!(budget.snapshot().used_bytes, 0);
        let mut all = budget.reserve_blocking(u64::MAX, None).unwrap();
        assert_eq!(
            all.try_grow(1).unwrap_err(),
            PayloadBufferError::TooLarge {
                requested: u128::from(u64::MAX) + 1,
                limit: u64::MAX,
            }
        );
        assert_eq!(budget.snapshot().used_bytes, u64::MAX);
    }

    #[test]
    fn growth_counts_transient_copies_and_refunds() {
        let budget = PayloadBufferBudget::new(8);
        let mut first = budget.reserve_blocking(2, None).unwrap();
        first.try_grow(2).unwrap();
        let second = budget.reserve_blocking(4, None).unwrap();
        assert_eq!(budget.snapshot().peak_bytes, 8);
        drop(first);
        assert_eq!(budget.snapshot().used_bytes, 4);
        drop(second);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[test]
    fn operation_slots_are_separate_and_closing_keeps_bytes_usable() {
        let budget = RetentionBudget::new(1, 8);
        let first = budget.reserve().unwrap();
        assert_eq!(
            budget.reserve().unwrap_err().reason,
            RetentionRejectReason::OperationsExhausted
        );
        let bytes = budget.payload_budget().reserve_blocking(8, None).unwrap();
        budget.close();
        assert_eq!(
            budget.reserve().unwrap_err().reason,
            RetentionRejectReason::Closed
        );
        drop(bytes);
        let later = budget.payload_budget().reserve_blocking(8, None).unwrap();
        assert_eq!(budget.snapshot().used_operations, 1);
        assert_eq!(budget.snapshot().used_retained_bytes, 8);
        drop((first, later));
        assert_eq!(budget.snapshot().used_operations, 0);
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
    }

    #[test]
    fn operation_slot_moves_across_threads() {
        let budget = RetentionBudget::new(1, 4);
        let permit = budget.reserve().unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            drop(permit);
            send.send(()).unwrap();
        })
        .join()
        .unwrap();
        receive.recv().unwrap();
        assert_eq!(budget.snapshot().used_operations, 0);
    }

    #[test]
    fn zero_slots_reject_even_empty_calls_and_clones_share_accounting() {
        let closed = RetentionBudget::new(0, 8);
        assert_eq!(
            closed.reserve().unwrap_err().reason,
            RetentionRejectReason::OperationsExhausted
        );
        let budget = RetentionBudget::new(1, 8);
        let clone = budget.clone();
        let slot = clone.reserve().unwrap();
        assert_eq!(budget.snapshot().used_operations, 1);
        let charge = budget.payload_budget().reserve_blocking(8, None).unwrap();
        assert_eq!(clone.snapshot().used_retained_bytes, 8);
        drop(budget);
        drop(slot);
        assert_eq!(clone.snapshot().used_operations, 0);
        drop(charge);
        assert_eq!(clone.snapshot().used_retained_bytes, 0);
    }

    #[test]
    fn cancelled_waiter_leaves_capacity_available() {
        let budget = PayloadBufferBudget::new(2);
        let held = budget.reserve_blocking(2, None).unwrap();
        let mut pending = Box::pin(budget.reserve(1));
        let waker = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
        assert!(matches!(
            pending.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ));
        drop(pending);
        drop(held);
        assert_eq!(budget.reserve_blocking(2, None).unwrap().bytes(), 2);
        assert_eq!(budget.snapshot().rejected_allocations, 0);
    }

    #[test]
    fn growth_above_total_limit_rejects_without_corrupting_original_charge() {
        let budget = PayloadBufferBudget::new(4);
        let mut held = budget.reserve_blocking(3, None).unwrap();
        assert_eq!(
            held.try_grow(2).unwrap_err(),
            PayloadBufferError::TooLarge {
                requested: 5,
                limit: 4
            }
        );
        assert_eq!(held.bytes(), 3);
        assert_eq!(budget.snapshot().used_bytes, 3);
        assert_eq!(budget.snapshot().rejected_allocations, 1);
        drop(held);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[test]
    fn two_partial_unknown_inputs_reject_growth_without_waiting_for_each_other() {
        let budget = PayloadBufferBudget::new(8);
        let mut first = budget.reserve_blocking(4, None).unwrap();
        let mut second = budget.reserve_blocking(4, None).unwrap();
        assert_eq!(
            first.try_grow(1),
            Err(PayloadBufferError::InsufficientCapacity {
                requested: 5,
                available: 0,
            })
        );
        assert_eq!(
            second.try_grow(1),
            Err(PayloadBufferError::InsufficientCapacity {
                requested: 5,
                available: 0,
            })
        );
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.used_bytes, 8);
        assert_eq!(snapshot.rejected_allocations, 2);
        assert_eq!(snapshot.rejected_bytes, 10);
        drop(first);
        second.try_grow(1).unwrap();
        assert_eq!(budget.snapshot().used_bytes, 5);
        drop(second);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[test]
    fn immediate_first_reservation_rejects_temporary_shortage_and_zero_still_succeeds() {
        let budget = PayloadBufferBudget::new(4);
        let held = budget.reserve_blocking(4, None).unwrap();
        assert_eq!(budget.try_reserve(0).unwrap().bytes(), 0);
        assert_eq!(
            budget.try_reserve(1).unwrap_err(),
            PayloadBufferError::InsufficientCapacity {
                requested: 1,
                available: 0,
            }
        );
        assert_eq!(budget.snapshot().rejected_bytes, 1);
        drop(held);
        assert_eq!(budget.try_reserve(4).unwrap().bytes(), 4);
    }

    #[test]
    fn retired_budget_observer_is_weak_and_tracks_real_owner() {
        let budget = PayloadBufferBudget::new(4);
        let observer = budget.downgrade();
        let reservation = budget.reserve_blocking(4, None).unwrap();
        drop(budget);
        assert_eq!(observer.snapshot().unwrap().used_bytes, 4);
        drop(reservation);
        assert!(observer.snapshot().is_none());
    }

    #[test]
    fn call_limits_select_slots_and_shared_payload_budget_without_freezing_snapshot() {
        let limits = c2_config::CallExecutionLimits {
            max_outstanding_calls: 3,
            payload_buffer_budget_bytes: 7,
        };
        let budget = RetentionBudget::from_call_limits(&limits);
        let before = budget.snapshot();
        assert_eq!((before.max_operations, before.max_retained_bytes), (3, 7));
        assert_eq!(budget.snapshot(), before);
        let reservation = budget.payload_budget().reserve_blocking(7, None).unwrap();
        assert_eq!(budget.snapshot().used_retained_bytes, 7);
        drop(reservation);
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
    }

    #[test]
    fn operation_permit_survives_creating_handle_and_unwind_refunds_storage() {
        let budget = RetentionBudget::new(1, 4);
        let observer = budget.payload_budget().downgrade();
        let slot = budget.reserve().unwrap();
        drop(budget);
        assert!(observer.is_alive());
        drop(slot);
        assert!(!observer.is_alive());

        let budget = PayloadBufferBudget::new(4);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = BudgetedBuffer::new(vec![1; 4], budget.reserve_blocking(4, None).unwrap());
            panic!("drop storage and B on unwind");
        }));
        assert!(panic.is_err());
        assert_eq!(budget.snapshot().used_bytes, 0);
    }

    #[test]
    fn concurrent_slot_admission_has_one_winner() {
        let budget = RetentionBudget::new(1, 4);
        let start = Arc::new(Barrier::new(3));
        let finish = Arc::new(Barrier::new(3));
        thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let budget = budget.clone();
                let start = start.clone();
                let finish = finish.clone();
                handles.push(scope.spawn(move || {
                    start.wait();
                    let admission = budget.reserve();
                    finish.wait();
                    admission.is_ok()
                }));
            }
            start.wait();
            finish.wait();
            assert_eq!(
                handles
                    .into_iter()
                    .map(|join| join.join().unwrap())
                    .filter(|ok| *ok)
                    .count(),
                1
            );
        });
        assert_eq!(budget.snapshot().used_operations, 0);
    }
}
