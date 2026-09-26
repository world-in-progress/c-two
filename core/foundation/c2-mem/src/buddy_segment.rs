//! Buddy-backed SHM segment (T1 allocations).
//!
//! Composes `ShmRegion` (raw SHM lifecycle) with `BuddyAllocator`
//! (block splitting/merging).

use crate::alloc::BuddyAllocator;
use crate::budget::BudgetReservation;
use crate::segment::ShmRegion;

/// A SHM region with a buddy allocator attached.
pub struct BuddySegment {
    region: ShmRegion,
    allocator: BuddyAllocator,
    /// Owner-creation budget charge for this backing. Declared last so it is
    /// dropped after `region` (munmap + shm_unlink), returning the charge
    /// exactly when the backing is actually released.
    budget_guard: Option<BudgetReservation>,
}

unsafe impl Send for BuddySegment {}
unsafe impl Sync for BuddySegment {}

impl BuddySegment {
    /// Create a new buddy segment.
    ///
    /// `size` is the desired minimum data capacity. The geometry is validated
    /// first through [`BuddyAllocator::checked_layout`]: unsupported layouts
    /// (data capacity beyond the wire's `u32` coordinates, header/bitmap
    /// overflow, spans beyond one addressable `isize` mapping) return an
    /// error before any region is created. The caller that created this
    /// backing is responsible for reserving `layout.total_size` bytes in a
    /// [`crate::budget::MemoryBudget`] and attaching the guard with
    /// [`BuddySegment::with_budget_guard`]; this raw creator stays
    /// budget-free.
    pub fn create(name: &str, size: usize, min_block: usize) -> Result<Self, String> {
        let layout = BuddyAllocator::checked_layout(size, min_block).ok_or_else(|| {
            format!(
                "buddy backing geometry unsupported: data capacity {size} with min block {min_block} exceeds the u32 data bound or the addressable mapping span"
            )
        })?;
        let region = ShmRegion::create(name, layout.total_size)?;

        let allocator =
            unsafe { BuddyAllocator::init(region.base_ptr(), layout.total_size, min_block) };

        Ok(Self {
            region,
            allocator,
            budget_guard: None,
        })
    }

    /// Open an existing buddy segment created by another process.
    ///
    /// Peer-opened views never carry an owner-creation budget charge.
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        let region = ShmRegion::open(name, expected_size)?;

        let allocator = unsafe { BuddyAllocator::attach(region.base_ptr(), region.size())? };

        Ok(Self {
            region,
            allocator,
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
    pub unsafe fn data_slice_mut(&mut self, offset: u32, len: usize) -> &mut [u8] {
        let ptr = self.allocator.data_ptr(offset);
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}
