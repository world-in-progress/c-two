use std::time::Duration;

use c2_config::LocalEndpointProtocol;
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

/// Project an optional explicit endpoint protocol.
///
/// PyO3 cannot infer a default for an `Option` parameter, so each entry point
/// below declares `protocol=None` explicitly and keeps the historical call
/// shapes working. `None` means "resolve the process client IPC policy";
/// `Some(value)` must be a canonical protocol name and is rejected before any
/// endpoint I/O. Probes never probe old and new endpoint derivations to find
/// a server.
fn endpoint_protocol(protocol: Option<&str>) -> PyResult<Option<LocalEndpointProtocol>> {
    protocol
        .map(|value| {
            value
                .parse::<LocalEndpointProtocol>()
                .map_err(PyValueError::new_err)
        })
        .transpose()
}

#[pyfunction]
#[pyo3(signature = (address, protocol=None))]
fn ipc_endpoint_name(address: &str, protocol: Option<&str>) -> PyResult<String> {
    let protocol = endpoint_protocol(protocol)?;
    let endpoint = match protocol {
        Some(protocol) => c2_core::direct_ipc_endpoint_with_protocol(address, protocol),
        None => c2_core::direct_ipc_endpoint(address),
    };
    endpoint
        .map(|endpoint| endpoint.os_name().to_string_lossy().into_owned())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
#[pyo3(signature = (address, timeout_seconds=0.5, protocol=None))]
fn ipc_ping(
    py: Python<'_>,
    address: &str,
    timeout_seconds: f64,
    protocol: Option<&str>,
) -> PyResult<bool> {
    let timeout = timeout_duration(timeout_seconds)?;
    let protocol = endpoint_protocol(protocol)?;
    py.detach(|| match protocol {
        Some(protocol) => c2_core::ping_direct_ipc_with_protocol(address, protocol, timeout),
        None => c2_core::ping_direct_ipc(address, timeout),
    })
    .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
#[pyo3(signature = (address, timeout_seconds=0.5, protocol=None))]
fn ipc_shutdown<'py>(
    py: Python<'py>,
    address: &str,
    timeout_seconds: f64,
    protocol: Option<&str>,
) -> PyResult<Bound<'py, PyDict>> {
    let timeout = timeout_duration(timeout_seconds)?;
    let protocol = endpoint_protocol(protocol)?;
    let outcome = py
        .detach(|| match protocol {
            Some(protocol) => {
                c2_core::shutdown_direct_ipc_with_protocol(address, protocol, timeout)
            }
            None => c2_core::shutdown_direct_ipc(address, timeout),
        })
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
    module.add_function(wrap_pyfunction!(ipc_endpoint_name, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_ping, module)?)?;
    module.add_function(wrap_pyfunction!(ipc_shutdown, module)?)?;
    Ok(())
}
