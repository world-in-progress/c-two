use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

fn timeout_duration(timeout_seconds: f64) -> PyResult<Duration> {
    if !timeout_seconds.is_finite() || timeout_seconds < 0.0 {
        return Err(PyValueError::new_err(
            "timeout must be a non-negative finite number",
        ));
    }
    Ok(Duration::from_secs_f64(timeout_seconds))
}

#[pyfunction]
fn ipc_socket_path(address: &str) -> PyResult<String> {
    c2_core::direct_ipc_socket_path(address)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
fn ipc_ping(py: Python<'_>, address: &str, timeout_seconds: f64) -> PyResult<bool> {
    let timeout = timeout_duration(timeout_seconds)?;
    py.detach(|| c2_core::ping_direct_ipc(address, timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
fn ipc_shutdown<'py>(
    py: Python<'py>,
    address: &str,
    timeout_seconds: f64,
) -> PyResult<Bound<'py, PyDict>> {
    let timeout = timeout_duration(timeout_seconds)?;
    let outcome = py
        .detach(|| c2_core::shutdown_direct_ipc(address, timeout))
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
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
    module.add_function(wrap_pyfunction!(ipc_socket_path, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_ping, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_shutdown, module)?)?;
    Ok(())
}
