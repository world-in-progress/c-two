use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use c2_core::{
    RouteConcurrency, RouteConcurrencyError, RouteConcurrencyGuard, RouteConcurrencySnapshot,
};

#[pyclass(name = "RouteConcurrency", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyRouteConcurrency {
    pub(crate) inner: RouteConcurrency,
}

impl PyRouteConcurrency {
    pub(crate) fn new(inner: RouteConcurrency) -> Self {
        Self { inner }
    }
}

#[pyclass(name = "RouteConcurrencySnapshot", frozen)]
pub struct PyRouteConcurrencySnapshot {
    #[pyo3(get)]
    pub mode: String,
    #[pyo3(get)]
    pub max_pending: Option<usize>,
    #[pyo3(get)]
    pub max_workers: Option<usize>,
    #[pyo3(get)]
    pub pending: usize,
    #[pyo3(get)]
    pub active_workers: usize,
    #[pyo3(get)]
    pub closed: bool,
    #[pyo3(get)]
    pub is_unconstrained: bool,
}

#[pyclass(name = "RouteConcurrencyGuard")]
pub struct PyRouteConcurrencyGuard {
    inner: Option<RouteConcurrencyGuard>,
}

impl From<RouteConcurrencySnapshot> for PyRouteConcurrencySnapshot {
    fn from(snapshot: RouteConcurrencySnapshot) -> Self {
        Self {
            mode: snapshot.mode.as_str().to_string(),
            max_pending: snapshot.max_pending,
            max_workers: snapshot.max_workers,
            pending: snapshot.pending,
            active_workers: snapshot.active_workers,
            closed: snapshot.closed,
            is_unconstrained: snapshot.is_unconstrained,
        }
    }
}

fn acquire_error_to_py(py: Python<'_>, err: RouteConcurrencyError) -> PyErr {
    match err {
        RouteConcurrencyError::Closed => {
            let exc = PyRuntimeError::new_err("route closed");
            let error_bytes =
                c2_error::C2Error::new(c2_error::ErrorCode::ResourceClosed, "route closed")
                    .to_wire_bytes();
            if let Err(attr_err) = exc
                .value(py)
                .setattr("error_bytes", PyBytes::new(py, &error_bytes))
            {
                return attr_err;
            }
            exc
        }
        RouteConcurrencyError::Capacity { field, limit } => PyRuntimeError::new_err(format!(
            "route concurrency capacity exceeded: {field}={limit}"
        )),
    }
}

#[pymethods]
impl PyRouteConcurrency {
    fn snapshot(&self) -> PyRouteConcurrencySnapshot {
        PyRouteConcurrencySnapshot::from(self.inner.snapshot())
    }

    fn execution_guard(
        &self,
        py: Python<'_>,
        method_idx: u16,
    ) -> PyResult<PyRouteConcurrencyGuard> {
        let sched = self.inner.clone();
        let guard = py
            .detach(move || sched.blocking_acquire(method_idx))
            .map_err(|err| acquire_error_to_py(py, err))?;
        Ok(PyRouteConcurrencyGuard { inner: Some(guard) })
    }

    #[getter]
    fn is_unconstrained(&self) -> bool {
        self.inner.is_unconstrained()
    }
}

#[pymethods]
impl PyRouteConcurrencyGuard {
    fn __enter__<'py>(slf: PyRefMut<'py, Self>) -> PyResult<PyRefMut<'py, Self>> {
        if slf.inner.is_none() {
            return Err(PyRuntimeError::new_err(
                "route concurrency guard already released",
            ));
        }
        Ok(slf)
    }

    fn __exit__(
        mut slf: PyRefMut<'_, Self>,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        slf.inner.take();
        false
    }
}

impl Drop for PyRouteConcurrencyGuard {
    fn drop(&mut self) {
        self.inner.take();
    }
}

pub(crate) fn register_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyRouteConcurrency>()?;
    parent.add_class::<PyRouteConcurrencySnapshot>()?;
    parent.add_class::<PyRouteConcurrencyGuard>()?;
    Ok(())
}
