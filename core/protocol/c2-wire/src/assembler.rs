//! Chunked payload reassembly backed by [`MemHandle`].
//!
//! Provides a single Rust implementation that writes chunks directly into a unified
//! [`MemHandle`] (buddy SHM, dedicated SHM, or file-backed mmap). The allocated
//! backing, its owner pool, and its reassembly budget reservation travel together
//! in one [`ReassemblyBacking`] carrier from admission to release.

use std::sync::Arc;

use c2_mem::MemPool;
use parking_lot::{RwLock, RwLockWriteGuard};

use crate::chunk::ChunkAdmissionError;
use crate::chunk::backing::{ReassemblyBacking, checked_capacity};

/// Reassembles chunked payloads into a contiguous [`MemHandle`].
///
/// # Lifecycle
/// ```text
/// new() → feed_chunk() × N → is_complete() → finish() → ReassemblyBacking
/// ```
///
/// Every exit path is ownership-safe: `finish()` on an incomplete assembly,
/// `abort()`, and plain drop all release the backing storage through the pool
/// authority and then refund the reassembly budget charge exactly once.
pub struct ChunkAssembler {
    total_chunks: usize,
    chunk_size: usize,
    received: usize,
    /// High-water mark: one past the last byte written.
    written_end: usize,
    backing: ReassemblyBacking,
    received_flags: Vec<bool>,
    /// Route name extracted from the first chunk (server-side).
    pub route_name: Option<String>,
    /// Method index extracted from the first chunk (server-side).
    pub method_idx: Option<u16>,
}

/// Checked immutable geometry. Only the canonical checks can create this token.
pub(crate) struct ChunkGeometry {
    total_chunks: usize,
    chunk_size: usize,
}

impl std::fmt::Debug for ChunkAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkAssembler")
            .field("total_chunks", &self.total_chunks)
            .field("received", &self.received)
            .field("written_end", &self.written_end)
            .finish()
    }
}

impl ChunkAssembler {
    /// Create a new assembler.
    ///
    /// `pool` is the shared owner pool that allocates the reassembly buffer
    /// and carries the canonical reassembly budget. `total_chunks` and
    /// `chunk_size` come from the first chunk's header. Per-message limits
    /// (`max_total_chunks`, `max_reassembly_bytes`) and geometry are checked
    /// before any budget reservation or allocation.
    pub fn new(
        pool: Arc<RwLock<MemPool>>,
        total_chunks: usize,
        chunk_size: usize,
        max_total_chunks: usize,
        max_reassembly_bytes: usize,
    ) -> Result<Self, ChunkAdmissionError> {
        let geometry = Self::check_geometry(
            total_chunks,
            chunk_size,
            max_total_chunks,
            max_reassembly_bytes,
        )?;
        let guard = pool.write();
        Self::new_with_guard(Arc::clone(&pool), guard, geometry)
    }

    pub(crate) fn check_geometry(
        total_chunks: usize,
        chunk_size: usize,
        max_total_chunks: usize,
        max_reassembly_bytes: usize,
    ) -> Result<ChunkGeometry, ChunkAdmissionError> {
        if total_chunks > max_total_chunks {
            return Err(ChunkAdmissionError::Protocol(format!(
                "total_chunks {total_chunks} exceeds limit {max_total_chunks}"
            )));
        }
        let capacity =
            checked_capacity(total_chunks, chunk_size).map_err(ChunkAdmissionError::Protocol)?;
        if capacity > max_reassembly_bytes {
            return Err(ChunkAdmissionError::Protocol(format!(
                "reassembly size {capacity} exceeds limit {max_reassembly_bytes}"
            )));
        }
        Ok(ChunkGeometry {
            total_chunks,
            chunk_size,
        })
    }

    /// Allocate from an already acquired pool guard; no pool lock wait occurs.
    pub(crate) fn new_with_guard(
        pool: Arc<RwLock<MemPool>>,
        guard: RwLockWriteGuard<'_, MemPool>,
        geometry: ChunkGeometry,
    ) -> Result<Self, ChunkAdmissionError> {
        let ChunkGeometry {
            total_chunks,
            chunk_size,
        } = geometry;
        let backing = ReassemblyBacking::admit_with_guard(pool, guard, total_chunks, chunk_size)
            .map_err(ChunkAdmissionError::Capacity)?;
        Ok(Self {
            total_chunks,
            chunk_size,
            received: 0,
            written_end: 0,
            backing,
            received_flags: vec![false; total_chunks],
            route_name: None,
            method_idx: None,
        })
    }

