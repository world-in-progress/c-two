//! POSIX shared memory region lifecycle management.
//!
//! Provides `ShmRegion` — a raw memory-mapped region with no
//! allocator attached.  The composition layer (`c2-mempool`)
//! pairs regions with allocators.

pub mod shm;

pub use shm::ShmRegion;
