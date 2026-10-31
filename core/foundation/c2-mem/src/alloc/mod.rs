//! Pure buddy allocation algorithm.
//!
//! Allocator metadata and locks are private to the pool owner process.

pub mod bitmap;
pub mod buddy;

pub use bitmap::{LevelBitmap, num_levels};
pub use buddy::{Allocation, BuddyAllocator, BuddyLayout, SEGMENT_VERSION};
