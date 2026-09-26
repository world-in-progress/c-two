//! Finite memory budget reservation cells.
//!
//! [`MemoryBudget`] tracks three independent finite byte cells —
//! [`BudgetKind::Shm`], [`BudgetKind::File`], and [`BudgetKind::Reassembly`]
//! — with limits chosen explicitly by the creator. [`MemoryBudget::reserve`]
//! either admits a byte charge and returns a move-only [`BudgetReservation`]
//! guard, or rejects the attempt before any allocation happens and records
//! rejection statistics.
//!
//! This module is pure accounting: it knows nothing about pools, mappings,
//! CRM, wire, or transport. Owners integrate it at their own allocation
//! seams; `MemPool` free/release paths and backing owners remain the only
//! release authorities for actual memory.
//!
//! Live usage uses checked arithmetic and can never wrap or silently
//! underflow. Rejection metrics may saturate. Updates happen in short
//! critical sections under one mutex, so [`MemoryBudget::snapshot`] observes
//! all three cells consistently; no allocation, I/O, payload work, or
//! callback runs under that lock. There is no default, unlimited, or
//! usage-reset mode: limits are finite and fixed at construction.

use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Which finite budget cell a reservation charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetKind {
    /// Owner-created shared-memory backing (buddy and dedicated segments),
    /// including allocator header/alignment overhead.
    Shm,
    /// Owner-created file backing (file spill).
    File,
    /// Allocated capacity of live chunk reassembly storage.
    Reassembly,
}

impl BudgetKind {
    /// Stable short name used in error and statistics messages.
    pub fn label(self) -> &'static str {
        match self {
            BudgetKind::Shm => "shm",
            BudgetKind::File => "file",
            BudgetKind::Reassembly => "reassembly",
        }
    }
}

impl fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Point-in-time counters for one budget cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetCellSnapshot {
    /// Finite limit fixed at construction; zero rejects positive charges.
    pub limit_bytes: u64,
    /// Currently reserved bytes.
    pub used_bytes: u64,
    /// High-water mark of `used_bytes`; persists across releases.
    pub peak_bytes: u64,
    /// Number of rejected `reserve` attempts; may saturate.
    pub rejected_allocations: u64,
    /// Bytes rejected across those attempts; may saturate.
    pub rejected_bytes: u64,
}

/// Consistent view of all three cells, taken under one lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub shm: BudgetCellSnapshot,
    pub file: BudgetCellSnapshot,
    pub reassembly: BudgetCellSnapshot,
}

impl BudgetSnapshot {
    /// Snapshot of one cell by kind.
    pub fn cell(&self, kind: BudgetKind) -> BudgetCellSnapshot {
        match kind {
            BudgetKind::Shm => self.shm,
            BudgetKind::File => self.file,
            BudgetKind::Reassembly => self.reassembly,
        }
    }
}

/// Rejection detail for a failed [`MemoryBudget::reserve`].
///
/// `requested + used` either exceeded `limit` or overflowed `u64`; both are
/// reported through this error and no charge is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetError {
    /// The cell that rejected the reservation.
    pub cell: BudgetKind,
    /// Bytes the caller asked to reserve.
    pub requested: u64,
    /// Bytes already live in that cell.
    pub used: u64,
    /// Finite limit of that cell.
    pub limit: u64,
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "memory budget cell '{}' rejected a {} byte reservation: {} of {} bytes already in use",
            self.cell, self.requested, self.used, self.limit
        )
    }
}

impl Error for BudgetError {}

#[derive(Debug)]
struct BudgetCell {
    limit_bytes: u64,
    used_bytes: u64,
    peak_bytes: u64,
    rejected_allocations: u64,
    rejected_bytes: u64,
}

