//! Owned reassembly backing: one RAII carrier from admission to release.
//!
//! [`ReassemblyBacking`] is the single owner of one live chunk-reassembly
//! allocation. It bundles, in one move-only value:
//!
//! - the fixed local allocation owner, including its mapping and release
//!   authority, checked once at admission, and
//! - chunk receipt flags, when the backing belongs to an assembler, together
//!   with their [`MemoryReservation`] charging the tracking bytes to the
//!   pool's role-domain memory cell.
//!
//! The pool itself charges the domain memory cell when it creates fresh
//! backing storage (a buddy or dedicated segment) and the file-spill cell for
//! a file mapping, so admission never duplicates that whole-payload charge.
//! Only the tracking metadata is reserved here, before any storage exists,
//! and is refunded only **after** the fixed owner released the allocation (or
//! the file mapping closed): the backing's fields drop in declaration order,
//! storage first, metadata charge second. Ordinary error and drop paths
//! therefore cannot return budget bytes while backing storage remains
//! allocated. Trimming the logical length never changes any charge: charges
//! cover allocated capacity, not logical length.

use c2_mem::budget::PayloadBufferError;
use c2_mem::{DomainBudget, MemPool, MemoryReservation, OwnedAllocation};
use parking_lot::{RwLock, RwLockWriteGuard};
use std::fmt;
use std::sync::Arc;

use super::ChunkAdmissionError;

/// Checked reassembly geometry shared by admission and limit checks.
///
/// Rejects zero chunk counts, zero chunk sizes, and products that overflow
/// the platform range before any reservation or mapping is attempted.
pub fn checked_capacity(total_chunks: usize, chunk_size: usize) -> Result<usize, String> {
    if total_chunks == 0 {
        return Err("total_chunks must be > 0".into());
    }
    if chunk_size == 0 {
        return Err("chunk_size must be > 0".into());
    }
    total_chunks.checked_mul(chunk_size).ok_or_else(|| {
        format!(
            "reassembly geometry overflow: {total_chunks} chunks x {chunk_size} bytes exceeds the addressable capacity range"
        )
    })
}

/// The tracking-metadata charge: either the domain memory cell or, when the
/// backing's storage took no fresh memory bytes, the file-spill cell.
enum MetadataReservation {
    Memory(MemoryReservation),
    FileSpill(c2_mem::FileSpillReservation),
}

impl MetadataReservation {
    fn charged_bytes(&self) -> u64 {
        match self {
            Self::Memory(reservation) => reservation.bytes(),
            Self::FileSpill(reservation) => reservation.bytes(),
        }
    }
}

/// Live backing state: the allocated handle and its metadata charge.
///
/// Field order is load-bearing: Rust drops fields in declaration order, so
/// the handle releases its storage before the metadata reservation refunds
/// its bytes. The pair is never handed out separately.
struct BackingState {
    received_flags: Vec<u8>,
    handle: OwnedAllocation,
    /// Capacity captured at admission; `capacity_bytes()` reports it while
    /// live and zero after release.
    capacity: usize,
    metadata_reservation: Option<MetadataReservation>,
}

/// Move-only RAII owner of one admitted live reassembly allocation.
///
/// Created only through [`ReassemblyBacking::admit`] and
/// [`ReassemblyBacking::admit_with_guard`]. There is no way to detach the
/// metadata charge from the backing: callers hold the whole carrier or
/// nothing, so a released backing always refunds exactly once and an
/// unreleased one keeps its charge. The carrier pins its actual mapping, so
/// finished reassemblies stay valid and charged even after their registry,
/// connection, or client is shut down and dropped.
///
/// Replacing or dropping the admission pool never changes the carrier's
/// mapping, release authority or original budget domain.
pub struct ReassemblyBacking {
    state: Option<BackingState>,
}

