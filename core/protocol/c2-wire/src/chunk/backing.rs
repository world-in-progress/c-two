//! Owned reassembly backing: one RAII carrier from admission to release.
//!
//! [`ReassemblyBacking`] is the single owner of one live chunk-reassembly
//! allocation. It bundles, in one move-only value:
//!
//! - the owner pool [`Arc`] and captured incarnation, checked together as
//!   the release authority for buddy/dedicated backing,
//! - the allocated [`MemHandle`] (file-spill mappings are self-owned by the
//!   handle value), and
//! - the [`BudgetReservation`] that charges the pool's canonical reassembly
//!   budget cell for the full allocated capacity.
//!
//! The reservation is taken **before** the pool allocates, and is refunded
//! only **after** the pool authority accepted the storage release (or the
//! file mapping closed). Ordinary error and drop paths therefore cannot
//! return budget bytes while backing storage remains allocated. Trimming the
//! logical length never changes the charge: the reservation covers allocated
//! capacity, not logical length.

use c2_mem::budget::{BudgetKind, BudgetReservation};
use c2_mem::{MemHandle, MemPool};
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
    handle: MemHandle,
    reservation: BudgetReservation,
    owner_incarnation: String,
}

impl BackingState {
    fn validate_access(&self, pool: &MemPool) -> Result<(), String> {
        // File mappings are held by the handle itself, independently of the
        // pool container. Only SHM coordinates require the original owner.
        if !self.handle.is_file_spill() {
            pool.validate_owner_incarnation(&self.owner_incarnation)?;
        }
        pool.validate_handle(&self.handle)
    }
}