impl BudgetCell {
    fn new(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            used_bytes: 0,
            peak_bytes: 0,
            rejected_allocations: 0,
            rejected_bytes: 0,
        }
    }

    fn snapshot(&self) -> BudgetCellSnapshot {
        BudgetCellSnapshot {
            limit_bytes: self.limit_bytes,
            used_bytes: self.used_bytes,
            peak_bytes: self.peak_bytes,
            rejected_allocations: self.rejected_allocations,
            rejected_bytes: self.rejected_bytes,
        }
    }

    /// Admits `bytes` or records the rejection. Invariant: `used_bytes`
    /// never exceeds `limit_bytes`.
    fn try_reserve(&mut self, kind: BudgetKind, bytes: u64) -> Result<(), BudgetError> {
        match self.used_bytes.checked_add(bytes) {
            Some(new_used) if new_used <= self.limit_bytes => {
                self.used_bytes = new_used;
                if new_used > self.peak_bytes {
                    self.peak_bytes = new_used;
                }
                Ok(())
            }
            _ => {
                self.rejected_allocations = self.rejected_allocations.saturating_add(1);
                self.rejected_bytes = self.rejected_bytes.saturating_add(bytes);
                Err(BudgetError {
                    cell: kind,
                    requested: bytes,
                    used: self.used_bytes,
                    limit: self.limit_bytes,
                })
            }
        }
    }

    /// Returns exactly `bytes` of a previously admitted charge. The release
    /// invariant makes underflow impossible; a violated invariant must fail
    /// explicitly rather than silently undercount live allocations.
    fn release(&mut self, kind: BudgetKind, bytes: u64) {
        self.used_bytes = self.used_bytes.checked_sub(bytes).unwrap_or_else(|| {
            panic!(
                "budget cell '{kind}' cannot release {bytes} bytes from {}",
                self.used_bytes
            )
        });
    }
}

#[derive(Debug)]
struct BudgetCells {
    shm: BudgetCell,
    file: BudgetCell,
    reassembly: BudgetCell,
}

impl BudgetCells {
    fn new(shm_limit: u64, file_limit: u64, reassembly_limit: u64) -> Self {
        Self {
            shm: BudgetCell::new(shm_limit),
            file: BudgetCell::new(file_limit),
            reassembly: BudgetCell::new(reassembly_limit),
        }
    }

    fn get_mut(&mut self, kind: BudgetKind) -> &mut BudgetCell {
        match kind {
            BudgetKind::Shm => &mut self.shm,
            BudgetKind::File => &mut self.file,
            BudgetKind::Reassembly => &mut self.reassembly,
        }
    }
}

#[derive(Debug)]
struct BudgetState {
    cells: Mutex<BudgetCells>,
}

/// Three finite byte budgets shared by clones and `Arc` holders.
///
/// Clones share the same underlying counters: a charge taken through one
/// handle is visible in every handle's [`MemoryBudget::snapshot`]. Dropping
/// the creating handle does not invalidate live [`BudgetReservation`] guards
/// or the shared accounting state.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    state: Arc<BudgetState>,
}

impl MemoryBudget {
    /// Creates a budget with three explicit finite limits. A zero limit
    /// rejects every positive charge in that cell; it is not unlimited.
    pub fn new(shm_limit: u64, file_limit: u64, reassembly_limit: u64) -> Self {
        Self {
            state: Arc::new(BudgetState {
                cells: Mutex::new(BudgetCells::new(shm_limit, file_limit, reassembly_limit)),
            }),
        }
    }

    /// Admits a byte charge against one cell and returns its guard, or
    /// rejects the attempt before any allocation happens.
    ///
    /// A zero-byte reservation is admitted harmlessly and returns a guard
    /// that charges nothing.
    pub fn reserve(&self, kind: BudgetKind, bytes: u64) -> Result<BudgetReservation, BudgetError> {
        let attempt = {
            let mut cells = lock_cells(&self.state.cells);
            cells.get_mut(kind).try_reserve(kind, bytes)
        };
        attempt.map(|()| BudgetReservation {
            state: Arc::clone(&self.state),
            kind,
            bytes,
        })
    }

    /// Consistent snapshot of all three cells.
    pub fn snapshot(&self) -> BudgetSnapshot {
        let cells = lock_cells(&self.state.cells);
        BudgetSnapshot {
            shm: cells.shm.snapshot(),
            file: cells.file.snapshot(),
            reassembly: cells.reassembly.snapshot(),
        }
    }
}

fn lock_cells(cells: &Mutex<BudgetCells>) -> std::sync::MutexGuard<'_, BudgetCells> {
    cells
        .lock()
        .expect("memory budget accounting mutex poisoned")
}

/// Move-only ownership of one admitted charge.
///
/// Not `Clone` or `Copy`: moving the guard transfers the charge, and the
/// charge is returned exactly once when the guard is dropped (including
/// during unwind). It holds only the accounting state `Arc` plus its bytes
/// and kind — never a pool, mapping, Runtime, or callback — so it cannot
/// create ownership cycles and outliving its creating [`MemoryBudget`] is
/// safe.
#[must_use = "retain the reservation until its backing or payload is released"]
#[derive(Debug)]
pub struct BudgetReservation {
    state: Arc<BudgetState>,
    kind: BudgetKind,
    bytes: u64,
}

