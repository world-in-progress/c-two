//! Buddy allocator operating on a single SHM segment.
//!
//! The allocator manages a power-of-two data region using hierarchical bitmaps.
//! Each level represents blocks of a specific size (level 0 = full segment,
//! level N = min_block_size). Allocation rounds up to the nearest power of two
//! and searches the appropriate level. Free merges buddy pairs recursively.

use crate::bitmap::LevelBitmap;
use crate::spinlock::ShmSpinlock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Magic number for segment validation.
pub const SEGMENT_MAGIC: u32 = 0xCC20_0001;
/// Current allocator version.
pub const SEGMENT_VERSION: u16 = 1;
/// Header is page-aligned (4KB).
pub const HEADER_ALIGN: usize = 4096;

/// SHM segment header layout (stored at offset 0 of the SHM region).
///
/// ```text
/// Offset  Size  Field
/// 0       4     magic (u32)
/// 4       2     version (u16)
/// 6       2     _pad0 (u16)
/// 8       8     total_size (u64) - total SHM size including header
/// 16      4     data_offset (u32) - page-aligned start of data region
/// 20      4     min_block (u32)
/// 24      2     max_levels (u16)
/// 26      2     _pad1 (u16)
/// 28      4     spinlock (u32) - atomic
/// 32      4     alloc_count (u32) - atomic, active allocations
/// 36      4     _pad2 (u32)
/// 40      8     free_bytes (u64) - atomic, available bytes
/// 48      ...   bitmap data (variable length, tightly packed)
/// ```
#[repr(C)]
pub struct SegmentHeader {
    pub magic: u32,
    pub version: u16,
    _pad0: u16,
    pub total_size: u64,
    pub data_offset: u32,
    pub min_block: u32,
    pub max_levels: u16,
    _pad1: u16,
    pub spinlock: u32,
    pub alloc_count: AtomicU32,
    _pad2: u32,
    pub free_bytes: AtomicU64,
    // Bitmap data follows immediately after this struct.
}

/// Result of a successful buddy allocation.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    /// Offset within the data region (not the entire SHM).
    pub offset: u32,
    /// Actual allocated size (power of two, >= requested).
    pub actual_size: u32,
    /// The buddy level this block belongs to.
    pub level: u16,
}

/// Buddy allocator for a single SHM segment.
pub struct BuddyAllocator {
    /// Pointer to the beginning of the SHM mapping.
    base: *mut u8,
    /// Total SHM size.
    total_size: usize,
    /// Offset where data region begins.
    data_offset: usize,
    /// Size of the data region (power of two).
    data_size: usize,
    /// Minimum allocation block size.
    min_block: usize,
    /// Number of buddy levels.
    levels: usize,
    /// Per-level bitmaps (level 0 = largest blocks).
    bitmaps: Vec<LevelBitmap>,
    /// Cross-process spinlock.
    lock: ShmSpinlock,
}

unsafe impl Send for BuddyAllocator {}
unsafe impl Sync for BuddyAllocator {}

