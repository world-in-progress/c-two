//! Owned reassembly backing: one RAII carrier from admission to release.
//!
//! [`ReassemblyBacking`] is the single owner of one live chunk-reassembly
//! allocation. It bundles, in one move-only value:
//!
//! - the owner pool [`Arc`], which stays the only release authority for
//!   buddy/dedicated backing,
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
use parking_lot::RwLock;
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
        let capacity = checked_capacity(total_chunks, chunk_size)?;
        let capacity_bytes = u64::try_from(capacity).map_err(|_| {
            format!(
                "reassembly capacity {capacity} bytes exceeds the budget accounting range"
            )
        })?;
        // One pool write critical section covers charge and allocation so no
        // other allocator can consume the admitted bytes in between.
        let (handle, reservation) = {
            let mut guard = pool.write();
            let budget = guard
                .budget()
                .ok_or_else(|| "reassembly pool carries no owner budget context".to_string())?;
            let reservation = budget
                .reserve(BudgetKind::Reassembly, capacity_bytes)
                .map_err(|e| e.to_string())?;
            match guard.alloc_handle(capacity) {
                Ok(handle) => (handle, reservation),
                // The reservation guard drops with this error return and
                // refunds the charge; no backing was created.
                Err(e) => return Err(e),
            }
        };
        Ok(Self {
            pool,
            state: Some(BackingState {
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

    /// Logical data length (may be trimmed below allocated capacity).
    pub fn len(&self) -> usize {
        self.state
            .as_ref()
            .map(|s| s.handle.len())
            .unwrap_or(0)
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
        let handle = &mut self.live_state_mut()?.handle;
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| "reassembly write span overflows the platform range".to_string())?;
        if end > handle.len() {
            return Err(format!(
                "reassembly write span {offset}..{end} is outside the {} byte allocation",
                handle.len()
            ));
        }
        let guard = pool.read();
        let slice = guard.handle_slice_mut(handle);
        slice[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Copy the logical bytes out of the backing without releasing it.
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        let handle = &self.live_state()?.handle;
        self.pool
            .read()
            .copy_handle_data(handle)
            .map_err(|e| format!("reassembly backing copy failed: {e}"))
    }

    /// Run `f` over the backing's logical slice under the pool read lock.
    ///
    /// For raw-pointer extraction (buffer views); the borrow ends when `f`
    /// returns.
    pub fn with_slice<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, String> {
        let handle = &self.live_state()?.handle;
        let pool = self.pool.read();
        pool.validate_handle(handle)
            .map_err(|e| format!("reassembly backing access failed: {e}"))?;
        Ok(f(pool.handle_slice(handle)))
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

    fn release_storage(&mut self) -> Result<(), String> {
        let Some(state) = self.state.as_ref() else {
            return Ok(());
        };
        {
            let mut pool = self.pool.write();
            pool.validate_handle(&state.handle)
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
        // release fails here (corrupted coordinates), the reservation is
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
            format!("/cc3k{:04x}{:04x}{label}", std::process::id() as u16, sequence),
        )
    }

    fn make_pool(label: &str) -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(make_mempool(label)))
    }

    #[test]
    fn failed_release_preserves_carrier_state_for_retry() {
        let pool = make_pool("a");
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

        // Drop keeps the same ordering guarantee: the charge is deliberately
        // not refunded when the pool authority cannot confirm storage release.
        drop(backing);
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
}
