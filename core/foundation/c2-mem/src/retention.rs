//! Generic pure-metadata retention budget for bounded continuations.
//!
//! [`RetentionBudget`] is a two-dimensional admission policy — every
//! [`RetentionPermit`] takes exactly one operation slot plus a finite
//! retained-input byte charge — shared by clones of the same budget. It is
//! the transport-continuation owned-input retention policy from the 0.7.1
//! call-deadline design: it bounds how many in-flight continuations may
//! keep their own request copies before the real owner releases them. It is
//! not a process RSS bound and not a complete HTTP buffering guarantee.
//!
//! This module is pure accounting. It never allocates, reads, or inspects
//! payload bytes, never creates SHM or file backing, never calls
//! `MemPool::free_at()` / `release_handle()` or any other release
//! authority, and shares nothing with the shm/file/reassembly cells in
//! [`crate::budget`]. Owners integrate it at their own takeover/copy seam.
//!
//! Updates happen in one short critical section under a single mutex, so
//! [`RetentionBudget::reserve`] admits the operation slot and the byte
//! charge atomically or not at all, and [`RetentionBudget::snapshot`]
//! observes a consistent read-only view without changing any counter. The
//! critical section runs no allocation, I/O, payload work, or callbacks,
//! and [`Drop for RetentionPermit`] only updates the two counters — it
//! never spawns threads or tasks and never waits on I/O. There is no
//! unlimited mode: both limits are finite and fixed at construction, and a
//! zero limit is rejective, not unbounded.

use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Consistent read-only view of one retention domain, taken under one lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSnapshot {
    /// Finite operation-slot limit fixed at construction; zero rejects
    /// every call.
    pub max_operations: u64,
    /// Finite retained-byte limit fixed at construction; zero admits only
    /// zero-byte operations.
    pub max_retained_bytes: u64,
    /// Live operation slots.
    pub used_operations: u64,
    /// Currently retained input bytes.
    pub used_retained_bytes: u64,
    /// High-water mark of `used_operations`; persists across releases.
    pub peak_operations: u64,
    /// High-water mark of `used_retained_bytes`; persists across releases.
    pub peak_retained_bytes: u64,
    /// Number of rejected `reserve` attempts across all reasons; may
    /// saturate.
    pub rejected_reservations: u64,
    /// Whether [`RetentionBudget::close`] stopped new admissions. Live
    /// permits stay billed until they are released.
    pub closed: bool,
}

/// Why a [`RetentionBudget::reserve`] attempt was rejected.
///
/// The budget takes no charge for a rejected attempt: no operation slot and
/// no bytes move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionRejectReason {
    /// Every operation slot is in use, or none exist
    /// (`max_operations == 0`): every call is rejected.
    OperationsExhausted,
    /// The retained-byte limit rejects the charge, or the charge would
    /// overflow the accounting range.
    RetainedBytesExhausted,
    /// The budget is closed; only already-admitted permits remain, until
    /// their owners release them.
    Closed,
}

impl fmt::Display for RetentionRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            RetentionRejectReason::OperationsExhausted => "no operation slot available",
            RetentionRejectReason::RetainedBytesExhausted => "retained-byte limit exceeded",
            RetentionRejectReason::Closed => "retention budget is closed",
        };
        f.write_str(message)
    }
}

/// Rejection detail for a failed [`RetentionBudget::reserve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionError {
    /// Why the attempt was rejected.
    pub reason: RetentionRejectReason,
    /// Bytes the caller asked to retain.
    pub requested_bytes: u64,
    /// Operation slots live when the attempt was rejected.
    pub used_operations: u64,
    /// Retained bytes live when the attempt was rejected.
    pub used_retained_bytes: u64,
    /// Finite operation-slot limit of the domain.
    pub max_operations: u64,
    /// Finite retained-byte limit of the domain.
    pub max_retained_bytes: u64,
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "retention budget rejected a {}-byte reservation ({}): {} of {} operation slots and {} of {} retained bytes in use",
            self.requested_bytes,
            self.reason,
            self.used_operations,
            self.max_operations,
            self.used_retained_bytes,
            self.max_retained_bytes,
        )
    }
}

impl Error for RetentionError {}

#[derive(Debug)]
struct RetentionInner {
    max_operations: u64,
    max_retained_bytes: u64,
    used_operations: u64,
    used_retained_bytes: u64,
    peak_operations: u64,
    peak_retained_bytes: u64,
    rejected_reservations: u64,
    closed: bool,
}

