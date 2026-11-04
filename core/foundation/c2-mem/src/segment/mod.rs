//! Platform-owned shared memory region lifecycle management.

pub mod shm;

pub use shm::ShmRegion;

/// A mapping's creation charge follows its last actual owner, including a
/// local allocation that outlives the pool's cache entry.
pub(crate) struct ChargedMapping {
    pub(crate) region: ShmRegion,
    pub(crate) reservation: Option<crate::BudgetReservation>,
}
