//! Read-only Python projections of native physical-connection observations.
use std::time::Duration;

use c2_core::{
    ConnectionClosed, ConnectionEvent, ConnectionEventWait, ConnectionEvents,
    ConnectionObservation, recv_connection_event, wait_connection_closed,
};
use pyo3::exceptions::{PyRuntimeError, PyStopIteration, PyValueError};
use pyo3::prelude::*;

pyo3::create_exception!(_native, ConnectionEventsLagged, PyRuntimeError);

fn wait_timeout(timeout: Option<f64>) -> PyResult<Option<Duration>> {
    timeout
        .map(|seconds| {
            Duration::try_from_secs_f64(seconds).map_err(|_| {
                PyValueError::new_err(
                    "timeout must be finite, non-negative, and representable as a Duration",
                )
            })
        })
        .transpose()
}

#[pyclass(
    name = "ConnectionClosed",
    module = "c_two._native",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct PyConnectionClosed {
    inner: ConnectionClosed,
}

#[pymethods]
impl PyConnectionClosed {
    #[getter]
    fn connection_id(&self) -> u64 {
        self.inner.connection_id()
    }
    #[getter]
    fn reason(&self) -> &'static str {
        self.inner.reason().as_str()
    }
    #[getter]
    fn operation(&self) -> &'static str {
        self.inner.operation()
    }
    #[getter]
    fn os_error(&self) -> Option<i32> {
        self.inner.os_error()
    }
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
    fn __repr__(&self) -> String {
        format!(
            "ConnectionClosed(id={}, reason={:?}, operation={:?}, os_error={:?})",
            self.connection_id(),
            self.reason(),
            self.operation(),
            self.os_error()
        )
    }
}

#[pyclass(
    name = "ConnectionObservation",
    module = "c_two._native",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub(crate) struct PyConnectionObservation {
    pub(crate) inner: ConnectionObservation,
}

#[pymethods]
impl PyConnectionObservation {
    #[getter]
    fn connection_id(&self) -> u64 {
        self.inner.connection_id()
    }

    fn snapshot(&self) -> Option<PyConnectionClosed> {
        self.inner
            .snapshot()
            .map(|inner| PyConnectionClosed { inner })
    }

    #[pyo3(signature = (timeout=None))]
    fn wait_closed(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Option<PyConnectionClosed>> {
        let timeout = wait_timeout(timeout)?;
        py.detach(|| {
            wait_connection_closed(&self.inner, timeout)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))
                .map(|closed| closed.map(|inner| PyConnectionClosed { inner }))
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "ConnectionObservation(id={}, closed={})",
            self.connection_id(),
            self.inner.snapshot().is_some()
        )
    }
}

#[pyclass(
    name = "ConnectionEvent",
    module = "c_two._native",
    frozen,
    skip_from_py_object
)]
pub(crate) struct PyConnectionEvent {
    inner: ConnectionEvent,
}

#[pymethods]
impl PyConnectionEvent {
    #[getter]
    fn kind(&self) -> &'static str {
        match &self.inner {
            ConnectionEvent::Opened(_) => "opened",
            ConnectionEvent::Closed(_) => "closed",
        }
    }
    #[getter]
    fn connection_id(&self) -> u64 {
        match &self.inner {
            ConnectionEvent::Opened(observation) => observation.connection_id(),
            ConnectionEvent::Closed(record) => record.connection_id(),
        }
    }
    #[getter]
    fn value(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.inner {
            ConnectionEvent::Opened(inner) => Ok(Py::new(
                py,
                PyConnectionObservation {
                    inner: inner.clone(),
                },
            )?
            .into_any()),
            ConnectionEvent::Closed(inner) => Ok(Py::new(
                py,
                PyConnectionClosed {
                    inner: inner.clone(),
                },
            )?
            .into_any()),
        }
    }
}

/// A subscription has one consumer, just like Rust's mutable Receiver. PyO3's
/// mutable borrow rejects simultaneous consumers instead of queueing a second
/// waiter behind an unbounded wait. Waiting releases the GIL and retains no host.
#[pyclass(
    name = "ConnectionEvents",
    module = "c_two._native",
    skip_from_py_object
)]
pub(crate) struct PyConnectionEvents {
    pub(crate) inner: ConnectionEvents,
}

#[pymethods]
impl PyConnectionEvents {
    #[pyo3(signature = (timeout=None))]
    fn recv(
        &mut self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Option<PyConnectionEvent>> {
        let timeout = wait_timeout(timeout)?;
        let received = py
            .detach(|| recv_connection_event(&mut self.inner, timeout))
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        match received {
            None => Ok(None),
            Some(ConnectionEventWait::Event(inner)) => Ok(Some(PyConnectionEvent { inner })),
            Some(ConnectionEventWait::Ended) => {
                Err(PyStopIteration::new_err("connection event source ended"))
            }
            Some(ConnectionEventWait::Lagged(missed)) => {
                let error = ConnectionEventsLagged::new_err(format!(
                    "connection event subscription lost {missed} events and ended"
                ));
                error.value(py).setattr("missed", missed)?;
                Err(error)
            }
        }
    }
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }
    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<PyConnectionEvent>> {
        self.recv(py, None)
    }
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyConnectionClosed>()?;
    module.add_class::<PyConnectionObservation>()?;
    module.add_class::<PyConnectionEvent>()?;
    module.add_class::<PyConnectionEvents>()?;
    module.add(
        "ConnectionEventsLagged",
        module.py().get_type::<ConnectionEventsLagged>(),
    )?;
    Ok(())
}
