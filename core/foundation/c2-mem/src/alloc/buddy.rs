//! Owner-local buddy allocation. No allocator state lives in the mapping.

use crate::alloc::bitmap::{LevelBitmap, num_levels};
use std::sync::Mutex;

/// SHM format negotiated by the IPC handshake; no version is stored in SHM.
pub const SEGMENT_VERSION: u16 = 4;

/// Trusted local allocation metadata, never remote release authority.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    pub offset: u32,
    pub actual_size: u32,
    pub level: u16,
}

/// Validated power-of-two capacity. SHM contains only payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyLayout {
    pub data_size: usize,
    pub total_size: usize,
}

struct State {
    bitmaps: Vec<LevelBitmap>,
    alloc_count: u32,
    free_bytes: u64,
}

/// Local state for one owner-created mapping. Critical sections only split
/// and merge free bits and update counters; they never call I/O or user code.
pub struct BuddyAllocator {
    base: *mut u8,
    data_size: usize,
    min_block: usize,
    state: Mutex<State>,
}

// The pointer does not grant safe payload access. The local mutex serializes
// allocator state; payload borrows are governed by the allocation owner.
unsafe impl Send for BuddyAllocator {}
unsafe impl Sync for BuddyAllocator {}

impl BuddyAllocator {
    /// Create local allocator state without reading or writing the backing.
    ///
    /// # Safety
    /// `base` names an exclusively owner-created writable region of
    /// `layout.total_size` bytes that outlives this allocator. No other
    /// allocator may manage it. `layout` is from `checked_layout`.
    pub unsafe fn init(base: *mut u8, layout: BuddyLayout, min_block: usize) -> Self {
        let levels = num_levels(layout.data_size, min_block);
        let mut bitmaps: Vec<_> = (0..levels)
            .map(|level| LevelBitmap::new(1usize << level))
            .collect();
        bitmaps[0].free_one(0);
        Self {
            base,
            data_size: layout.data_size,
            min_block,
            state: Mutex::new(State {
                bitmaps,
                alloc_count: 0,
                free_bytes: layout.data_size as u64,
            }),
        }
    }

    pub fn checked_layout(min_data_capacity: usize, min_block: usize) -> Option<BuddyLayout> {
        if min_block == 0 || !min_block.is_power_of_two() {
            return None;
        }
        let data_size = min_data_capacity.checked_next_power_of_two()?;
        if min_data_capacity == 0
            || data_size > u32::MAX as usize
            || data_size > isize::MAX as usize
            || min_block > data_size
        {
            return None;
        }
        Some(BuddyLayout {
            data_size,
            total_size: data_size,
        })
    }

    pub fn alloc(&self, size: usize) -> Option<Allocation> {
        if size == 0 || size > self.data_size {
            return None;
        }
        let actual_size = size.checked_next_power_of_two()?.max(self.min_block);
        let target_level = (self.data_size / actual_size).ilog2() as usize;
        let mut state = self.state.lock().expect("buddy allocator mutex poisoned");
        let (donor_level, mut index) = (0..=target_level)
            .rev()
            .find_map(|level| state.bitmaps[level].alloc_one().map(|index| (level, index)))?;
        for level in donor_level + 1..=target_level {
            index *= 2;
            state.bitmaps[level].free_one(index + 1);
        }
        state.alloc_count += 1;
        state.free_bytes -= actual_size as u64;
        Some(Allocation {
            offset: (index * actual_size) as u32,
            actual_size: actual_size as u32,
            level: target_level as u16,
        })
    }

    /// Release using the exact level minted with the local allocation.
    ///
    /// # Safety
    /// The caller uniquely owns this live allocation, the offset and level
    /// are unchanged, and all accesses (including borrowed peer reads) ended.
    pub unsafe fn free(&self, offset: u32, level: u16) {
        let mut level = level as usize;
        let size = self.data_size >> level;
        let mut index = offset as usize / size;
        let mut state = self.state.lock().expect("buddy allocator mutex poisoned");
        state.bitmaps[level].free_one(index);
        state.alloc_count -= 1;
        state.free_bytes += size as u64;
        while level > 0 && state.bitmaps[level].is_free(index ^ 1) {
            state.bitmaps[level].mark_used(index);
            state.bitmaps[level].mark_used(index ^ 1);
            level -= 1;
            index /= 2;
            state.bitmaps[level].free_one(index);
        }
    }