impl ReassemblyBacking {
    /// Allocate a backing handle from the pool's role-domain budget.
    ///
    /// Geometry errors and budget rejections happen before any mapping is
    /// created; a rejected admission leaves no filesystem or SHM trace and
    /// only bumps the budget's rejection statistics. The pool write guard is
    /// held across the allocation so an allocation failure refunds the
    /// metadata charge on the early-return drop.
    pub fn admit(
        pool: Arc<RwLock<MemPool>>,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<Self, String> {
        // Keep standalone invalid geometry rejection ahead of lock waiting.
        checked_capacity(total_chunks, chunk_size)?;
        let guard = pool.write();
        Self::admit_with_guard(Arc::clone(&pool), guard, total_chunks, chunk_size, 0)
            .map_err(|error| error.to_string())
    }

    /// Admission with an already acquired write guard from this exact pool.
    /// Registry callers acquire it nonblockingly under their entry borrow;
    /// this constructor never waits for the pool lock or enters the registry.
    pub(crate) fn admit_with_guard(
        pool: Arc<RwLock<MemPool>>,
        mut guard: RwLockWriteGuard<'_, MemPool>,
        total_chunks: usize,
        chunk_size: usize,
        tracked_chunks: usize,
    ) -> Result<Self, ChunkAdmissionError> {
        if !std::ptr::eq(Arc::as_ptr(&pool), RwLockWriteGuard::rwlock(&guard)) {
            return Err(ChunkAdmissionError::Protocol(
                "reassembly admission guard belongs to a different pool".into(),
            ));
        }
        let capacity =
            checked_capacity(total_chunks, chunk_size).map_err(ChunkAdmissionError::Protocol)?;
        let metadata_bytes = tracked_chunks as u64;
        let capacity_bytes = u64::try_from(capacity).map_err(|_| {
            ChunkAdmissionError::Capacity(format!(
                "reassembly capacity {capacity} bytes exceeds the budget accounting range"
            ))
        })?;
        // One pool write critical section covers charges and allocation so no
        // other allocator can consume the admitted bytes in between.
        let budget: DomainBudget = guard
            .budget()
            .ok_or_else(|| {
                ChunkAdmissionError::Capacity(
                    "reassembly pool carries no owner budget context".into(),
                )
            })?
            .clone();
        let memory_limit = budget.snapshot().memory.limit_bytes;
        if memory_limit == 0 {
            // With a zero memory limit this reservation always fails TooLarge,
            // recording the rejection in the memory cell before any storage
            // or file mapping could be created.
            let _ = budget.try_reserve_memory(capacity_bytes);
            return Err(ChunkAdmissionError::Capacity(
                "role-domain memory budget is zero: positive reassembly is disabled".into(),
            ));
        }
        // Metadata first: charge the tracking flags before any storage exists.
        // A capacity shortfall defers the decision until the backing's own
        // charge is known; the whole payload is never reserved up front and
        // blocking waits are forbidden on this path.
        let mut metadata_reservation = None;
        let mut metadata_deferred = false;
        if metadata_bytes > 0 {
            match budget.try_reserve_memory(metadata_bytes) {
                Ok(reservation) => {
                    metadata_reservation = Some(MetadataReservation::Memory(reservation))
                }
                Err(PayloadBufferError::TooLarge { .. }) => {
                    return Err(ChunkAdmissionError::Capacity(format!(
                        "reassembly tracking metadata {metadata_bytes} bytes exceeds the role-domain memory budget"
                    )));
                }
                Err(PayloadBufferError::InsufficientCapacity { .. })
                | Err(PayloadBufferError::DeadlineExceeded) => metadata_deferred = true,
            }
        }
        // The pool charges the memory cell itself when it creates fresh
        // backing storage (a buddy or dedicated segment) and the file-spill
        // cell for a file mapping; reuse of an already-mapped buddy block
        // takes no new charge. Never double-charge the capacity here.
        let reused_before = guard.stats().buddy_reused_allocs;
        let handle = guard.alloc_owned_with_reason(capacity).map_err(|error| {
            let message = error.to_string();
            match error.memory_shortfall() {
                Some((requested, available)) if requested <= memory_limit - metadata_bytes => {
                    let deferred = if metadata_deferred { metadata_bytes } else { 0 };
                    ChunkAdmissionError::MemoryPressure {
                        needed_bytes: requested + deferred - available,
                        message,
                    }
                }
                _ => ChunkAdmissionError::Capacity(message),
            }
        })?;
        if metadata_deferred {
            // Retry the memory cell once now that the backing's own charge is
            // known. File-spill backings and reused buddy blocks took no new
            // memory bytes, so only those may charge their metadata to the
            // file-spill cell instead.
            let file_spill_eligible =
                handle.is_file_spill() || guard.stats().buddy_reused_allocs > reused_before;
            match budget.try_reserve_memory(metadata_bytes) {
                Ok(reservation) => {
                    metadata_reservation = Some(MetadataReservation::Memory(reservation))
                }
                Err(memory_error) => {
                    let reserved = if file_spill_eligible {
                        budget
                            .try_reserve_file_spill(metadata_bytes)
                            .ok()
                            .map(MetadataReservation::FileSpill)
                    } else {
                        None
                    };
                    match reserved {
                        Some(reservation) => metadata_reservation = Some(reservation),
                        None => {
                            drop(handle);
                            let message = format!(
                                "reassembly tracking metadata {metadata_bytes} bytes exceeded both the role-domain memory and file-spill budgets"
                            );
                            return Err(match memory_error {
                                PayloadBufferError::InsufficientCapacity { available, .. } => {
                                    ChunkAdmissionError::MemoryPressure {
                                        needed_bytes: metadata_bytes - available,
                                        message,
                                    }
                                }
                                _ => ChunkAdmissionError::Capacity(message),
                            });
                        }
                    }
                }
            }
        }
        // The tracking allocation is fallible only after its charge exists.
        let mut received_flags = Vec::new();
        if let Err(error) = received_flags.try_reserve_exact(tracked_chunks) {
            drop(handle);
            return Err(ChunkAdmissionError::Capacity(format!(
                "reassembly chunk tracking allocation failed: {error}"
            )));
        }
        received_flags.resize(tracked_chunks, 0);
        drop(guard);
        Ok(Self {
            state: Some(BackingState {
                received_flags,
                handle,
                capacity,
                metadata_reservation,
            }),
        })
    }

    fn live_state(&self) -> Result<&BackingState, String> {
        self.state
            .as_ref()
            .ok_or_else(|| "reassembly backing already released".to_string())
    }

    fn live_state_mut(&mut self) -> Result<&mut BackingState, String> {
        self.state
            .as_mut()
            .ok_or_else(|| "reassembly backing already released".to_string())
    }

    /// Transfer the charged tracking allocation to the assembler while this
    /// backing is still uniquely owned. Its charge stays with the backing.
    pub(crate) fn take_received_flags(&mut self) -> Result<Vec<u8>, String> {
        Ok(std::mem::take(&mut self.live_state_mut()?.received_flags))
    }

    pub(crate) fn restore_received_flags(&mut self, states: Vec<u8>) -> Result<(), String> {
        self.live_state_mut()?.received_flags = states;
        Ok(())
    }

    /// Obtain the stable allocation address before any parallel copy token
    /// exists. Only checked, disjoint token ranges may use this address.
    pub(crate) fn allocation_address(&mut self) -> Result<usize, String> {
        Ok(self.live_state_mut()?.handle.as_mut_slice().as_mut_ptr() as usize)
    }

    /// Logical data length (may be trimmed below allocated capacity).
    pub fn len(&self) -> usize {
        self.state.as_ref().map(|s| s.handle.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_file_spill(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.handle.is_file_spill())
            .unwrap_or(false)
    }

    pub fn is_buddy(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.handle.is_buddy())
            .unwrap_or(false)
    }

    pub fn is_dedicated(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.handle.is_dedicated())
            .unwrap_or(false)
    }

