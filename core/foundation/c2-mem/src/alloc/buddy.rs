//! Buddy allocator operating on a single SHM segment.
//!
//! The allocator manages a power-of-two data region using hierarchical bitmaps.
//! Each level represents blocks of a specific size (level 0 = full segment,
//! level N = min_block_size). Allocation rounds up to the nearest power of two
//! and searches the appropriate level. Free merges buddy pairs recursively.
//!
//! Crash safety is fail closed: when the segment lock reports an unavailable
//! backing (an observed-dead holder, a panicked critical section, or
//! contention that exhausts the bounded spin budget), `alloc` returns
//! `None` and `free` returns a descriptive `Err` without touching allocator
//! state. See `alloc::spinlock` for the lock word encoding.

use crate::alloc::bitmap::LevelBitmap;
use crate::alloc::spinlock::ShmSpinlock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Magic number for segment validation.
pub const SEGMENT_MAGIC: u32 = 0xCC20_0001;
/// Current allocator version.
///
/// Version 3 records each live allocation's identity and level. Earlier
/// layouts cannot validate release identity and are rejected by `attach`.
pub const SEGMENT_VERSION: u16 = 3;
/// Header is page-aligned (4KB).
pub const HEADER_ALIGN: usize = 4096;
const ALLOCATION_LEVEL_BITS: u32 = 6;
const ALLOCATION_LEVEL_MASK: u64 = (1 << ALLOCATION_LEVEL_BITS) - 1;
const MAX_ALLOCATION_SEQUENCE: u64 = u64::MAX >> ALLOCATION_LEVEL_BITS;

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
/// 28      4     _pad2 (u32) - keeps the lock word 8-byte aligned
/// 32      8     spinlock (u64) - atomic lock/poison word; 0 = unlocked,
///               otherwise the full holder PID in the lower 32 bits, or a
///               poison-flagged word that makes the backing refuse all
///               further allocator mutation (see `alloc::spinlock`)
/// 40      4     alloc_count (u32) - atomic, active allocations
/// 44      4     _pad3 (u32)
/// 48      8     free_bytes (u64) - atomic, available bytes
/// 56      8     allocation_sequence (u64) - never wraps within a backing
/// 64      ...   free bitmaps, then one u64 live record per min-block start
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
    _pad2: u32,
    pub spinlock: u64,
    pub alloc_count: AtomicU32,
    _pad3: u32,
    pub free_bytes: AtomicU64,
    pub allocation_sequence: AtomicU64,
    // Bitmap data follows immediately after this struct.
}

// The lock word is a cross-process AtomicU64: verify the documented layout so
// the spinlock and both shared counters land on 8-byte-aligned offsets in the
// shared mapping on every supported backend (x64, arm64).
const _: () = {
    assert!(std::mem::size_of::<SegmentHeader>() == 64);
    assert!(std::mem::offset_of!(SegmentHeader, spinlock) == 32);
    assert!(std::mem::offset_of!(SegmentHeader, spinlock) % std::mem::align_of::<u64>() == 0);
    assert!(std::mem::offset_of!(SegmentHeader, free_bytes) % std::mem::align_of::<u64>() == 0);
    assert!(std::mem::offset_of!(SegmentHeader, allocation_sequence) == 56);
};

/// Result of a successful buddy allocation.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    /// Offset within the data region (not the entire SHM).
    pub offset: u32,
    /// Actual allocated size (power of two, >= requested).
    pub actual_size: u32,
    /// The buddy level this block belongs to.
    pub level: u16,
    /// Identity of this allocation within its concrete backing generation.
    pub allocation_id: u64,
}

/// Validated geometry for one buddy backing, derived only by
/// [`BuddyAllocator::checked_layout`].
///
/// `data_size` is the usable power-of-two data region, bounded by the wire's
/// `u32` buddy coordinates; `total_size` additionally includes the page-
/// aligned header, free bitmaps and allocation records. The data offset must
/// fit u32 and the complete mapping must fit one addressable `isize` span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyLayout {
    /// Usable data region size (power of two, `<= u32::MAX`).
    pub data_size: usize,
    /// Total mapping size: aligned metadata plus data (`<= isize::MAX`).
    pub total_size: usize,
}

/// Buddy allocator for a single SHM segment.
pub struct BuddyAllocator {
    /// Pointer to the beginning of the SHM mapping.
    base: *mut u8,
    /// Total SHM size (retained for future stats/diagnostics).
    _total_size: usize,
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
    /// One record per min-block start; accessed only under `lock`.
    allocation_records: *mut AtomicU64,
    /// Cross-process spinlock.
    lock: ShmSpinlock,
}

unsafe impl Send for BuddyAllocator {}
unsafe impl Sync for BuddyAllocator {}

