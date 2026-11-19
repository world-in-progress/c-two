use pyo3::prelude::*;
use pyo3::types::PyDict;

use c2_error::ErrorCode;

/// Project the canonical Rust error registry.
///
/// Each entry maps its Rust member name to
/// ``(code, side, stage)`` where ``side`` and ``stage`` are ``None`` for
/// codes without an adapter side/stage classification. Python derives its
/// ``ErrorCode`` enum and read-only side/stage metadata from this projection;
/// no in-process C2E1 wire round-trip exists anymore.
#[pyfunction]
fn error_registry(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for entry in ErrorCode::registry() {
        let side = entry.side.map(|side| side.as_str());
        let stage = entry.stage.map(|stage| stage.as_str());
        dict.set_item(entry.name, (u16::from(entry.code), side, stage))?;
    }
    Ok(dict.into_any().unbind())
}

pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(error_registry, m)?)?;
    Ok(())
}
