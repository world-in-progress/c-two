//! C-Two native extensions — unified PyO3 entry point.
//!
//! This crate exposes the memory pool and the HTTP relay
//! as a single Python extension module: `c_two._native`.
//!
//! Python usage:
//! ```python
//! from c_two._native import MemPool, PoolConfig   # memory pool
//! from c_two._native import NativeRelay            # relay
//! ```

#[cfg(feature = "python")]
mod mem_ffi;
#[cfg(feature = "python")]
mod relay_ffi;
#[cfg(feature = "python")]
mod wire_ffi;

#[cfg(feature = "python")]
use pyo3::prelude::*;

/// The unified `c_two._native` Python module.
///
/// Registers all memory pool classes + relay classes at the
/// top level of the module (flat namespace, not submodules).
#[cfg(feature = "python")]
#[pymodule(name = "_native", gil_used = false)]
fn c2_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    mem_ffi::register_module(m)?;
    relay_ffi::register_module(m)?;
    wire_ffi::register_module(m)?;
    Ok(())
}
