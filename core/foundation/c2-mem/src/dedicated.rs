//! Dedicated SHM segment for oversized allocations (T3).
//!
//! These bypass the buddy allocator — the entire region serves
//! one allocation.  A 64-byte header at the start of the SHM region
//! carries an `AtomicU32 read_done` flag for cross-process lifecycle
//! coordination (iceoryx2-inspired).

use crate::budget::BudgetReservation;
use crate::segment::ShmRegion;
use std::sync::atomic::{AtomicU32, Ordering};

/// Header size in bytes (cache-line aligned).
pub const DEDICATED_HEADER_SIZE: usize = 64;

const PAGE_SIZE: usize = 4096;

/// A dedicated SHM segment (no buddy allocator).
///
/// Layout: `[64B header | payload data]`
/// Header byte 0..4: `read_done: AtomicU32` (0 = unread, 1 = done)
/// Header byte 4..64: reserved padding
pub struct DedicatedSegment {
    region: ShmRegion,
    /// Owner-creation budget charge for this backing. Declared last so it is
    /// dropped after `region` (munmap + shm_unlink). A freed-but-pending-GC
    /// entry keeps its segment, and therefore its charge, until `gc_dedicated`
    /// actually removes it.
    budget_guard: Option<BudgetReservation>,
}

unsafe impl Send for DedicatedSegment {}
unsafe impl Sync for DedicatedSegment {}

impl DedicatedSegment {
    /// Checked total backing size for a dedicated segment with `data_size`
    /// usable payload bytes: `page_align(data_size + header)`. Returns `None`
    /// when the geometry overflows the platform address space or the addressable
    /// `isize` mapping span.
    ///
    /// This is the single owner of the dedicated geometry formula — creation,
    /// size validation, and budget charging all derive their byte count from
    /// it, so charged bytes always equal mapped bytes.
    pub fn required_shm_size(data_size: usize) -> Option<usize> {
        let total = data_size.checked_add(DEDICATED_HEADER_SIZE)?;
        let padded = total.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        (padded <= isize::MAX as usize).then_some(padded)
    }

    /// Create a dedicated segment for a single large allocation.
    ///
    /// `data_size` is the usable payload capacity. The actual SHM region is
    /// `required_shm_size(data_size)`; unsupported overflowing geometries
    /// return an error before any region is created. The creator owns the
    /// budget reservation for those bytes and attaches it with
    /// [`DedicatedSegment::with_budget_guard`].
    pub fn create(name: &str, data_size: usize) -> Result<Self, String> {
        let aligned = Self::required_shm_size(data_size).ok_or_else(|| {
            format!("dedicated backing geometry unsupported for {data_size} payload bytes")
        })?;
        let region = ShmRegion::create(name, aligned)?;

        // Initialize header: read_done = 0
        let hdr = region.base_ptr() as *const AtomicU32;
        unsafe {
            (*hdr).store(0, Ordering::Release);
        }

        Ok(Self {
            region,
            budget_guard: None,
        })
    }

    /// Open an existing dedicated segment.
    ///
    /// `data_size` is the expected payload capacity (used for size validation).
    /// Peer-opened views never carry an owner-creation budget charge.
    pub fn open(name: &str, data_size: usize) -> Result<Self, String> {
        let aligned = Self::required_shm_size(data_size).ok_or_else(|| {
            format!("dedicated backing geometry unsupported for {data_size} payload bytes")
        })?;
        let region = ShmRegion::open(name, aligned)?;
        Ok(Self {
            region,
            budget_guard: None,
        })
    }

    /// Attach this backing's single owner-creation budget reservation.
    ///
    /// Crate-private and single-shot by invariant: the reservation is
    /// attached exactly once, immediately after a budgeted creation succeeds
    /// and before the owner is published into a pool, so no caller can
    /// replace or drop the charge while the backing is live. A second
    /// attachment is an internal bug and panics rather than silently
    /// releasing the live charge. There is no way to clear the guard.
    pub(crate) fn with_budget_guard(mut self, guard: BudgetReservation) -> Self {
        assert!(
            self.budget_guard.is_none(),
            "a backing carries at most one owner-creation reservation"
        );
        self.budget_guard = Some(guard);
        self
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
        unsafe {
            (*hdr).store(1, Ordering::Release);
        }
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

        unsafe {
            *creator.data_ptr() = 0xAB;
        }

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

    #[test]
    fn test_required_region_size_rejects_overflow_before_mapping() {
        // Header addition alone overflows.
        assert_eq!(DedicatedSegment::required_shm_size(usize::MAX), None);
        // data + header == usize::MAX: the page round-up overflows even though
        // the addition itself fits.
        assert_eq!(
            DedicatedSegment::required_shm_size(usize::MAX - DEDICATED_HEADER_SIZE),
            None
        );

        // Largest representable input: (data + header) + (PAGE_SIZE - 1) lands
        // exactly below usize::MAX, but exceeds the addressable isize mapping span.
        let last = usize::MAX - (PAGE_SIZE - 1) - DEDICATED_HEADER_SIZE;
        assert_eq!(DedicatedSegment::required_shm_size(last), None);
        // One byte past that boundary overflows the round-up.
        assert_eq!(DedicatedSegment::required_shm_size(last + 1), None);

        // Sanity for ordinary sizes: unchanged behavior.
        assert_eq!(DedicatedSegment::required_shm_size(8192), Some(12288));
        assert_eq!(DedicatedSegment::required_shm_size(1), Some(PAGE_SIZE));
    }

    #[test]
    fn test_create_and_open_reject_unrepresentable_sizes_via_checked_logic() {
        let name = format!("/c2ded_ovf_{}", std::process::id());
        // `usize::MAX - HEADER_SIZE` overflows during page alignment; the
        // checked helper must reject it before any mapping attempt in both
        // debug and release builds (no wrapping arithmetic anywhere).
        for size in [
            usize::MAX,
            usize::MAX - DEDICATED_HEADER_SIZE,
            usize::MAX - (PAGE_SIZE - 1) - DEDICATED_HEADER_SIZE + 1,
        ] {
            let err = match DedicatedSegment::create(&name, size) {
                Err(err) => err,
                Ok(_) => panic!("create({size}) must fail the checked guard"),
            };
            assert!(
                err.contains("geometry unsupported"),
                "create({size}) must fail the checked guard, got: {err}"
            );
            let err = match DedicatedSegment::open(&name, size) {
                Err(err) => err,
                Ok(_) => panic!("open({size}) must fail the checked guard"),
            };
            assert!(
                err.contains("geometry unsupported"),
                "open({size}) must fail the checked guard, got: {err}"
            );
        }
    }
}