impl RetentionInner {
    fn snapshot(&self) -> RetentionSnapshot {
        RetentionSnapshot {
            max_operations: self.max_operations,
            max_retained_bytes: self.max_retained_bytes,
            used_operations: self.used_operations,
            used_retained_bytes: self.used_retained_bytes,
            peak_operations: self.peak_operations,
            peak_retained_bytes: self.peak_retained_bytes,
            rejected_reservations: self.rejected_reservations,
            closed: self.closed,
        }
    }

    /// Admits one operation slot plus `bytes`, or records the rejection.
    /// Invariants: `used_operations <= max_operations` and
    /// `used_retained_bytes <= max_retained_bytes` always hold.
    fn try_reserve(&mut self, bytes: u64) -> Result<(), RetentionError> {
        let mut rejected = |reason| {
            self.rejected_reservations = self.rejected_reservations.saturating_add(1);
            RetentionError {
                reason,
                requested_bytes: bytes,
                used_operations: self.used_operations,
                used_retained_bytes: self.used_retained_bytes,
                max_operations: self.max_operations,
                max_retained_bytes: self.max_retained_bytes,
            }
        };

        if self.closed {
            return Err(rejected(RetentionRejectReason::Closed));
        }
        // This also covers `max_operations == 0`: zero slots reject every
        // call, including zero-byte calls.
        if self.used_operations >= self.max_operations {
            return Err(rejected(RetentionRejectReason::OperationsExhausted));
        }
        match self.used_retained_bytes.checked_add(bytes) {
            Some(new_bytes) if new_bytes <= self.max_retained_bytes => {
                self.used_retained_bytes = new_bytes;
                if new_bytes > self.peak_retained_bytes {
                    self.peak_retained_bytes = new_bytes;
                }
                // Bounded by the check above, so this cannot overflow.
                self.used_operations += 1;
                if self.used_operations > self.peak_operations {
                    self.peak_operations = self.used_operations;
                }
                Ok(())
            }
            // A zero-byte limit still admits zero-byte operations and
            // rejects every positive charge; overflow rejects without
            // wrapping.
            _ => Err(rejected(RetentionRejectReason::RetainedBytesExhausted)),
        }
    }

    /// Returns exactly one slot and `bytes` of a previously admitted
    /// reservation. The admission invariant makes underflow impossible; a
    /// violated invariant must fail explicitly rather than silently
    /// undercount live continuations.
    fn release(&mut self, bytes: u64) {
        self.used_operations = self.used_operations.checked_sub(1).unwrap_or_else(|| {
            panic!(
                "retention budget cannot release an operation slot from {}",
                self.used_operations
            )
        });
        self.used_retained_bytes =
            self.used_retained_bytes
                .checked_sub(bytes)
                .unwrap_or_else(|| {
                    panic!(
                        "retention budget cannot release {bytes} bytes from {}",
                        self.used_retained_bytes
                    )
                });
    }
}

#[derive(Debug)]
struct RetentionState {
    inner: Mutex<RetentionInner>,
}

fn lock_inner(inner: &Mutex<RetentionInner>) -> std::sync::MutexGuard<'_, RetentionInner> {
    inner
        .lock()
        .expect("retention budget accounting mutex poisoned")
}

/// Two finite retention limits shared by clones and `Arc` holders.
///
/// Clones share the same underlying domain: a permit taken through one
/// handle is visible in every handle's [`RetentionBudget::snapshot`], and
/// [`RetentionBudget::close`] through any handle stops new admissions for
/// the whole domain. Dropping the creating handle does not invalidate live
/// [`RetentionPermit`] guards or the shared accounting state.
///
/// The byte dimension is a retention policy for continuation-owned input,
/// not an RSS bound or a full HTTP buffering guarantee, and it never
/// replaces the real memory-release authorities.
#[derive(Debug, Clone)]
pub struct RetentionBudget {
    state: Arc<RetentionState>,
}

impl RetentionBudget {
    /// Creates a budget with two explicit finite limits. A zero
    /// `max_operations` rejects every call; a zero `max_retained_bytes`
    /// admits only zero-byte operations. Neither zero means unlimited.
    pub fn new(max_operations: u64, max_retained_bytes: u64) -> Self {
        Self {
            state: Arc::new(RetentionState {
                inner: Mutex::new(RetentionInner {
                    max_operations,
                    max_retained_bytes,
                    used_operations: 0,
                    used_retained_bytes: 0,
                    peak_operations: 0,
                    peak_retained_bytes: 0,
                    rejected_reservations: 0,
                    closed: false,
                }),
            }),
        }
    }

