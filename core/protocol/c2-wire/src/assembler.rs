//! Chunked payload reassembly in fixed local storage.
//!
//! Provides a single Rust implementation that writes chunks directly into a unified
//! [`c2_mem::OwnedAllocation`] (buddy SHM, dedicated SHM, or file-backed mmap).
//! Its fixed mapping, release authority, and tracking-metadata charge
//! travel together in one [`ReassemblyBacking`] carrier from admission to release.

use std::sync::Arc;

use c2_mem::MemPool;
use parking_lot::{RwLock, RwLockWriteGuard};

use crate::chunk::ChunkAdmissionError;
use crate::chunk::backing::{ReassemblyBacking, checked_capacity};

/// Reassembles chunked payloads into a contiguous owned allocation.
///
/// # Lifecycle
/// ```text
/// new() → feed_chunk() × N → is_complete() → finish() → ReassemblyBacking
/// ```
///
/// Every exit path is ownership-safe: `finish()` on an incomplete assembly,
/// `abort()`, and plain drop all release storage through its fixed allocation
/// owner and then refund the tracking-metadata charge exactly once.
pub struct ChunkAssembler {
    total_chunks: usize,
    chunk_size: usize,
    received: usize,
    /// High-water mark: one past the last byte written.
    written_end: usize,
    backing: Arc<ReassemblyBacking>,
    /// 0 unseen, 1 reserved for a copy token, 2 committed.
    chunk_state: Vec<u8>,
    allocation_address: usize,
}

/// One move-only, checked write range. It pins the original backing and its
/// charge even if the registry removes the assembly during the copy.
pub struct PreparedChunk {
    backing: Arc<ReassemblyBacking>,
    allocation_address: usize,
    chunk_idx: usize,
    offset: usize,
    len: usize,
}

/// A completed write awaiting the assembler's short metadata commit.
pub struct CopiedChunk(PreparedChunk);

