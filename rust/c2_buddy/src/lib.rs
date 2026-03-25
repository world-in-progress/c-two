//! C-Two SHM Buddy Allocator
//!
//! A cross-process buddy allocator over POSIX shared memory, designed for
//! the C-Two IPC v3 transport. Provides zero-syscall dynamic allocation
//! within pre-mapped SHM segments.

pub mod allocator;
pub mod bitmap;
#[cfg(feature = "python")]
pub mod ffi;
pub mod pool;
pub mod segment;
pub mod spinlock;

#[cfg(feature = "python")]
use pyo3::prelude::*;

/// Python module entry point.
#[cfg(feature = "python")]
#[pymodule(gil_used = false)]
fn c2_buddy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    ffi::register_module(m)?;
    Ok(())
}