    /// Creates a budget from the canonical [`c2_config::CallExecutionLimits`].
    ///
    /// The plain-data limits live in `c2-config`; this one-way conversion in
    /// `c2-mem` (which already depends on `c2-config`) follows the same
    /// pattern as [`crate::MemoryBudget::from_limits`].
    pub fn from_call_limits(limits: &c2_config::CallExecutionLimits) -> Self {
        Self::new(
            limits.max_outstanding_calls,
            limits.retained_input_budget_bytes,
        )
    }

    /// Admits one operation slot plus a `bytes` charge atomically and
    /// returns its move-only permit, or rejects the attempt before anything
    /// is charged.
    ///
    /// When any check fails — closed, out of slots, byte limit exceeded, or
    /// overflow — neither the slot nor any bytes are taken. A zero-byte
    /// reservation is admitted harmlessly (subject to the slot limit) and
    /// returns a permit that charges no bytes.
    pub fn reserve(&self, bytes: u64) -> Result<RetentionPermit, RetentionError> {
        let attempt = {
            let mut inner = lock_inner(&self.state.inner);
            inner.try_reserve(bytes)
        };
        attempt.map(|()| RetentionPermit {
            state: Arc::clone(&self.state),
            bytes,
        })
    }

    /// Stops new admissions for the whole shared domain. Existing permits
    /// are not forcibly refunded: each stays billed — visible in
    /// `used_operations` / `used_retained_bytes` — until its owner releases
    /// it, and its eventual release still returns exactly its two counts.
    /// Idempotent.
    pub fn close(&self) {
        lock_inner(&self.state.inner).closed = true;
    }

    /// Consistent read-only snapshot of the shared domain. Observing never
    /// changes any counter.
    pub fn snapshot(&self) -> RetentionSnapshot {
        lock_inner(&self.state.inner).snapshot()
    }
}

/// Move-only ownership of one admitted retention charge: one operation slot
/// plus `bytes`.
///
/// Not `Clone` or `Copy`: moving the permit transfers the charge, and both
/// counts are returned exactly once when the permit is dropped (including
/// during unwind). It holds only the shared accounting state `Arc` plus its
/// byte count — never a payload, pool, mapping, connection, or callback —
/// so it can be moved across threads to whoever owns the retained input,
/// and outliving its creating [`RetentionBudget`] is safe. Dropping it
/// spawns no thread or task and waits on no I/O.
///
/// Closing the budget does not force a refund: a permit dropped after
/// [`RetentionBudget::close`] still returns its two counts, while new
/// admissions stay rejected.
#[must_use = "retain the permit until the retained input is released by its real owner"]
#[derive(Debug)]
pub struct RetentionPermit {
    state: Arc<RetentionState>,
    bytes: u64,
}

impl RetentionPermit {
    /// The retained byte count this permit charges.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for RetentionPermit {
    fn drop(&mut self) {
        let mut inner = lock_inner(&self.state.inner);
        inner.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Barrier;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn reserve_charges_one_slot_and_bytes_together() {
        let budget = RetentionBudget::new(5, 100);
        let permit = budget.reserve(40).unwrap();

        assert_eq!(permit.bytes(), 40);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 40);
        assert_eq!(snap.peak_operations, 1);
        assert_eq!(snap.peak_retained_bytes, 40);
        assert!(!snap.closed);

        let second = budget.reserve(60).unwrap();
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 2);
        assert_eq!(snap.used_retained_bytes, 100);
        assert_eq!(snap.peak_retained_bytes, 100);

