//! Pure buddy allocation algorithm.
//!
//! Operates on raw `*mut u8` pointers with no awareness of SHM,
//! files, or any OS resource. The caller provides memory; this
//! crate manages block splitting and merging.

pub mod buddy;
pub mod bitmap;
pub mod spinlock;

pub use buddy::{Allocation, BuddyAllocator, SegmentHeader, HEADER_ALIGN, SEGMENT_MAGIC, SEGMENT_VERSION};
pub use bitmap::{LevelBitmap, num_levels, total_bitmap_bytes};
pub use spinlock::ShmSpinlock;
