use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use c2_error::{C2Error, ErrorCode};

#[pyfunction]
fn error_registry(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for entry in ErrorCode::registry() {
        dict.set_item(entry.name, u16::from(entry.code))?;
    }
    Ok(dict.into_any().unbind())
}

#[pyfunction]
fn decode_error_legacy(py: Python<'_>, data: &[u8]) -> PyResult<Option<Py<PyAny>>> {
    let Some(err) = C2Error::from_legacy_bytes(data)
        .map_err(|e| PyValueError::new_err(e.to_string()))?
    else {
        return Ok(None);
    };

    let dict = PyDict::new(py);
    dict.set_item("code", u16::from(err.code))?;
    dict.set_item("name", err.code.name())?;
    dict.set_item("message", err.message)?;
    Ok(Some(dict.into_any().unbind()))
}

#[pyfunction]
fn encode_error_legacy<'py>(
    py: Python<'py>,
    code: u16,
    message: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    let code = ErrorCode::try_from(code).unwrap_or(ErrorCode::Unknown);
    let err = C2Error::new(code, message);
    Ok(PyBytes::new(py, &err.to_legacy_bytes()))
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(error_registry, m)?)?;
    m.add_function(wrap_pyfunction!(decode_error_legacy, m)?)?;
    m.add_function(wrap_pyfunction!(encode_error_legacy, m)?)?;
    Ok(())
}