impl PreparedChunk {
    /// Copy into the token's exclusive range using the caller's source owner.
    pub fn copy_with(
        self,
        copy: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<CopiedChunk, String> {
        // SAFETY: preparation reserved a disjoint, initialized span of the
        // mapped backing, which stays alive through the token's Arc.
        let destination = unsafe {
            std::slice::from_raw_parts_mut(
                (self.allocation_address as *mut u8).add(self.offset),
                self.len,
            )
        };
        copy(destination)?;
        Ok(CopiedChunk(self))
    }

    pub fn copy_from(self, data: &[u8]) -> Result<CopiedChunk, String> {
        if data.len() != self.len {
            return Err(format!(
                "prepared chunk length {} differs from {}",
                data.len(),
                self.len
            ));
        }
        // SAFETY: prepare_chunk uniquely reserves this chunk index under the
        // assembler's exclusive borrow. Checked geometry yields disjoint spans;
        // the Arc pins the exact allocation, mapping and budget until commit or
        // drop. No safe reader of the unfinished backing is exposed.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.allocation_address as *mut u8).add(self.offset),
                data.len(),
            );
        }
        Ok(CopiedChunk(self))
    }
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
    /// and carries the role-domain budget. `total_chunks` and
    /// `chunk_size` come from the first chunk's header. The per-message byte
    /// limit and geometry are checked
    /// before any budget reservation or allocation.
    pub fn new(
        pool: Arc<RwLock<MemPool>>,
        total_chunks: usize,
        chunk_size: usize,
        max_payload_size: u64,
    ) -> Result<Self, ChunkAdmissionError> {
        let geometry = Self::check_geometry(total_chunks, chunk_size, max_payload_size)?;
        let guard = pool.write();
        Self::new_with_guard(Arc::clone(&pool), guard, geometry)
    }

    pub(crate) fn check_geometry(
        total_chunks: usize,
        chunk_size: usize,
        max_payload_size: u64,
    ) -> Result<ChunkGeometry, ChunkAdmissionError> {
        let capacity =
            checked_capacity(total_chunks, chunk_size).map_err(ChunkAdmissionError::Protocol)?;
        if u64::try_from(capacity).map_or(true, |capacity| capacity > max_payload_size) {
            return Err(ChunkAdmissionError::Capacity(format!(
                "reassembly size {capacity} exceeds limit {max_payload_size}"
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
        // Each tracking state occupies one byte. The backing reserves exactly
        // those metadata bytes against the role-domain memory cell and keeps
        // the charge for the backing's whole lifetime.
        let mut backing = ReassemblyBacking::admit_with_guard(
            pool,
            guard,
            total_chunks,
            chunk_size,
            total_chunks,
        )?;
        let chunk_state = backing
            .take_received_flags()
            .map_err(ChunkAdmissionError::Capacity)?;
        let allocation_address = backing
            .allocation_address()
            .map_err(ChunkAdmissionError::Capacity)?;
        Ok(Self {
            total_chunks,
            chunk_size,
            received: 0,
            written_end: 0,
            backing: Arc::new(backing),
            chunk_state,
            allocation_address,
        })
    }

    /// Feed a chunk into the assembler.
    ///
    /// Returns `true` when all chunks have been received.
    pub fn feed_chunk(&mut self, chunk_idx: usize, data: &[u8]) -> Result<bool, String> {
        let token = self.prepare_chunk(chunk_idx, data.len())?;
        let copied = token.copy_from(data)?;
        self.commit_chunk(copied)
    }

    /// Reserve one nonoverlapping copy span while the assembler is locked.
    pub fn prepare_chunk(&mut self, chunk_idx: usize, len: usize) -> Result<PreparedChunk, String> {
        if chunk_idx >= self.total_chunks {
            return Err(format!(
                "chunk_idx {chunk_idx} >= total_chunks {}",
                self.total_chunks
            ));
        }
        if self.chunk_state[chunk_idx] != 0 {
            return Err(format!("duplicate chunk_idx {chunk_idx}"));
        }
        if len > self.chunk_size {
            return Err(format!(
                "data length {} exceeds chunk_size {}",
                len, self.chunk_size
            ));
        }
        let offset = chunk_idx
            .checked_mul(self.chunk_size)
            .ok_or_else(|| "chunk offset overflow".to_string())?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| "chunk end overflow".to_string())?;
        if end > self.backing.len() {
            return Err("chunk span exceeds backing".into());
        }
        self.chunk_state[chunk_idx] = 1;
        Ok(PreparedChunk {
            backing: Arc::clone(&self.backing),
            allocation_address: self.allocation_address,
            chunk_idx,
            offset,
            len,
        })
    }

    /// Publish a completed copy. All bytes are in place before completion.
    pub fn commit_chunk(&mut self, copied: CopiedChunk) -> Result<bool, String> {
        let token = copied.0;
        if !Arc::ptr_eq(&self.backing, &token.backing)
            || self.chunk_state.get(token.chunk_idx) != Some(&1)
        {
            return Err("chunk token belongs to another or retired assembly".into());
        }
        self.chunk_state[token.chunk_idx] = 2;
        self.received += 1;
        let end = token.offset + token.len;
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

    /// The allocated payload capacity charged to the budget (the tracking
    /// metadata is charged separately, one byte per chunk).
    pub fn capacity_bytes(&self) -> u64 {
        self.backing.capacity_bytes()
    }

    /// Consume the assembler and return its owned backing carrier.
    ///
    /// The returned backing's logical length is trimmed to `written_end` (one
    /// past the last byte written), which may be less than the allocated and
    /// charged payload capacity if the last chunk was short. The backing also
    /// retains its chunk flags and their charge; trimming never refunds storage.
    ///
    /// On an incomplete assembly this returns `Err`; the assembler value
    /// drops on that return, releasing storage through its fixed allocation
    /// owner and refunding the tracking-metadata charge exactly once.
    pub fn finish(self) -> Result<ReassemblyBacking, String> {
        if !self.is_complete() {
            return Err(format!(
                "incomplete: {}/{} chunks",
                self.received, self.total_chunks
            ));
        }
        let written_end = self.written_end;
        let ChunkAssembler {
            backing,
            chunk_state,
            ..
        } = self;
        let mut backing = Arc::try_unwrap(backing)
            .map_err(|_| "in-flight chunk copy still owns backing".to_string())?;
        backing.restore_received_flags(chunk_state)?;
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

    /// One past the last byte written (actual data length).
    pub fn written_end(&self) -> usize {
        self.written_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::DomainBudget;
    use c2_mem::config::PoolConfig;

    static NEXT_POOL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn domain(memory_bytes: u64, file_bytes: u64) -> DomainBudget {
        DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: memory_bytes,
            file_spill_budget_bytes: file_bytes,
            max_outstanding_calls: 0,
        })
    }

    fn unique_prefix(tag: &str) -> String {
        let sequence = NEXT_POOL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "/cc3ab{:04x}{:04x}{tag}",
            std::process::id() as u16,
            sequence
        )
    }

    fn assembler_pool_config(spill_threshold: f64) -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            max_segments: 2,
            max_dedicated_segments: 2,
            buddy_idle_decay_secs: 0.0,
            spill_threshold,
            spill_dir: std::env::temp_dir().join("c2_asm_test"),
            ..PoolConfig::default()
        }
    }

    /// SHM-first pool (pressure heuristic disabled) with its own domain.
    fn test_pool_arc() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                assembler_pool_config(1.0),
                unique_prefix(""),
                domain(16 * (1 << 20), 1 << 20),
            )
            .unwrap(),
        ))
    }

    /// Pure file-spill pool: the threshold 0.0 denies every new SHM backing,
    /// so the file-spill cell tracks capacity bytes exactly and the memory
    /// cell tracks only the tracking metadata.
    fn file_pool_arc(memory_bytes: u64, file_bytes: u64) -> (Arc<RwLock<MemPool>>, DomainBudget) {
        let budget = domain(memory_bytes, file_bytes);
        (
            Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    assembler_pool_config(0.0),
                    unique_prefix("file"),
                    budget.clone(),
                )
                .unwrap(),
            )),
            budget,
        )
    }

    fn charged_bytes(budget: &DomainBudget) -> u64 {
        let snapshot = budget.snapshot();
        snapshot.memory.used_bytes + snapshot.file_spill.used_bytes
    }

    #[test]
    fn release_file_does_not_wait_for_public_pool_borrow() {
        let (pool, budget) = file_pool_arc(8192, 8192);
        let mut asm = ChunkAssembler::new(pool.clone(), 2, 4096, 8192).unwrap();
        assert!(asm.backing.is_file_spill());
        assert!(!asm.feed_chunk(0, &[27; 4096]).unwrap());
        assert_eq!(budget.snapshot().file_spill.used_bytes, 8192);
        assert_eq!(budget.snapshot().memory.used_bytes, 2);
        let _reader = pool.read();
        asm.abort();
        assert_eq!(budget.snapshot().file_spill.used_bytes, 0);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
    }

    #[test]
    fn test_single_chunk() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 1, 4096, 8 * (1 << 30)).unwrap();
        let data = b"hello world";
        let complete = asm.feed_chunk(0, data).unwrap();
        assert!(complete);
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), data.len());
        assert_eq!(&backing.copy_bytes().unwrap()[..data.len()], data);
        backing.release();
    }

    #[test]
    fn test_multi_chunk_in_order() {
        let (pool, budget) = file_pool_arc(1 << 20, 1 << 20);
        let mut asm = ChunkAssembler::new(pool.clone(), 3, 8, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(0, b"aaaaaaaa").unwrap()); // full
        assert!(!asm.feed_chunk(1, b"bbbbbbbb").unwrap()); // full
        assert!(asm.feed_chunk(2, b"cc").unwrap()); // last, short
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 18); // 8 + 8 + 2
        let slice = backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        // Trim happened but the charged capacity and metadata stay until release.
        assert_eq!(backing.capacity_bytes(), 24);
        assert_eq!(charged_bytes(&budget), 24 + 3);
        backing.release();
        assert_eq!(charged_bytes(&budget), 0);
    }

    #[test]
    fn prepared_ranges_copy_concurrently_and_commit_out_of_order() {
        use std::sync::Barrier;

        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool, 2, 4096, 8192).unwrap();
        let first = asm.prepare_chunk(0, 4096).unwrap();
        let second = asm.prepare_chunk(1, 4096).unwrap();
        assert!(asm.prepare_chunk(0, 4096).is_err());
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [(first, 0x11), (second, 0x22)]
            .into_iter()
            .map(|(token, byte)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    token.copy_from(&vec![byte; 4096]).unwrap()
                })
            })
            .collect();
        barrier.wait();
        let mut copies: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(!asm.commit_chunk(copies.pop().unwrap()).unwrap());
        assert!(asm.commit_chunk(copies.pop().unwrap()).unwrap());
        let backing = asm.finish().unwrap();
        let bytes = backing.copy_bytes().unwrap();
        assert_eq!(&bytes[..4096], &[0x11; 4096]);
        assert_eq!(&bytes[4096..], &[0x22; 4096]);
    }

    #[test]
    fn prepared_copy_keeps_original_charge_after_assembly_abort() {
        let (pool, budget) = file_pool_arc(8192, 8192);
        let mut asm = ChunkAssembler::new(pool, 2, 4096, 8192).unwrap();
        let token = asm.prepare_chunk(0, 4096).unwrap();
        asm.abort();
        assert_eq!(charged_bytes(&budget), 8192 + 2);
        let copied = token.copy_from(&[7; 4096]).unwrap();
        assert_eq!(charged_bytes(&budget), 8192 + 2);
        drop(copied);
        assert_eq!(charged_bytes(&budget), 0);
    }

    #[test]
    fn test_out_of_order() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 2, 8, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(1, b"second").unwrap()); // last, short
        assert!(asm.feed_chunk(0, b"firstttt").unwrap()); // full
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 14); // written_end = max(8+6, 0+8) = 14
        let slice = backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..8], b"firstttt");
        assert_eq!(&slice[8..14], b"second");
        backing.release();
    }

    #[test]
    fn test_empty_tiny_many_short_and_reverse_chunks() {
        let pool = test_pool_arc();
        for (total_chunks, chunk_size, last_size, reverse) in [
            (1, 1024, 0, false),
            (5, 1, 1, false),
            (100, 64, 30, false),
            (3, 4096, 1, false),
            (5, 256, 100, true),
        ] {
            let chunks: Vec<Vec<u8>> = (0..total_chunks)
                .map(|idx| {
                    let len = if idx + 1 == total_chunks {
                        last_size
                    } else {
                        chunk_size
                    };
                    vec![idx as u8; len]
                })
                .collect();
            let mut asm =
                ChunkAssembler::new(pool.clone(), total_chunks, chunk_size, 8 * (1 << 30)).unwrap();
            for step in 0..total_chunks {
                let idx = if reverse {
                    total_chunks - 1 - step
                } else {
                    step
                };
                assert_eq!(
                    asm.feed_chunk(idx, &chunks[idx]).unwrap(),
                    step + 1 == total_chunks
                );
                assert_eq!(asm.received(), step + 1);
            }
            let mut backing = asm.finish().unwrap();
            assert_eq!(backing.len(), (total_chunks - 1) * chunk_size + last_size);
            assert_eq!(backing.copy_bytes().unwrap(), chunks.concat());
            assert_eq!(backing.capacity_bytes(), (total_chunks * chunk_size) as u64);
            backing.release();
        }
    }

    #[test]
    fn test_duplicate_chunk_rejected() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool.clone(), 2, 8, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(0, b"data0000").unwrap());
        assert_eq!(asm.received(), 1);
        let err = asm.feed_chunk(0, b"dup").unwrap_err();
        assert!(err.contains("duplicate"));
        assert_eq!(asm.received(), 1);
        assert!(!asm.is_complete());
        assert!(asm.feed_chunk(1, b"tail").unwrap());
        assert_eq!(asm.received(), 2);
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.copy_bytes().unwrap(), b"data0000tail");
        backing.release();
    }

    #[test]
    fn test_abort_releases_handle_and_charge() {
        let budget = domain(1 << 20, 0);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    segment_size: 64 * 1024,
                    max_segments: 1,
                    min_retained_segments: 1,
                    max_dedicated_segments: 0,
                    spill_threshold: 1.0,
                    ..assembler_pool_config(1.0)
                },
                unique_prefix("abort"),
                budget.clone(),
            )
            .unwrap(),
        ));
        let mut asm = ChunkAssembler::new(pool.clone(), 4, 4096, 8 * (1 << 30)).unwrap();
        assert!(asm.backing.is_buddy());
        assert_eq!(pool.read().stats().alloc_count, 1);
        assert!(!asm.feed_chunk(0, &[7; 200]).unwrap());
        assert_eq!(asm.received(), 1);
        assert_eq!(asm.written_end(), 200);
        assert_eq!(asm.capacity_bytes(), 4 * 4096);
        // The memory cell carries the buddy segment's creation charge plus
        // the four tracking metadata bytes.
        let charged = budget.snapshot().memory.used_bytes;
        assert_eq!(charged, pool.read().stats().buddy_data_bytes + 4);
        asm.abort();
        assert_eq!(pool.read().stats().alloc_count, 0);
        // The retained idle segment keeps its creation charge; only the
        // metadata bytes were refunded with the assembly.
        assert_eq!(
            budget.snapshot().memory.used_bytes,
            charged - 4,
            "the tracking metadata must refund with the assembly"
        );
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
        assert_eq!(
            budget.snapshot().memory.used_bytes,
            charged - 4,
            "buddy block reuse takes no new charge"
        );
    }

    #[test]
    fn test_finish_incomplete_fails_and_returns_charge_once() {
        let (pool, budget) = file_pool_arc(1 << 20, 64 * 1024);
        let mut asm = ChunkAssembler::new(pool.clone(), 3, 8, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(0, b"data").unwrap();
        assert_eq!(charged_bytes(&budget), 24 + 3);
        let err = asm.finish().unwrap_err();
        assert!(err.contains("incomplete"));
        // The dropped incomplete assembler released storage and refunded.
        assert_eq!(charged_bytes(&budget), 0);
        // The pool remains usable for a fresh assembly.
        let fresh = ChunkAssembler::new(pool, 1, 4096, 8 * (1 << 30)).unwrap();
        fresh.abort();
    }

    #[test]
    fn test_zero_chunks_rejected() {
        let pool = test_pool_arc();
        let err = ChunkAssembler::new(pool, 0, 4096, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("total_chunks must be > 0"));
    }

    #[test]
    fn test_zero_chunk_size_rejected_before_charge_or_allocation() {
        let (pool, budget) = file_pool_arc(1 << 20, 64 * 1024);
        let err = ChunkAssembler::new(pool.clone(), 1, 0, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(err.contains("chunk_size must be > 0"));
        // No charge taken and no mapping created.
        let snap = budget.snapshot();
        assert_eq!(snap.memory.used_bytes, 0);
        assert_eq!(snap.memory.rejected_allocations, 0);
        assert_eq!(snap.file_spill.used_bytes, 0);
        assert_eq!(pool.read().stats().total_segments, 0);
        assert_eq!(pool.read().stats().dedicated_segments, 0);
        assert_eq!(pool.read().stats().file_spill_allocs, 0);
    }

    #[test]
    fn test_oversized_chunk_data() {
        let pool = test_pool_arc();
        let mut asm = ChunkAssembler::new(pool, 2, 8, 8 * (1 << 30)).unwrap();
        let err = asm.feed_chunk(0, &[0u8; 16]).unwrap_err();
        assert!(err.contains("exceeds chunk_size"));
        asm.abort();
    }

    #[test]
    fn budget_rejection_leaves_no_mapping_and_reports_cell_and_size() {
        let (pool, budget) = file_pool_arc(1 << 20, 100);
        let err = ChunkAssembler::new(pool.clone(), 2, 64, 8 * (1 << 30))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("file spill budget"),
            "error must name the cell: {err}"
        );
        assert!(err.contains("128"), "error must name the size: {err}");
        assert!(err.contains("100"), "error must name the limit: {err}");

        // The rejection happened before any mapping: no buddy or dedicated
        // segments, no file, and only rejection statistics moved. The
        // metadata charge was taken and refunded.
        let stats = pool.read().stats();
        assert_eq!(stats.total_segments, 0);
        assert_eq!(stats.dedicated_segments, 0);
        assert_eq!(stats.file_spill_allocs, 0);
        let snap = budget.snapshot();
        assert_eq!(snap.memory.used_bytes, 0);
        assert_eq!(snap.memory.peak_bytes, 2);
        assert_eq!(snap.memory.rejected_allocations, 0);
        assert_eq!(snap.file_spill.used_bytes, 0);
        assert_eq!(snap.file_spill.rejected_allocations, 1);
        assert_eq!(snap.file_spill.rejected_bytes, 128);
    }

    #[test]
    fn finished_backing_keeps_charge_and_pool_alive_after_creators_drop() {
        let (pool, budget) = file_pool_arc(1 << 20, 64 * 1024);
        let registry_view = pool.clone();
        let mut asm = ChunkAssembler::new(pool, 2, 8, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(0, b"firstttt").unwrap();
        asm.feed_chunk(1, b"sec").unwrap();
        let mut backing = asm.finish().unwrap();
        assert_eq!(backing.len(), 11);

        // The registry-equivalent owner (the outer pool Arc handle) is gone;
        // the carrier keeps the pool alive, the content readable, and the
        // full capacity and metadata charged.
        drop(registry_view);
        assert_eq!(charged_bytes(&budget), 16 + 2);
        assert_eq!(backing.copy_bytes().unwrap(), b"firsttttsec");
        // Idempotent release refunds exactly once.
        backing.release();
        backing.release();
        assert_eq!(charged_bytes(&budget), 0);
        assert!(backing.is_released());
    }
}