impl BuddyAllocator {
    /// Initialize a new buddy allocator on fresh SHM memory.
    ///
    /// # Safety
    /// `base` must point to a freshly-created SHM region of `total_size` bytes.
    /// The caller is responsible for the SHM lifecycle.
    pub unsafe fn init(base: *mut u8, total_size: usize, min_block: usize) -> Self {
        assert!(min_block.is_power_of_two());
        assert!(total_size >= HEADER_ALIGN + min_block);

        let data_offset = Self::compute_data_offset(total_size, min_block);
        let data_size = Self::round_down_pow2(total_size - data_offset);

        // Write header.
        let header = &mut *(base as *mut SegmentHeader);
        header.magic = SEGMENT_MAGIC;
        header.version = SEGMENT_VERSION;
        header.total_size = total_size as u64;
        header.data_offset = data_offset as u32;
        header.min_block = min_block as u32;

        let levels = Self::count_levels(data_size, min_block);
        header.max_levels = levels as u16;
        header.alloc_count = AtomicU32::new(0);
        header.free_bytes = AtomicU64::new(data_size as u64);

        // Initialize spinlock.
        let lock = ShmSpinlock::new(
            base.add(std::mem::offset_of!(SegmentHeader, spinlock)),
        );
        lock.init();

        // Initialize bitmaps.
        let bitmaps = Self::create_bitmaps(base, data_size, min_block, levels);
        for bm in &bitmaps {
            // Only level 0 (the whole data region as one block) starts free.
            // Other levels start all-used; they become free via splitting.
        }
        // Mark all bits used first.
        for bm in &bitmaps {
            // Initialize all as used (0).
            for i in 0..bm.word_count() {
                let word_ptr = base.add(Self::bitmap_data_offset())
                    as *mut std::sync::atomic::AtomicU64;
                // Already zeroed by SHM (ftruncate fills with zeros).
            }
        }
        // Only the top level (level 0) has one free block.
        bitmaps[0].free_one(0);

        Self {
            base,
            total_size,
            data_offset,
            data_size,
            min_block,
            levels,
            bitmaps,
            lock,
        }
    }

    /// Attach to an existing SHM segment (another process created it).
    ///
    /// # Safety
    /// `base` must point to a valid SHM region with a properly initialized header.
    pub unsafe fn attach(base: *mut u8, total_size: usize) -> Result<Self, &'static str> {
        let header = &*(base as *const SegmentHeader);
        if header.magic != SEGMENT_MAGIC {
            return Err("invalid segment magic");
        }
        if header.version != SEGMENT_VERSION {
            return Err("unsupported segment version");
        }

        let data_offset = header.data_offset as usize;
        let min_block = header.min_block as usize;
        let levels = header.max_levels as usize;
        let data_size = Self::round_down_pow2(total_size - data_offset);

        let lock = ShmSpinlock::new(
            base.add(std::mem::offset_of!(SegmentHeader, spinlock)),
        );
        let bitmaps = Self::create_bitmaps(base, data_size, min_block, levels);

