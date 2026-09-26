//! Pure buddy allocation algorithm.
//!
//! Operates on raw `*mut u8` pointers with no awareness of SHM,
//! files, or any OS resource.

pub mod bitmap;
pub mod buddy;
pub mod spinlock;

pub use bitmap::{LevelBitmap, num_levels, total_bitmap_bytes};
pub use buddy::{
    Allocation, BuddyAllocator, BuddyLayout, HEADER_ALIGN, SEGMENT_MAGIC, SEGMENT_VERSION,
    SegmentHeader,
};
pub use spinlock::ShmSpinlock;