    pub fn data_ptr(&self, offset: u32) -> *mut u8 {
        self.base.wrapping_add(offset as usize)
    }
    pub fn data_size(&self) -> usize {
        self.data_size
    }
    pub fn min_block(&self) -> usize {
        self.min_block
    }
    pub fn alloc_count(&self) -> u32 {
        self.state
            .lock()
            .expect("buddy allocator mutex poisoned")
            .alloc_count
    }
    pub fn free_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("buddy allocator mutex poisoned")
            .free_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator(size: usize) -> (BuddyAllocator, Vec<u8>) {
        let layout = BuddyAllocator::checked_layout(size, 4096).unwrap();
        let mut bytes = vec![0xA5; layout.total_size];
        let allocator = unsafe { BuddyAllocator::init(bytes.as_mut_ptr(), layout, 4096) };
        (allocator, bytes)
    }

    #[test]
    fn allocation_never_touches_payload_and_offset_starts_at_zero() {
        let (allocator, bytes) = allocator(8192);
        let block = allocator.alloc(8192).unwrap();
        assert_eq!(block.offset, 0);
        assert_eq!(allocator.data_ptr(0), bytes.as_ptr().cast_mut());
        unsafe { allocator.free(block.offset, block.level) };
        assert!(bytes.iter().all(|byte| *byte == 0xA5));
    }

    #[test]
    fn split_merge_fragmentation_and_capacity() {
        let (allocator, _bytes) = allocator(64 * 1024);
        let mut blocks = Vec::new();
        for index in 0..16 {
            let block = allocator.alloc(4000).unwrap();
            assert_eq!(block.actual_size, 4096);
            assert_eq!(block.offset, index * 4096);
            blocks.push(block);
        }
        assert!(allocator.alloc(1).is_none());
        for block in blocks.iter().step_by(2) {
            unsafe { allocator.free(block.offset, block.level) };
        }
        assert!(allocator.alloc(8192).is_none());
        for block in blocks.iter().skip(1).step_by(2) {
            unsafe { allocator.free(block.offset, block.level) };
        }
        assert_eq!(allocator.alloc_count(), 0);
        assert_eq!(allocator.free_bytes(), 64 * 1024);
        let whole = allocator.alloc(64 * 1024).unwrap();
        assert_eq!(whole.level, 0);
        unsafe { allocator.free(whole.offset, whole.level) };
    }

    #[test]
    fn concurrent_local_allocations_are_disjoint() {
        let (allocator, _bytes) = allocator(64 * 1024);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..8 {
                let allocator = &allocator;
                let barrier = &barrier;
                threads.push(scope.spawn(move || {
                    let block = allocator.alloc(8192).unwrap();
                    barrier.wait();
                    unsafe { allocator.free(block.offset, block.level) };
                    block.offset
                }));
            }
            let mut offsets: Vec<_> = threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            assert_eq!(offsets.len(), 8);
        });
        assert_eq!(allocator.free_bytes(), 64 * 1024);
        assert_eq!(allocator.alloc_count(), 0);
    }

    #[test]
    fn poisoned_local_state_fails_without_recovery() {
        let (allocator, _bytes) = allocator(8192);
        let _ = std::panic::catch_unwind(|| {
            let _guard = allocator.state.lock().unwrap();
            panic!("injected local panic");
        });
        assert!(std::panic::catch_unwind(|| allocator.alloc(4096)).is_err());
        assert!(std::panic::catch_unwind(|| allocator.alloc_count()).is_err());
    }

    #[test]
    fn checked_geometry_and_request_overflow() {
        for (size, min) in [
            (0, 4096),
            (8192, 0),
            (8192, 3000),
            (8192, 16384),
            (usize::MAX, 4096),
        ] {
            assert_eq!(BuddyAllocator::checked_layout(size, min), None);
        }
        let layout = BuddyAllocator::checked_layout(8193, 4096).unwrap();
        assert_eq!(layout.total_size, 16384);
        assert_eq!(layout.data_size, layout.total_size);
        let (allocator, _bytes) = allocator(8192);
        assert!(allocator.alloc(0).is_none());
        assert!(allocator.alloc(usize::MAX).is_none());
    }
}