impl BuddyAllocator {
    /// Initialize a new buddy allocator on fresh SHM memory.
    ///
    /// # Safety
    /// `base` must point to a freshly-created, zeroed SHM region of
    /// `layout.total_size` bytes, which outlives the allocator.
    /// The caller is responsible for the SHM lifecycle.
    pub unsafe fn init(base: *mut u8, layout: BuddyLayout, min_block: usize) -> Self {
        assert_eq!(
            Self::checked_layout(layout.data_size, min_block),
            Some(layout)
        );
        let BuddyLayout {
            data_size,
            total_size,
        } = layout;
        let data_offset = total_size - data_size;

        // R-C3: Verify SHM base address is properly aligned for SegmentHeader.
        assert!(
            (base as usize).is_multiple_of(std::mem::align_of::<SegmentHeader>()),
            "SHM base address must be aligned to SegmentHeader requirements"
        );

        // R-C4: Verify atomics are lock-free for cross-process SHM correctness.
        // On stable Rust, is_lock_free() is unavailable; cfg!(target_has_atomic)
        // verifies the platform supports native (lock-free) atomics.
        assert!(
            cfg!(target_has_atomic = "64"),
            "AtomicU64 must be lock-free for cross-process SHM"
        );
        assert!(
            cfg!(target_has_atomic = "32"),
            "AtomicU32 must be lock-free for cross-process SHM"
        );

        // Write header.
        // SAFETY: caller guarantees `base` is a valid, writable SHM pointer
        // with at least `total_size` bytes, aligned for SegmentHeader.
        let header = unsafe { &mut *(base as *mut SegmentHeader) };
        header.magic = SEGMENT_MAGIC;
        header.version = SEGMENT_VERSION;
        header.total_size = total_size as u64;
        header.data_offset = data_offset as u32;
        header.min_block = min_block as u32;

        let levels = Self::count_levels(data_size, min_block);
        header.max_levels = levels as u16;
        header.alloc_count = AtomicU32::new(0);
        header.free_bytes = AtomicU64::new(data_size as u64);
        header.allocation_sequence = AtomicU64::new(0);

        // Initialize spinlock.
        // SAFETY: spinlock offset is within the header region.
        let lock =
            unsafe { ShmSpinlock::new(base.add(std::mem::offset_of!(SegmentHeader, spinlock))) };
        lock.init();

        // Initialize bitmaps — SHM is zero-filled by ftruncate, so all bits
        // start as "used" (0). Only level-0 needs one block marked free.
        let (bitmaps, allocation_records) = Self::create_bitmaps(base, data_size, levels);
        bitmaps[0].free_one(0);

        Self {
            base,
            _total_size: total_size,
            data_offset,
            data_size,
            min_block,
            levels,
            bitmaps,
            allocation_records,
            lock,
        }
    }

    /// Attach to an existing SHM segment (another process created it).
    ///
    /// # Safety
    /// `base` must point to a valid SHM region with a properly initialized header.
    pub unsafe fn attach(base: *mut u8, total_size: usize) -> Result<Self, &'static str> {
        if total_size < std::mem::size_of::<SegmentHeader>() {
            return Err("mapping is smaller than the allocator header");
        }
        // R-C3: Verify alignment for SegmentHeader access in attach path too.
        assert!(
            (base as usize).is_multiple_of(std::mem::align_of::<SegmentHeader>()),
            "SHM base address must be aligned to SegmentHeader requirements"
        );

        // SAFETY: caller guarantees `base` is a valid, initialized SHM pointer.
        let header = unsafe { &*(base as *const SegmentHeader) };
        if header.magic != SEGMENT_MAGIC {
            return Err("invalid segment magic");
        }
        if header.version != SEGMENT_VERSION {
            return Err("unsupported segment version (expected 3)");
        }

        // OS mappings may include page-rounded trailing capacity. Geometry is
        // defined by the header's logical size, bounded by the actual mapping.
        let mapped_size = total_size;
        let total_size =
            usize::try_from(header.total_size).map_err(|_| "allocator size is not addressable")?;
        if total_size > mapped_size {
            return Err("allocator layout exceeds the mapped capacity");
        }

        let data_offset = header.data_offset as usize;
        let min_block = header.min_block as usize;
        let levels = header.max_levels as usize;
        let data_size = total_size
            .checked_sub(data_offset)
            .ok_or("invalid allocator data offset")?;
        let layout =
            Self::checked_layout(data_size, min_block).ok_or("invalid allocator geometry")?;
        if layout.data_size != data_size
            || layout.total_size != total_size
            || layout.total_size - layout.data_size != data_offset
            || levels != Self::count_levels(data_size, min_block)
        {
            return Err("allocator metadata layout does not match header");
        }

        // SAFETY: spinlock offset is within the validated header region.
        let lock =
            unsafe { ShmSpinlock::new(base.add(std::mem::offset_of!(SegmentHeader, spinlock))) };
        let (bitmaps, allocation_records) = Self::create_bitmaps(base, data_size, levels);

