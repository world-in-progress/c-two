//! Buddy-backed SHM segment (T1 allocations).
//!
//! Composes `ShmRegion` (raw SHM lifecycle) with `BuddyAllocator`
//! (block splitting/merging).

use crate::alloc::BuddyAllocator;
use crate::segment::ShmRegion;

/// A SHM region with a buddy allocator attached.
pub struct BuddySegment {
    region: ShmRegion,
    allocator: BuddyAllocator,
}

unsafe impl Send for BuddySegment {}
unsafe impl Sync for BuddySegment {}

impl BuddySegment {
    /// Create a new buddy segment.
    ///
    /// `size` is the desired minimum data capacity. The actual SHM will be
    /// larger to accommodate the header + bitmaps.
    pub fn create(name: &str, size: usize, min_block: usize) -> Result<Self, String> {
        let actual_size = BuddyAllocator::required_shm_size(size, min_block);
        let region = ShmRegion::create(name, actual_size)?;

        let allocator = unsafe {
            BuddyAllocator::init(region.base_ptr(), actual_size, min_block)
        };

        Ok(Self { region, allocator })
    }

    /// Open an existing buddy segment created by another process.
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        let region = ShmRegion::open(name, expected_size)?;

        let allocator = unsafe {
            BuddyAllocator::attach(region.base_ptr(), region.size())?
        };

        Ok(Self { region, allocator })
    }

    pub fn allocator(&self) -> &BuddyAllocator {
        &self.allocator
    }

    pub fn region(&self) -> &ShmRegion {
        &self.region
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    pub fn size(&self) -> usize {
        self.region.size()
    }

    pub fn base_ptr(&self) -> *mut u8 {
        self.region.base_ptr()
    }

    pub fn is_owner(&self) -> bool {
        self.region.is_owner()
    }

    /// Get a slice of the data region at given offset and length.
    ///
    /// # Safety
    /// The caller must ensure the offset+len is within a valid allocation.
    pub unsafe fn data_slice(&self, offset: u32, len: usize) -> &[u8] {
        let ptr = self.allocator.data_ptr(offset);
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// Get a mutable slice of the data region at given offset and length.
    ///
    /// # Safety
    /// The caller must ensure exclusive access to this region.
    pub unsafe fn data_slice_mut(&self, offset: u32, len: usize) -> &mut [u8] {
        let ptr = self.allocator.data_ptr(offset);
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}
