//! Owned reassembly backing: one RAII carrier from admission to release.
//!
//! [`ReassemblyBacking`] is the single owner of one live chunk-reassembly
//! allocation. It bundles, in one move-only value:
//!
//! - the fixed local allocation owner, including its mapping and release
//!   authority, checked once at admission, and
//! - chunk receipt flags, when the backing belongs to an assembler;
//! - the [`BudgetReservation`] charging payload and flags to the pool's
//!   canonical reassembly budget cell.
//!
//! The reservation is taken **before** the pool allocates, and is refunded
//! only **after** the fixed owner released the allocation (or the file
//! mapping closed). Ordinary error and drop paths therefore cannot
//! return budget bytes while backing storage remains allocated. Trimming the
//! logical length never changes the charge: the reservation covers allocated
//! capacity, not logical length.

use c2_mem::budget::{BudgetKind, BudgetReservation};
use c2_mem::{MemPool, OwnedAllocation};
use parking_lot::{RwLock, RwLockWriteGuard};
use std::fmt;
use std::sync::Arc;

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

/// Live backing state: the allocated handle and its budget charge.
///
/// Kept together so every release path drops storage before the charge; the
/// pair is never handed out separately.
struct BackingState {
    received_flags: Vec<bool>,
    handle: OwnedAllocation,
    reservation: BudgetReservation,
}

/// Move-only RAII owner of one admitted live reassembly allocation.
///
/// Created only through [`ReassemblyBacking::admit`], which reserves the
/// reassembly budget before allocating. There is no way to detach the
/// reservation from the backing: callers hold the whole carrier or nothing,
/// so a released backing always refunds exactly once and an unreleased one
/// keeps its charge. The carrier pins its actual mapping, so finished
/// reassemblies stay valid and charged even after their registry, connection,
/// or client is shut down and dropped.
///
/// Replacing or dropping the admission pool never changes the carrier's
/// mapping, release authority or original budget domain.
pub struct ReassemblyBacking {
    state: Option<BackingState>,
}

impl ReassemblyBacking {
    /// Reserve `total_chunks × chunk_size` reassembly bytes from the pool's
    /// canonical budget, then allocate the backing handle.
    ///
    /// Geometry errors and budget rejections happen before any mapping is
    /// created; a rejected admission leaves no filesystem or SHM trace and
    /// only bumps the budget's rejection statistics. The reservation guard is
    /// held across the allocation so an allocation failure refunds the charge
    /// on the early-return drop.
    pub fn admit(
        pool: Arc<RwLock<MemPool>>,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<Self, String> {
        // Keep standalone invalid geometry rejection ahead of lock waiting.
        checked_capacity(total_chunks, chunk_size)?;
        let guard = pool.write();
        Self::admit_with_guard(Arc::clone(&pool), guard, total_chunks, chunk_size, 0)
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
    ) -> Result<Self, String> {
        if !std::ptr::eq(Arc::as_ptr(&pool), RwLockWriteGuard::rwlock(&guard)) {
            return Err("reassembly admission guard belongs to a different pool".to_string());
        }
        let capacity = checked_capacity(total_chunks, chunk_size)?;
        let metadata_bytes = tracked_chunks * std::mem::size_of::<bool>();
        let charged_capacity = capacity.checked_add(metadata_bytes).ok_or_else(|| {
            "reassembly capacity including metadata exceeds the addressable range".to_string()
        })?;
        let capacity_bytes = u64::try_from(charged_capacity).map_err(|_| {
            format!(
                "reassembly capacity {charged_capacity} bytes exceeds the budget accounting range"
            )
        })?;
        // One pool write critical section covers charge and allocation so no
        // other allocator can consume the admitted bytes in between.
        let (received_flags, handle, reservation) = {
            let budget = guard
                .budget()
                .ok_or_else(|| "reassembly pool carries no owner budget context".to_string())?;
            let reservation = budget
                .reserve(BudgetKind::Reassembly, capacity_bytes)
                .map_err(|e| e.to_string())?;
            // Make tracking allocation fallible before creating a pool handle
            // that requires an explicit release through its owner.
            let mut received_flags = Vec::new();
            received_flags
                .try_reserve_exact(tracked_chunks)
                .map_err(|error| format!("reassembly chunk tracking allocation failed: {error}"))?;
            received_flags.resize(tracked_chunks, false);
            match guard.alloc_owned(capacity) {
                Ok(handle) => (received_flags, handle, reservation),
                // The reservation guard drops with this error return and
                // refunds the charge; no backing was created.
                Err(e) => return Err(e),
            }
        };
        drop(guard);
        Ok(Self {
            state: Some(BackingState {
                received_flags,
                handle,
                reservation,
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

    pub(crate) fn chunk_received(&self, index: usize) -> Result<bool, String> {
        Ok(self.live_state()?.received_flags[index])
    }

    pub(crate) fn mark_chunk_received(&mut self, index: usize) -> Result<(), String> {
        self.live_state_mut()?.received_flags[index] = true;
        Ok(())
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

    /// Charged payload and tracking capacity in bytes (never the trimmed length).
    ///
    /// Returns the reserved capacity while the backing is live and zero after
    /// release.
    pub fn capacity_bytes(&self) -> u64 {
        self.state
            .as_ref()
            .map(|s| s.reservation.bytes())
            .unwrap_or(0)
    }

    /// Whether the backing storage and its charge were already released.
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
    /// This is a logical trim only: the reassembly charge keeps covering the
    /// full allocated capacity until [`ReassemblyBacking::release`], so a
    /// trimmed (finished) assembly stays charged and cannot be grown back.
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

    /// Release the fixed allocation owner, then refund its original capacity.
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
                .field("capacity_bytes", &state.reservation.bytes())
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

    fn make_mempool(label: &str) -> MemPool {
        let sequence = NEXT_POOL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 2,
                buddy_idle_decay_secs: 0.0,
                spill_threshold: 1.0,
                spill_dir: std::env::temp_dir().join("c2_backing_test"),
                ..PoolConfig::default()
            },
            format!(
                "/cc3k{:04x}{:04x}{label}",
                std::process::id() as u16,
                sequence
            ),
        )
        .unwrap()
    }

    fn make_pool(label: &str) -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(make_mempool(label)))
    }

    #[test]
    fn borrowed_backing_does_not_block_other_allocations_or_releases() {
        use std::sync::{Barrier, mpsc};
        use std::time::Duration;

        for file in [false, true] {
            let budget = c2_mem::MemoryBudget::new(if file { 0 } else { 1 << 20 }, 8192, 8192);
            let pool = Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    make_mempool("short-copy").config().clone(),
                    "short-copy".into(),
                    budget.clone(),
                )
                .unwrap(),
            ));
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
            assert_eq!(charged_during_copy.reassembly.used_bytes, 4096);
            if file {
                assert_eq!(charged_during_copy.file.used_bytes, 4096);
            }
            backing.release();
            assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
            assert_eq!(budget.snapshot().file.used_bytes, 0);
        }
    }

