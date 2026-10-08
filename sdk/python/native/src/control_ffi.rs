use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::endpoint_ffi::{PyLocalEndpointContext, resolve_context};

pub(crate) fn timeout_duration(timeout_seconds: f64) -> PyResult<Duration> {
    crate::runtime_session_ffi::checked_shutdown_timeout(timeout_seconds)
        .map_err(PyValueError::new_err)
}

#[pyfunction]
#[pyo3(signature = (address, *, root=None, context=None))]
fn ipc_endpoint_name(
    address: &str,
    root: Option<String>,
    context: Option<&PyLocalEndpointContext>,
) -> PyResult<String> {
    let context = resolve_context(root, context)?;
    let endpoint = c2_core::direct_ipc_endpoint_with_context(address, &context);
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
    py.detach(|| c2_core::ping_direct_ipc_with_context(address, &context, timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
#[pyo3(signature = (address, timeout_seconds=0.5, *, root=None, context=None))]
fn ipc_shutdown<'py>(
    py: Python<'py>,
    address: &str,
    timeout_seconds: f64,
    root: Option<String>,
    context: Option<&PyLocalEndpointContext>,
) -> PyResult<Bound<'py, PyDict>> {
    let timeout = timeout_duration(timeout_seconds)?;
    let context = resolve_context(root, context)?;
    let outcome = py
        .detach(|| c2_core::shutdown_direct_ipc_with_context(address, &context, timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    shutdown_ack_dict(py, outcome)
}

pub(crate) fn shutdown_ack_dict<'py>(
    py: Python<'py>,
    outcome: c2_core::DirectIpcShutdownOutcome,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("acknowledged", outcome.acknowledged)?;
    result.set_item("shutdown_started", outcome.shutdown_started)?;
    result.set_item("server_stopped", outcome.server_stopped)?;
    let routes = outcome
        .route_outcomes
        .into_iter()
        .map(|route| {
            let item = PyDict::new(py);
            item.set_item("route_name", route.route_name)?;
            item.set_item("active_drained", route.active_drained)?;
            item.set_item("closed_reason", route.closed_reason)?;
            Ok(item)
        })
        .collect::<PyResult<Vec<_>>>()?;
    result.set_item("route_outcomes", PyList::new(py, routes)?)?;
    Ok(result)
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(ipc_endpoint_name, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_ping, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_shutdown, module)?)?;
    Ok(())
}
