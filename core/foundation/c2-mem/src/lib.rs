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
pub mod peer;
pub mod pool;
mod pressure;
pub mod retention;
pub mod segment;
pub mod spill;

pub use alloc::{Allocation, BuddyAllocator, BuddyLayout};
pub use buddy_segment::BuddySegment;
pub use budget::{
    BudgetCellSnapshot, BudgetError, BudgetKind, BudgetObserver, BudgetObserverWeak,
    BudgetReservation, BudgetSnapshot, MemoryBudget,
};
pub use config::{PoolAllocation, PoolConfig, PoolStats};
pub use dedicated::DedicatedSegment;
pub use handle::MemHandle;
pub use peer::{PeerMappingCache, PeerMappingError, PeerRead};
pub use pool::MemPool;
pub use retention::{
    RetentionBudget, RetentionError, RetentionPermit, RetentionRejectReason, RetentionSnapshot,
};
pub use segment::ShmRegion;
pub use spill::{available_physical_memory, create_file_spill};