        Ok(Self {
            base,
            total_size,
            data_offset,
            data_size,
            min_block,
            levels,
            bitmaps,
            lock,
        })
    }

    /// Allocate a block of at least `size` bytes. Returns None if segment is full.
    pub fn alloc(&self, size: usize) -> Option<Allocation> {
        if size == 0 || size > self.data_size {
            return None;
        }

        let actual_size = size.next_power_of_two().max(self.min_block);
        let target_level = self.size_to_level(actual_size)?;

        self.lock.with_lock(|| self.alloc_with_split(target_level, actual_size))
    }

    /// Free a previously allocated block.
    pub fn free(&self, offset: u32, level: u16) {
        self.lock.with_lock(|| self.free_and_merge(offset, level));
    }

    /// Get a pointer to the data at the given offset within the data region.
    pub fn data_ptr(&self, offset: u32) -> *mut u8 {
        unsafe { self.base.add(self.data_offset + offset as usize) }
    }

    /// Get the data region size.
    pub fn data_size(&self) -> usize {
        self.data_size
    }

    /// Get the minimum block size.
    pub fn min_block(&self) -> usize {
        self.min_block
    }

    /// Get current number of active allocations.
    pub fn alloc_count(&self) -> u32 {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        header.alloc_count.load(Ordering::Acquire)
    }

    /// Get current free bytes.
    pub fn free_bytes(&self) -> u64 {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        header.free_bytes.load(Ordering::Acquire)
    }

    // --- Internal methods ---

    fn alloc_with_split(&self, target_level: usize, actual_size: usize) -> Option<Allocation> {
        // Scan from target_level upward to find a free block.
        let mut donor_level = None;
        let mut donor_idx = 0;

        for lvl in (0..=target_level).rev() {
            if let Some(idx) = self.bitmaps[lvl].alloc_one() {
                donor_level = Some(lvl);
                donor_idx = idx;
                break;
            }
        }

        let donor_lvl = donor_level?;

        if donor_lvl == target_level {
            // Found at exact level, no splitting needed.
            self.update_stats_alloc(actual_size);
            return Some(Allocation {
                offset: self.block_to_offset(target_level, donor_idx),
                actual_size: actual_size as u32,
                level: target_level as u16,
            });
        }

        // Split the donor block down to target level.
        let mut current_idx = donor_idx;
        for lvl in (donor_lvl + 1)..=target_level {
            let left_child = current_idx * 2;
            let right_child = current_idx * 2 + 1;
            // Mark the right buddy as free (we'll use the left one).
            self.bitmaps[lvl].free_one(right_child);
            current_idx = left_child;
        }

        self.update_stats_alloc(actual_size);
        Some(Allocation {
            offset: self.block_to_offset(target_level, current_idx),
            actual_size: actual_size as u32,
            level: target_level as u16,
        })
    }

    fn free_and_merge(&self, offset: u32, level: u16) {
        let level = level as usize;
        let block_size = self.level_block_size(level);
        let block_idx = offset as usize / block_size;

        // Mark this block as free.
        self.bitmaps[level].free_one(block_idx);
        self.update_stats_free(block_size);

        // Try to merge with buddy, recursing upward.
        let mut current_level = level;
        let mut current_idx = block_idx;

        while current_level > 0 {
            let buddy_idx = current_idx ^ 1; // XOR to find buddy.
            if !self.bitmaps[current_level].is_free(buddy_idx) {
                break; // Buddy is in use, can't merge.
            }

            // Both buddies are free — merge into parent.
            self.bitmaps[current_level].mark_used(current_idx);
            self.bitmaps[current_level].mark_used(buddy_idx);

            // Move up one level.
            current_level -= 1;
            current_idx /= 2;
            self.bitmaps[current_level].free_one(current_idx);
        }
    }

    fn update_stats_alloc(&self, size: usize) {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        header.alloc_count.fetch_add(1, Ordering::Release);
        header.free_bytes.fetch_sub(size as u64, Ordering::Release);
    }

    fn update_stats_free(&self, size: usize) {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        header.alloc_count.fetch_sub(1, Ordering::Release);
        header.free_bytes.fetch_add(size as u64, Ordering::Release);
    }

    /// Convert a (level, block_index) to a byte offset in the data region.
    fn block_to_offset(&self, level: usize, block_idx: usize) -> u32 {
        let block_size = self.level_block_size(level);
        (block_idx * block_size) as u32
    }

    /// Get the block size at a given level.
    fn level_block_size(&self, level: usize) -> usize {
        self.data_size >> level
    }

    /// Convert a requested allocation size to its buddy level.
    fn size_to_level(&self, actual_size: usize) -> Option<usize> {
        if actual_size > self.data_size {
            return None;
        }
        // Level 0 = data_size, level 1 = data_size/2, ...
        let mut level = 0;
        let mut block_size = self.data_size;
        while block_size > actual_size && level < self.levels - 1 {
            level += 1;
            block_size /= 2;
        }
        Some(level)
    }

    fn create_bitmaps(
        base: *mut u8,
        data_size: usize,
        _min_block: usize,
        levels: usize,
    ) -> Vec<LevelBitmap> {
        let mut bitmaps = Vec::with_capacity(levels);
        let mut bitmap_ptr = unsafe { base.add(Self::bitmap_data_offset()) };
        let mut block_size = data_size;

        for _ in 0..levels {
            let num_blocks = data_size / block_size;
            let bm = unsafe { LevelBitmap::new(bitmap_ptr, num_blocks) };
            let byte_size = bm.byte_size();
            bitmaps.push(bm);
            bitmap_ptr = unsafe { bitmap_ptr.add(byte_size) };
            block_size /= 2;
        }

        bitmaps
    }

    fn compute_data_offset(total_size: usize, min_block: usize) -> usize {
        let data_candidate = Self::round_down_pow2(total_size - HEADER_ALIGN);
        let bitmap_bytes =
            crate::bitmap::total_bitmap_bytes(data_candidate, min_block);
        let header_need = std::mem::size_of::<SegmentHeader>() + bitmap_bytes;
        // Round up to page alignment.
        let data_offset = (header_need + HEADER_ALIGN - 1) & !(HEADER_ALIGN - 1);
        data_offset
    }

    fn count_levels(data_size: usize, min_block: usize) -> usize {
        crate::bitmap::num_levels(data_size, min_block)
    }

    fn round_down_pow2(n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        1 << (usize::BITS - 1 - n.leading_zeros())
    }

    fn bitmap_data_offset() -> usize {
        std::mem::size_of::<SegmentHeader>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_allocator(total_size: usize, min_block: usize) -> (BuddyAllocator, Vec<u8>) {
        let mut buffer = vec![0u8; total_size];
        let alloc = unsafe { BuddyAllocator::init(buffer.as_mut_ptr(), total_size, min_block) };
        (alloc, buffer)
    }

    #[test]
    fn test_basic_alloc_free() {
        let (alloc, _buf) = make_allocator(64 * 1024, 4096); // 64KB total
        let a = alloc.alloc(4096).expect("alloc 4KB");
        assert_eq!(a.actual_size, 4096);
        alloc.free(a.offset, a.level);
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_multiple_allocs() {
        let (alloc, _buf) = make_allocator(256 * 1024, 4096); // 256KB total
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        assert_ne!(a.offset, b.offset);
        alloc.free(a.offset, a.level);
        alloc.free(b.offset, b.level);
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_power_of_two_rounding() {
        let (alloc, _buf) = make_allocator(256 * 1024, 4096);
        // Request 5KB → rounds up to 8KB.
        let a = alloc.alloc(5000).unwrap();
        assert_eq!(a.actual_size, 8192);
        alloc.free(a.offset, a.level);
    }

    #[test]
    fn test_buddy_merge() {
        let (alloc, _buf) = make_allocator(128 * 1024, 4096); // 128KB, data ~64KB
        let data_size = alloc.data_size();
        let initial_free = alloc.free_bytes();

        // Allocate two min-blocks that should be buddies.
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();

        // Free both — should merge back.
        alloc.free(a.offset, a.level);
        alloc.free(b.offset, b.level);

        // Free bytes should be restored.
        assert_eq!(alloc.free_bytes(), initial_free);
    }

    #[test]
    fn test_exhaustion_returns_none() {
        let (alloc, _buf) = make_allocator(16 * 1024, 4096); // Tiny: ~8KB data
        let data_size = alloc.data_size();
        // Allocate the entire data region.
        let a = alloc.alloc(data_size);
        assert!(a.is_some());
        // Second allocation should fail.
        assert!(alloc.alloc(4096).is_none());
        alloc.free(a.unwrap().offset, a.unwrap().level);
    }

    #[test]
    fn test_zero_and_oversized() {
        let (alloc, _buf) = make_allocator(64 * 1024, 4096);
        assert!(alloc.alloc(0).is_none());
        assert!(alloc.alloc(alloc.data_size() + 1).is_none());
    }

    #[test]
    fn test_splitting_and_fragmentation() {
        // 128KB total, ~64KB data, 4KB min = up to 16 min-blocks.
        let (alloc, _buf) = make_allocator(128 * 1024, 4096);

        // Allocate several small blocks.
        let mut allocs = Vec::new();
        for _ in 0..4 {
            if let Some(a) = alloc.alloc(4096) {
                allocs.push(a);
            }
        }
        assert!(allocs.len() >= 2);

        // Free alternating blocks to create fragmentation.
        for (i, a) in allocs.iter().enumerate() {
            if i % 2 == 0 {
                alloc.free(a.offset, a.level);
            }
        }

        // Should still be able to allocate small blocks.
        let x = alloc.alloc(4096);
        assert!(x.is_some());
    }
}
