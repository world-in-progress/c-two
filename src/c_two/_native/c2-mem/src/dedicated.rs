//! Dedicated SHM segment for oversized allocations (T3).
//!
//! These bypass the buddy allocator — the entire region serves
//! one allocation.  A 64-byte header at the start of the SHM region
//! carries an `AtomicU32 read_done` flag for cross-process lifecycle
//! coordination (iceoryx2-inspired).

use crate::segment::ShmRegion;
use std::sync::atomic::{AtomicU32, Ordering};

/// Header size in bytes (cache-line aligned).
pub const DEDICATED_HEADER_SIZE: usize = 64;

const PAGE_SIZE: usize = 4096;

fn page_align(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// A dedicated SHM segment (no buddy allocator).
///
/// Layout: `[64B header | payload data]`
/// Header byte 0..4: `read_done: AtomicU32` (0 = unread, 1 = done)
/// Header byte 4..64: reserved padding
pub struct DedicatedSegment {
    region: ShmRegion,
}

unsafe impl Send for DedicatedSegment {}
unsafe impl Sync for DedicatedSegment {}

impl DedicatedSegment {
    /// Create a dedicated segment for a single large allocation.
    ///
    /// `data_size` is the usable payload capacity.  The actual SHM region
    /// is `page_align(data_size + DEDICATED_HEADER_SIZE)`.
    pub fn create(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::create(name, aligned)?;

        // Initialize header: read_done = 0
        let hdr = region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(0, Ordering::Release); }

        Ok(Self { region })
    }

    /// Open an existing dedicated segment.
    ///
    /// `data_size` is the expected payload capacity (used for size validation).
    pub fn open(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::open(name, aligned)?;
        Ok(Self { region })
    }

    /// Pointer to the start of the payload data region (past the header).
    pub fn data_ptr(&self) -> *mut u8 {
        unsafe { self.region.base_ptr().add(DEDICATED_HEADER_SIZE) }
    }

    /// Usable data capacity in bytes (excludes the 64-byte header).
    pub fn data_size(&self) -> usize {
        self.region.size() - DEDICATED_HEADER_SIZE
    }

    /// Total SHM region size including header.
    pub fn size(&self) -> usize {
        self.region.size()
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    /// Signal that the reader has finished consuming the payload.
    ///
    /// Called by the non-owner (reader) side after reading is complete.
    /// The owner (creator) polls `is_read_done()` during GC to decide
    /// when it is safe to `shm_unlink`.
    pub fn mark_read_done(&self) {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(1, Ordering::Release); }
    }

    /// Check whether the reader has signalled completion.
    pub fn is_read_done(&self) -> bool {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).load(Ordering::Acquire) == 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_flag_lifecycle() {
        let name = format!("/c2ded_test_{}", std::process::id());
        let data_size = 4096;

        let creator = DedicatedSegment::create(&name, data_size).unwrap();
        assert!(!creator.is_read_done());

        unsafe { *creator.data_ptr() = 0xAB; }

        let reader = DedicatedSegment::open(&name, data_size).unwrap();
        assert_eq!(unsafe { *reader.data_ptr() }, 0xAB);
        assert!(!reader.is_read_done());

        reader.mark_read_done();
        assert!(reader.is_read_done());
        assert!(creator.is_read_done());

        assert!(creator.data_size() >= data_size);

        drop(reader);
        drop(creator);
    }

    #[test]
    fn test_data_ptr_offset() {
        let name = format!("/c2ded_off_{}", std::process::id());
        let seg = DedicatedSegment::create(&name, 8192).unwrap();

        let base = seg.region.base_ptr() as usize;
        let data = seg.data_ptr() as usize;
        assert_eq!(data - base, DEDICATED_HEADER_SIZE);

        drop(seg);
    }
}