impl BudgetReservation {
    /// The cell this guard charges.
    pub fn kind(&self) -> BudgetKind {
        self.kind
    }

    /// The charged byte count.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        let mut cells = lock_cells(&self.state.cells);
        cells.get_mut(self.kind).release(self.kind, self.bytes);
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
    fn cells_are_independent() {
        let budget = MemoryBudget::new(100, 200, 300);
        let shm = budget.reserve(BudgetKind::Shm, 60).unwrap();
        let file = budget.reserve(BudgetKind::File, 200).unwrap();

        let snap = budget.snapshot();
        assert_eq!(
            snap.shm,
            BudgetCellSnapshot {
                limit_bytes: 100,
                used_bytes: 60,
                peak_bytes: 60,
                rejected_allocations: 0,
                rejected_bytes: 0,
            }
        );
        assert_eq!(snap.file.used_bytes, 200);
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert_eq!(snap.cell(BudgetKind::File).limit_bytes, 200);

        // A full cell rejects while the others still admit.
        let err = budget.reserve(BudgetKind::File, 1).unwrap_err();
        assert_eq!(
            err,
            BudgetError {
                cell: BudgetKind::File,
                requested: 1,
                used: 200,
                limit: 200,
            }
        );
        assert!(err.to_string().contains("file"));
        assert!(budget.reserve(BudgetKind::Shm, 41).is_err());
        let reassembly = budget.reserve(BudgetKind::Reassembly, 300).unwrap();

        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 60);
        assert_eq!(snap.file.rejected_allocations, 1);
        assert_eq!(snap.file.rejected_bytes, 1);
        assert_eq!(snap.shm.rejected_allocations, 1);
        assert_eq!(snap.shm.rejected_bytes, 41);
        assert_eq!(snap.reassembly.rejected_allocations, 0);

