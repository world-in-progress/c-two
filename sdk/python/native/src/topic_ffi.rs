//! PyO3 projection of registration-scoped publish/subscribe topics (ADR 0005).
//!
//! Publishers bind to one native `Registration` instance and fail with
//! `ResourceClosed` (707) once that registration ends; subscriptions expose
//! blocking delivery waits that always release the Python GIL.

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::types::{PyBytes, PyDict};

use c2_core::{LagPolicy, SubscribeOptions, TopicStart};
use c2_error::{C2Error, ErrorCode};

use crate::core_error_ffi::core_error_to_py;
use crate::stream_ffi::close_timeout;

pub(crate) fn subscribe_options(start: &str, on_lag: &str) -> PyResult<SubscribeOptions> {
    let start = match start {
        "latest" => TopicStart::Latest,
        "next" => TopicStart::Next,
        other => {
            return Err(PyValueError::new_err(format!(
                "invalid topic start {other:?}: expected 'latest' or 'next'",
            )));
        }
    };
    let on_lag = match on_lag {
        "skip" => LagPolicy::Skip,
        "error" => LagPolicy::Error,
        other => {
            return Err(PyValueError::new_err(format!(
                "invalid topic lag policy {other:?}: expected 'skip' or 'error'",
            )));
        }
    };
    Ok(SubscribeOptions { start, on_lag })
}

/// One registration-scoped native publisher handle.
#[pyclass(name = "NativeTopicPublisher", frozen, skip_from_py_object)]
pub(crate) struct PyTopicPublisher {
    inner: c2_core::TopicPublisher,
}

impl PyTopicPublisher {
    pub(crate) fn new(inner: c2_core::TopicPublisher) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyTopicPublisher {
    /// Publish one immutable payload and return its monotonic sequence number.
    ///
    /// Exact `bytes` travel on their Python-owned carrier without an extra
    /// copy; every other buffer is snapshotted under the GIL first. The
    /// native write itself runs detached from Python.
    fn publish(&self, py: Python<'_>, payload: &Bound<'_, PyAny>) -> PyResult<u64> {
        if let Ok(bytes) = payload.cast_exact::<PyBytes>() {
            let owner = PyBackedBytes::from(bytes.clone());
            let publisher = self.inner.clone();
            return py
                .detach(move || publisher.publish_bytes(&owner))
                .map_err(core_error_to_py);
        }
        let buffer = PyBuffer::<u8>::get(payload)?;
        let mut snapshot = vec![0_u8; buffer.len_bytes()];
        buffer.copy_to_slice(py, &mut snapshot)?;
        drop(buffer);
        py.detach(move || self.inner.publish(snapshot))
            .map_err(core_error_to_py)
    }

    #[getter]
    fn topic_name(&self) -> &str {
        self.inner.topic_name()
    }

    #[getter]
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    #[getter]
    fn head_seq(&self) -> u64 {
        self.inner.head_seq()
    }
}

/// Same-process subscription attached to one registration's topic channel.
#[pyclass(name = "NativeLocalTopicSubscription", frozen, skip_from_py_object)]
pub(crate) struct PyLocalTopicSubscription {
    inner: c2_core::LocalTopicSubscription,
}

impl PyLocalTopicSubscription {
    pub(crate) fn new(inner: c2_core::LocalTopicSubscription) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyLocalTopicSubscription {
    /// Block until the next delivery; the wait always releases the GIL.
    ///
    /// Returns `(seq, dropped, payload_bytes)`: the frame is copied out of the
    /// shared ring into a Python-owned local copy before it is handed on.
    fn next_delivery<'py>(&self, py: Python<'py>) -> PyResult<(u64, u64, Bound<'py, PyBytes>)> {
        let (seq, dropped, payload) = py
            .detach(|| {
                self.inner
                    .next_delivery()
                    .map(|delivery| (delivery.seq(), delivery.dropped(), delivery.clone_bytes()))
            })
            .map_err(core_error_to_py)?;
        Ok((seq, dropped, PyBytes::new(py, &payload)))
    }

    #[getter]
    fn start_seq(&self) -> u64 {
        self.inner.start_seq()
    }

    #[getter]
    fn topic_name(&self) -> &str {
        self.inner.topic_name()
    }

    fn cancel(&self) {
        self.inner.cancel();
    }

    /// Cancel and observe the terminal receipt. Local subscriptions settle
    /// immediately, so no caller budget is accepted.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let receipt = py.detach(|| self.inner.close());
        topic_close_receipt(py, receipt)
    }
}

/// Client-owned view of one remote topic subscription.
#[pyclass(name = "NativeTopicSubscription", frozen, skip_from_py_object)]
pub(crate) struct PyTopicSubscription {
    inner: c2_core::CoreTopicSubscription,
}

impl PyTopicSubscription {
    pub(crate) fn new(inner: c2_core::CoreTopicSubscription) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyTopicSubscription {
    /// Block until the next delivery; the wait always releases the GIL.
    ///
    /// Returns `(seq, dropped, response)` where the response owns the local
    /// copy charged to the client domain, mirroring unary response owners.
    fn next_delivery(
        &self,
        py: Python<'_>,
    ) -> PyResult<(u64, u64, crate::core_ffi::PyCoreResponse)> {
        let (seq, dropped, held) = py
            .detach(|| {
                self.inner
                    .next_delivery()
                    .map(|delivery| (delivery.seq(), delivery.dropped(), delivery.into_held()))
            })
            .map_err(core_error_to_py)?;
        Ok((seq, dropped, crate::core_ffi::PyCoreResponse::new(held)))
    }

    fn check_deadline(&self) -> PyResult<()> {
        self.inner.check_deadline().map_err(core_error_to_py)
    }

    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.cancel()).map_err(core_error_to_py)
    }

    #[getter]
    fn start_seq(&self) -> u64 {
        self.inner.start_seq()
    }

    #[getter]
    fn topic_name(&self) -> &str {
        self.inner.topic_name()
    }

    #[pyo3(signature = (timeout=Some(30.0)))]
    fn close<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyDict>> {
        let timeout = close_timeout(timeout)?;
        let receipt = py.detach(|| self.inner.close_with_timeout(timeout));
        topic_close_receipt(py, receipt)
    }
}

pub(crate) fn topic_close_receipt<'py>(
    py: Python<'py>,
    receipt: c2_core::TopicCloseReceipt,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    result.set_item("completed", receipt.completed)?;
    result.set_item("complete_items", receipt.complete_items)?;
    match receipt.terminal {
        Some(Err(error)) => result.set_item("error", core_error_to_py(error).value(py))?,
        _ => result.set_item("error", py.None())?,
    }
    Ok(result)
}

/// The `ResourceClosed` (707) refusal every closed registration publisher reports.
pub(crate) fn registration_closed_error(route_name: &str) -> c2_core::Error {
    c2_core::Error::Semantic(C2Error::new(
        ErrorCode::ResourceClosed,
        format!("registration for route {route_name:?} is closed"),
    ))
}

/// The `ResourceNotFound` (701) refusal for same-process subscribe with no host.
pub(crate) fn route_host_absent_error(route_name: &str) -> c2_core::Error {
    c2_core::Error::Semantic(C2Error::new(
        ErrorCode::ResourceNotFound,
        format!("route not found: {route_name}"),
    ))
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyTopicPublisher>()?;
    module.add_class::<PyLocalTopicSubscription>()?;
    module.add_class::<PyTopicSubscription>()?;
    Ok(())
}
