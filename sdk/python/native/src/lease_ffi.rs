use std::sync::Arc;
use std::time::Duration;

use c2_mem::{
    BufferLeaseGuard, BufferLeaseMeta, BufferLeaseTracker, BufferStorage, LeaseDirection,
    LeaseRetention,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

#[pyclass(name = "BufferLeaseTracker", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyBufferLeaseTracker {
    pub(crate) inner: Arc<BufferLeaseTracker>,
}

#[pyclass(name = "BufferLease", frozen)]
pub struct PyBufferLease {
    guard: parking_lot::Mutex<Option<BufferLeaseGuard>>,
}

impl PyBufferLeaseTracker {
    pub(crate) fn from_arc(inner: Arc<BufferLeaseTracker>) -> Self {
        Self { inner }
    }

    pub(crate) fn track_retained_guard(
        &self,
        route_name: &str,
        method_name: &str,
        direction: &str,
        storage: &str,
        bytes: usize,
    ) -> PyResult<BufferLeaseGuard> {
        Ok(self.inner.track(BufferLeaseMeta {
            route_name: route_name.to_string(),
            method_name: method_name.to_string(),
            direction: parse_direction(direction)?,
            retention: LeaseRetention::Retained,
            storage: parse_storage(storage)?,
            bytes,
        }))
    }

    pub(crate) fn stats_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        lease_stats_dict(py, &self.inner.stats())
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
            list.append(lease_snapshot_dict(py, &snapshot)?)?;
        }
        Ok(list)
    }
}

/// Build the Python shape of one retained-lease snapshot.
///
/// Shared by a single tracker and by the retired-observation sweeps, so the
/// projection can report retired lease metadata from snapshot values without
/// ever handing the retired trackers themselves to Python.
pub(crate) fn lease_snapshot_dict<'py>(
    py: Python<'py>,
    snapshot: &c2_mem::BufferLeaseSnapshot,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("id", snapshot.id)?;
    dict.set_item("route_name", &snapshot.route_name)?;
    dict.set_item("method_name", &snapshot.method_name)?;
    dict.set_item("direction", snapshot.direction.as_str())?;
    dict.set_item("retention", snapshot.retention.as_str())?;
    dict.set_item("storage", snapshot.storage.as_str())?;
    dict.set_item("bytes", snapshot.bytes)?;
    dict.set_item("age_seconds", snapshot.age_seconds)?;
    Ok(dict)
}

/// Build the Python shape of one composed lease-stats view.
///
/// Shared by a single tracker and by the runtime session's merged view
/// (live tracker plus trackers retained for retired sessions), so the public
/// shape stays identical whether or not a retired domain is still reporting.
pub(crate) fn lease_stats_dict<'py>(
    py: Python<'py>,
    stats: &c2_mem::BufferLeaseStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("active_leases", stats.active_leases)?;
    dict.set_item("active_holds", stats.active_holds)?;
    dict.set_item("total_leased_bytes", stats.total_leased_bytes)?;
    dict.set_item("total_held_bytes", stats.total_held_bytes)?;
    dict.set_item("oldest_hold_seconds", stats.oldest_hold_seconds)?;

    let by_storage = PyDict::new(py);
    for storage in [
        BufferStorage::Inline,
        BufferStorage::Shm,
        BufferStorage::Handle,
        BufferStorage::FileSpill,
    ] {
        let value = stats.by_storage.get(&storage).cloned().unwrap_or_default();
        by_storage.set_item(storage.as_str(), storage_stats_dict(py, &value)?)?;
    }
    dict.set_item("by_storage", by_storage)?;

    let by_direction = PyDict::new(py);
    for direction in [
        LeaseDirection::ClientResponse,
        LeaseDirection::ResourceInput,
    ] {
        let value = stats
            .by_direction
            .get(&direction)
            .cloned()
            .unwrap_or_default();
        by_direction.set_item(direction.as_str(), direction_stats_dict(py, &value)?)?;
    }
    dict.set_item("by_direction", by_direction)?;
    Ok(dict)
}

fn parse_storage(storage: &str) -> PyResult<BufferStorage> {
    match storage {
        "inline" => Ok(BufferStorage::Inline),
        "shm" => Ok(BufferStorage::Shm),
        "handle" => Ok(BufferStorage::Handle),
        "file_spill" => Ok(BufferStorage::FileSpill),
        other => Err(PyValueError::new_err(format!(
            "invalid buffer storage {other:?}"
        ))),
    }
}

fn parse_direction(direction: &str) -> PyResult<LeaseDirection> {
    match direction {
        "client_response" => Ok(LeaseDirection::ClientResponse),
        "resource_input" => Ok(LeaseDirection::ResourceInput),
        other => Err(PyValueError::new_err(format!(
            "invalid lease direction {other:?}"
        ))),
    }
}

fn storage_stats_dict<'py>(
    py: Python<'py>,
    stats: &c2_mem::StorageLeaseStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("active_leases", stats.active_leases)?;
    dict.set_item("active_holds", stats.active_holds)?;
    dict.set_item("total_leased_bytes", stats.total_leased_bytes)?;
    dict.set_item("total_held_bytes", stats.total_held_bytes)?;
    Ok(dict)
}

fn direction_stats_dict<'py>(
    py: Python<'py>,
    stats: &c2_mem::DirectionLeaseStats,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("active_leases", stats.active_leases)?;
    dict.set_item("active_holds", stats.active_holds)?;
    dict.set_item("total_leased_bytes", stats.total_leased_bytes)?;
    dict.set_item("total_held_bytes", stats.total_held_bytes)?;
    Ok(dict)
}

#[pymethods]
impl PyBufferLeaseTracker {
    #[new]
    #[pyo3(signature = (track_transient=false))]
    fn new(track_transient: bool) -> Self {
        Self {
            inner: Arc::new(BufferLeaseTracker::new(track_transient)),
        }
    }

    #[pyo3(signature = (route_name, method_name, direction, storage, bytes))]
    fn track_retained(
        &self,
        route_name: &str,
        method_name: &str,
        direction: &str,
        storage: &str,
        bytes: usize,
    ) -> PyResult<PyBufferLease> {
        let guard =
            self.track_retained_guard(route_name, method_name, direction, storage, bytes)?;
        Ok(PyBufferLease {
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
impl PyBufferLease {
    fn release(&self) {
        self.guard.lock().take();
    }

    fn __enter__<'py>(slf: PyRef<'py, Self>) -> PyResult<PyRef<'py, Self>> {
        if slf.guard.lock().is_none() {
            return Err(PyRuntimeError::new_err("buffer lease already released"));
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

impl Drop for PyBufferLease {
    fn drop(&mut self) {
        self.guard.lock().take();
    }
}

pub(crate) fn register_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyBufferLeaseTracker>()?;
    parent.add_class::<PyBufferLease>()?;
    Ok(())
}