        drop((shm, file, reassembly));
        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.file.used_bytes, 0);
        assert_eq!(snap.reassembly.used_bytes, 0);
    }

    #[test]
    fn zero_limit_rejects_positive_bytes_and_admits_zero() {
        let budget = MemoryBudget::new(0, 0, 0);
        let err = budget.reserve(BudgetKind::Shm, 1).unwrap_err();
        assert_eq!(
            err,
            BudgetError {
                cell: BudgetKind::Shm,
                requested: 1,
                used: 0,
                limit: 0,
            }
        );
        let err = budget.reserve(BudgetKind::Reassembly, 7).unwrap_err();
        assert_eq!(err.cell, BudgetKind::Reassembly);
        assert!(err.to_string().contains("reassembly"));

        let snap = budget.snapshot();
        assert_eq!(snap.shm.rejected_allocations, 1);
        assert_eq!(snap.shm.rejected_bytes, 1);
        assert_eq!(snap.reassembly.rejected_allocations, 1);
        assert_eq!(snap.reassembly.rejected_bytes, 7);

        // Zero bytes are admitted harmlessly even against a zero limit.
        let guard = budget.reserve(BudgetKind::File, 0).unwrap();
        assert_eq!(guard.bytes(), 0);
        assert_eq!(guard.kind(), BudgetKind::File);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        drop(guard);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
    }

    #[test]
    fn overflow_is_rejected_without_wrapping() {
        let budget = MemoryBudget::new(u64::MAX, u64::MAX, u64::MAX);
        let near = budget.reserve(BudgetKind::Shm, u64::MAX - 5).unwrap();

        let err = budget.reserve(BudgetKind::Shm, 6).unwrap_err();
        assert_eq!(
            err,
            BudgetError {
                cell: BudgetKind::Shm,
                requested: 6,
                used: u64::MAX - 5,
                limit: u64::MAX,
            }
        );

        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, u64::MAX - 5);
        assert_eq!(snap.shm.peak_bytes, u64::MAX - 5);
        assert_eq!(snap.shm.rejected_allocations, 1);
        assert_eq!(snap.shm.rejected_bytes, 6);

        drop(near);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
    }

    #[test]
    fn release_and_move_transfer_return_the_charge_once() {
        let budget = MemoryBudget::new(100, 100, 100);
        let guard = budget.reserve(BudgetKind::Shm, 40).unwrap();
        assert_eq!(budget.snapshot().shm.used_bytes, 40);

        fn consume(guard: BudgetReservation) {
            assert_eq!(guard.bytes(), 40);
            // Released exactly once, at this scope's end.
        }
        consume(guard);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);

        // Peak is a high-water mark: it persists across releases.
        let again = budget.reserve(BudgetKind::Shm, 100).unwrap();
        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 100);
        assert_eq!(snap.shm.peak_bytes, 100);
        drop(again);
        let snap = budget.snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.shm.peak_bytes, 100);
    }

    #[test]
    fn clones_share_counters_and_guards_survive_creator_drop() {
        let budget = MemoryBudget::new(50, 50, 50);
        let observer = budget.clone();
        let guard = budget.reserve(BudgetKind::Shm, 30).unwrap();
        drop(budget);

        // The clone observes the same shared counters.
        assert_eq!(observer.snapshot().shm.used_bytes, 30);
        let second = observer
            .reserve(BudgetKind::Shm, 20)
            .expect("clone shares the same cell and counters");
        assert_eq!(observer.snapshot().shm.used_bytes, 50);

        // The creator is gone; guards keep the accounting Arc alive and the
        // charges intact.
        drop(second);
        assert_eq!(observer.snapshot().shm.used_bytes, 30);
        drop(guard);
        let snap = observer.snapshot();
        assert_eq!(snap.shm.used_bytes, 0);
        assert_eq!(snap.shm.peak_bytes, 50);
    }

    #[test]
    fn guard_retains_state_after_all_budget_handles_drop() {
        let (guard, weak) = {
            let budget = MemoryBudget::new(64, 64, 64);
            let weak = Arc::downgrade(&budget.state);
            (budget.reserve(BudgetKind::File, 32).unwrap(), weak)
        };
        let retained = weak.upgrade().expect("guard must retain accounting state");
        assert_eq!(lock_cells(&retained.cells).file.used_bytes, 32);
        drop(retained);
        drop(guard);
        assert!(
            weak.upgrade().is_none(),
            "last guard must release its state"
        );
    }

    #[test]
    fn panic_unwind_releases_the_charge() {
        let budget = MemoryBudget::new(100, 100, 100);
        let retained = budget.reserve(BudgetKind::Reassembly, 25).unwrap();

        let result = catch_unwind(AssertUnwindSafe(|| {
            let guard = budget.reserve(BudgetKind::Reassembly, 10).unwrap();
            assert_eq!(budget.snapshot().reassembly.used_bytes, 35);
            let _ = &guard;
            panic!("unwind while a reservation is live");
        }));
        assert!(result.is_err());

        // The unwound guard returned its charge; the unaffected one kept it.
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.used_bytes, 25);
        assert_eq!(snap.reassembly.peak_bytes, 35);
        drop(retained);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn concurrent_overcommit_rejects_beyond_the_limit() {
        const THREADS: usize = 8;
        const CHUNK: u64 = 300;
        // floor(1000 / 300) == 3; with no releases during the phase, an
        // attempt succeeds exactly while fewer than 3 prior charges are
        // live, so the outcome is deterministic once the barrier releases.
        const EXPECTED_OK: usize = 3;

        let budget = MemoryBudget::new(1000, 1000, 1000);
        let barrier = Arc::new(Barrier::new(THREADS));
        let (tx, rx) = mpsc::channel();

        for _ in 0..THREADS {
            let budget = budget.clone();
            let barrier = barrier.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                barrier.wait();
                let outcome = budget.reserve(BudgetKind::Shm, CHUNK);
                tx.send(outcome).expect("receiver alive");
            });
        }
        drop(tx);
        let outcomes: Vec<Result<BudgetReservation, BudgetError>> = rx.iter().collect();
        assert_eq!(outcomes.len(), THREADS);

        let successes: Vec<BudgetReservation> =
            outcomes.into_iter().filter_map(Result::ok).collect();
        assert_eq!(successes.len(), EXPECTED_OK);

        // No guard was released during the phase, so usage grew to exactly
        // the successful total and every guard stays within the cap.
        let snap = budget.snapshot();
        let expected_used = EXPECTED_OK as u64 * CHUNK;
        assert_eq!(snap.shm.used_bytes, expected_used);
        assert_eq!(snap.shm.peak_bytes, expected_used);
        assert!(snap.shm.used_bytes <= snap.shm.limit_bytes);
        assert!(snap.shm.peak_bytes <= snap.shm.limit_bytes);
        let rejected = THREADS as u64 - EXPECTED_OK as u64;
        assert_eq!(snap.shm.rejected_allocations, rejected);
        assert_eq!(snap.shm.rejected_bytes, rejected * CHUNK);
        assert_eq!(snap.file.used_bytes, 0);
        assert_eq!(snap.reassembly.used_bytes, 0);

        drop(successes);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
    }
}
