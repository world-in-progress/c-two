//! Hierarchical bitmap for buddy allocator levels.
//!
//! Each level has a bitmap tracking free/used blocks. The bitmap is stored
//! as an array of u64 words for efficient scanning via trailing_zeros().
//!
//! # Safety
//!
//! All bitmap operations (`alloc_one`, `free_one`, `mark_used`) **MUST** be
//! called while holding the segment's `ShmSpinlock`. The spinlock is the
//! actual synchronization primitive that guarantees mutual exclusion across
//! processes.
//!
//! `alloc_one` uses CAS internally for historical reasons (it predates the
//! spinlock design), but the spinlock is the authoritative guard. Do **NOT**
//! call bitmap methods outside the spinlock critical section.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bitmap for a single buddy level, stored in shared memory.
///
/// Layout in SHM: array of AtomicU64 words, each tracking 64 blocks.
/// Bit = 1 means FREE, bit = 0 means USED (or non-existent).
///
/// # Safety
///
/// All methods on `LevelBitmap` assume the caller holds the segment's
/// [`ShmSpinlock`](crate::alloc::spinlock::ShmSpinlock). The atomic operations
/// provide cross-process visibility (shared-memory coherence), **not**
/// lock-free concurrency — concurrent unsynchronized access is undefined
/// behavior at the allocator level.
pub struct LevelBitmap {
    /// Pointer to the first u64 word in SHM.
    base: *mut AtomicU64,
    /// Number of u64 words in this level's bitmap.
    num_words: usize,
    /// Total number of blocks at this level.
    num_blocks: usize,
}

unsafe impl Send for LevelBitmap {}
unsafe impl Sync for LevelBitmap {}

impl LevelBitmap {
    /// Create a new LevelBitmap pointing at SHM memory.
    ///
    /// # Safety
    /// `base` must point to valid, properly aligned SHM memory that will
    /// outlive this struct. The memory must be at least `num_words * 8` bytes.
    pub unsafe fn new(base: *mut u8, num_blocks: usize) -> Self {
        let num_words = (num_blocks + 63) / 64;
        Self {
            base: base as *mut AtomicU64,
            num_words,
            num_blocks,
        }
    }

    /// Initialize all blocks as free (set bits to 1).
    /// Only valid blocks are marked; trailing bits in the last word are 0.
    pub fn init_all_free(&self) {
        let full_words = self.num_blocks / 64;
        let remainder = self.num_blocks % 64;

        for i in 0..full_words {
            self.word(i).store(u64::MAX, Ordering::Release);
        }
        if remainder > 0 {
            // Only set `remainder` bits in the last word.
            let mask = (1u64 << remainder) - 1;
            self.word(full_words).store(mask, Ordering::Release);
        }
        // Zero out any extra words (shouldn't exist if sized correctly).
        for i in (full_words + if remainder > 0 { 1 } else { 0 })..self.num_words {
            self.word(i).store(0, Ordering::Release);
        }
    }

    /// Find and claim a free block. Returns the block index, or None.
    /// Uses atomic CAS to ensure cross-process safety.
    pub fn alloc_one(&self) -> Option<usize> {
        for word_idx in 0..self.num_words {
            let w = self.word(word_idx);
            loop {
                let val = w.load(Ordering::Acquire);
                if val == 0 {
                    break; // No free bits in this word.
                }
                let bit = val.trailing_zeros() as usize;
                let block_idx = word_idx * 64 + bit;
                if block_idx >= self.num_blocks {
                    break; // Trailing bit beyond valid range.
                }
                let new_val = val & !(1u64 << bit);
                if w.compare_exchange_weak(val, new_val, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Some(block_idx);
                }
                // CAS failed → retry this word (another process grabbed it).
            }
        }
        None
    }

    /// Mark a specific block as free.
    pub fn free_one(&self, block_idx: usize) {
        debug_assert!(block_idx < self.num_blocks);
        let word_idx = block_idx / 64;
        let bit = block_idx % 64;
        self.word(word_idx).fetch_or(1u64 << bit, Ordering::Release);
    }

    /// Check if a specific block is free.
    pub fn is_free(&self, block_idx: usize) -> bool {
        debug_assert!(block_idx < self.num_blocks);
        let word_idx = block_idx / 64;
        let bit = block_idx % 64;
        (self.word(word_idx).load(Ordering::Acquire) >> bit) & 1 == 1
    }

