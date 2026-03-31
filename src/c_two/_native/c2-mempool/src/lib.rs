//! Unified memory pool with tiered fallback for C-Two IPC.
//!
//! Composes allocators (`c2-alloc`) with OS resources (`c2-segment`)
//! into a pool that manages buddy segments (T1/T2) and dedicated
//! segments (T3).

pub mod buddy_segment;
pub mod config;
pub mod dedicated;
pub mod pool;

pub use buddy_segment::BuddySegment;
pub use config::{PoolAllocation, PoolConfig, PoolStats};
pub use dedicated::DedicatedSegment;
pub use pool::MemPool;
