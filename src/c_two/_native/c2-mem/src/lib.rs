//! C-Two shared memory subsystem.
//!
//! Single crate providing the full memory stack for IPC transport:
//!
//! - `alloc` — pure buddy allocation algorithm (no OS deps)
//! - `segment` — POSIX shared memory region lifecycle
//! - Pool layer — `MemPool` composing `BuddySegment` + `DedicatedSegment`

pub mod alloc;
pub mod segment;
pub mod buddy_segment;
pub mod config;
pub mod dedicated;
pub mod pool;

pub use alloc::{BuddyAllocator, Allocation, SegmentHeader, ShmSpinlock};
pub use segment::ShmRegion;
pub use buddy_segment::BuddySegment;
pub use config::{PoolAllocation, PoolConfig, PoolStats};
pub use dedicated::DedicatedSegment;
pub use pool::MemPool;