    /// Mark a specific block as used (clear the bit).
    pub fn mark_used(&self, block_idx: usize) {
        debug_assert!(block_idx < self.num_blocks);
        let word_idx = block_idx / 64;
        let bit = block_idx % 64;
        self.word(word_idx).fetch_and(!(1u64 << bit), Ordering::Release);
    }

    /// Number of words in this bitmap.
    pub fn word_count(&self) -> usize {
        self.num_words
    }

    /// Total bytes needed for this bitmap.
    pub fn byte_size(&self) -> usize {
        self.num_words * 8
    }

    /// Total number of blocks at this level.
    pub fn block_count(&self) -> usize {
        self.num_blocks
    }

    fn word(&self, idx: usize) -> &AtomicU64 {
        debug_assert!(idx < self.num_words);
        unsafe { &*self.base.add(idx) }
    }
}

/// Calculate total bitmap bytes needed for all levels.
pub fn total_bitmap_bytes(segment_data_size: usize, min_block: usize) -> usize {
    let mut bytes = 0usize;
    let mut block_size = segment_data_size;
    while block_size >= min_block {
        let num_blocks = segment_data_size / block_size;
        let num_words = (num_blocks + 63) / 64;
        bytes += num_words * 8;
        block_size /= 2;
    }
    bytes
}

/// Calculate number of levels for given segment data size and min block.
pub fn num_levels(segment_data_size: usize, min_block: usize) -> usize {
    let mut levels = 0;
    let mut block_size = segment_data_size;
    while block_size >= min_block {
        levels += 1;
        block_size /= 2;
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    fn make_bitmap(num_blocks: usize) -> (LevelBitmap, *mut u8, Layout) {
        let num_words = (num_blocks + 63) / 64;
        let layout = Layout::from_size_align(num_words * 8, 8).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        let bm = unsafe { LevelBitmap::new(ptr, num_blocks) };
        (bm, ptr, layout)
    }

    #[test]
    fn test_init_and_alloc() {
        let (bm, ptr, layout) = make_bitmap(4);
        bm.init_all_free();
        assert!(bm.is_free(0));
        assert!(bm.is_free(3));
        let idx = bm.alloc_one().unwrap();
        assert_eq!(idx, 0);
        assert!(!bm.is_free(0));
        unsafe { dealloc(ptr, layout) };
    }

    #[test]
    fn test_alloc_all_then_fail() {
        let (bm, ptr, layout) = make_bitmap(3);
        bm.init_all_free();
        assert_eq!(bm.alloc_one(), Some(0));
        assert_eq!(bm.alloc_one(), Some(1));
        assert_eq!(bm.alloc_one(), Some(2));
        assert_eq!(bm.alloc_one(), None);
        unsafe { dealloc(ptr, layout) };
    }

    #[test]
    fn test_free_and_realloc() {
        let (bm, ptr, layout) = make_bitmap(2);
        bm.init_all_free();
        let a = bm.alloc_one().unwrap();
        let b = bm.alloc_one().unwrap();
        assert_eq!(bm.alloc_one(), None);
        bm.free_one(a);
        assert_eq!(bm.alloc_one(), Some(a));
        bm.free_one(b);
        assert_eq!(bm.alloc_one(), Some(b));
        unsafe { dealloc(ptr, layout) };
    }

    #[test]
    fn test_large_bitmap() {
        let (bm, ptr, layout) = make_bitmap(256);
        bm.init_all_free();
        for i in 0..256 {
            let idx = bm.alloc_one().unwrap();
            assert_eq!(idx, i);
        }
        assert_eq!(bm.alloc_one(), None);
        // Free all even blocks.
        for i in (0..256).step_by(2) {
            bm.free_one(i);
        }
        for i in (0..256).step_by(2) {
            assert_eq!(bm.alloc_one(), Some(i));
        }
        unsafe { dealloc(ptr, layout) };
    }

    #[test]
    fn test_bitmap_size_calculations() {
        // 256MB data, 4KB min block → 65536 min blocks, 16 levels
        let data_size = 256 * 1024 * 1024;
        let min_blk = 4096;
        let levels = num_levels(data_size, min_blk);
        assert_eq!(levels, 17); // 256MB, 128MB, ..., 4KB = 17 levels
        let total = total_bitmap_bytes(data_size, min_blk);
        // Level 0: 1 block → 1 word = 8B
        // Level 1: 2 blocks → 1 word = 8B
        // ...
        // Level 16: 65536 blocks → 1024 words = 8192B
        // Total ~16KB
        assert!(total > 0 && total <= 32768);
    }
}