    /// Feed a chunk into the assembler.
    ///
    /// Returns `true` when all chunks have been received.
    pub fn feed_chunk(&mut self, chunk_idx: usize, data: &[u8]) -> Result<bool, String> {
        if chunk_idx >= self.total_chunks {
            return Err(format!(
                "chunk_idx {chunk_idx} >= total_chunks {}",
                self.total_chunks
            ));
        }
        if self.received_flags[chunk_idx] {
            return Err(format!("duplicate chunk_idx {chunk_idx}"));
        }
        if data.len() > self.chunk_size {
            return Err(format!(
                "data length {} exceeds chunk_size {}",
                data.len(),
                self.chunk_size
            ));
        }
        let offset = chunk_idx * self.chunk_size;
        self.backing.write_at(offset, data)?;
        self.received_flags[chunk_idx] = true;
        self.received += 1;
        let end = offset + data.len();
        if end > self.written_end {
            self.written_end = end;
        }
        Ok(self.received == self.total_chunks)
    }

    /// Check if all chunks have been received.
    pub fn is_complete(&self) -> bool {
        self.received == self.total_chunks
    }

    /// Number of chunks received so far.
    pub fn received(&self) -> usize {
        self.received
    }

    /// The allocated (and budget-charged) capacity in bytes.
    pub fn capacity_bytes(&self) -> u64 {
        self.backing.capacity_bytes()
    }

    /// Consume the assembler and return its owned backing carrier.
    ///
    /// The returned backing's logical length is trimmed to `written_end` (one
    /// past the last byte written), which may be less than the allocated and
    /// charged `total_chunks × chunk_size` capacity if the last chunk was
    /// short. The trim never changes the charge.
    ///
    /// On an incomplete assembly this returns `Err`; the assembler value
    /// drops on that return, releasing the backing storage through the pool
    /// authority and refunding the reassembly charge exactly once.
    pub fn finish(self) -> Result<ReassemblyBacking, String> {
        if !self.is_complete() {
            return Err(format!(
                "incomplete: {}/{} chunks",
                self.received, self.total_chunks
            ));
        }
        let written_end = self.written_end;
        let ChunkAssembler { mut backing, .. } = self;
        backing.trim_to(written_end)?;
        Ok(backing)
    }

    /// Abort reassembly, releasing the backing and its budget charge.
    ///
    /// Equivalent to dropping the assembler; kept as an explicit lifecycle
    /// marker for callers.
    pub fn abort(self) {
        drop(self)
    }

    /// Try to release without consuming the assembler on pool contention.
    ///
    /// Cleanup owners retain this assembler on `Ok(false)` or an error and
    /// retry it later. Only a successful release permits removing the entry;
    /// its subsequent Drop then has no storage or reservation left to release.
    pub fn try_release(&mut self) -> Result<bool, String> {
        self.backing.try_release()
    }

