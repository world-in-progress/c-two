//! Buddy-backed SHM segment (T1 allocations).
//!
//! Composes `ShmRegion` (raw SHM lifecycle) with `BuddyAllocator`
//! (block splitting/merging).

use crate::alloc::BuddyAllocator;
use crate::budget::BudgetReservation;
use crate::segment::ShmRegion;

/// A payload mapping, with local allocator state only in its owner process.
pub struct BuddySegment {
    region: ShmRegion,
    allocator: Option<BuddyAllocator>,
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
    /// (data capacity beyond the wire's `u32` coordinates,
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

        let allocator = unsafe { BuddyAllocator::init(region.base_ptr(), layout, min_block) };

        Ok(Self {
            region,
            allocator: Some(allocator),
            budget_guard: None,
        })
    }

    /// Open an existing buddy segment created by another process.
    ///
    /// Peer-opened views never carry an owner-creation budget charge.
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        let region = ShmRegion::open(name, expected_size)?;

        Ok(Self {
            region,
            allocator: None,
            budget_guard: None,
        })
    }

    /// Attach this backing's single owner-creation budget reservation.
    ///
    /// The pool attaches this once after budgeted creation and before
    /// publishing the backing.
    pub(crate) fn with_budget_guard(mut self, guard: BudgetReservation) -> Self {
        self.budget_guard = Some(guard);
        self
    }

    pub fn allocator(&self) -> &BuddyAllocator {
        self.allocator
            .as_ref()
            .expect("peer mappings have no allocator")
    }

    pub fn is_owner(&self) -> bool {
        self.allocator.is_some()
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
}