    /// Charged payload capacity in bytes (never the trimmed length, never the
    /// tracking metadata).
    ///
    /// Returns the admitted capacity while the backing is live and zero after
    /// release.
    pub fn capacity_bytes(&self) -> u64 {
        self.state.as_ref().map(|s| s.capacity as u64).unwrap_or(0)
    }

    /// Whether the backing storage and its charges were already released.
    pub fn is_released(&self) -> bool {
        self.state.is_none()
    }

    /// The file-spill backing path, when this backing is file-backed.
    ///
    /// Diagnostic accessor for cleanup proofs (for example Windows
    /// delete-on-close); mutating or releasing stays with the carrier.
    pub fn file_spill_path(&self) -> Option<std::path::PathBuf> {
        self.state
            .as_ref()
            .and_then(|s| s.handle.file_spill_path())
            .map(std::path::Path::to_path_buf)
    }

    /// Write chunk data at an absolute byte offset inside the allocation.
    ///
    /// The span must lie within the current logical length; during assembly
    /// that length equals the full allocated capacity.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        let state = self.live_state_mut()?;
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| "reassembly write span overflows the platform range".to_string())?;
        if end > state.handle.len() {
            return Err(format!(
                "reassembly write span {offset}..{end} is outside the {} byte allocation",
                state.handle.len()
            ));
        }
        let slice = state.handle.as_mut_slice();
        slice[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Copy the logical bytes out of the backing without releasing it.
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        let state = self.live_state()?;
        Ok(state.handle.as_slice().to_vec())
    }

    /// Run `f` over the fixed owner's logical slice. The owner borrow pins
    /// mapping and allocation through the callback without taking a pool lock.
    pub fn with_slice<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, String> {
        let state = self.live_state()?;
        Ok(f(state.handle.as_slice()))
    }

    /// Shrink the logical data length.
    ///
    /// This is a logical trim only: charges keep covering the full allocated
    /// capacity until [`ReassemblyBacking::release`], so a trimmed (finished)
    /// assembly stays charged and cannot be grown back.
    pub fn trim_to(&mut self, new_len: usize) -> Result<(), String> {
        let handle = &mut self.live_state_mut()?.handle;
        if new_len > handle.len() {
            return Err(format!(
                "cannot grow reassembly logical length to {new_len} beyond the {} byte allocation",
                handle.len()
            ));
        }
        handle.trim_to(new_len)?;
        Ok(())
    }

    /// Release the fixed allocation owner, then refund the metadata charge.
    pub fn release(&mut self) {
        drop(self.state.take());
    }
}

