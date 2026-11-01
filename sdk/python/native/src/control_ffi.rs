use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::endpoint_ffi::{PyLocalEndpointContext, resolve_context};

pub(crate) fn timeout_duration(timeout_seconds: f64) -> PyResult<Duration> {
    crate::runtime_session_ffi::checked_timeout(timeout_seconds).map_err(PyValueError::new_err)
}

#[pyfunction]
#[pyo3(signature = (address, *, root=None, context=None))]
fn ipc_endpoint_name(
    address: &str,
    root: Option<String>,
    context: Option<&PyLocalEndpointContext>,
) -> PyResult<String> {
    let context = resolve_context(root, context)?;
    let endpoint = c2_core::direct_ipc_endpoint(address, Some(&context));
    endpoint
        .map(|endpoint| endpoint.os_name().to_string_lossy().into_owned())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
#[pyo3(signature = (address, timeout_seconds=0.5, *, root=None, context=None))]
fn ipc_ping(
    py: Python<'_>,
    address: &str,
    timeout_seconds: f64,
    root: Option<String>,
    context: Option<&PyLocalEndpointContext>,
) -> PyResult<bool> {
    let timeout = timeout_duration(timeout_seconds)?;
    let context = resolve_context(root, context)?;
    py.detach(|| c2_core::ping_direct_ipc(address, Some(&context), timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Return "initiated", "absent" or "unconfirmed" for a direct shutdown request.
/// Only "initiated" confirms initiation; completion uses the original Host receipt.
/// "absent" means this endpoint has no listener.
#[pyfunction]
#[pyo3(signature = (address, timeout_seconds=0.5, *, root=None, context=None))]
fn ipc_shutdown(
    py: Python<'_>,
    address: &str,
    timeout_seconds: f64,
    root: Option<String>,
    context: Option<&PyLocalEndpointContext>,
) -> PyResult<&'static str> {
    let timeout = timeout_duration(timeout_seconds)?;
    let context = resolve_context(root, context)?;
    let outcome = py
        .detach(|| c2_core::shutdown_direct_ipc(address, Some(&context), timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(shutdown_outcome_name(outcome))
}

pub(crate) fn shutdown_outcome_name(outcome: c2_core::DirectIpcShutdownOutcome) -> &'static str {
    match outcome {
        c2_core::DirectIpcShutdownOutcome::Initiated => "initiated",
        c2_core::DirectIpcShutdownOutcome::Absent => "absent",
        c2_core::DirectIpcShutdownOutcome::Unconfirmed => "unconfirmed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_shutdown_outcomes_project_to_distinct_strings() {
        use c2_core::DirectIpcShutdownOutcome;
        for (outcome, expected) in [
            (DirectIpcShutdownOutcome::Initiated, "initiated"),
            (DirectIpcShutdownOutcome::Absent, "absent"),
            (DirectIpcShutdownOutcome::Unconfirmed, "unconfirmed"),
        ] {
            assert_eq!(shutdown_outcome_name(outcome), expected);
        }
    }
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(ipc_endpoint_name, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_ping, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_shutdown, module)?)?;
    Ok(())
}
