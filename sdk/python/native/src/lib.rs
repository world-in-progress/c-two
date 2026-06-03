//! C-Two Python SDK native extension.
//!
//! This crate exposes the Rust runtime to Python as `c_two._native`.
//! It is intentionally owned by `sdk/python`, not `core`, because it uses
//! PyO3 and Python-specific buffer, exception, and module semantics.

#[cfg(feature = "python")]
mod client_ffi;
#[cfg(feature = "python")]
mod config_ffi;
#[cfg(feature = "python")]
mod error_ffi;
#[cfg(feature = "python")]
mod http_ffi;
#[cfg(feature = "python")]
mod ipc_control_ffi;
#[cfg(feature = "python")]
mod lease_ffi;
#[cfg(feature = "python")]
mod mem_ffi;
#[cfg(feature = "python")]
mod response_backing;
#[cfg(feature = "python")]
mod route_concurrency_ffi;
#[cfg(feature = "python")]
mod runtime_session_ffi;
#[cfg(feature = "python")]
mod server_ffi;
#[cfg(feature = "python")]
pub(crate) mod shm_buffer;
#[cfg(feature = "python")]
mod wire_ffi;
#[cfg(feature = "python")]
mod writable_sink;

#[cfg(feature = "python")]
use pyo3::prelude::*;

/// The unified `c_two._native` Python module.
///
/// Registers all native classes and helpers at the top level of the
/// module (flat namespace, not submodules).
#[cfg(feature = "python")]
#[pymodule(name = "_native", gil_used = false)]
fn c2_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error_ffi::register_module(m)?;
    config_ffi::register_module(m)?;
    client_ffi::register_module(m)?;
    ipc_control_ffi::register_module(m)?;
    http_ffi::register_module(m)?;
    lease_ffi::register_module(m)?;
    mem_ffi::register_module(m)?;
    response_backing::register_module(m)?;
    route_concurrency_ffi::register_module(m)?;
    runtime_session_ffi::register_module(m)?;
    server_ffi::register_module(m)?;
    shm_buffer::register_module(m)?;
    wire_ffi::register_module(m)?;
    Ok(())
}