        Ok(Self {
            base,
            _total_size: total_size,
            data_offset,
            data_size,
            min_block,
            levels,
            bitmaps,
            allocation_records,
            lock,
        })
    }

    /// Allocate a block of at least `size` bytes.
    ///
    /// Returns `None` when the segment is full or when the backing is
    /// unavailable: held by an observed-dead process, poisoned by panic, or contended past
    /// the bounded spin budget. A refused allocation never mutates allocator
    /// state.
    pub fn alloc(&self, size: usize) -> Option<Allocation> {
        if size == 0 || size > self.data_size {
            return None;
        }

        let actual_size = size.next_power_of_two().max(self.min_block);
        let target_level = self.size_to_level(actual_size)?;

        match self
            .lock
            .with_lock(|| self.alloc_with_split(target_level, actual_size))
        {
            Ok(allocation) => allocation,
            Err(_) => None,
        }
    }

    /// Free a previously allocated block.
    ///
    /// Returns a descriptive `Err` — never a panic — when the backing is
    /// unavailable (poisoned or contended past the bounded spin budget) or
    /// when the free itself is invalid (bad level/offset, double free).
    pub fn free(&self, offset: u32, level: u16, allocation_id: u64) -> Result<(), String> {
        match self
            .lock
            .with_lock(|| self.free_and_merge(offset, level, allocation_id))
        {
            Ok(result) => result,
            Err(err) => Err(format!("buddy free refused: {err}")),
        }
    }

    /// Poisoned counters may reflect an interrupted mutation and cannot
    /// authorize backing retirement, even when they currently report zero.
    pub fn is_poisoned(&self) -> bool {
        self.lock.is_poisoned()
    }

    /// Called under the pool's allocation/cache lock. A busy or poisoned
    /// allocator cannot authorize retirement through an intermediate counter.
    pub fn can_retire(&self) -> bool {
        // One CAS attempt, with no spin, yield, or process-liveness probe.
        // Observing an unlocked word and then loading the counter separately
        // would race a peer's last free before its bitmap merge completes.
        if self
            .lock
            .try_lock_budget(std::process::id(), 1, u32::MAX)
            .is_err()
        {
            return false;
        }
        let idle = self.alloc_count() == 0;
        self.lock.unlock();
        idle
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
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        // Exhaustion must refuse before alloc_one changes any free bit.
        let sequence = header
            .allocation_sequence
            .load(Ordering::Relaxed)
            .checked_add(1)
            .filter(|sequence| *sequence <= MAX_ALLOCATION_SEQUENCE)?;
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

        // Split the donor block down to target level.
        let mut current_idx = donor_idx;
        for lvl in (donor_lvl + 1)..=target_level {
            let left_child = current_idx * 2;
            let right_child = current_idx * 2 + 1;
            // Mark the right buddy as free (we'll use the left one).
            self.bitmaps[lvl].free_one(right_child);
            current_idx = left_child;
        }

        let offset = self.block_to_offset(target_level, current_idx);
        let allocation_id = (sequence << ALLOCATION_LEVEL_BITS) | target_level as u64;
        header
            .allocation_sequence
            .store(sequence, Ordering::Relaxed);
        self.allocation_record(offset)
            .store(allocation_id, Ordering::Relaxed);
        self.update_stats_alloc(actual_size);
        Some(Allocation {
            offset,
            actual_size: actual_size as u32,
            level: target_level as u16,
            allocation_id,
        })
    }

    fn free_and_merge(&self, offset: u32, level: u16, allocation_id: u64) -> Result<(), String> {
        let level = level as usize;

        // R-I4: Validate level is within range.
        if level >= self.levels {
            return Err(format!(
                "free: level {} exceeds max {}",
                level,
                self.levels - 1
            ));
        }

        let block_size = self.level_block_size(level);

        // R-I4: Validate offset alignment.
        if !(offset as usize).is_multiple_of(block_size) {
            return Err(format!(
                "free: offset {} not aligned to block size {} at level {}",
                offset, block_size, level
            ));
        }

        let block_idx = offset as usize / block_size;
        let blocks_at_level = self.data_size / block_size;

        // R-I4: Validate block index.
        if block_idx >= blocks_at_level {
            return Err(format!(
                "free: block index {} out of range (max {}) at level {}",
                block_idx,
                blocks_at_level - 1,
                level
            ));
        }

        // R-C5: Double-free detection — block must be currently allocated.
        // After a prior free merges children into a larger parent, the child
        // bitmap entries are marked used again. Check ancestors first so a
        // repeated free of a merged child cannot corrupt allocator accounting.
        for ancestor_level in 0..level {
            let ancestor_idx = block_idx >> (level - ancestor_level);
            if self.bitmaps[ancestor_level].is_free(ancestor_idx) {
                return Err(format!(
                    "double free detected: block {} at level {} is already covered by free ancestor {} at level {}",
                    block_idx, level, ancestor_idx, ancestor_level
                ));
            }
        }
        if self.bitmaps[level].is_free(block_idx) {
            return Err(format!(
                "double free detected: block {} at level {} is already free",
                block_idx, level
            ));
        }

        let record = self.allocation_record(offset);
        if allocation_id == 0
            || allocation_id & ALLOCATION_LEVEL_MASK != level as u64
            || record.load(Ordering::Relaxed) != allocation_id
        {
            return Err("free: allocation identity does not match the live block".into());
        }

        record.store(0, Ordering::Relaxed);
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

        Ok(())
    }

    fn allocation_record(&self, offset: u32) -> &AtomicU64 {
        // Allocation derives this offset from the checked tree geometry;
        // free validates alignment and bounds before reaching this lookup.
        unsafe {
            &*self
                .allocation_records
                .add(offset as usize / self.min_block)
        }
    }

    fn update_stats_alloc(&self, size: usize) {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        header.alloc_count.fetch_add(1, Ordering::Release);
        header.free_bytes.fetch_sub(size as u64, Ordering::Release);
    }

    fn update_stats_free(&self, size: usize) {
        let header = unsafe { &*(self.base as *const SegmentHeader) };
        // R-I4: Use fetch_update for saturating decrement — prevents u32
        // underflow if a double-free somehow bypasses bitmap validation.
        let _ = header
            .alloc_count
            .fetch_update(Ordering::Release, Ordering::Relaxed, |prev| {
                if prev > 0 { Some(prev - 1) } else { None }
            });
        header.free_bytes.fetch_add(size as u64, Ordering::Release);
    }

    /// Convert a (level, block_index) to a byte offset in the data region.
    fn block_to_offset(&self, level: usize, block_idx: usize) -> u32 {
        let block_size = self.level_block_size(level);
        let offset = block_idx * block_size;
        // R-I3: Guard against u32 truncation on the computed offset.
        debug_assert!(
            offset <= u32::MAX as usize,
            "block offset {} exceeds u32::MAX",
            offset
        );
        offset as u32
    }

    /// Get the block size at a given level.
    fn level_block_size(&self, level: usize) -> usize {
        self.data_size >> level
    }

    /// Convert a requested allocation size to its buddy level.
    pub fn size_to_level(&self, actual_size: usize) -> Option<usize> {
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
        levels: usize,
    ) -> (Vec<LevelBitmap>, *mut AtomicU64) {
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

        (bitmaps, bitmap_ptr.cast())
    }

    /// Checked next power of two; `usize::next_power_of_two` would wrap or
    /// panic near `usize::MAX`.
    fn checked_next_pow2(n: usize) -> Option<usize> {
        if n <= 1 {
            return Some(1);
        }
        let bits = usize::BITS - n.leading_zeros();
        if n.is_power_of_two() {
            1usize.checked_shl(bits - 1)
        } else {
            1usize.checked_shl(bits)
        }
    }

    /// Compute the minimum validated layout whose data region holds at least
    /// `min_data_capacity` bytes (after buddy power-of-two rounding), or
    /// `None` when the geometry is unsupported.
    ///
    /// This is the single owner of the segment-geometry formula; creation and
    /// budget charging must both consume a layout derived from it so the
    /// charged bytes always equal the mapped bytes. Rejections cover:
    /// non-power-of-two or non-positive `min_block`, `min_block` larger than
    /// the data region, a next-power-of-two overflow, a data region above the
    /// wire's `u32` buddy coordinates, and header/bitmap/alignment additions
    /// or the final size overflowing the addressable `isize` mapping span.
    ///
    /// With `data_size` a validated power of two `<= u32::MAX` and
    /// `min_block` a dividing power of two, `total_bitmap_bytes` is bounded
    /// by roughly `data_size / 4` and cannot overflow; every subsequent
    /// addition is checked regardless.
    pub fn checked_layout(min_data_capacity: usize, min_block: usize) -> Option<BuddyLayout> {
        if min_block == 0 || !min_block.is_power_of_two() {
            return None;
        }
        let data_size = Self::checked_next_pow2(min_data_capacity)?;
        // The wire's buddy coordinates (offset, block size) are u32.
        if data_size > u32::MAX as usize || min_block > data_size {
            return None;
        }
        let bitmap_bytes = crate::alloc::bitmap::total_bitmap_bytes(data_size, min_block);
        let record_bytes = (data_size / min_block).checked_mul(std::mem::size_of::<u64>())?;
        let header_need = std::mem::size_of::<SegmentHeader>()
            .checked_add(bitmap_bytes)?
            .checked_add(record_bytes)?;
        let align_mask = HEADER_ALIGN - 1;
        let data_offset = header_need.checked_add(align_mask)? & !align_mask;
        let total_size = data_offset.checked_add(data_size)?;
        if data_offset > u32::MAX as usize || total_size > isize::MAX as usize {
            return None;
        }
        Some(BuddyLayout {
            data_size,
            total_size,
        })
    }

    fn count_levels(data_size: usize, min_block: usize) -> usize {
        crate::alloc::bitmap::num_levels(data_size, min_block)
    }

    fn bitmap_data_offset() -> usize {
        std::mem::size_of::<SegmentHeader>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_allocator(data_size: usize, min_block: usize) -> (BuddyAllocator, Vec<u8>) {
        let layout = BuddyAllocator::checked_layout(data_size, min_block).unwrap();
        let mut buffer = vec![0u8; layout.total_size];
        let alloc = unsafe { BuddyAllocator::init(buffer.as_mut_ptr(), layout, min_block) };
        (alloc, buffer)
    }

    fn assert_invalid_release_preserves_live_allocator(
        alloc: &BuddyAllocator,
        offset: u32,
        level: u16,
        allocation_id: u64,
    ) {
        // These tests own the entire mapping and never access payload bytes.
        let before = unsafe { std::slice::from_raw_parts(alloc.base, alloc.data_offset) }.to_vec();
        let result = alloc.free(offset, level, allocation_id);
        assert!(result.is_err(), "invalid release was accepted: {result:?}");
        assert_eq!(
            unsafe { std::slice::from_raw_parts(alloc.base, alloc.data_offset) },
            before,
            "rejected release changed allocator metadata"
        );
        assert!(
            !alloc.can_retire(),
            "a live allocation must prevent retirement"
        );
    }

    #[test]
    fn r01_rejects_left_child_of_live_allocation() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096);
        let parent = alloc.alloc(8192).unwrap();
        assert_invalid_release_preserves_live_allocator(
            &alloc,
            parent.offset,
            parent.level + 1,
            parent.allocation_id,
        );
        assert!(alloc.alloc(4096).is_none());
        alloc
            .free(parent.offset, parent.level, parent.allocation_id)
            .unwrap();
    }

    #[test]
    fn r01_rejects_right_child_of_live_allocation() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096);
        let parent = alloc.alloc(8192).unwrap();
        assert_invalid_release_preserves_live_allocator(
            &alloc,
            parent.offset + 4096,
            parent.level + 1,
            parent.allocation_id,
        );
        assert!(alloc.alloc(4096).is_none());
        alloc
            .free(parent.offset, parent.level, parent.allocation_id)
            .unwrap();
    }

    #[test]
    fn r01_rejects_parent_of_live_children() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096);
        let left = alloc.alloc(4096).unwrap();
        let right = alloc.alloc(4096).unwrap();
        assert_invalid_release_preserves_live_allocator(
            &alloc,
            left.offset,
            left.level - 1,
            left.allocation_id,
        );
        assert!(alloc.alloc(4096).is_none());
        alloc
            .free(left.offset, left.level, left.allocation_id)
            .unwrap();
        alloc
            .free(right.offset, right.level, right.allocation_id)
            .unwrap();
    }

    #[test]
    fn r01_rejects_parent_with_one_live_child() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096);
        let left = alloc.alloc(4096).unwrap();
        assert_invalid_release_preserves_live_allocator(
            &alloc,
            left.offset,
            left.level - 1,
            left.allocation_id,
        );
        let right = alloc.alloc(4096).unwrap();
        assert_eq!(right.offset, left.offset + left.actual_size);
        alloc
            .free(left.offset, left.level, left.allocation_id)
            .unwrap();
        alloc
            .free(right.offset, right.level, right.allocation_id)
            .unwrap();
    }

    #[test]
    fn r01_rejects_released_allocation_after_same_slot_reuse() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096);
        let old = alloc.alloc(8192).unwrap();
        alloc
            .free(old.offset, old.level, old.allocation_id)
            .unwrap();
        let live = alloc.alloc(8192).unwrap();
        assert_eq!((old.offset, old.level), (live.offset, live.level));
        assert_ne!(old.allocation_id, live.allocation_id);
        assert_invalid_release_preserves_live_allocator(
            &alloc,
            old.offset,
            old.level,
            old.allocation_id,
        );
        assert!(alloc.alloc(4096).is_none());
        alloc
            .free(live.offset, live.level, live.allocation_id)
            .unwrap();
    }

    #[test]
    fn checked_layout_includes_allocation_records() {
        // Handwritten offsets, including metadata larger than the data region.
        for (capacity, min_block, data_offset) in [
            (8192, 4096, 4096),
            (8192, 8192, 4096),
            (8192, 8, 12288),
            (64 * 1024, 4096, 4096),
            (256 * 1024 * 1024, 4096, 544768),
            (1usize << 30, 4096, 2166784),
        ] {
            let layout = BuddyAllocator::checked_layout(capacity, min_block)
                .unwrap_or_else(|| panic!("supported geometry {capacity}/{min_block}"));
            assert_eq!(layout.data_size, capacity);
            assert_eq!(layout.total_size, data_offset + layout.data_size);
            if capacity <= 64 * 1024 {
                let (alloc, mut buffer) = make_allocator(capacity, min_block);
                let attached =
                    unsafe { BuddyAllocator::attach(buffer.as_mut_ptr(), buffer.len()) }.unwrap();
                assert_eq!(alloc.data_offset, data_offset);
                assert_eq!(attached.data_size(), capacity);
                let block = alloc.alloc(capacity).unwrap();
                attached
                    .free(block.offset, block.level, block.allocation_id)
                    .unwrap();
                assert_eq!(alloc.alloc_count(), 0);
                assert_eq!(alloc.free_bytes(), capacity as u64);
                let next = attached.alloc(capacity).unwrap();
                assert_ne!(next.allocation_id, block.allocation_id);
                alloc
                    .free(next.offset, next.level, next.allocation_id)
                    .unwrap();
            }
        }
    }

    #[test]
    fn r01_rejects_missing_other_and_wrong_level_identities() {
        let (alloc, _buf) = make_allocator(8192, 4096);
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        for id in [0, b.allocation_id, a.allocation_id ^ 1] {
            assert_invalid_release_preserves_live_allocator(&alloc, a.offset, a.level, id);
        }
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        alloc.free(b.offset, b.level, b.allocation_id).unwrap();
    }

    #[test]
    fn r01_allocation_sequence_exhaustion_never_reuses_identity() {
        let (alloc, _buf) = make_allocator(8192, 4096);
        let header = unsafe { &*(alloc.base.cast::<SegmentHeader>()) };
        alloc
            .lock
            .with_lock(|| {
                header
                    .allocation_sequence
                    .store(MAX_ALLOCATION_SEQUENCE - 1, Ordering::Relaxed)
            })
            .unwrap();
        let live = alloc.alloc(4096).unwrap();
        assert_eq!(
            live.allocation_id >> ALLOCATION_LEVEL_BITS,
            MAX_ALLOCATION_SEQUENCE
        );
        let before = unsafe { std::slice::from_raw_parts(alloc.base, alloc.data_offset) }.to_vec();
        assert!(alloc.alloc(4096).is_none());
        assert_eq!(
            unsafe { std::slice::from_raw_parts(alloc.base, alloc.data_offset) },
            before
        );
        alloc
            .free(live.offset, live.level, live.allocation_id)
            .unwrap();
        assert_eq!(alloc.free_bytes(), 8192);
        assert!(alloc.can_retire());
        assert!(alloc.alloc(4096).is_none());
    }

    #[test]
    fn checked_layout_rejects_unsupported_geometries_pure_preflight() {
        // min_block must be a positive power of two that divides the data region.
        assert!(BuddyAllocator::checked_layout(8192, 0).is_none());
        assert!(BuddyAllocator::checked_layout(8192, 3000).is_none());
        assert!(BuddyAllocator::checked_layout(4096, 8192).is_none());

        // Near-u32 and near-usize bounds: pure arithmetic, no mapping.
        if usize::BITS >= 64 {
            // 4 GiB data capacity leaves the wire's u32 buddy coordinates.
            let four_gib = 1usize << 32;
            assert!(BuddyAllocator::checked_layout(four_gib, 4096).is_none());
            assert!(BuddyAllocator::checked_layout(four_gib + 5, 4096).is_none());
            // The largest supported power-of-two capacity still validates.
            assert!(BuddyAllocator::checked_layout(1usize << 31, 4096).is_some());
            // Dense records for 1-byte blocks cannot fit the u32 data offset.
            assert!(BuddyAllocator::checked_layout(1usize << 31, 1).is_none());
            // Next power of two overflows near usize::MAX.
            assert!(BuddyAllocator::checked_layout(usize::MAX, 4096).is_none());
            assert!(BuddyAllocator::checked_layout((1usize << 63) + 1, 4096).is_none());
        }
    }

    #[test]
    fn test_basic_alloc_free() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096); // 64KB total
        let a = alloc.alloc(4096).expect("alloc 4KB");
        assert_eq!(a.actual_size, 4096);
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_multiple_allocs() {
        let (alloc, _buf) = make_allocator(128 * 1024, 4096); // 256KB total
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        assert_ne!(a.offset, b.offset);
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        alloc.free(b.offset, b.level, b.allocation_id).unwrap();
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_power_of_two_rounding() {
        let (alloc, _buf) = make_allocator(128 * 1024, 4096);
        // Request 5KB → rounds up to 8KB.
        let a = alloc.alloc(5000).unwrap();
        assert_eq!(a.actual_size, 8192);
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
    }

    #[test]
    fn test_buddy_merge() {
        let (alloc, _buf) = make_allocator(64 * 1024, 4096); // 128KB, data ~64KB
        let initial_free = alloc.free_bytes();

        // Allocate two min-blocks that should be buddies.
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();

        // Free both — should merge back.
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        alloc.free(b.offset, b.level, b.allocation_id).unwrap();

        // Free bytes should be restored.
        assert_eq!(alloc.free_bytes(), initial_free);
    }

    #[test]
    fn test_exhaustion_returns_none() {
        let (alloc, _buf) = make_allocator(8 * 1024, 4096); // Tiny: ~8KB data
        let data_size = alloc.data_size();
        // Allocate the entire data region.
        let a = alloc.alloc(data_size);
        assert!(a.is_some());
        // Second allocation should fail.
        assert!(alloc.alloc(4096).is_none());
        alloc
            .free(
                a.unwrap().offset,
                a.unwrap().level,
                a.unwrap().allocation_id,
            )
            .unwrap();
    }

    #[test]
    fn test_zero_and_oversized() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        assert!(alloc.alloc(0).is_none());
        assert!(alloc.alloc(alloc.data_size() + 1).is_none());
    }

    #[test]
    fn test_splitting_and_fragmentation() {
        // 128KB total, ~64KB data, 4KB min = up to 16 min-blocks.
        let (alloc, _buf) = make_allocator(64 * 1024, 4096);

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
                alloc.free(a.offset, a.level, a.allocation_id).unwrap();
            }
        }

        // Should still be able to allocate small blocks.
        let x = alloc.alloc(4096);
        assert!(x.is_some());
    }

    #[test]
    fn test_double_free_returns_error() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        // Allocate two blocks so the first one's buddy is in use,
        // preventing merge on free (which changes bitmap state).
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        // Second free of the same block should fail.
        let result = alloc.free(a.offset, a.level, a.allocation_id);
        assert!(result.is_err(), "double free should return error");
        assert!(
            result.unwrap_err().contains("double free"),
            "should report double free"
        );
        alloc.free(b.offset, b.level, b.allocation_id).unwrap();
    }

    #[test]
    fn test_double_free_after_merge_returns_error() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        let a = alloc.alloc(4096).unwrap();
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();

        let result = alloc.free(a.offset, a.level, a.allocation_id);
        assert!(
            result.is_err(),
            "double free after merge should return error"
        );
        assert!(
            result.unwrap_err().contains("double free"),
            "should report double free"
        );
    }

    #[test]
    fn test_free_invalid_level() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        let a = alloc.alloc(4096).unwrap();
        let result = alloc.free(a.offset, 99, a.allocation_id);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("level"),
            "should reject invalid level"
        );
        // Clean up properly.
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
    }

    #[test]
    fn test_free_invalid_offset() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        let a = alloc.alloc(4096).unwrap();
        // Misaligned offset.
        let result = alloc.free(a.offset + 1, a.level, a.allocation_id);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("not aligned"),
            "should reject misaligned offset"
        );
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
    }

    #[test]
    fn test_merge_stress_various_free_orders() {
        let (alloc, _buf) = make_allocator(128 * 1024, 4096);
        let initial_free = alloc.free_bytes();

        // Allocate many small blocks.
        let mut allocs = Vec::new();
        for _ in 0..8 {
            if let Some(a) = alloc.alloc(4096) {
                allocs.push(a);
            }
        }
        assert!(allocs.len() >= 4);

        // Free in reverse order — should still fully merge.
        for a in allocs.iter().rev() {
            alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        }
        assert_eq!(alloc.free_bytes(), initial_free);
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_attach_rejects_old_layout_version() {
        let layout = BuddyAllocator::checked_layout(32 * 1024, 4096).unwrap();
        let total_size = layout.total_size;
        let mut buffer = vec![0u8; total_size];
        let alloc = unsafe { BuddyAllocator::init(buffer.as_mut_ptr(), layout, 4096) };
        drop(alloc);

        for version in [1, 2] {
            unsafe {
                (*buffer.as_mut_ptr().cast::<SegmentHeader>()).version = version;
            }
            let err = unsafe { BuddyAllocator::attach(buffer.as_mut_ptr(), total_size) }
                .err()
                .expect("attach must reject the old layout version");
            assert!(
                err.contains("version"),
                "old layout was not rejected: {err}"
            );
        }
    }

    #[test]
    fn attach_rejects_inconsistent_allocation_record_geometry() {
        let (alloc, mut buffer) = make_allocator(8192, 8);
        drop(alloc);
        let ptr = buffer.as_mut_ptr();
        let (offset, min_block, levels, total_size) = unsafe {
            let header = &*ptr.cast::<SegmentHeader>();
            (
                header.data_offset,
                header.min_block,
                header.max_levels,
                header.total_size,
            )
        };
        assert!(unsafe { BuddyAllocator::attach(ptr, 63) }.is_err());
        for (bad_offset, bad_min, bad_levels, bad_total) in [
            (offset + 8, min_block, levels, total_size),
            (offset, 0, levels, total_size),
            (offset, 1, levels, total_size),
            (offset, min_block, levels + 1, total_size),
            (u32::MAX, min_block, levels, total_size),
            (offset, min_block, levels, total_size - 1),
        ] {
            unsafe {
                let header = &mut *ptr.cast::<SegmentHeader>();
                header.data_offset = bad_offset;
                header.min_block = bad_min;
                header.max_levels = bad_levels;
                header.total_size = bad_total;
            }
            assert!(unsafe { BuddyAllocator::attach(ptr, buffer.len()) }.is_err());
        }
    }

    #[test]
    fn attach_uses_logical_geometry_with_page_rounded_capacity() {
        for (capacity, min_block, mapped_size) in
            [(8192, 4096, 16384), (32768, 4096, 49152), (8192, 8, 32768)]
        {
            let (alloc, mut buffer) = make_allocator(capacity, min_block);
            drop(alloc);
            buffer.resize(mapped_size, 0);
            let attached =
                unsafe { BuddyAllocator::attach(buffer.as_mut_ptr(), buffer.len()) }.unwrap();
            assert_eq!(attached.data_size(), capacity);
            let block = attached.alloc(capacity).unwrap();
            assert!(attached.alloc(min_block).is_none());
            attached
                .free(block.offset, block.level, block.allocation_id)
                .unwrap();
        }
    }

    #[test]
    fn test_alloc_returns_none_and_free_err_when_poisoned() {
        use crate::alloc::spinlock::PID_MASK;

        let (alloc, _buf) = make_allocator(32 * 1024, 4096);
        alloc.lock.poison_panicked(std::process::id());

        let free_before = alloc.free_bytes();
        let count_before = alloc.alloc_count();

        assert!(
            alloc.alloc(4096).is_none(),
            "alloc must refuse poisoned backing"
        );
        let err = alloc.free(0, 1, 0).unwrap_err();
        assert!(
            err.contains("buddy free refused") && err.contains("poisoned"),
            "free should describe the refusal: {err}"
        );

        assert_eq!(
            alloc.free_bytes(),
            free_before,
            "refusal must not mutate stats"
        );
        assert_eq!(
            alloc.alloc_count(),
            count_before,
            "refusal must not mutate stats"
        );
        assert_eq!(
            alloc.lock.load_word_for_test() & PID_MASK,
            std::process::id() as u64,
            "poison must preserve the holder PID"
        );
    }

    #[test]
    fn test_contended_acquire_leaves_allocator_untouched() {
        let (mut alloc, _buf) = make_allocator(32 * 1024, 4096);
        // Recovery threshold beyond the budget: expiry must happen without
        // ever probing (and without ever touching) the live holder.
        alloc.lock.force_budgets(1_000, 2_000);
        let alloc = std::sync::Arc::new(alloc);

        let holder_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let holder = {
            let alloc = std::sync::Arc::clone(&alloc);
            let started = std::sync::Arc::clone(&holder_started);
            let stop = std::sync::Arc::clone(&holder_stop);
            std::thread::spawn(move || {
                alloc
                    .lock
                    .try_lock()
                    .expect("holder acquires an uncontended lock");
                started.store(true, std::sync::atomic::Ordering::Release);
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                alloc.lock.unlock();
            })
        };

        while !holder_started.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let free_before = alloc.free_bytes();
        let count_before = alloc.alloc_count();

        assert!(
            alloc.alloc(4096).is_none(),
            "bounded acquire must refuse while the holder is alive"
        );
        assert_eq!(
            alloc.free_bytes(),
            free_before,
            "refused alloc must mutate nothing"
        );
        assert_eq!(
            alloc.alloc_count(),
            count_before,
            "refused alloc must mutate nothing"
        );
        assert!(
            !alloc.lock.is_poisoned(),
            "a live same-process holder must never be poisoned"
        );

        holder_stop.store(true, std::sync::atomic::Ordering::Release);
        holder.join().unwrap();

        // After the holder releases, the untouched allocator still works and
        // its bitmaps behave exactly as if the refusal never happened.
        let a = alloc
            .alloc(4096)
            .expect("alloc succeeds after holder releases");
        alloc.free(a.offset, a.level, a.allocation_id).unwrap();
        assert_eq!(alloc.alloc_count(), 0);
    }

    #[test]
    fn test_panic_in_critical_section_poisons_allocator() {
        let (alloc, _buf) = make_allocator(32 * 1024, 4096);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = alloc
                .lock
                .with_lock(|| -> u32 { panic!("allocator bug inside critical section") });
        }));
        assert!(result.is_err(), "panic must propagate after poisoning");

        assert!(alloc.lock.is_poisoned(), "panic must poison the backing");

        let free_before = alloc.free_bytes();
        assert!(
            alloc.alloc(4096).is_none(),
            "alloc refuses a panicked backing"
        );
        let err = alloc.free(0, 1, 0).unwrap_err();
        assert!(
            err.contains("panicked"),
            "free should name the panic poison: {err}"
        );
        assert_eq!(
            alloc.free_bytes(),
            free_before,
            "refusal must not mutate stats"
        );
    }

    #[test]
    fn test_poison_visible_to_newly_attached_allocator() {
        let layout = BuddyAllocator::checked_layout(32 * 1024, 4096).unwrap();
        let total_size = layout.total_size;
        let mut buffer = vec![0u8; total_size];
        let creator = unsafe { BuddyAllocator::init(buffer.as_mut_ptr(), layout, 4096) };
        creator.lock.poison_panicked(std::process::id());
        drop(creator);

        // The poison lives in the shared word, so a fresh attach observes it.
        let attached = unsafe { BuddyAllocator::attach(buffer.as_mut_ptr(), total_size).unwrap() };
        assert!(attached.lock.is_poisoned());
        assert!(
            attached.alloc(4096).is_none(),
            "attached allocator must refuse"
        );
        assert!(
            attached.free(0, 1, 0).unwrap_err().contains("poisoned"),
            "attached allocator free must refuse descriptively"
        );
    }

    /// Environment variable that switches a re-invoked test binary into
    /// holder-child mode; the value is the SHM region name to attach to.
    const CRASH_CHILD_ENV: &str = "C2_MEM_CRASH_TEST_SHM";
    /// Shared region data capacity used by the killed-holder test.
    const CRASH_TEST_DATA_CAPACITY: usize = 64 * 1024;

    #[cfg(unix)]
    fn unlink_shm_region(name: &str) {
        if let Ok(c_name) = std::ffi::CString::new(name) {
            unsafe { libc::shm_unlink(c_name.as_ptr()) };
        }
    }

    #[cfg(not(unix))]
    fn unlink_shm_region(_name: &str) {}

    /// Body of the holder child: attach to the shared segment, acquire the
    /// shared lock, signal readiness through the first data byte, then stay
    /// alive until the parent kills it (or a safety expiry fires so a leaked
    /// child can never linger).
    fn crash_child_main(name: &str) {
        use crate::ShmRegion;

        let layout = BuddyAllocator::checked_layout(CRASH_TEST_DATA_CAPACITY, 4096)
            .expect("supported crash-test geometry");
        let total_size = layout.total_size;
        let region = ShmRegion::open(name, total_size).expect("holder child opens shared region");
        let alloc = unsafe {
            BuddyAllocator::attach(region.base_ptr(), region.size()).expect("child attach")
        };
        alloc
            .lock
            .try_lock()
            .expect("child acquires the shared lock");
        // Readiness signal while holding the lock: byte 0 of the data region
        // is plain test-protocol storage, not allocator state.
        let ready = unsafe { &*alloc.data_ptr(0).cast::<std::sync::atomic::AtomicU8>() };
        ready.store(1, Ordering::Release);
        let expiry = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < expiry {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn test_killed_holder_refuses_shared_segment_without_mutation() {
        if let Ok(name) = std::env::var(CRASH_CHILD_ENV) {
            crash_child_main(&name);
            return;
        }

        use crate::ShmRegion;
        use crate::alloc::spinlock::SpinlockError;

        let name = format!(
            "/c2mem-crash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let layout = BuddyAllocator::checked_layout(CRASH_TEST_DATA_CAPACITY, 4096)
            .expect("supported crash-test geometry");
        let total_size = layout.total_size;
        let region = ShmRegion::create(&name, total_size).expect("parent creates shared region");
        let mut alloc = unsafe { BuddyAllocator::init(region.base_ptr(), layout, 4096) };
        alloc.lock.force_budgets(100_000, 1_000);

        // Genuine two-process setup: the holder is a real child process that
        // maps the same SHM region and acquires the shared lock. `--exact`
        // needs the full test path so the child harness runs only this test,
        // which detects the child-mode env var and becomes the holder.
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "alloc::buddy::tests::test_killed_holder_refuses_shared_segment_without_mutation",
                ])
                .env(CRASH_CHILD_ENV, &name)
                .spawn()
                .expect("spawn holder child"),
        );
        let child_pid = child.0.id();

        // Wait until the child holds the shared lock and signals readiness.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let ready = unsafe { &*alloc.data_ptr(0).cast::<std::sync::atomic::AtomicU8>() };
        while ready.load(Ordering::Acquire) != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "holder child never signaled readiness"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let free_before = alloc.free_bytes();
        let count_before = alloc.alloc_count();

        // Simulate the crash: kill (SIGKILL / TerminateProcess) and reap the
        // child while it holds the shared lock.
        child.0.kill().expect("kill the holder child");
        let status = child.0.wait().expect("reap killed holder child");
        assert!(
            !status.success(),
            "holder must be killed while holding the lock"
        );

        // Refuse the dead holder without rewriting the shared lock word.
        // Its nonzero word continues to block allocation and retirement.
        let err = alloc
            .lock
            .try_lock_budget(std::process::id(), 100_000, 1_000)
            .unwrap_err();
        assert_eq!(err, SpinlockError::DeadHolder { holder: child_pid });
        assert!(!alloc.lock.is_poisoned());
        assert!(!alloc.can_retire());
        assert_eq!(
            alloc.lock.load_word_for_test(),
            child_pid as u64,
            "observer must leave the killed child's lock word untouched"
        );

        // The allocator surface refuses with unchanged allocator state.
        assert_eq!(
            alloc.free_bytes(),
            free_before,
            "refusal must not mutate allocator state"
        );
        assert_eq!(
            alloc.alloc_count(),
            count_before,
            "refusal must not mutate allocator state"
        );
        assert!(
            alloc.alloc(4096).is_none(),
            "alloc refuses the dead-held shared segment"
        );
        let free_err = alloc.free(0, 1, 0).unwrap_err();
        assert!(
            free_err.contains("dead holder"),
            "free describes the dead-holder refusal: {free_err}"
        );
        assert_eq!(alloc.lock.load_word_for_test(), child_pid as u64);
        assert_eq!(alloc.alloc_count(), count_before);
        assert_eq!(alloc.free_bytes(), free_before);

        unlink_shm_region(&name);
    }
}