/// Move-only RAII owner of one admitted live reassembly allocation.
///
/// Created only through [`ReassemblyBacking::admit`], which reserves the
/// reassembly budget before allocating. There is no way to detach the
/// reservation from the backing: callers hold the whole carrier or nothing,
/// so a released backing always refunds exactly once and an unreleased one
/// keeps its charge. The carrier owns its pool [`Arc`], so finished
/// reassemblies stay valid and charged even after their registry, connection,
/// or client is shut down and dropped.
///
/// Replacing the `MemPool` inside the shared lock does not transfer SHM
/// authority: access and release reject a different incarnation or a peer
/// cache. A failed explicit release remains retryable after the original
/// owner is restored; it never refunds the charge ahead of storage release.
pub struct ReassemblyBacking {
    pool: Arc<RwLock<MemPool>>,
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
        Self::admit_with_guard(Arc::clone(&pool), guard, total_chunks, chunk_size)
    }

    /// Admission with an already acquired write guard from this exact pool.
    /// Registry callers acquire it nonblockingly while holding their shard;
    /// this constructor never waits for the pool lock or enters the registry.
    pub(crate) fn admit_with_guard(
        pool: Arc<RwLock<MemPool>>,
        mut guard: RwLockWriteGuard<'_, MemPool>,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<Self, String> {
        if !std::ptr::eq(Arc::as_ptr(&pool), RwLockWriteGuard::rwlock(&guard)) {
            return Err("reassembly admission guard belongs to a different pool".to_string());
        }
        let capacity = checked_capacity(total_chunks, chunk_size)?;
        let capacity_bytes = u64::try_from(capacity).map_err(|_| {
            format!("reassembly capacity {capacity} bytes exceeds the budget accounting range")
        })?;
        // One pool write critical section covers charge and allocation so no
        // other allocator can consume the admitted bytes in between.
        let (handle, reservation, owner_incarnation) = {
            let budget = guard
                .budget()
                .ok_or_else(|| "reassembly pool carries no owner budget context".to_string())?;
            let reservation = budget
                .reserve(BudgetKind::Reassembly, capacity_bytes)
                .map_err(|e| e.to_string())?;
            match guard.alloc_handle(capacity) {
                Ok(handle) => (handle, reservation, guard.prefix().to_owned()),
                // The reservation guard drops with this error return and
                // refunds the charge; no backing was created.
                Err(e) => return Err(e),
            }
        };
        drop(guard);
        Ok(Self {
            pool,
            state: Some(BackingState {
                handle,
                reservation,
                owner_incarnation,
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

    /// The charged allocation capacity in bytes (never the trimmed length).
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
        match self.state.as_ref().map(|s| &s.handle) {
            Some(MemHandle::FileSpill { path, .. }) => Some(path.clone()),
            _ => None,
        }
    }

    /// Write chunk data at an absolute byte offset inside the allocation.
    ///
    /// The span must lie within the current logical length; during assembly
    /// that length equals the full allocated capacity.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        let pool = self.pool.clone();
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
        let guard = pool.read();
        state
            .validate_access(&guard)
            .map_err(|e| format!("reassembly backing write validation failed: {e}"))?;
        let slice = guard.handle_slice_mut(&mut state.handle);
        slice[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Copy the logical bytes out of the backing without releasing it.
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        let state = self.live_state()?;
        let pool = self.pool.read();
        state
            .validate_access(&pool)
            .map_err(|e| format!("reassembly backing copy validation failed: {e}"))?;
        pool.copy_handle_data(&state.handle)
            .map_err(|e| format!("reassembly backing copy failed: {e}"))
    }

    /// Run `f` over the backing's logical slice under the pool read lock.
    ///
    /// For raw-pointer extraction (buffer views); the borrow ends when `f`
    /// returns.
    pub fn with_slice<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, String> {
        let state = self.live_state()?;
        let pool = self.pool.read();
        state
            .validate_access(&pool)
            .map_err(|e| format!("reassembly backing access failed: {e}"))?;
        Ok(f(pool.handle_slice(&state.handle)))
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
        handle.set_len(new_len);
        Ok(())
    }

    /// Release the backing storage, then refund the reassembly charge.
    ///
    /// Idempotent. Buddy/dedicated storage is released through the pool
    /// authority (`free_at`), and a file-spill mapping closes when its handle
    /// value drops; the reservation refunds only after that succeeded. A
    /// failed release keeps the charge and the state so the caller may retry.
    pub fn release(&mut self) -> Result<(), String> {
        self.release_storage()
    }

    /// Try to release without waiting for the pool lock.
    ///
    /// `Ok(false)` leaves the entire carrier (storage, exact owner pool and
    /// reservation) unchanged. The caller must retain it for retry; dropping
    /// it would use the ordinary blocking RAII fallback. Validation and the
    /// physical release share the acquired write guard, just as in `release`.
    /// An error also preserves the live state instead of refunding its charge.
    pub fn try_release(&mut self) -> Result<bool, String> {
        if self.state.is_none() {
            return Ok(true);
        }
        let pool = Arc::clone(&self.pool);
        let Some(mut guard) = pool.try_write() else {
            return Ok(false);
        };
        self.release_storage_with_pool(&mut guard)?;
        Ok(true)
    }

    fn release_storage(&mut self) -> Result<(), String> {
        if self.state.is_none() {
            return Ok(());
        }
        let pool = Arc::clone(&self.pool);
        let mut guard = pool.write();
        self.release_storage_with_pool(&mut guard)
    }

    /// Both release entry points keep authority validation and mutation in
    /// this same pool critical section. Incarnation checks belong here too.
    fn release_storage_with_pool(&mut self, pool: &mut MemPool) -> Result<(), String> {
        let Some(state) = self.state.as_ref() else {
            return Ok(());
        };
        state
            .validate_access(pool)
            .map_err(|e| format!("reassembly release validation failed: {e}"))?;
        match &state.handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                allocation_size,
                ..
            } => {
                pool.free_at(
                    u32::from(*seg_idx),
                    *generation,
                    *offset,
                    *allocation_size,
                    false,
                )
                .map_err(|e| format!("reassembly backing release failed: {e}"))?;
            }
            MemHandle::Dedicated { seg_idx, len } => {
                let data_size = u32::try_from(*len).map_err(|_| {
                    "dedicated reassembly length exceeds the wire address space".to_string()
                })?;
                pool.free_at(u32::from(*seg_idx), 0, 0, data_size, true)
                    .map_err(|e| format!("reassembly backing release failed: {e}"))?;
            }
            MemHandle::FileSpill { .. } => {
                // Self-owned mapping: it closes when the handle value
                // drops below, before the reservation refunds.
            }
        }
        // Storage release accepted. Drop the handle value (closing any file
        // mapping) first, then refund exactly the charged capacity.
        let state = self.state.take().expect("live state checked above");
        drop(state.handle);
        drop(state.reservation);
        Ok(())
    }
}

impl Drop for ReassemblyBacking {
    fn drop(&mut self) {
        // Best-effort release with the same ordering guarantee: the charge
        // refunds only after the pool authority accepted the release. If the
        // release fails here (invalid handle or changed owner), the reservation is
        // deliberately forgotten rather than refunded — budget bytes stay
        // charged instead of silently uncounting backing that may still be
        // allocated.
        if self.release_storage().is_err() {
            if let Some(state) = self.state.take() {
                drop(state.handle);
                std::mem::forget(state.reservation);
            }
        }
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
                dedicated_crash_timeout_secs: 0.0,
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
    }

    fn make_pool(label: &str) -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(make_mempool(label)))
    }

    #[test]
    fn try_release_file_preserves_owner_under_live_carrier_callback() {
        use std::time::{Duration, Instant};

        let budget = c2_mem::MemoryBudget::new(0, 8192, 8192);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            make_mempool("try-release-file").config().clone(),
            format!("/cc3try{:08x}", std::process::id()),
            budget.clone(),
        )));
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 4096).unwrap();
        backing.write_at(0, &[21; 4096]).unwrap();
        let mut held = ReassemblyBacking::admit(Arc::clone(&pool), 1, 4096).unwrap();
        held.write_at(0, &[34; 4096]).unwrap();
        assert!(backing.is_file_spill() && held.is_file_spill());

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            held.with_slice(|bytes| {
                assert_eq!(bytes, &[34; 4096]);
                ready_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(1));
                assert_eq!(bytes, &[34; 4096]);
            })
            .unwrap();
            held
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let released = backing.try_release().unwrap();
        let elapsed = started.elapsed();
        let live = budget.snapshot();
        let _ = release_tx.send(());
        let mut held = holder.join().unwrap();
        assert!(!released);
        assert!(elapsed < Duration::from_millis(200), "{elapsed:?}");
        assert!(!backing.is_released());
        assert_eq!(backing.capacity_bytes(), 4096);
        assert_eq!(live.file.used_bytes, 8192);
        assert_eq!(live.reassembly.used_bytes, 8192);
        assert!(Arc::ptr_eq(&backing.pool, &pool));
        assert_eq!(backing.copy_bytes().unwrap(), &[21; 4096]);
        assert_eq!(held.copy_bytes().unwrap(), &[34; 4096]);

        assert!(backing.try_release().unwrap());
        assert!(backing.is_released());
        assert!(backing.copy_bytes().is_err());
        assert_eq!(budget.snapshot().file.used_bytes, 4096);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 4096);
        // Idempotent release needs no lock once the storage is gone.
        {
            let _guard = pool.write();
            assert!(backing.try_release().unwrap());
        }
        assert_eq!(held.copy_bytes().unwrap(), &[34; 4096]);
        held.release().unwrap();
        assert!(held.with_slice(|_| ()).is_err());
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn admission_guard_must_belong_to_carrier_pool_before_charging() {
        let make = || {
            let budget = c2_mem::MemoryBudget::new(0, 0, 0);
            let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    buddy_enabled: false,
                    spill_threshold: 0.0,
                    ..PoolConfig::default()
                },
                "admission-guard-pure".to_string(),
                budget.clone(),
            )));
            (pool, budget)
        };
        let (owner, owner_budget) = make();
        let (other, other_budget) = make();
        let error =
            ReassemblyBacking::admit_with_guard(owner.clone(), other.write(), 1, 128).unwrap_err();
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
    fn failed_release_preserves_carrier_state_for_retry() {
        let pool = make_pool("a");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, 512).unwrap();
        backing.write_at(0, &[3u8; 512]).unwrap();

        // Replace the pool authority under the carrier: the carrier's handle
        // coordinates are no longer valid, so release must fail.
        *pool.write() = make_mempool("b");

        let error = backing.release().unwrap_err();
        assert!(
            error.contains("validation"),
            "release failure must name the validation step: {error}"
        );
        // A failed release keeps the whole carrier: still live (not released),
        // still describing the same allocation, and still retryable with the
        // same error instead of silently uncounting the charge.
        assert!(!backing.is_released());
        assert_eq!(backing.len(), 512);
        assert_eq!(backing.capacity_bytes(), 512);
        assert!(backing.is_buddy());
        assert!(backing.release().unwrap_err().contains("validation"));
        assert!(!backing.is_released());
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);

        // Drop keeps the same ordering guarantee: the charge is deliberately
        // not refunded when the pool authority cannot confirm storage release.
        drop(backing);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
    }

    #[test]
    fn explicit_release_marks_carrier_released_and_idempotent() {
        let pool = make_pool("c");
        let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 2, 256).unwrap();
        backing.trim_to(300).unwrap();
        assert_eq!(backing.len(), 300);
        assert_eq!(backing.capacity_bytes(), 512);

        backing.release().unwrap();
        assert!(backing.is_released());
        assert_eq!(backing.len(), 0);
        assert_eq!(backing.capacity_bytes(), 0);
        assert!(!backing.is_buddy() && !backing.is_dedicated() && !backing.is_file_spill());
        assert!(backing.copy_bytes().is_err());
        assert!(backing.with_slice(|s| s.len()).is_err());
        // Idempotent, and no second refund.
        backing.release().unwrap();
        assert!(backing.is_released());
    }

    #[test]
    fn empty_replacement_write_returns_error_and_restored_owner_can_release() {
        let pool = make_pool("empty");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let owner = std::mem::replace(&mut *pool.write(), make_mempool("empty"));

        // This must return Err, not panic in handle_slice_mut's expect.
        assert!(backing.write_at(0, &[0x33; 512]).is_err());
        assert!(backing.copy_bytes().is_err());
        assert!(backing.with_slice(|_| panic!("unexpected access")).is_err());
        assert!(backing.release().unwrap_err().contains("validation"));
        assert!(!backing.is_released());
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);

        *pool.write() = owner;
        assert_eq!(backing.copy_bytes().unwrap(), vec![0x11; 512]);
        backing.release().unwrap();
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    fn coordinates(handle: &MemHandle) -> (u16, u32, u32) {
        match handle {
            MemHandle::Buddy {
                seg_idx,
                generation,
                offset,
                ..
            } => (*seg_idx, *generation, *offset),
            MemHandle::Dedicated { seg_idx, .. } => (*seg_idx, 0, 0),
            MemHandle::FileSpill { .. } => panic!("test requires SHM backing"),
        }
    }

    fn colliding_replacement_is_rejected(buddy_enabled: bool) {
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
            coordinates(&backing.live_state().unwrap().handle),
            coordinates(&fresh)
        );
        assert_ne!(pool.read().prefix(), replacement.prefix());
        let owner = std::mem::replace(&mut *pool.write(), replacement);

        let copy = backing.copy_bytes();
        assert!(
            copy.is_err(),
            "old carrier read replacement bytes: {:?}",
            copy.as_ref().map(|bytes| bytes.first().copied())
        );
        assert!(copy.unwrap_err().contains("owner"));
        assert!(backing.with_slice(|_| panic!("unexpected access")).is_err());
        assert!(backing.write_at(0, &vec![0x33; size]).is_err());
        for _ in 0..2 {
            assert!(backing.try_release().unwrap_err().contains("validation"));
            assert!(!backing.is_released());
            assert_eq!(budget.snapshot().reassembly.used_bytes, size as u64);
            assert!(backing.release().unwrap_err().contains("validation"));
            assert!(!backing.is_released());
            assert_eq!(backing.len(), size);
            assert_eq!(backing.capacity_bytes(), size as u64);
            assert_eq!(budget.snapshot().reassembly.used_bytes, size as u64);
            assert_eq!(owner.stats().alloc_count, 1);
            assert_eq!(pool.read().stats().alloc_count, 1);
            assert_eq!(
                pool.read().copy_handle_data(&fresh).unwrap(),
                vec![0x22; size]
            );
            assert_eq!(replacement_budget.snapshot().reassembly.used_bytes, 0);
        }

        // Restore the actual owner, then retry: no wrong free or early refund.
        let replacement = std::mem::replace(&mut *pool.write(), owner);
        assert_eq!(backing.copy_bytes().unwrap(), vec![0x11; size]);
        backing.write_at(0, &[0x44]).unwrap();
        assert_eq!(backing.with_slice(|s| s[0]).unwrap(), 0x44);
        backing.release().unwrap();
        assert!(backing.is_released());
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        backing.release().unwrap();
        *pool.write() = replacement;
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert_eq!(
            pool.read().copy_handle_data(&fresh).unwrap(),
            vec![0x22; size]
        );
        pool.write().release_handle(fresh);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn colliding_buddy_replacement_cannot_read_write_or_release() {
        colliding_replacement_is_rejected(true);
    }

    #[test]
    fn colliding_dedicated_replacement_cannot_read_write_or_release() {
        colliding_replacement_is_rejected(false);
    }

    #[test]
    fn peer_with_same_incarnation_cannot_replace_owner_authority() {
        let pool = make_pool("peer");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let prefix = pool.read().prefix().to_owned();
        let mut peer = MemPool::open_peer(PoolConfig::default(), prefix.clone());
        let (seg, generation, _) = coordinates(&backing.live_state().unwrap().handle);
        peer.ensure_peer_segment(u32::from(seg), generation, 512)
            .unwrap();
        // The peer has the same mapped bytes and valid coordinates, but it
        // cannot take the owner carrier's release authority.
        peer.validate_handle(&backing.live_state().unwrap().handle)
            .unwrap();
        let owner = std::mem::replace(&mut *pool.write(), peer);
        assert_eq!(pool.read().prefix(), prefix);
        assert!(backing.copy_bytes().is_err());
        assert!(backing.with_slice(|_| panic!("unexpected access")).is_err());
        assert!(backing.write_at(0, &[0x33]).is_err());
        assert!(backing.try_release().is_err());
        assert!(!backing.is_released());
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        assert!(backing.release().is_err());
        assert_eq!(owner.stats().alloc_count, 1);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        *pool.write() = owner;
        assert_eq!(backing.copy_bytes().unwrap(), vec![0x11; 512]);
        backing.release().unwrap();
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn drop_with_colliding_replacement_does_not_free_or_refund() {
        let pool = make_pool("drop");
        let budget = pool.read().budget().unwrap().clone();
        let mut backing = ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
        backing.write_at(0, &[0x11; 512]).unwrap();
        let mut replacement = make_mempool("drop");
        let mut fresh = replacement.alloc_handle(512).unwrap();
        replacement.handle_slice_mut(&mut fresh).fill(0x22);
        assert_eq!(
            coordinates(&backing.live_state().unwrap().handle),
            coordinates(&fresh)
        );
        let owner = std::mem::replace(&mut *pool.write(), replacement);

        drop(backing);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
        assert_eq!(owner.stats().alloc_count, 1);
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert_eq!(
            pool.read().copy_handle_data(&fresh).unwrap(),
            vec![0x22; 512]
        );
        pool.write().release_handle(fresh);
        assert_eq!(pool.read().stats().alloc_count, 0);
        // Dropping the detached owner closes its SHM. The failed carrier drop
        // intentionally cannot promise a refund: its charge remains retained.
        drop(owner);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
    }

    #[test]
    fn self_owned_file_survives_pool_replacement_and_releases_its_charge() {
        // Pure file path: no SHM seam is attempted, including under sandboxing.
        let budget = c2_mem::MemoryBudget::new(0, 4096, 4096);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            PoolConfig {
                max_segments: 0,
                min_retained_segments: 0,
                max_dedicated_segments: 0,
                ..PoolConfig::default()
            },
            "/cc3fileowner".into(),
            budget.clone(),
        )));
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
        backing.release().unwrap();
        backing.release().unwrap();
        assert!(backing.is_released());
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert!(!path.exists());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }
}
