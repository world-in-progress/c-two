//! C-Two SHM Buddy Allocator
//!
//! A cross-process buddy allocator over POSIX shared memory, designed for
//! the C-Two IPC v3 transport. Provides zero-syscall dynamic allocation
//! within pre-mapped SHM segments.

pub mod allocator;
pub mod bitmap;
pub mod ffi;
pub mod pool;
pub mod segment;
pub mod spinlock;

use pyo3::prelude::*;

/// Python module entry point.
#[pymodule(gil_used = false)]
fn c2_buddy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    ffi::register_module(m)?;
    Ok(())
}