impl Drop for ReassemblyBacking {
    fn drop(&mut self) {
        self.release();
    }
}

impl fmt::Debug for ReassemblyBacking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            Some(state) => f
                .debug_struct("ReassemblyBacking")
                .field("handle", &state.handle)
                .field("capacity_bytes", &state.capacity)
                .field(
                    "metadata_bytes",
                    &state
                        .metadata_reservation
                        .as_ref()
                        .map(MetadataReservation::charged_bytes),
                )
                .finish_non_exhaustive(),
            None => f
                .debug_struct("ReassemblyBacking")
                .field("released", &true)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;

    static NEXT_POOL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn domain(memory_bytes: u64, file_bytes: u64) -> DomainBudget {
        DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: memory_bytes,
            file_spill_budget_bytes: file_bytes,
            max_outstanding_calls: 0,
        })
    }

    fn pool_config(spill_threshold: f64) -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            max_segments: 2,
            max_dedicated_segments: 2,
            buddy_idle_decay_secs: 0.0,
            spill_threshold,
            spill_dir: std::env::temp_dir().join("c2_backing_test"),
            ..PoolConfig::default()
        }
    }

    fn unique_prefix(label: &str) -> String {
        let sequence = NEXT_POOL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "/cc3k{:04x}{:04x}{label}",
            std::process::id() as u16,
            sequence
        )
    }

    /// SHM-first pool (pressure heuristic disabled) with its own domain.
    fn make_mempool(label: &str) -> (MemPool, DomainBudget) {
        let budget = domain(1 << 20, 1 << 20);
        let pool = MemPool::new_with_prefix_and_budget(
            pool_config(1.0),
            unique_prefix(label),
            budget.clone(),
        )
        .unwrap();
        (pool, budget)
    }

    fn make_pool(label: &str) -> (Arc<RwLock<MemPool>>, DomainBudget) {
        let (pool, budget) = make_mempool(label);
        (Arc::new(RwLock::new(pool)), budget)
    }

    /// Pure file-spill pool: the threshold 0.0 denies every new SHM backing.
    fn make_file_pool(label: &str, memory: u64, file: u64) -> (Arc<RwLock<MemPool>>, DomainBudget) {
        let budget = domain(memory, file);
        let pool = MemPool::new_with_prefix_and_budget(
            pool_config(0.0),
            unique_prefix(label),
            budget.clone(),
        )
        .unwrap();
        (Arc::new(RwLock::new(pool)), budget)
    }

    #[test]
    fn zero_memory_budget_rejects_positive_reassembly_and_records_it() {
        let (pool, budget) = make_file_pool("zero-memory", 0, 1 << 20);
        let error = ReassemblyBacking::admit(pool.clone(), 1, 4096).unwrap_err();
        assert!(
            error.contains("zero"),
            "error must name the zero budget: {error}"
        );
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.memory.rejected_allocations, 1);
        assert_eq!(snapshot.memory.rejected_bytes, 4096);
        assert_eq!(snapshot.memory.used_bytes, 0);
        assert_eq!(snapshot.memory.peak_bytes, 0);
        assert_eq!(snapshot.file_spill.used_bytes, 0);
        assert_eq!(snapshot.file_spill.rejected_allocations, 0);
        assert_eq!(pool.read().stats().total_segments, 0);
        assert_eq!(pool.read().stats().dedicated_segments, 0);
        assert_eq!(pool.read().stats().file_spill_allocs, 0);
    }

    #[test]
    fn metadata_is_charged_to_memory_and_refunded_only_after_release() {
        let (pool, budget) = make_file_pool("metadata-order", 1 << 20, 8192);
        let before = budget.snapshot();
        let mut backing =
            ReassemblyBacking::admit_with_guard(pool.clone(), pool.write(), 4, 128, 4).unwrap();
        assert!(backing.is_file_spill());
        // The file-spill cell carries the capacity; the memory cell carries
        // only the tracking metadata (one byte per tracked chunk).
        let live = budget.snapshot();
        assert_eq!(
            live.file_spill.used_bytes,
            before.file_spill.used_bytes + 512
        );
        assert_eq!(live.memory.used_bytes, before.memory.used_bytes + 4);
        assert_eq!(backing.capacity_bytes(), 512);
        backing.release();
        let after = budget.snapshot();
        assert_eq!(after.memory.used_bytes, before.memory.used_bytes);
        assert_eq!(after.file_spill.used_bytes, before.file_spill.used_bytes);
    }

    #[test]
    fn exhausted_memory_cell_falls_back_to_file_spill_with_metadata_on_file_spill() {
        let (pool, budget) = make_file_pool("full-memory", 4096, 8192);
        // Occupy the entire memory cell from the same domain budget.
        let hold = budget.try_reserve_memory(4096).unwrap();
        let before = budget.snapshot();
        let mut backing =
            ReassemblyBacking::admit_with_guard(pool.clone(), pool.write(), 2, 128, 2).unwrap();
        assert!(backing.is_file_spill());
        let live = budget.snapshot();
        // Capacity and deferred metadata both charge the file-spill cell.
        assert_eq!(
            live.file_spill.used_bytes,
            before.file_spill.used_bytes + 256 + 2
        );
        assert_eq!(live.memory.used_bytes, before.memory.used_bytes);
        backing.release();
        let after = budget.snapshot();
        assert_eq!(after.memory.used_bytes, before.memory.used_bytes);
        assert_eq!(after.file_spill.used_bytes, before.file_spill.used_bytes);
        drop(hold);
    }

    #[test]
    fn borrowed_backing_does_not_block_other_allocations_or_releases() {
        use std::sync::{Barrier, mpsc};
        use std::time::Duration;

        for file in [false, true] {
            let (pool, budget) = if file {
                make_file_pool("short-copy", 1 << 20, 8192)
            } else {
                make_pool("short-copy")
            };
            let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 4096).unwrap();
            assert_eq!(backing.is_file_spill(), file);
            backing.write_at(0, &[7; 4096]).unwrap();
            backing.trim_to(17).unwrap();
            let entered = Arc::new(Barrier::new(2));
            let resume = Arc::new(Barrier::new(2));
            let reader_entered = entered.clone();
            let reader_resume = resume.clone();
            let reader = std::thread::spawn(move || {
                let copy = backing
                    .with_slice(|bytes| {
                        reader_entered.wait();
                        reader_resume.wait();
                        bytes.to_vec()
                    })
                    .unwrap();
                assert_eq!(copy, vec![7; 17]);
                backing
            });
            entered.wait();
            let (done, observed) = mpsc::channel();
            let allocating = pool.clone();
            let worker = std::thread::spawn(move || {
                let mut other = ReassemblyBacking::admit(allocating, 1, 4096).unwrap();
                other.write_at(0, &[9; 4096]).unwrap();
                other.release();
                done.send(()).unwrap();
            });
            let progress = observed.recv_timeout(Duration::from_secs(2));
            let charged_during_copy = budget.snapshot();
            resume.wait();
            let mut backing = reader.join().unwrap();
            worker.join().unwrap();
            progress.expect("another allocation/release waited for a backing borrow");
            if file {
                assert_eq!(charged_during_copy.file_spill.used_bytes, 4096);
                assert_eq!(charged_during_copy.memory.used_bytes, 0);
            } else {
                assert_eq!(charged_during_copy.file_spill.used_bytes, 0);
                assert!(charged_during_copy.memory.used_bytes >= 4096);
            }
            backing.release();
            if file {
                assert_eq!(budget.snapshot().file_spill.used_bytes, 0);
                assert_eq!(budget.snapshot().memory.used_bytes, 0);
            } else {
                // The retained idle buddy segment keeps its creation charge;
                // releasing the carrier freed its block without new charges.
                assert_eq!(
                    budget.snapshot().memory.used_bytes,
                    charged_during_copy.memory.used_bytes
                );
            }
        }
    }

    #[test]
    fn file_release_is_independent_of_another_carrier_borrow() {
        let (pool, budget) = make_file_pool("file-release", 1 << 20, 8192);
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 4096).unwrap();
        let mut other = ReassemblyBacking::admit(pool.clone(), 1, 4096).unwrap();
        other.write_at(0, &[34; 4096]).unwrap();
        other
            .with_slice(|bytes| {
                backing.release();
                assert!(backing.is_released());
                assert!(backing.copy_bytes().is_err());
                assert!(backing.write_at(0, &[21]).unwrap_err().contains("released"));
                assert_eq!(bytes, &[34; 4096]);
                assert_eq!(budget.snapshot().file_spill.used_bytes, 4096);
            })
            .unwrap();
        {
            let _guard = pool.write();
            backing.release();
            other.release();
        }
        assert!(other.with_slice(|_| ()).is_err());
        assert_eq!(budget.snapshot().file_spill.used_bytes, 0);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
    }

    #[test]
    fn admission_guard_must_belong_to_carrier_pool_before_charging() {
        let make = || {
            let budget = domain(1 << 20, 1 << 20);
            let pool = Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    PoolConfig {
                        pool_enabled: false,
                        spill_threshold: 0.0,
                        ..pool_config(0.0)
                    },
                    unique_prefix("admission-guard-pure"),
                    budget.clone(),
                )
                .unwrap(),
            ));
            (pool, budget)
        };
        let (owner, owner_budget) = make();
        let (other, other_budget) = make();
        let error = ReassemblyBacking::admit_with_guard(owner.clone(), other.write(), 1, 128, 0)
            .unwrap_err();
        assert!(error.to_string().contains("different pool"));
        for (pool, budget) in [(owner, owner_budget), (other, other_budget)] {
            let snapshot = budget.snapshot();
            assert_eq!(snapshot.memory.used_bytes, 0);
            assert_eq!(snapshot.memory.peak_bytes, 0);
            assert_eq!(snapshot.memory.rejected_allocations, 0);
            assert_eq!(snapshot.file_spill.used_bytes, 0);
            assert_eq!(pool.read().stats().total_segments, 0);
            assert_eq!(pool.read().stats().dedicated_segments, 0);
        }
    }

    #[test]
    fn explicit_release_marks_carrier_released_and_idempotent() {
        let (pool, _budget) = make_file_pool("explicit-release", 1 << 20, 1 << 20);
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 2, 256).unwrap();
        backing.trim_to(300).unwrap();
        assert_eq!(backing.len(), 300);
        assert_eq!(backing.capacity_bytes(), 512);

        backing.release();
        assert!(backing.is_released());
        assert_eq!(backing.len(), 0);
        assert_eq!(backing.capacity_bytes(), 0);
        assert!(!backing.is_buddy() && !backing.is_dedicated() && !backing.is_file_spill());
        assert!(backing.copy_bytes().is_err());
        assert!(backing.with_slice(|s| s.len()).is_err());
        // Idempotent, and no second refund.
        backing.release();
        assert!(backing.is_released());
    }

    #[test]
    fn empty_pool_replacement_preserves_fixed_backing_owner() {
        let (pool, _budget) = make_pool("empty");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let (replacement, replacement_budget) = make_mempool("empty");
        let owner = std::mem::replace(&mut *pool.write(), replacement);
        drop(owner);
        backing.write_at(0, &[0x33; 512]).unwrap();
        assert_eq!(backing.copy_bytes().unwrap(), vec![0x33; 512]);
        assert_eq!(backing.with_slice(|bytes| bytes[0]).unwrap(), 0x33);
        // The original domain keeps the buddy segment's charge while the
        // carrier pins its mapping, independent of the replacement pool.
        assert!(budget.snapshot().memory.used_bytes >= 512);
        assert_eq!(replacement_budget.snapshot().memory.used_bytes, 0);
        backing.release();
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        assert_eq!(budget.snapshot().file_spill.used_bytes, 0);
        assert!(
            backing
                .write_at(0, &[0x33])
                .unwrap_err()
                .contains("released")
        );
    }

    fn coordinates(a: c2_mem::PoolAllocation) -> (u16, u32, u32) {
        (a.seg_idx as u16, a.generation, a.offset)
    }

    fn colliding_replacement_preserves_original_owner(pool_enabled: bool) {
        // Identical labels and geometry still create distinct incarnations.
        let (first, first_budget) = make_mempool("collision");
        // Without the buddy pool a size beyond the buddy block limit selects
        // dedicated SHM; with it, a small block selects a buddy segment.
        let size = if pool_enabled { 512 } else { 128 * 1024 };
        let pool = Arc::new(RwLock::new(first));
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, size).unwrap();
        assert_eq!(backing.is_buddy(), pool_enabled);
        assert_eq!(backing.is_dedicated(), !pool_enabled);
        backing.write_at(0, &vec![0x11; size]).unwrap();
        let charged = first_budget.snapshot().memory.used_bytes;
        assert!(charged >= size as u64);

        let (mut replacement, replacement_budget) = make_mempool("collision");
        let mut fresh = replacement.alloc_handle(size).unwrap();
        replacement.handle_slice_mut(&mut fresh).fill(0x22);
        assert_eq!(
            coordinates(
                backing
                    .live_state()
                    .unwrap()
                    .handle
                    .shm_allocation()
                    .unwrap()
            ),
            coordinates(fresh.shm_allocation().unwrap())
        );
        assert_ne!(pool.read().prefix(), replacement.prefix());
        let mut owner = std::mem::replace(&mut *pool.write(), replacement);

        assert_eq!(backing.copy_bytes().unwrap(), vec![0x11; size]);
        backing.write_at(0, &[0x44]).unwrap();
        assert_eq!(backing.with_slice(|bytes| bytes[0]).unwrap(), 0x44);
        // The original domain keeps exactly its own charge through the swap.
        assert_eq!(first_budget.snapshot().memory.used_bytes, charged);
        assert_eq!(owner.stats().alloc_count, 1);
        assert!(replacement_budget.snapshot().memory.used_bytes >= size as u64);
        assert_eq!(
            pool.read().copy_handle_data(&fresh).unwrap(),
            vec![0x22; size]
        );

        backing.release();
        assert!(backing.is_released());
        assert_eq!(owner.stats().alloc_count, 0);
        // Dedicated storage unmaps with its last pin. A retained idle buddy
        // segment keeps its creation charge until retention is disabled.
        owner.set_idle_backing_retention(false);
        assert_eq!(first_budget.snapshot().memory.used_bytes, 0);
        backing.release();
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert_eq!(
            pool.read().copy_handle_data(&fresh).unwrap(),
            vec![0x22; size]
        );
        pool.write().release_handle(fresh);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn colliding_buddy_replacement_keeps_original_storage() {
        colliding_replacement_preserves_original_owner(true);
    }

    #[test]
    fn colliding_dedicated_replacement_keeps_original_storage() {
        colliding_replacement_preserves_original_owner(false);
    }

    #[test]
    fn self_owned_file_survives_pool_replacement_and_releases_its_charge() {
        // Pure file path: no SHM seam is attempted, including under sandboxing.
        let budget = domain(1 << 20, 4096);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    max_segments: 0,
                    min_retained_segments: 0,
                    max_dedicated_segments: 0,
                    ..pool_config(0.0)
                },
                unique_prefix("file-owner"),
                budget.clone(),
            )
            .unwrap(),
        ));
        let mut backing = ReassemblyBacking::admit(pool.clone(), 2, 256).unwrap();
        assert!(backing.is_file_spill());
        let path = backing.file_spill_path().unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let (replacement, _replacement_budget) = make_mempool("file");
        *pool.write() = replacement;
        assert!(
            backing
                .write_at(usize::MAX, &[1])
                .unwrap_err()
                .contains("overflows")
        );
        assert!(backing.write_at(512, &[1]).unwrap_err().contains("outside"));
        backing.write_at(0, &[0x22]).unwrap();
        backing.trim_to(300).unwrap();
        assert_eq!(backing.with_slice(|s| s.len()).unwrap(), 300);
        let mut expected = vec![0x11; 300];
        expected[0] = 0x22;
        assert_eq!(backing.copy_bytes().unwrap(), expected);
        assert_eq!(budget.snapshot().file_spill.used_bytes, 512);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        backing.release();
        backing.release();
        assert!(backing.is_released());
        assert_eq!(budget.snapshot().file_spill.used_bytes, 0);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        assert!(!path.exists());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }
}
