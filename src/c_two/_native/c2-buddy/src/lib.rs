//! C-Two SHM Buddy Allocator
//!
//! A cross-process buddy allocator over POSIX shared memory, designed for
//! the C-Two IPC transport. Provides zero-syscall dynamic allocation
//! within pre-mapped SHM segments.

pub mod allocator;
pub mod bitmap;
pub mod pool;
pub mod segment;
pub mod spinlock;
