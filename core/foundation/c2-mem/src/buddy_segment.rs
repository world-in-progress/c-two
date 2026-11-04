//! Buddy-backed SHM segment (T1 allocations).
//!
//! Composes `ShmRegion` (raw SHM lifecycle) with `BuddyAllocator`
//! (block splitting/merging).

use crate::alloc::BuddyAllocator;
use crate::budget::BudgetReservation;
use crate::segment::ChargedMapping;
use crate::segment::ShmRegion;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct CacheState {
    mapping: Option<Arc<ChargedMapping>>,
    retain: bool,
    idle_since: Option<Instant>,
}

/// A payload mapping, with local allocator state only in its owner process.
pub struct BuddySegment {
    name: String,
    size: usize,
    cache: Mutex<CacheState>,
    allocator: Option<BuddyAllocator>,
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
            name: region.name().to_owned(),
            size: region.size(),
            cache: Mutex::new(CacheState {
                mapping: Some(Arc::new(ChargedMapping {
                    region,
                    reservation: None,
                })),
                retain: true,
                idle_since: None,
            }),
            allocator: Some(allocator),
        })
    }

    /// Open an existing buddy segment created by another process.
    ///
    /// Peer-opened views never carry an owner-creation budget charge.
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        let region = ShmRegion::open(name, expected_size)?;

        Ok(Self {
            name: region.name().to_owned(),
            size: region.size(),
            cache: Mutex::new(CacheState {
                mapping: Some(Arc::new(ChargedMapping {
                    region,
                    reservation: None,
                })),
                retain: true,
                idle_since: None,
            }),
            allocator: None,
        })
    }

    /// Attach this backing's single owner-creation budget reservation.
    ///
    /// The pool attaches this once after budgeted creation and before
    /// publishing the backing.
    pub(crate) fn with_budget_guard(mut self, guard: BudgetReservation) -> Self {
        Arc::get_mut(
            self.cache
                .get_mut()
                .expect("buddy cache poisoned")
                .mapping
                .as_mut()
                .unwrap(),
        )
        .expect("backing not published")
        .reservation = Some(guard);
        self
    }

    pub(crate) fn pin(&self) -> Arc<ChargedMapping> {
        self.try_pin().expect("live allocation pins its mapping")
    }

    pub(crate) fn try_pin(&self) -> Option<Arc<ChargedMapping>> {
        self.cache
            .lock()
            .expect("buddy cache poisoned")
            .mapping
            .clone()
    }

    pub(crate) fn is_mapped(&self) -> bool {
        self.cache
            .lock()
            .expect("buddy cache poisoned")
            .mapping
            .is_some()
    }

    pub(crate) fn alloc(&self, size: usize) -> Option<crate::Allocation> {
        let mut cache = self.cache.lock().expect("buddy cache poisoned");
        cache.mapping.as_ref()?;
        let allocation = self.allocator().alloc(size)?;
        cache.idle_since = None;
        Some(allocation)
    }

    /// Release the exact local allocation after its owner's last borrow.
    pub(crate) unsafe fn free(&self, offset: u32, level: u16) {
        let removed = {
            let mut cache = self.cache.lock().expect("buddy cache poisoned");
            unsafe { self.allocator().free(offset, level) };
            if self.allocator().alloc_count() == 0 {
                cache.idle_since = Some(Instant::now());
                if !cache.retain {
                    cache.mapping.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(removed);
    }

    pub(crate) fn set_retention(&self, retain: bool) {
        let removed = {
            let mut cache = self.cache.lock().expect("buddy cache poisoned");
            cache.retain = retain;
            if !retain && self.allocator().alloc_count() == 0 {
                cache.mapping.take()
            } else {
                None
            }
        };
        drop(removed);
    }

    pub(crate) fn expired(&self, now: Instant, delay: Duration) -> bool {
        let mut cache = self.cache.lock().expect("buddy cache poisoned");
        self.allocator().alloc_count() == 0
            && now.duration_since(*cache.idle_since.get_or_insert(now)) >= delay
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
        &self.name
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Raw address metadata, without mapping retention or payload access.
    /// An owner's allocator keeps the address even after backing retirement;
    /// safe payload access requires the allocation owner's scoped borrow.
    pub fn base_ptr(&self) -> *mut u8 {
        match &self.allocator {
            Some(allocator) => allocator.data_ptr(0),
            None => self.pin().region.base_ptr(),
        }
    }
}
