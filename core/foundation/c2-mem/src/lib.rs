//! C-Two shared memory subsystem.
//!
//! Single crate providing the full memory stack for IPC transport:
//!
//! - `alloc` — pure buddy allocation algorithm (no OS deps)
//! - `segment` — POSIX shared memory region lifecycle
//! - `budget` — finite memory budget reservation cells
//! - Pool layer — `MemPool` composing `BuddySegment` + `DedicatedSegment`

pub mod alloc;
pub mod buddy_segment;
pub mod budget;
pub mod config;
pub mod dedicated;
pub mod handle;
pub mod lease;
pub mod pool;
pub mod segment;
pub mod spill;

pub use alloc::{Allocation, BuddyAllocator, SegmentHeader, ShmSpinlock};
pub use buddy_segment::BuddySegment;
pub use budget::{
    BudgetCellSnapshot, BudgetError, BudgetKind, BudgetReservation, BudgetSnapshot, MemoryBudget,
};
pub use config::{PoolAllocation, PoolConfig, PoolStats};
pub use dedicated::DedicatedSegment;
pub use handle::MemHandle;
pub use lease::{
    BufferLeaseGuard, BufferLeaseMeta, BufferLeaseSnapshot, BufferLeaseStats, BufferLeaseTracker,
    BufferStorage, DirectionLeaseStats, LeaseDirection, LeaseRetention, StorageLeaseStats,
};
pub use pool::{FreeResult, MemPool};
pub use segment::ShmRegion;
pub use spill::{available_physical_memory, create_file_spill, should_spill};
