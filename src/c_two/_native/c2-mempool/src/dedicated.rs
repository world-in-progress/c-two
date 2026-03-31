//! Dedicated SHM segment for oversized allocations (T3).
//!
//! These bypass the buddy allocator — the entire region serves
//! one allocation.

use c2_segment::ShmRegion;

/// A dedicated SHM segment (no buddy allocator).
pub struct DedicatedSegment {
    region: ShmRegion,
}

unsafe impl Send for DedicatedSegment {}
unsafe impl Sync for DedicatedSegment {}

impl DedicatedSegment {
    /// Create a dedicated segment for a single large allocation.
    pub fn create(name: &str, size: usize) -> Result<Self, String> {
        // Page-align the size.
        let page_size = 4096;
        let aligned_size = (size + page_size - 1) & !(page_size - 1);
        let region = ShmRegion::create(name, aligned_size)?;
        Ok(Self { region })
    }

    /// Open an existing dedicated segment.
    pub fn open(name: &str, size: usize) -> Result<Self, String> {
        let region = ShmRegion::open(name, size)?;
        Ok(Self { region })
    }

    pub fn data_ptr(&self) -> *mut u8 {
        self.region.base_ptr()
    }

    pub fn size(&self) -> usize {
        self.region.size()
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }
}
