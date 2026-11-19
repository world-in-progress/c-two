use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

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
        RouteConcurrencyError::DomainAdmission { .. } => {
            crate::core_error_ffi::core_error_to_py(c2_error::C2Error::from(err).into())
        }
        // Route admission refuses the call before the resource runs: project
        // the canonical semantic error (Closed -> ResourceClosed, Unavailable
        // and Capacity -> ResourceUnavailable, matching c2-core's host
        // admission mapping) as a typed c_two.error exception that carries
        // transport_phase='pre_dispatch'.
        refusal @ (RouteConcurrencyError::Closed
        | RouteConcurrencyError::Unavailable
        | RouteConcurrencyError::Capacity { .. }) => crate::core_error_ffi::semantic_error_to_py(
            py,
            c2_error::C2Error::from(refusal),
            "pre_dispatch",
        ),
    }
}

#[pymethods]
impl PyRouteConcurrency {
    fn open_stream(
        &self,
        py: Python<'_>,
        method_idx: u16,
        factory: Py<PyAny>,
    ) -> PyResult<crate::stream_ffi::PyCoreLocalResponseStream> {
        crate::stream_ffi::PyCoreLocalResponseStream::open(
            py,
            self.inner.clone(),
            method_idx,
            factory,
        )
    }

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
    fn __enter__<'py>(mut slf: PyRefMut<'py, Self>) -> PyResult<PyRefMut<'py, Self>> {
        let py = slf.py();
        let Some(mut guard) = slf.inner.take() else {
            return Err(PyRuntimeError::new_err(
                "route concurrency guard already released",
            ));
        };
        // A pause can race execution_guard() returning, including while Python
        // waits to enter its context. Failed entry drops the existing guard.
        guard.commit().map_err(|err| acquire_error_to_py(py, err))?;
        slf.inner = Some(guard);
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
