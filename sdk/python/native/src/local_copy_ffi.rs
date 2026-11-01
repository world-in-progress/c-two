use std::sync::Arc;
use std::time::Duration;

use c2_core::memory::{LocalCopyDirection, LocalCopyGuard, LocalCopyMeta, LocalCopyTracker};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

#[pyclass(name = "LocalCopyTracker", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyLocalCopyTracker {
    pub(crate) inner: Arc<LocalCopyTracker>,
}

#[pyclass(name = "LocalCopyGuard", frozen)]
pub struct PyLocalCopyGuard {
    guard: parking_lot::Mutex<Option<LocalCopyGuard>>,
}

impl PyLocalCopyTracker {
    pub(crate) fn from_arc(inner: Arc<LocalCopyTracker>) -> Self {
        Self { inner }
    }

    pub(crate) fn track_retained_guard(
        &self,
        route_name: &str,
        method_name: &str,
        direction: &str,
        bytes: usize,
    ) -> PyResult<LocalCopyGuard> {
        Ok(self.inner.track(LocalCopyMeta {
            route_name: route_name.to_string(),
            method_name: method_name.to_string(),
            direction: parse_direction(direction)?,
            bytes,
        }))
    }

    pub(crate) fn stats_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        local_copy_stats_dict(py, &self.inner.stats())
    }

    pub(crate) fn sweep_retained_list<'py>(
        &self,
        py: Python<'py>,
        threshold_seconds: f64,
    ) -> PyResult<Bound<'py, PyList>> {
        if !threshold_seconds.is_finite() || threshold_seconds < 0.0 {
            return Err(PyValueError::new_err(
                "threshold_seconds must be a non-negative finite number",
            ));
        }

        let threshold = Duration::from_secs_f64(threshold_seconds);
        let list = PyList::empty(py);
        for snapshot in self.inner.sweep_retained(threshold) {
            list.append(local_copy_snapshot_dict(py, &snapshot)?)?;
        }
        Ok(list)
    }
}

/// Build the Python shape of one local-copy snapshot.
///
/// Shared by a single tracker and by the retired-observation sweeps, so the
/// projection can report retired local-copy metadata from snapshot values without
/// ever handing the retired trackers themselves to Python.
pub(crate) fn local_copy_snapshot_dict<'py>(
    py: Python<'py>,
    snapshot: &c2_core::memory::LocalCopySnapshot,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("id", snapshot.id)?;
    dict.set_item("route_name", &snapshot.route_name)?;
    dict.set_item("method_name", &snapshot.method_name)?;
    dict.set_item("direction", snapshot.direction.as_str())?;
    dict.set_item("bytes", snapshot.bytes)?;
    dict.set_item("age_seconds", snapshot.age_seconds)?;
    Ok(dict)
}

/// Build the Python shape of one composed local-copy stats view.
///
/// Shared by a single tracker and by the runtime session's merged view
/// (live tracker plus trackers retained for retired sessions), so the public
/// shape stays identical whether or not a retired domain is still reporting.
pub(crate) fn local_copy_stats_dict<'py>(
    py: Python<'py>,
    stats: &c2_core::memory::LocalCopyStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("active_holds", stats.active_holds)?;
    dict.set_item("total_held_bytes", stats.total_held_bytes)?;
    dict.set_item("oldest_hold_seconds", stats.oldest_hold_seconds)?;

    let by_direction = PyDict::new(py);
    for direction in [
        LocalCopyDirection::ClientResponse,
        LocalCopyDirection::ResourceInput,
    ] {
        let value = stats
            .by_direction
            .get(&direction)
            .cloned()
            .unwrap_or_default();
        by_direction.set_item(direction.as_str(), hold_bucket_stats_dict(py, &value)?)?;
    }
    dict.set_item("by_direction", by_direction)?;
    Ok(dict)
}

fn parse_direction(direction: &str) -> PyResult<LocalCopyDirection> {
    match direction {
        "client_response" => Ok(LocalCopyDirection::ClientResponse),
        "resource_input" => Ok(LocalCopyDirection::ResourceInput),
        other => Err(PyValueError::new_err(format!(
            "invalid local copy direction {other:?}"
        ))),
    }
}

fn hold_bucket_stats_dict<'py>(
    py: Python<'py>,
    stats: &c2_core::memory::LocalCopyBucketStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("active_holds", stats.active_holds)?;
    dict.set_item("total_held_bytes", stats.total_held_bytes)?;
    Ok(dict)
}

#[pymethods]
impl PyLocalCopyTracker {
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(LocalCopyTracker::new()),
        }
    }

    #[pyo3(signature = (route_name, method_name, direction, bytes))]
    fn track_retained(
        &self,
        route_name: &str,
        method_name: &str,
        direction: &str,
        bytes: usize,
    ) -> PyResult<PyLocalCopyGuard> {
        let guard = self.track_retained_guard(route_name, method_name, direction, bytes)?;
        Ok(PyLocalCopyGuard {
            guard: parking_lot::Mutex::new(Some(guard)),
        })
    }

    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.stats_dict(py)
    }

    fn sweep_retained<'py>(
        &self,
        py: Python<'py>,
        threshold_seconds: f64,
    ) -> PyResult<Bound<'py, PyList>> {
        self.sweep_retained_list(py, threshold_seconds)
    }
}

#[pymethods]
impl PyLocalCopyGuard {
    fn release(&self) {
        self.guard.lock().take();
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyResult<PyRef<'py, Self>> {
        if slf.guard.lock().is_none() {
            return Err(PyRuntimeError::new_err("local copy already released"));
        }
        Ok(slf)
    }

    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        self.release();
        false
    }
}

impl Drop for PyLocalCopyGuard {
    fn drop(&mut self) {
        self.guard.lock().take();
    }
}

pub(crate) fn register_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyLocalCopyTracker>()?;
    parent.add_class::<PyLocalCopyGuard>()?;
    Ok(())
}