    /// One past the last byte written (actual data length).
    pub fn written_end(&self) -> usize {
        self.written_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::MemoryBudget;
    use c2_mem::budget::BudgetKind;
    use c2_mem::config::PoolConfig;

    fn test_pool_arc() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(MemPool::new(PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_asm_test"),
            ..PoolConfig::default()
        })))
    }

    fn budgeted_pool_arc(reassembly_limit: u64) -> (Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        (
            Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    segment_size: 64 * 1024,
                    min_block_size: 4096,
                    max_segments: 2,
                    max_dedicated_segments: 2,
                    dedicated_crash_timeout_secs: 0.0,
                    buddy_idle_decay_secs: 0.0,
                    spill_threshold: 1.0,
                    spill_dir: std::env::temp_dir().join("c2_asm_test_budget"),
                    ..PoolConfig::default()
                },
                format!("/cc3ab{:08x}", std::process::id()),
                budget.clone(),
            ))),
            budget,
        )
    }

    #[test]
    fn try_release_file_keeps_partial_assembly_for_retry() {
        let budget = MemoryBudget::new(0, 8192, 8192);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            PoolConfig {
                spill_threshold: 0.0,
                ..PoolConfig::default()
            },
            "assembler-try-release-file".into(),
            budget.clone(),
        )));
        let mut asm = ChunkAssembler::new(Arc::clone(&pool), 2, 4096, 2, 8192).unwrap();
        assert!(asm.backing.is_file_spill());
        assert!(!asm.feed_chunk(0, &[27; 4096]).unwrap());
        {
            let _reader = pool.read();
            assert!(!asm.try_release().unwrap());
            assert_eq!(asm.received(), 1);
            assert_eq!(asm.written_end(), 4096);
            assert_eq!(asm.capacity_bytes(), 8192);
            assert_eq!(budget.snapshot().file.used_bytes, 8192);
            assert_eq!(budget.snapshot().reassembly.used_bytes, 8192);
        }
        // Failed cleanup left data and chunk geometry intact, rather than
        // consuming the assembler or silently refunding an unfinished owner.
        assert!(asm.feed_chunk(1, &[38; 1024]).unwrap());
        assert_eq!(&asm.backing.copy_bytes().unwrap()[..4096], &[27; 4096]);
        assert_eq!(&asm.backing.copy_bytes().unwrap()[4096..5120], &[38; 1024]);
        assert!(asm.try_release().unwrap());
        assert_eq!(asm.capacity_bytes(), 0);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        let _writer = pool.write();
        assert!(asm.try_release().unwrap());
        drop(asm); // successful cleanup's Drop must not reacquire the pool
    }

    #[test]
    fn test_single_chunk() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 1, 4096, 512, 8 * (1 << 30)).unwrap();
        let data = b"hello world";
        let complete = asm.feed_chunk(0, data).unwrap();
        assert!(complete);
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), data.len());
        assert_eq!(&backing.copy_bytes().unwrap()[..data.len()], data);
        backing.release().unwrap();
    }

    #[test]
    fn test_multi_chunk_in_order() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 3, 8, 512, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(0, b"aaaaaaaa").unwrap()); // full
        assert!(!asm.feed_chunk(1, b"bbbbbbbb").unwrap()); // full
        assert!(asm.feed_chunk(2, b"cc").unwrap()); // last, short
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 18); // 8 + 8 + 2
        let slice = backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        // Trim happened but the full capacity stays charged until release.
        assert_eq!(backing.capacity_bytes(), 24);
        backing.release().unwrap();
    }

    #[test]
    fn test_out_of_order() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 2, 8, 512, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(1, b"second").unwrap()); // last, short
        assert!(asm.feed_chunk(0, b"firstttt").unwrap()); // full
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 14); // written_end = max(8+6, 0+8) = 14
        let slice = backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..8], b"firstttt");
        assert_eq!(&slice[8..14], b"second");
        backing.release().unwrap();
    }

    #[test]
    fn test_duplicate_chunk_rejected() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 2, 8, 512, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(0, b"data").unwrap();
        let err = asm.feed_chunk(0, b"dup").unwrap_err();
        assert!(err.contains("duplicate"));
        asm.abort();
    }

    #[test]
    fn test_abort_releases_handle_and_charge() {
        let budget = MemoryBudget::new(1 << 20, 0, 64 * 1024);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                min_retained_segments: 1,
                max_dedicated_segments: 0,
                spill_threshold: 1.0,
                ..PoolConfig::default()
            },
            format!("/cc3abort{:08x}", std::process::id()),
            budget.clone(),
        )));
        let asm = ChunkAssembler::new(pool.clone(), 4, 4096, 512, 8 * (1 << 30)).unwrap();
        assert!(asm.backing.is_buddy());
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert_eq!(
            budget.snapshot().cell(BudgetKind::Reassembly).used_bytes,
            4 * 4096
        );
        asm.abort();
        assert_eq!(budget.snapshot().cell(BudgetKind::Reassembly).used_bytes, 0);
        assert_eq!(pool.read().stats().alloc_count, 0);
        // The only 64 KiB buddy remains mapped. No extra segment, dedicated
        // allocation, or file fallback may hide an unreleased assembly block.
        let before = pool.read().stats();
        assert_eq!(before.total_segments, 1);
        assert_eq!(before.buddy_data_bytes, 64 * 1024);
        let mut guard = pool.write();
        let handle = guard.alloc_handle(64 * 1024).unwrap();
        assert!(handle.is_buddy());
        let after = guard.stats();
        assert_eq!(after.total_segments, 1);
        assert_eq!(after.buddy_expanded_allocs, before.buddy_expanded_allocs);
        assert_eq!(after.dedicated_allocs, 0);
        assert_eq!(after.file_spill_allocs, 0);
        guard.release_handle(handle);
        assert_eq!(guard.stats().alloc_count, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn test_finish_incomplete_fails_and_returns_charge_once() {
        let (pool, budget) = budgeted_pool_arc(64 * 1024);
        let mut asm = ChunkAssembler::new(pool.clone(), 3, 8, 512, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(0, b"data").unwrap();
        assert_eq!(
            budget.snapshot().cell(BudgetKind::Reassembly).used_bytes,
            24
        );
        let err = asm.finish().unwrap_err();
        assert!(err.contains("incomplete"));
        // The dropped incomplete assembler released storage and refunded.
        assert_eq!(budget.snapshot().cell(BudgetKind::Reassembly).used_bytes, 0);
        // The pool remains usable for a fresh assembly.
        let fresh = ChunkAssembler::new(pool, 1, 4096, 512, 8 * (1 << 30)).unwrap();
        fresh.abort();
    }

    #[test]
    fn test_zero_chunks_rejected() {
        let pool = test_pool_arc();
        let err = ChunkAssembler::new(pool, 0, 4096, 512, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("total_chunks must be > 0"));
    }

    #[test]
    fn test_zero_chunk_size_rejected_before_charge_or_allocation() {
        let (pool, budget) = budgeted_pool_arc(64 * 1024);
        let err = ChunkAssembler::new(pool.clone(), 1, 0, 512, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("chunk_size must be > 0"));
        // No charge taken and no mapping created.
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert_eq!(snap.reassembly.rejected_allocations, 0);
        assert_eq!(pool.read().stats().total_segments, 0);
        assert_eq!(pool.read().stats().dedicated_segments, 0);
    }

    #[test]
    fn test_oversized_chunk_data() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool, 2, 8, 512, 8 * (1 << 30)).unwrap();
        let err = asm.feed_chunk(0, &[0u8; 16]).unwrap_err();
        assert!(err.contains("exceeds chunk_size"));
        asm.abort();
    }

    #[test]
    fn budget_rejection_leaves_no_mapping_and_reports_cell_and_size() {
        let (pool, budget) = budgeted_pool_arc(100);
        let err = ChunkAssembler::new(pool.clone(), 2, 64, 512, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'reassembly'"),
            "error must name the cell: {err}"
        );
        assert!(err.contains("128"), "error must name the size: {err}");
        assert!(err.contains("100"), "error must name the limit: {err}");

        // The rejection happened before any mapping: no buddy or dedicated
        // segments, and only rejection statistics moved.
        let stats = pool.read().stats();
        assert_eq!(stats.total_segments, 0);
        assert_eq!(stats.dedicated_segments, 0);
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert_eq!(snap.reassembly.rejected_allocations, 1);
        assert_eq!(snap.reassembly.rejected_bytes, 128);
    }

    #[test]
    fn finished_backing_keeps_charge_and_pool_alive_after_creators_drop() {
        let (pool, budget) = budgeted_pool_arc(64 * 1024);
        let registry_view = pool.clone();
        let mut asm = ChunkAssembler::new(pool, 2, 8, 512, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(0, b"firstttt").unwrap();
        asm.feed_chunk(1, b"sec").unwrap();
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 11);

        // The registry-equivalent owner (the outer pool Arc handle) is gone;
        // the carrier keeps the pool alive, the content readable, and the
        // full capacity charged.
        drop(registry_view);
        assert_eq!(
            budget.snapshot().cell(BudgetKind::Reassembly).used_bytes,
            16
        );
        assert_eq!(backing.copy_bytes().unwrap(), b"firsttttsec");
        // Idempotent release refunds exactly once.
        backing.release().unwrap();
        backing.release().unwrap();
        assert_eq!(budget.snapshot().cell(BudgetKind::Reassembly).used_bytes, 0);
        assert!(backing.is_released());
    }
}