        drop((permit, second));
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
        // Peaks persist across releases.
        assert_eq!(snap.peak_operations, 2);
        assert_eq!(snap.peak_retained_bytes, 100);
    }

    #[test]
    fn operation_limit_rejects_independently_of_bytes() {
        let budget = RetentionBudget::new(2, u64::MAX);
        let _first = budget.reserve(0).unwrap();
        let _second = budget.reserve(0).unwrap();

        let err = budget.reserve(0).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::OperationsExhausted);
        assert_eq!(err.used_operations, 2);
        assert_eq!(err.max_operations, 2);
        assert!(err.to_string().contains("operation slot"));
    }

    #[test]
    fn byte_limit_rejects_independently_of_slots() {
        let budget = RetentionBudget::new(u64::MAX, 100);
        let _permit = budget.reserve(80).unwrap();

        let err = budget.reserve(21).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::RetainedBytesExhausted);
        assert_eq!(err.requested_bytes, 21);
        assert_eq!(err.used_retained_bytes, 80);
        assert_eq!(err.max_retained_bytes, 100);
    }

    #[test]
    fn simultaneous_over_limit_rejects_completely() {
        let budget = RetentionBudget::new(1, 100);
        let _permit = budget.reserve(100).unwrap();

        // Both dimensions are exhausted; the first check names the reason
        // and nothing is charged.
        let err = budget.reserve(1).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::OperationsExhausted);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 100);

        // Reaching the byte limit first reports bytes even though slots
        // remain.
        let budget = RetentionBudget::new(5, 50);
        let _permit = budget.reserve(50).unwrap();
        let err = budget.reserve(1).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::RetainedBytesExhausted);
    }

    #[test]
    fn failed_reservations_leave_no_half_charge() {
        let budget = RetentionBudget::new(2, 100);
        let _permit = budget.reserve(60).unwrap();

        assert!(budget.reserve(41).is_err());
        assert!(budget.reserve(50).is_err());

        let snap = budget.snapshot();
        // Neither the slot nor any bytes moved; only the rejection counter
        // advanced.
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 60);
        assert_eq!(snap.rejected_reservations, 2);
    }

    #[test]
    fn zero_operation_slots_reject_every_call() {
        let budget = RetentionBudget::new(0, u64::MAX);
        let err = budget.reserve(0).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::OperationsExhausted);
        let err = budget.reserve(1).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::OperationsExhausted);

        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
        assert_eq!(snap.rejected_reservations, 2);
    }

    #[test]
    fn zero_byte_limit_admits_zero_byte_operations_only() {
        let budget = RetentionBudget::new(8, 0);

        let permit = budget.reserve(0).expect("zero-byte operation is admitted");
        assert_eq!(permit.bytes(), 0);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 0);

        let err = budget.reserve(1).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::RetainedBytesExhausted);
        assert_eq!(err.requested_bytes, 1);
        assert_eq!(err.used_retained_bytes, 0);

        drop(permit);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.rejected_reservations, 1);
    }

    #[test]
    fn overflow_at_u64_boundaries_is_rejected_without_wrapping() {
        let budget = RetentionBudget::new(u64::MAX, u64::MAX);
        let near = budget.reserve(u64::MAX - 5).unwrap();

        let err = budget.reserve(6).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::RetainedBytesExhausted);
        assert_eq!(err.used_retained_bytes, u64::MAX - 5);
        assert_eq!(err.max_retained_bytes, u64::MAX);

        // Slot accounting also holds at the boundary.
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, u64::MAX - 5);
        assert_eq!(snap.peak_retained_bytes, u64::MAX - 5);
        assert_eq!(snap.rejected_reservations, 1);

        drop(near);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
    }

    #[test]
    fn closed_budget_rejects_new_calls_and_keeps_live_permits_billed() {
        let budget = RetentionBudget::new(4, 100);
        let permit = budget.reserve(60).unwrap();

        budget.close();
        budget.close(); // Idempotent.

        let err = budget.reserve(0).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::Closed);
        let err = budget.reserve(10).unwrap_err();
        assert_eq!(err.reason, RetentionRejectReason::Closed);

        // The pre-close permit stays billed until its owner releases it.
        let snap = budget.snapshot();
        assert!(snap.closed);
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 60);

        drop(permit);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
        assert!(
            snap.closed,
            "release after close must not reopen admissions"
        );
        assert_eq!(
            budget.reserve(0).unwrap_err().reason,
            RetentionRejectReason::Closed
        );
    }

    #[test]
    fn clones_share_one_domain_and_close_is_domain_wide() {
        let budget = RetentionBudget::new(2, 200);
        let clone = budget.clone();
        let permit = budget.reserve(50).unwrap();

        // The clone observes and reserves into the same domain.
        assert_eq!(clone.snapshot().used_retained_bytes, 50);
        let clone_permit = clone.reserve(150).unwrap();
        assert_eq!(budget.snapshot().used_retained_bytes, 200);

        // Closing through any handle stops the whole domain.
        clone.close();
        assert!(budget.snapshot().closed);
        assert_eq!(
            budget.reserve(1).unwrap_err().reason,
            RetentionRejectReason::Closed
        );

        drop((permit, clone_permit));
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
    }

    #[test]
    fn permits_move_across_threads_and_survive_creator_drop() {
        let budget = RetentionBudget::new(2, 100);
        let permit = budget.reserve(70).unwrap();
        let snapshot_handle = budget.clone();
        drop(budget);

        // Moving the permit transfers its charge to another thread.
        let moved = thread::spawn(move || {
            assert_eq!(permit.bytes(), 70);
            permit // and back again
        })
        .join()
        .expect("permit move thread must not panic");
        assert_eq!(snapshot_handle.snapshot().used_retained_bytes, 70);

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RetentionPermit>();

        // Released exactly once, wherever the owner drops it.
        drop(moved);
        let snap = snapshot_handle.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
        assert_eq!(snap.peak_retained_bytes, 70);
    }

    #[test]
    fn permit_drop_after_all_budget_handles_drop_returns_the_charge() {
        let (permit, weak) = {
            let budget = RetentionBudget::new(3, 64);
            let weak = Arc::downgrade(&budget.state);
            (budget.reserve(32).unwrap(), weak)
        };
        let retained = Arc::clone(&permit.state);
        drop(permit);
        assert_eq!(lock_inner(&retained.inner).used_retained_bytes, 0);
        drop(retained);
        assert!(
            weak.upgrade().is_none(),
            "last permit must release the accounting state"
        );
    }

    #[test]
    fn snapshot_observation_does_not_change_counters() {
        let budget = RetentionBudget::new(5, 100);
        let _permit = budget.reserve(40).unwrap();
        let _rejected = budget.reserve(100).unwrap_err();

        let before = budget.snapshot();
        for _ in 0..3 {
            let again = budget.snapshot();
            assert_eq!(again, before);
        }

        // Rejections stay recorded and unchanged by observation.
        assert_eq!(before.rejected_reservations, 1);
        assert_eq!(before.used_operations, 1);
        assert_eq!(before.used_retained_bytes, 40);
    }

    #[test]
    fn concurrent_race_admits_exactly_the_limited_operations() {
        const THREADS: usize = 8;
        // Both dimensions bind at the same point: floor(900 / 300) == 3
        // slots and 3 * 300 == 900 bytes.
        const EXPECTED_OK: usize = 3;

        let budget = RetentionBudget::new(3, 900);
        let barrier = Arc::new(Barrier::new(THREADS));
        let (tx, rx) = mpsc::channel();

        for _ in 0..THREADS {
            let budget = budget.clone();
            let barrier = barrier.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                barrier.wait();
                let outcome = budget.reserve(300);
                tx.send(outcome).expect("receiver alive");
            });
        }
        drop(tx);
        let outcomes: Vec<Result<RetentionPermit, RetentionError>> = rx.iter().collect();
        assert_eq!(outcomes.len(), THREADS);

        let successes: Vec<RetentionPermit> = outcomes.into_iter().filter_map(Result::ok).collect();
        assert_eq!(successes.len(), EXPECTED_OK);

        // No permit was released during the phase, so both counters sit at
        // exactly the successful totals.
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, EXPECTED_OK as u64);
        assert_eq!(snap.used_retained_bytes, EXPECTED_OK as u64 * 300);
        assert_eq!(snap.peak_operations, EXPECTED_OK as u64);
        assert_eq!(snap.peak_retained_bytes, EXPECTED_OK as u64 * 300);
        let rejected = THREADS as u64 - EXPECTED_OK as u64;
        assert_eq!(snap.rejected_reservations, rejected);

        drop(successes);
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 0);
        assert_eq!(snap.used_retained_bytes, 0);
    }

    #[test]
    fn from_call_limits_uses_the_canonical_dimensions() {
        let limits = c2_config::CallExecutionLimits::default();
        let budget = RetentionBudget::from_call_limits(&limits);
        let snap = budget.snapshot();
        assert_eq!(snap.max_operations, 1024);
        assert_eq!(snap.max_retained_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(snap.used_operations, 0);
        assert!(!snap.closed);

        // Zeroed canonical limits reject rather than mean unlimited.
        let zeroed = RetentionBudget::from_call_limits(&c2_config::CallExecutionLimits::zeroed());
        assert_eq!(
            zeroed.reserve(1).unwrap_err().reason,
            RetentionRejectReason::OperationsExhausted
        );
    }

    #[test]
    fn panic_unwind_releases_both_counts_once() {
        let budget = RetentionBudget::new(10, 100);
        let retained = budget.reserve(25).unwrap();

        let result = catch_unwind(AssertUnwindSafe(|| {
            let permit = budget.reserve(10).unwrap();
            let snap = budget.snapshot();
            assert_eq!(snap.used_operations, 2);
            assert_eq!(snap.used_retained_bytes, 35);
            let _ = &permit;
            panic!("unwind while a permit is live");
        }));
        assert!(result.is_err());

        // The unwound permit returned both counts; the unaffected one kept
        // its charge.
        let snap = budget.snapshot();
        assert_eq!(snap.used_operations, 1);
        assert_eq!(snap.used_retained_bytes, 25);
        assert_eq!(snap.peak_retained_bytes, 35);
        drop(retained);
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
    }
}