    #[test]
    fn file_release_is_independent_of_another_carrier_borrow() {
        let budget = c2_mem::MemoryBudget::new(0, 8192, 8192);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                make_mempool("file-release").config().clone(),
                "file-release".into(),
                budget.clone(),
            )
            .unwrap(),
        ));
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
                assert_eq!(budget.snapshot().file.used_bytes, 4096);
                assert_eq!(budget.snapshot().reassembly.used_bytes, 4096);
            })
            .unwrap();
        {
            let _guard = pool.write();
            backing.release();
            other.release();
        }
        assert!(other.with_slice(|_| ()).is_err());
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn admission_guard_must_belong_to_carrier_pool_before_charging() {
        let make = || {
            let budget = c2_mem::MemoryBudget::new(0, 0, 0);
            let pool = Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    PoolConfig {
                        buddy_enabled: false,
                        spill_threshold: 0.0,
                        ..PoolConfig::default()
                    },
                    "admission-guard-pure".to_string(),
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
        assert!(error.contains("different pool"));
        for (pool, budget) in [(owner, owner_budget), (other, other_budget)] {
            let snapshot = budget.snapshot();
            assert_eq!(snapshot.reassembly.used_bytes, 0);
            assert_eq!(snapshot.reassembly.peak_bytes, 0);
            assert_eq!(snapshot.reassembly.rejected_allocations, 0);
            assert_eq!(snapshot.file.used_bytes, 0);
            assert_eq!(snapshot.shm.used_bytes, 0);
            assert_eq!(pool.read().stats().total_segments, 0);
            assert_eq!(pool.read().stats().dedicated_segments, 0);
        }
    }

    #[test]
    fn explicit_release_marks_carrier_released_and_idempotent() {
        let pool = make_pool("c");
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
        let pool = make_pool("empty");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let owner = std::mem::replace(&mut *pool.write(), make_mempool("empty"));
        drop(owner);
        backing.write_at(0, &[0x33; 512]).unwrap();
        assert_eq!(backing.copy_bytes().unwrap(), vec![0x33; 512]);
        assert_eq!(backing.with_slice(|bytes| bytes[0]).unwrap(), 0x33);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        backing.release();
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
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

    fn colliding_replacement_preserves_original_owner(buddy_enabled: bool) {
        // Identical labels and geometry still create distinct incarnations.
        let first = make_mempool("collision");
        // A size beyond the buddy block limit selects dedicated SHM.
        let size = if buddy_enabled { 512 } else { 128 * 1024 };
        let budget = first.budget().unwrap().clone();
        let pool = Arc::new(RwLock::new(first));
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, size).unwrap();
        assert_eq!(backing.is_buddy(), buddy_enabled);
        assert_eq!(backing.is_dedicated(), !buddy_enabled);
        backing.write_at(0, &vec![0x11; size]).unwrap();

        let mut replacement = make_mempool("collision");
        let replacement_budget = replacement.budget().unwrap().clone();
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
        let owner = std::mem::replace(&mut *pool.write(), replacement);

        assert_eq!(backing.copy_bytes().unwrap(), vec![0x11; size]);
        backing.write_at(0, &[0x44]).unwrap();
        assert_eq!(backing.with_slice(|bytes| bytes[0]).unwrap(), 0x44);
        assert_eq!(budget.snapshot().reassembly.used_bytes, size as u64);
        assert_eq!(owner.stats().alloc_count, 1);
        assert_eq!(
            pool.read().copy_handle_data(&fresh).unwrap(),
            vec![0x22; size]
        );
        assert_eq!(replacement_budget.snapshot().reassembly.used_bytes, 0);

        backing.release();
        assert!(backing.is_released());
        assert_eq!(owner.stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
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
        let budget = c2_mem::MemoryBudget::new(0, 4096, 4096);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    max_segments: 0,
                    min_retained_segments: 0,
                    max_dedicated_segments: 0,
                    ..PoolConfig::default()
                },
                "/cc3fileowner".into(),
                budget.clone(),
            )
            .unwrap(),
        ));
        let mut backing = ReassemblyBacking::admit(pool.clone(), 2, 256).unwrap();
        assert!(backing.is_file_spill());
        let path = backing.file_spill_path().unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        *pool.write() = make_mempool("file");
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
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        assert_eq!(budget.snapshot().file.used_bytes, 512);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
        backing.release();
        backing.release();
        assert!(backing.is_released());
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert!(!path.exists());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }
}
