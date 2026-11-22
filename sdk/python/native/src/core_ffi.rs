use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyBufferError, PyRuntimeError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::types::{PyBytes, PyDict, PyList};

use c2_core::memory::LocalCopyGuard;
use c2_core::{
    CallBudget, CallOptions, CallTimeout, Client, EncodedResponse, EncodedService,
    EncodedStreamService, HeldResponse, LifecycleError, ObservedPath, PreparedCall, ResponseSink,
    ServerFailure,
};
use c2_error::{C2Error, ErrorCode};
use c2_mem::{RecycledBytes, WritableSlab};

use crate::config_ffi::PyConnectAttempt;
use crate::core_error_ffi::{core_error_to_py, lifecycle_error_to_py};
use crate::local_copy_ffi::PyLocalCopyTracker;

#[pyclass(name = "CoreClient", frozen, skip_from_py_object)]
pub(crate) struct PyCoreClient {
    inner: Mutex<Option<Client>>,
    route_name: String,
    local_copy_tracker: PyLocalCopyTracker,
}

impl PyCoreClient {
    pub(crate) fn new(client: Client, local_copy_tracker: PyLocalCopyTracker) -> Self {
        let route_name = client.expected_route().route_name.clone();
        Self {
            inner: Mutex::new(Some(client)),
            route_name,
            local_copy_tracker,
        }
    }

    // Keep this view lock out of preparation, config, materialization and I/O.
    fn client(&self) -> Result<Client, c2_core::Error> {
        self.inner.lock().clone().ok_or_else(|| {
            LifecycleError::Server(ServerFailure::new("Core client is closed".into())).into()
        })
    }
}

#[pyclass(name = "NativeCallOptions", frozen, skip_from_py_object)]
struct PyNativeCallOptions {
    options: CallOptions,
}

#[pyclass(name = "NativeCallBudget", frozen, skip_from_py_object)]
struct PyNativeCallBudget {
    inner: Mutex<Option<CallBudget>>,
}

#[pymethods]
impl PyNativeCallBudget {
    fn connect_attempt(&self) -> PyResult<PyConnectAttempt> {
        let inner = self.inner.lock();
        let budget = inner
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("call budget has already been consumed"))?;
        Ok(PyConnectAttempt {
            inner: budget.connect_attempt(),
        })
    }
}

fn inherit_timeout() -> Py<PyAny> {
    Python::attach(|py| py.Ellipsis())
}

fn call_options(py: Python<'_>, timeout: &Py<PyAny>) -> PyResult<CallOptions> {
    let timeout = timeout.bind(py);
    let timeout = if timeout.is(&py.Ellipsis()) {
        CallTimeout::Inherit
    } else if timeout.is_none() {
        CallTimeout::Unlimited
    } else {
        CallTimeout::try_after_seconds(timeout.extract::<f64>()?)
            .map_err(|error| PyValueError::new_err(error.to_string()))?
    };
    Ok(CallOptions::with_timeout(timeout))
}

#[pymethods]
impl PyNativeCallOptions {
    #[new]
    #[pyo3(signature = (*, timeout=inherit_timeout()))]
    fn new(py: Python<'_>, timeout: Py<PyAny>) -> PyResult<Self> {
        Ok(Self {
            options: call_options(py, &timeout)?,
        })
    }

    fn validate_thread(&self) -> PyResult<()> {
        if self.options.effective_timeout(None).is_some() {
            return Err(core_error_to_py(c2_core::Error::Semantic(
                C2Error::new(
                    ErrorCode::UnsupportedCallMode,
                    "finite call timeout is unsupported for synchronous thread-local calls",
                )
                .with_details(BTreeMap::from([
                    ("transport_phase".into(), "pre_dispatch".into()),
                    ("fallback_eligible".into(), "false".into()),
                    ("route_withdrawal".into(), "false".into()),
                ])),
            )));
        }
        Ok(())
    }

    fn call_budget(&self) -> PyResult<PyNativeCallBudget> {
        let budget = CallBudget::new(self.options, None).map_err(core_error_to_py)?;
        Ok(PyNativeCallBudget {
            inner: Mutex::new(Some(budget)),
        })
    }
}

/// One native scope and finite slot, reserved before caller-side serialization.
/// Taking the Option makes every execution attempt single use, including errors.
#[pyclass(name = "NativePreparedCall", frozen, skip_from_py_object)]
struct PyNativePreparedCall {
    inner: Mutex<Option<PreparedCall>>,
}

impl PyNativePreparedCall {
    fn take(&self) -> Option<PreparedCall> {
        self.inner.lock().take()
    }

    fn execute<R: Send>(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        release_input: bool,
        finish: impl FnOnce(c2_core::EncodedCall) -> Result<R, c2_core::Error> + Send,
    ) -> PyResult<R> {
        let mut prepared = py.detach(|| self.take()).ok_or_else(|| {
            PyValueError::new_err("prepared call has already been consumed or closed")
        })?;
        if let Ok(bytes) = data.cast_exact::<PyBytes>() {
            let owner = PyBackedBytes::from(bytes.clone());
            return py
                .detach(move || {
                    let nbytes = owner.len();
                    if let Some(mut slab) = prepared.try_alloc_request_slab(nbytes)? {
                        slab.as_mut_slice().copy_from_slice(&owner);
                        prepared.encode_slab(slab).and_then(finish)
                    } else {
                        prepared
                            .encode(nbytes, move || Ok(owner.to_vec()))
                            .and_then(finish)
                    }
                })
                .map_err(core_error_to_py);
        }

        let plan_len = deferred_plan_len(data)?;
        // Do not keep a Python buffer export across a potentially blocking
        // input admission. Recheck the length when reacquiring the export.
        let nbytes = match plan_len {
            Some(len) => len,
            None => PyBuffer::<u8>::get(data)?.len_bytes(),
        };
        let slab = py
            .detach(|| {
                let slab = prepared.try_alloc_request_slab(nbytes)?;
                if slab.is_none() {
                    prepared.charge_input(nbytes as u64)?;
                }
                Ok::<_, c2_core::Error>(slab)
            })
            .map_err(core_error_to_py)?;
        if let Some(mut slab) = slab {
            if plan_len.is_some() {
                slab = write_deferred_slab(data, slab)?;
            } else {
                checked_buffer(data, nbytes)?.copy_to_slice(py, slab.as_mut_slice())?;
                if release_input {
                    data.call_method0("release")?;
                }
            }
            return py
                .detach(move || prepared.encode_slab(slab).and_then(finish))
                .map_err(core_error_to_py);
        }
        let request = if plan_len.is_some() {
            materialize_deferred_plan(data, nbytes)?
        } else {
            let bytes = snapshot_python_buffer(py, &checked_buffer(data, nbytes)?)?;
            if release_input {
                data.call_method0("release")?;
            }
            bytes
        };
        py.detach(move || prepared.encode_vec(request).and_then(finish))
            .map_err(core_error_to_py)
    }
}

#[pymethods]
impl PyNativePreparedCall {
    fn charge_input(&self, py: Python<'_>, nbytes: u64) -> PyResult<()> {
        // Reattach only after dropping the preparation lock, including errors.
        let result = py.detach(|| {
            self.inner
                .lock()
                .as_mut()
                .map(|prepared| prepared.charge_input(nbytes))
        });
        result
            .ok_or_else(|| {
                PyValueError::new_err("prepared call has already been consumed or closed")
            })?
            .map_err(core_error_to_py)
    }

    #[pyo3(signature = (data, *, release_input = false))]
    fn call(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        release_input: bool,
    ) -> PyResult<PyCoreResponse> {
        self.execute(py, data, release_input, c2_core::EncodedCall::call_held)
            .map(PyCoreResponse::new)
    }

    #[pyo3(signature = (data, *, release_input = false))]
    fn stream(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        release_input: bool,
    ) -> PyResult<crate::stream_ffi::PyCoreResponseStream> {
        self.execute(py, data, release_input, c2_core::EncodedCall::stream)
            .map(crate::stream_ffi::PyCoreResponseStream::new)
    }

    fn close(&self, py: Python<'_>) {
        if self.inner.try_lock().is_some_and(|inner| inner.is_none()) {
            return;
        }
        py.detach(|| {
            let closed = self.inner.lock().take();
            drop(closed);
        });
    }
}

#[pymethods]
impl PyCoreClient {
    fn connection_observation(&self) -> PyResult<crate::connection_ffi::PyConnectionObservation> {
        self.client()
            .map_err(core_error_to_py)?
            .connection_observation()
            .map(|inner| crate::connection_ffi::PyConnectionObservation { inner })
            .map_err(core_error_to_py)
    }

    #[getter]
    fn mode(&self) -> PyResult<&'static str> {
        Ok(
            match self.client().map_err(core_error_to_py)?.observed_path() {
                ObservedPath::DirectIpc | ObservedPath::RelayAwareLocalIpc => "ipc",
                ObservedPath::ExplicitRelay | ObservedPath::RelayAwareRelay => "http",
            },
        )
    }

    #[getter]
    fn route_name(&self) -> &str {
        &self.route_name
    }

    #[getter]
    fn local_copy_tracker(&self) -> PyLocalCopyTracker {
        self.local_copy_tracker.clone()
    }

    #[getter]
    fn observed_path(&self) -> PyResult<&'static str> {
        Ok(
            match self.client().map_err(core_error_to_py)?.observed_path() {
                ObservedPath::DirectIpc => "direct_ipc",
                ObservedPath::ExplicitRelay => "explicit_relay",
                ObservedPath::RelayAwareLocalIpc => "relay_aware_local_ipc",
                ObservedPath::RelayAwareRelay => "relay_aware_relay",
            },
        )
    }

    #[pyo3(signature = (*, timeout=inherit_timeout()))]
    fn with_call_options(&self, py: Python<'_>, timeout: Py<PyAny>) -> PyResult<Self> {
        let client = self.client().map_err(core_error_to_py)?;
        Ok(Self::new(
            client.with_call_options(call_options(py, &timeout)?),
            self.local_copy_tracker.clone(),
        ))
    }

    fn begin_call(&self, py: Python<'_>, method_name: &str) -> PyResult<PyNativePreparedCall> {
        let prepared = py
            .detach(|| self.client()?.begin_call(method_name))
            .map_err(core_error_to_py)?;
        Ok(PyNativePreparedCall {
            inner: Mutex::new(Some(prepared)),
        })
    }

    fn begin_call_with_budget(
        &self,
        py: Python<'_>,
        method_name: &str,
        budget: &PyNativeCallBudget,
    ) -> PyResult<PyNativePreparedCall> {
        let budget = budget
            .inner
            .lock()
            .take()
            .ok_or_else(|| PyValueError::new_err("call budget has already been consumed"))?;
        let prepared = py
            .detach(|| self.client()?.begin_call_with_budget(method_name, budget))
            .map_err(core_error_to_py)?;
        Ok(PyNativePreparedCall {
            inner: Mutex::new(Some(prepared)),
        })
    }

    fn begin_stream(&self, py: Python<'_>, method_name: &str) -> PyResult<PyNativePreparedCall> {
        let prepared = py
            .detach(|| self.client()?.begin_stream(method_name))
            .map_err(core_error_to_py)?;
        Ok(PyNativePreparedCall {
            inner: Mutex::new(Some(prepared)),
        })
    }

    fn begin_stream_with_budget(
        &self,
        py: Python<'_>,
        method_name: &str,
        budget: &PyNativeCallBudget,
    ) -> PyResult<PyNativePreparedCall> {
        let budget = budget
            .inner
            .lock()
            .take()
            .ok_or_else(|| PyValueError::new_err("call budget has already been consumed"))?;
        let prepared = py
            .detach(|| self.client()?.begin_stream_with_budget(method_name, budget))
            .map_err(core_error_to_py)?;
        Ok(PyNativePreparedCall {
            inner: Mutex::new(Some(prepared)),
        })
    }

    fn call(
        &self,
        py: Python<'_>,
        method_name: &str,
        data: &Bound<'_, PyAny>,
    ) -> PyResult<PyCoreResponse> {
        self.begin_call(py, method_name)?.call(py, data, false)
    }

    /// Subscribe to a topic declared on this client's bound route contract.
    ///
    /// Waits for the server `READY(start_seq)` before returning, so an empty
    /// topic confirms attachment immediately. `timeout` follows the same
    /// convention as `with_call_options`: omitted inherits the path policy,
    /// `None` selects an unbounded subscription.
    #[pyo3(signature = (topic, start="latest", on_lag="skip", timeout=inherit_timeout()))]
    fn subscribe(
        &self,
        py: Python<'_>,
        topic: &str,
        start: &str,
        on_lag: &str,
        timeout: Py<PyAny>,
    ) -> PyResult<crate::topic_ffi::PyTopicSubscription> {
        let options = crate::topic_ffi::subscribe_options(start, on_lag)?;
        // An omitted timeout inherits this view's bound `CallOptions`
        // (preserving `with_call_options`), exactly like unary calls; only an
        // explicit `None` or numeric timeout constructs an overriding policy.
        let inherited = timeout.bind(py).is(&py.Ellipsis());
        let explicit = if inherited {
            None
        } else {
            Some(call_options(py, &timeout)?)
        };
        let client = self.client().map_err(core_error_to_py)?;
        let subscription = py
            .detach(move || match explicit {
                None => client.subscribe(topic, options),
                Some(call_options) => client.subscribe_with_options(topic, options, call_options),
            })
            .map_err(core_error_to_py)?;
        Ok(crate::topic_ffi::PyTopicSubscription::new(subscription))
    }

    fn close(&self) {
        let closed = self.inner.lock().take();
        drop(closed);
    }

    #[getter]
    fn is_connected(&self) -> bool {
        self.inner.lock().is_some()
    }
}

#[pyclass(name = "ResponseBuffer", frozen, skip_from_py_object)]
pub(crate) struct PyCoreResponse {
    inner: Mutex<Option<HeldResponse>>,
    retired_pinned: Mutex<Option<HeldResponse>>,
    local_copy: Mutex<Option<LocalCopyGuard>>,
    data_len: usize,
    exports: AtomicU32,
    invalidated: AtomicBool,
    #[cfg(test)]
    before_release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PyCoreResponse {
    pub(crate) fn new(held: HeldResponse) -> Self {
        let data_len = held.bytes().len();
        Self {
            inner: Mutex::new(Some(held)),
            retired_pinned: Mutex::new(None),
            local_copy: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
            invalidated: AtomicBool::new(false),
            #[cfg(test)]
            before_release: Mutex::new(None),
        }
    }

    fn ensure_not_exported(&self) -> PyResult<()> {
        if self.exports.load(Ordering::Acquire) == 0 {
            Ok(())
        } else {
            Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ))
        }
    }
}

#[pymethods]
impl PyCoreResponse {
    fn __len__(&self) -> PyResult<usize> {
        let inner = self.inner.lock();
        match inner.as_ref() {
            Some(held) if !held.is_released() => Ok(self.data_len),
            _ => Err(PyValueError::new_err("buffer already released")),
        }
    }

    fn __bytes__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let inner = self.inner.lock();
        match inner.as_ref() {
            Some(held) if !held.is_released() => Ok(PyBytes::new(py, held.bytes())),
            _ => Err(PyValueError::new_err("buffer already released")),
        }
    }

    #[getter]
    fn is_released(&self) -> bool {
        self.inner
            .lock()
            .as_ref()
            .is_none_or(HeldResponse::is_released)
    }

    #[getter]
    fn export_count(&self) -> u32 {
        self.exports.load(Ordering::Acquire)
    }

    fn release(&self) -> PyResult<()> {
        #[cfg(test)]
        if let Some(pause) = self.before_release.lock().take() {
            Python::attach(|py| py.detach(pause));
        }
        let mut inner = self.inner.lock();
        self.ensure_not_exported()?;
        let Some(mut held) = inner.take() else {
            return Ok(());
        };
        let _local_copy = self.local_copy.lock().take();
        held.invalidate_then_release(|| Ok(()))
            .map_err(lifecycle_error_to_py)
    }

    #[pyo3(signature = (invalidator, value, *, expected_exports = 0))]
    fn invalidate_then_release(
        &self,
        py: Python<'_>,
        invalidator: Py<PyAny>,
        value: Py<PyAny>,
        expected_exports: u32,
    ) -> PyResult<()> {
        #[cfg(test)]
        if let Some(pause) = self.before_release.lock().take() {
            Python::attach(|py| py.detach(pause));
        }
        // Reject external memoryview exports before running the invalidator so
        // a failed release keeps the decoded owner usable until callers release
        // their views. Retire public state under the lock before running Python.
        let held = {
            let mut inner = self.inner.lock();
            if self.exports.load(Ordering::Acquire) > expected_exports {
                return Err(PyBufferError::new_err(
                    "cannot release: buffer is currently exported as memoryview",
                ));
            }
            inner.take()
        };
        let Some(mut held) = held else {
            return Ok(());
        };
        let invalidation = if self.invalidated.swap(true, Ordering::AcqRel) {
            Ok(())
        } else {
            invalidator.call1(py, (value,)).map(|_| ())
        };
        match invalidation {
            Ok(()) => {
                if let Err(export_error) = self.ensure_not_exported() {
                    *self.inner.lock() = Some(held);
                    return Err(export_error);
                }
                let _local_copy = self.local_copy.lock().take();
                drop(held);
                Ok(())
            }
            Err(primary) => {
                let _local_copy = self.local_copy.lock().take();
                if self.exports.load(Ordering::Acquire) > 0 {
                    // Keep backing memory valid while an un-invalidated exporter
                    // still holds a Py_buffer; release its transport charge now
                    // and recycle when the final export drops.
                    held.detach_budget();
                    *self.retired_pinned.lock() = Some(held);
                    if self.exports.load(Ordering::Acquire) == 0 {
                        let _ = self.retired_pinned.lock().take();
                    }
                } else {
                    drop(held);
                }
                Err(primary)
            }
        }
    }

    fn detach_for_owned(&self) {
        let mut inner = self.inner.lock();
        if let Some(held) = inner.as_mut() {
            held.detach_budget();
            self.local_copy.lock().take();
        }
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="client_response"))]
    fn track_retained(
        &self,
        tracker: &PyLocalCopyTracker,
        route_name: &str,
        method_name: &str,
        direction: &str,
    ) -> PyResult<()> {
        let inner = self.inner.lock();
        if inner.as_ref().is_none_or(HeldResponse::is_released) {
            return Ok(());
        }
        let local_copy =
            tracker.track_retained_guard(route_name, method_name, direction, self.data_len)?;
        *self.local_copy.lock() = Some(local_copy);
        Ok(())
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let inner = this.inner.lock();
        let held = inner
            .as_ref()
            .filter(|held| !held.is_released())
            .ok_or_else(|| PyBufferError::new_err("buffer already released"))?;
        let bytes = held.bytes();
        unsafe {
            fill_byte_buffer(
                slf.as_any(),
                view,
                flags,
                bytes.as_ptr().cast_mut(),
                bytes.len(),
                true,
            )?;
        }
        this.exports.fetch_add(1, Ordering::Release);
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        if self.exports.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _ = self.retired_pinned.lock().take();
        }
    }
}

struct LocalRequestBuffer {
    bytes: RecycledBytes,
    local_copy: Option<LocalCopyGuard>,
}

#[pyclass(name = "CoreRequestBuffer", frozen, skip_from_py_object)]
struct PyCoreRequestBuffer {
    // One lock serializes buffer access, export admission, release and tracking.
    inner: Mutex<Option<LocalRequestBuffer>>,
    exports: AtomicU32,
    #[cfg(test)]
    before_release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    before_track_retained: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PyCoreRequestBuffer {
    #[cfg(test)]
    fn new(bytes: Vec<u8>) -> Self {
        Self::new_recycled(bytes.into())
    }

    fn new_recycled(bytes: RecycledBytes) -> Self {
        Self {
            inner: Mutex::new(Some(LocalRequestBuffer {
                bytes,
                local_copy: None,
            })),
            exports: AtomicU32::new(0),
            #[cfg(test)]
            before_release: Mutex::new(None),
            #[cfg(test)]
            before_track_retained: Mutex::new(None),
        }
    }
}

#[pymethods]
impl PyCoreRequestBuffer {
    fn __len__(&self) -> PyResult<usize> {
        self.inner
            .lock()
            .as_ref()
            .map(|state| state.bytes.len())
            .ok_or_else(|| PyValueError::new_err("buffer already released"))
    }

    fn release(&self) -> PyResult<()> {
        #[cfg(test)]
        if let Some(pause) = self.before_release.lock().take() {
            Python::attach(|py| py.detach(pause));
        }
        let mut inner = self.inner.lock();
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ));
        }
        inner.take();
        Ok(())
    }

    fn detach_for_owned(&self) {
        if let Some(state) = self.inner.lock().as_mut() {
            state.local_copy.take();
        }
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="resource_input"))]
    fn track_retained(
        &self,
        tracker: &PyLocalCopyTracker,
        route_name: &str,
        method_name: &str,
        direction: &str,
    ) -> PyResult<()> {
        #[cfg(test)]
        if let Some(pause) = self.before_track_retained.lock().take() {
            Python::attach(|py| py.detach(pause));
        }
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return Ok(());
        };
        state.local_copy = Some(tracker.track_retained_guard(
            route_name,
            method_name,
            direction,
            state.bytes.len(),
        )?);
        Ok(())
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let inner = this.inner.lock();
        let state = inner
            .as_ref()
            .ok_or_else(|| PyBufferError::new_err("buffer already released"))?;
        unsafe {
            fill_byte_buffer(
                slf.as_any(),
                view,
                flags,
                state.bytes.as_ptr().cast_mut(),
                state.bytes.len(),
                true,
            )?;
        }
        this.exports.fetch_add(1, Ordering::Release);
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

pub(crate) struct PyCoreService {
    route_name: String,
    dispatcher: Py<PyAny>,
}

impl PyCoreService {
    pub(crate) fn new(route_name: String, dispatcher: Py<PyAny>) -> Self {
        Self {
            route_name,
            dispatcher,
        }
    }
}

impl PyCoreService {
    fn dispatch_response(
        &self,
        method_index: u16,
        request: RecycledBytes,
        sink: Option<&ResponseSink>,
    ) -> Result<EncodedResponse, C2Error> {
        Python::attach(|py| {
            let invoke = || -> PyResult<EncodedResponse> {
                let request = Py::new(py, PyCoreRequestBuffer::new_recycled(request))?;
                let sink = Bound::new(py, PyResponseSink::new(sink))?;
                let scoped = ResponseSinkScope(sink.get());
                let result = self
                    .dispatcher
                    .call1(py, (self.route_name.as_str(), method_index, request, &sink))?;
                if scoped.0.state.lock().response.is_none() {
                    let value = result.bind(py);
                    let written = scoped.0.write(py, value);
                    if let Ok(view) = value.cast_exact::<pyo3::types::PyMemoryView>() {
                        let released = view.call_method0("release");
                        written?;
                        released?;
                    } else {
                        written?;
                    }
                }
                let response = scoped.0.close();
                Ok(response.expect("the dispatcher or fallback wrote a response"))
            };
            invoke().map_err(|error| python_error_to_c2(py, error))
        })
    }
}

impl EncodedService for PyCoreService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        match self.dispatch_response(method_index, request.into(), None)? {
            EncodedResponse::Inline(bytes) => Ok(bytes),
            EncodedResponse::Shm(_) => unreachable!("no response pool for inline invocation"),
        }
    }

    fn invoke_into(
        &self,
        method_index: u16,
        request: RecycledBytes,
        sink: &ResponseSink,
    ) -> Result<EncodedResponse, C2Error> {
        self.dispatch_response(method_index, request, Some(sink))
    }
}

impl EncodedStreamService for PyCoreService {
    fn open_stream(
        &self,
        method_index: u16,
        request: Vec<u8>,
    ) -> Result<Box<dyn c2_core::EncodedStream>, C2Error> {
        self.open_stream_recycled(method_index, request.into())
    }

    fn open_stream_recycled(
        &self,
        method_index: u16,
        request: RecycledBytes,
    ) -> Result<Box<dyn c2_core::EncodedStream>, C2Error> {
        Python::attach(|py| {
            let request = Py::new(py, PyCoreRequestBuffer::new_recycled(request))
                .map_err(|error| python_error_to_c2(py, error))?;
            let iterator = self
                .dispatcher
                .call1(py, (self.route_name.as_str(), method_index, request))
                .map_err(|error| python_error_to_c2(py, error))?;
            Ok(Box::new(crate::stream_ffi::PyEncodedStream { iterator })
                as Box<dyn c2_core::EncodedStream>)
        })
    }
}

unsafe fn fill_byte_buffer(
    owner: &Bound<'_, PyAny>,
    view: *mut ffi::Py_buffer,
    flags: std::os::raw::c_int,
    data: *mut u8,
    len: usize,
    readonly: bool,
) -> PyResult<()> {
    if readonly && flags & ffi::PyBUF_WRITABLE != 0 {
        return Err(PyBufferError::new_err("buffer is read-only"));
    }
    let len = isize::try_from(len).map_err(|_| PyBufferError::new_err("buffer is too large"))?;
    // SAFETY: getbuffer supplies a writable Py_buffer slot and its owner pins data.
    unsafe {
        (*view).buf = data.cast();
        (*view).obj = ffi::Py_NewRef(owner.as_ptr());
        (*view).len = len;
        (*view).readonly = i32::from(readonly);
        (*view).itemsize = 1;
        (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
            c"B".as_ptr().cast_mut()
        } else {
            std::ptr::null_mut()
        };
        (*view).ndim = 1;
        (*view).shape = if flags & ffi::PyBUF_ND != 0 {
            &mut (*view).len
        } else {
            std::ptr::null_mut()
        };
        (*view).strides = if flags & ffi::PyBUF_STRIDES == ffi::PyBUF_STRIDES {
            &mut (*view).itemsize
        } else {
            std::ptr::null_mut()
        };
        (*view).suboffsets = std::ptr::null_mut();
        (*view).internal = std::ptr::null_mut();
    }
    Ok(())
}

#[pyclass(frozen, skip_from_py_object)]
struct PyWritableSlab {
    slab: Mutex<Option<WritableSlab>>,
    exports: AtomicU32,
}

impl PyWritableSlab {
    fn new(slab: WritableSlab) -> Self {
        Self {
            slab: Mutex::new(Some(slab)),
            exports: AtomicU32::new(0),
        }
    }

    fn take_unexported(&self) -> PyResult<WritableSlab> {
        let mut slab = self.slab.lock();
        if self.exports.load(Ordering::Acquire) != 0 {
            return Err(PyBufferError::new_err("cannot consume an exported slab"));
        }
        slab.take()
            .ok_or_else(|| PyBufferError::new_err("slab already consumed"))
    }
}

#[pymethods]
impl PyWritableSlab {
    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.get();
        let mut slab = this.slab.lock();
        let slab = slab
            .as_mut()
            .ok_or_else(|| PyBufferError::new_err("slab already consumed"))?;
        unsafe {
            fill_byte_buffer(
                slf.as_any(),
                view,
                flags,
                slab.as_mut_ptr(),
                slab.len(),
                false,
            )?;
        }
        this.exports.fetch_add(1, Ordering::Release);
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

#[pyclass(frozen, skip_from_py_object)]
struct PyScopedWritableBuffer {
    data: *mut u8,
    len: usize,
    active: AtomicBool,
    exports: AtomicU32,
    gate: Mutex<()>,
}

// SAFETY: the pointer is exposed only while the scope owns its exclusive
// borrow. The gate serializes export admission against scope retirement.
unsafe impl Send for PyScopedWritableBuffer {}
unsafe impl Sync for PyScopedWritableBuffer {}

#[pymethods]
impl PyScopedWritableBuffer {
    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.get();
        let _gate = this.gate.lock();
        if !this.active.load(Ordering::Acquire) {
            return Err(PyBufferError::new_err("writable buffer scope has ended"));
        }
        unsafe { fill_byte_buffer(slf.as_any(), view, flags, this.data, this.len, false)? };
        this.exports.fetch_add(1, Ordering::Release);
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

struct WritableBufferScope<'a>(&'a PyScopedWritableBuffer);

impl Drop for WritableBufferScope<'_> {
    fn drop(&mut self) {
        let _gate = self.0.gate.lock();
        self.0.active.store(false, Ordering::Release);
        if self.0.exports.load(Ordering::Acquire) != 0 {
            // Python buffers cannot be revoked. Unwinding would give Core
            // back the destination while a foreign writer can still use it.
            eprintln!("FastDB retained a writable stream buffer beyond its encoding scope");
            std::process::abort();
        }
    }
}

pub(crate) fn write_deferred_range(
    plan: &Bound<'_, PyAny>,
    destination: &mut [u8],
) -> PyResult<()> {
    let buffer = Bound::new(
        plan.py(),
        PyScopedWritableBuffer {
            data: destination.as_mut_ptr(),
            len: destination.len(),
            active: AtomicBool::new(true),
            exports: AtomicU32::new(0),
            gate: Mutex::new(()),
        },
    )?;
    let _scope = WritableBufferScope(buffer.get());
    plan.call_method1("_c2_write_slab", (&buffer,))?;
    Ok(())
}

struct ResponseSinkState {
    sink: Option<NonNull<ResponseSink>>,
    response: Option<EncodedResponse>,
}

// SAFETY: only the scope that borrowed the sink installs the pointer. All
// dereferences and its removal run under the same state lock.
unsafe impl Send for ResponseSinkState {}

#[pyclass(frozen, skip_from_py_object)]
struct PyResponseSink {
    state: Mutex<ResponseSinkState>,
    active: AtomicBool,
}

struct ResponseSinkScope<'a>(&'a PyResponseSink);

impl Drop for ResponseSinkScope<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl PyResponseSink {
    fn new(sink: Option<&ResponseSink>) -> Self {
        Self {
            state: Mutex::new(ResponseSinkState {
                sink: sink.map(NonNull::from),
                response: None,
            }),
            active: AtomicBool::new(true),
        }
    }

    fn close(&self) -> Option<EncodedResponse> {
        let mut state = self.state.lock();
        self.active.store(false, Ordering::Release);
        state.sink = None;
        state.response.take()
    }

    fn ensure_writable(&self, state: &ResponseSinkState) -> PyResult<()> {
        if !self.active.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("response sink scope has ended"));
        }
        if state.response.is_some() {
            return Err(PyRuntimeError::new_err("response sink already written"));
        }
        Ok(())
    }

    fn try_alloc(&self, len: usize) -> PyResult<Option<WritableSlab>> {
        let state = self.state.lock();
        self.ensure_writable(&state)?;
        match state.sink {
            // SAFETY: the active scope holds the sink alive until it clears
            // the pointer with this same mutex.
            Some(sink) => unsafe { sink.as_ref() }
                .try_alloc_slab(len)
                .map_err(PyRuntimeError::new_err),
            None => Ok(None),
        }
    }
}

#[pymethods]
impl PyResponseSink {
    fn write(&self, py: Python<'_>, payload: &Bound<'_, PyAny>) -> PyResult<()> {
        let mut inline = Vec::new();
        let mut slab = None;
        if !payload.is_none() {
            if let Some(len) = deferred_plan_len(payload)? {
                if let Some(target) = self.try_alloc(len)? {
                    slab = Some(write_deferred_slab(payload, target)?);
                } else {
                    inline = materialize_deferred_plan(payload, len)?;
                }
            } else {
                let buffer = PyBuffer::<u8>::get(payload)?;
                if let Some(mut target) = self.try_alloc(buffer.len_bytes())? {
                    buffer.copy_to_slice(py, target.as_mut_slice())?;
                    slab = Some(target);
                } else {
                    inline = snapshot_python_buffer(py, &buffer)?;
                }
            }
        }
        let mut state = self.state.lock();
        self.ensure_writable(&state)?;
        state.response = Some(match slab {
            // SAFETY: checked active under the state lock. A slab was only
            // allocated if this scope has a native response pool.
            Some(slab) => unsafe { state.sink.unwrap().as_ref() }
                .finish_slab(slab)
                .map_err(PyRuntimeError::new_err)?,
            None => EncodedResponse::Inline(inline),
        });
        Ok(())
    }
}

pub(crate) fn deferred_plan_len(value: &Bound<'_, PyAny>) -> PyResult<Option<usize>> {
    if value.hasattr("_c2_write_slab")? && value.hasattr("_c2_materialize_bytes")? {
        Ok(Some(value.call_method0("_c2_total_bytes")?.extract()?))
    } else {
        Ok(None)
    }
}

pub(crate) fn write_deferred_slab(
    plan: &Bound<'_, PyAny>,
    slab: WritableSlab,
) -> PyResult<WritableSlab> {
    let slab = Bound::new(plan.py(), PyWritableSlab::new(slab))?;
    plan.call_method1("_c2_write_slab", (&slab,))?;
    slab.get().take_unexported()
}

pub(crate) fn materialize_deferred_plan(plan: &Bound<'_, PyAny>, len: usize) -> PyResult<Vec<u8>> {
    let materialized = plan.call_method0("_c2_materialize_bytes")?;
    snapshot_python_buffer(plan.py(), &checked_buffer(&materialized, len)?)
}

pub(crate) fn checked_buffer(value: &Bound<'_, PyAny>, len: usize) -> PyResult<PyBuffer<u8>> {
    let buffer = PyBuffer::<u8>::get(value)?;
    if buffer.len_bytes() != len {
        return Err(PyValueError::new_err(
            "buffer size changed during payload-budget admission",
        ));
    }
    Ok(buffer)
}

pub(crate) fn snapshot_python_buffer(py: Python<'_>, buffer: &PyBuffer<u8>) -> PyResult<Vec<u8>> {
    let mut bytes = vec![0_u8; buffer.len_bytes()];
    buffer.copy_to_slice(py, &mut bytes)?;
    Ok(bytes)
}

pub(crate) fn python_error_to_c2(py: Python<'_>, error: PyErr) -> C2Error {
    if let Some(mut semantic) = semantic_payload_from_exception(py, &error) {
        // A resource callback's error is already canonical: strip any inner
        // call-control detail so downstream admission or routing flags never
        // leak into this call's classification.
        c2_core::strip_callback_error_phases(&mut semantic);
        return semantic;
    }
    C2Error::new(ErrorCode::ResourceFunctionExecuting, error.to_string()).with_details(
        BTreeMap::from([
            ("cause_owner".to_string(), "python".to_string()),
            (
                "python_exception".to_string(),
                error
                    .get_type(py)
                    .name()
                    .map_or_else(|_| "<unknown>".to_string(), |name| name.to_string()),
            ),
        ]),
    )
}

/// Extract the canonical `(code, message, details)` triple directly from a
/// `c_two.error` exception: a `CCError` (or subclass), or a `CoreError`
/// carrying a semantic payload. No in-process C2E1 wire round-trip exists.
fn semantic_payload_from_exception(py: Python<'_>, error: &PyErr) -> Option<C2Error> {
    let value = error.value(py);
    let module = py.import("c_two.error").ok()?;
    let cc_error_class = module.getattr("CCError").ok()?;
    if value.is_instance(&cc_error_class).ok()? {
        let code = value.getattr("code").ok()?.extract::<u16>().ok()?;
        let message = value.getattr("message").ok()?.extract::<String>().ok()?;
        let details = value
            .getattr("details")
            .ok()?
            .extract::<BTreeMap<String, String>>()
            .ok()?;
        let code = ErrorCode::try_from(code).ok()?;
        return Some(C2Error::new(code, message).with_details(details));
    }
    let core_error_class = module.getattr("CoreError").ok()?;
    if value.is_instance(&core_error_class).ok()? {
        type SemanticSnapshot = (u16, String, Vec<(String, String)>, Option<String>);
        let (code, message, entries, _) = value
            .getattr("_semantic_snapshot")
            .ok()?
            .extract::<Option<SemanticSnapshot>>()
            .ok()??;
        let code = ErrorCode::try_from(code).ok()?;
        return Some(C2Error::new(code, message).with_details(entries.into_iter().collect()));
    }
    None
}

#[pyfunction]
fn core_capability_receipt<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
    const ROWS: [(&str, &str, &str, &str, bool); 12] = [
        (
            "descriptor_release_identity",
            "c2-contract",
            "projection",
            "native_projection",
            true,
        ),
        (
            "route_lifecycle",
            "c2-core",
            "facade",
            "native_facade",
            true,
        ),
        ("direct_ipc", "c2-core", "supported", "supported", true),
        ("explicit_relay", "c2-core", "supported", "supported", true),
        ("relay_aware", "c2-core", "supported", "supported", true),
        (
            "generated_client_service",
            "c2-codegen",
            "supported",
            "supported",
            true,
        ),
        (
            "no_payload_record_object_graph",
            "fastdb",
            "supported",
            "supported",
            true,
        ),
        (
            "owned_held_borrowed",
            "c2-core+fastdb",
            "projection",
            "projection",
            true,
        ),
        (
            "structured_c2_error",
            "c2-error",
            "typed",
            "typed_exception",
            true,
        ),
        (
            "structured_fastdb_cause",
            "fastdb+c2-error",
            "preserved",
            "preserved",
            true,
        ),
        (
            "python_pickle_thread_local",
            "python-sdk",
            "not_copied",
            "explicitly_nonportable",
            false,
        ),
        (
            "advanced_runtime_embedding",
            "c2-core",
            "public_crate",
            "native_binding",
            true,
        ),
    ];

    let receipt = PyDict::new(py);
    receipt.set_item("schema", "c-two.sdk-capability-parity.v1")?;
    let rows = PyList::empty(py);
    for (capability, authority, rust, python, portable) in ROWS {
        let row = PyDict::new(py);
        row.set_item("capability", capability)?;
        row.set_item("authority", authority)?;
        row.set_item("rust", rust)?;
        row.set_item("python", python)?;
        row.set_item("portable", portable)?;
        rows.append(row)?;
    }
    receipt.set_item("rows", rows)?;
    Ok(receipt)
}

pub(crate) fn register_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyCoreClient>()?;
    module.add_class::<PyNativeCallOptions>()?;
    module.add_class::<PyNativeCallBudget>()?;
    module.add_class::<PyNativePreparedCall>()?;
    module.add_class::<PyCoreResponse>()?;
    module.add_class::<PyCoreRequestBuffer>()?;
    module.add_function(wrap_pyfunction!(core_capability_receipt, module)?)?;
    Ok(())
}

#[cfg(test)]
mod payload_tests {
    use super::*;
    use pyo3::types::{PyMemoryView, PyModule};
    use std::ffi::CString;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn module<'py>(py: Python<'py>, source: &str) -> Bound<'py, PyModule> {
        PyModule::from_code(
            py,
            &CString::new(source).unwrap(),
            c"payload_test.py",
            c"payload_test",
        )
        .unwrap()
    }

    #[test]
    fn callback_keeps_native_semantic_snapshot_and_drops_inner_controls() {
        Python::initialize();
        Python::attach(|py| {
            let detail_pairs = [
                ("transport_phase", "pre_dispatch"),
                ("dispatch_phase", "pre_dispatch"),
                ("fallback_eligible", "true"),
                ("route_withdrawal", "true"),
                ("marker", "keep"),
            ];
            let semantic = C2Error::new(ErrorCode::CallDeadlineExceeded, "native refusal")
                .with_details(
                    detail_pairs
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .collect(),
                );
            let error = core_error_to_py(c2_core::Error::Semantic(semantic));
            let value = error.value(py);
            value
                .setattr("code", u16::from(ErrorCode::ResourceClosed))
                .unwrap();
            value
                .setattr("semantic_message", "changed diagnostic")
                .unwrap();
            value.setattr("details", PyDict::new(py)).unwrap();

            let callback_error = python_error_to_c2(py, error);
            assert_eq!(callback_error.code, ErrorCode::CallDeadlineExceeded);
            assert_eq!(callback_error.message, "native refusal");
            assert_eq!(
                callback_error.details,
                BTreeMap::from([("marker".into(), "keep".into())]),
            );
        });
    }

    fn pause_once(
        slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>,
    ) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (reached, waiting) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        *slot.lock() = Some(Box::new(move || {
            reached.send(()).unwrap();
            paused.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
        (waiting, resume)
    }

    fn tracker() -> PyLocalCopyTracker {
        PyLocalCopyTracker::from_arc(Arc::new(c2_core::memory::LocalCopyTracker::new()))
    }

    fn assert_counts(tracker: &PyLocalCopyTracker, holds: usize, bytes: usize) {
        let stats = tracker.inner.stats();
        assert_eq!(stats.active_holds, holds);
        assert_eq!(stats.total_held_bytes, bytes);
    }

    fn assert_buffer_error(py: Python<'_>, result: PyResult<()>, message: &str) {
        let error = result.unwrap_err();
        assert!(error.is_instance_of::<PyBufferError>(py));
        assert_eq!(error.value(py).str().unwrap().to_str().unwrap(), message);
    }

    fn response_release_race(invalidate: bool) {
        Python::initialize();
        Python::attach(|py| {
            let response = Py::new(
                py,
                PyCoreResponse::new(HeldResponse::from_owned_bytes(vec![b'r'; 32])),
            )
            .unwrap();
            let tracker = tracker();
            response
                .borrow(py)
                .track_retained(&tracker, "response", "race", "client_response")
                .unwrap();
            let calls = PyList::empty(py);
            let invalidator = calls.getattr("append").unwrap().unbind();
            let worker_invalidator = invalidator.clone_ref(py);
            let (reached, resume) = pause_once(&response.borrow(py).before_release);
            let worker_response = response.clone_ref(py);
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    let owner = worker_response.borrow(py);
                    if invalidate {
                        owner.invalidate_then_release(py, worker_invalidator, py.None(), 0)
                    } else {
                        owner.release()
                    }
                })
            });
            py.detach(move || reached.recv_timeout(Duration::from_secs(10)))
                .unwrap();
            let exported = PyBuffer::<u8>::get(response.bind(py).as_any());
            resume.send(()).unwrap();
            let result = py.detach(|| worker.join()).unwrap();
            let exported = exported.unwrap();
            assert_buffer_error(
                py,
                result,
                "cannot release: buffer is currently exported as memoryview",
            );
            let owner = response.borrow(py);
            assert!(!owner.is_released());
            assert!(owner.local_copy.lock().is_some());
            assert_counts(&tracker, 1, 32);
            assert_eq!(calls.len(), 0);
            assert_eq!(exported.to_vec(py).unwrap(), vec![b'r'; 32]);
            drop(exported);
            if invalidate {
                owner
                    .invalidate_then_release(py, invalidator.clone_ref(py), py.None(), 0)
                    .unwrap();
            } else {
                owner.release().unwrap();
            }
            assert!(owner.is_released());
            assert!(owner.local_copy.lock().is_none());
            owner
                .invalidate_then_release(py, invalidator, py.None(), 0)
                .unwrap();
            owner.release().unwrap();
            assert_eq!(calls.len(), usize::from(invalidate));
            assert_counts(&tracker, 0, 0);
            assert_buffer_error(
                py,
                PyMemoryView::from(response.bind(py).as_any()).map(|_| ()),
                "buffer already released",
            );
        });
    }

    #[test]
    fn detached_response_keeps_buffer_address_and_refunds_charges() {
        Python::initialize();
        Python::attach(|py| {
            let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits::default());
            let bytes = vec![b'd'; 32];
            let ptr = bytes.as_ptr();
            let reservation = budget.try_reserve_memory(bytes.capacity() as u64).unwrap();
            let held =
                HeldResponse::from_budgeted_bytes(c2_mem::BudgetedBuffer::new(bytes, reservation));
            let owner = Bound::new(py, PyCoreResponse::new(held)).unwrap();
            let tracker = tracker();
            owner
                .get()
                .track_retained(&tracker, "response", "detach", "client_response")
                .unwrap();
            let view = PyBuffer::<u8>::get(owner.as_any()).unwrap();
            assert_eq!(view.buf_ptr().cast_const().cast::<u8>(), ptr);
            assert_eq!(budget.snapshot().memory.used_bytes, 32);
            owner.get().detach_for_owned();
            assert_eq!(budget.snapshot().memory.used_bytes, 0);
            assert_counts(&tracker, 0, 0);
            assert_eq!(view.to_vec(py).unwrap(), vec![b'd'; 32]);
            assert!(!owner.get().is_released());
            drop(view);
            owner.get().release().unwrap();
        });
    }

    #[test]
    fn fastdb_external_owner_shares_the_response_buffer_until_invalidation() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                r#"
import json
import c_two
from c_two.crm.payload_plan import fastdb_payload_binding
from fastdb4py.payload import Builder, CompiledSpec
spec = {'schema': 'fastdb.payload.v1', 'profile': 'record.v1',
        'entries': [{'id': 'value', 'cardinality': 'one',
                     'type': {'kind': 'u8', 'nullable': False}}], 'components': []}
binding = fastdb_payload_binding(spec, label='native-test')
with CompiledSpec.compile(json.dumps(spec).encode()) as compiled:
    with Builder.create(compiled) as builder:
        builder.entry_begin(0, 1).value_u8(17)
        deferred = binding.serialize(builder.freeze())
"#,
            );
            let binding = module.getattr("binding").unwrap();
            let deferred = module.getattr("deferred").unwrap();
            let mut serialized = vec![0; deferred_plan_len(&deferred).unwrap().unwrap()];
            write_deferred_range(&deferred, &mut serialized).unwrap();
            let address = serialized.as_ptr();
            let response = Bound::new(
                py,
                PyCoreResponse::new(HeldResponse::from_owned_bytes(serialized)),
            )
            .unwrap();
            let view = PyMemoryView::from(response.as_any()).unwrap();
            let payload = binding.call_method1("deserialize", (&view,)).unwrap();
            view.call_method0("release").unwrap();
            assert_eq!(response.get().exports.load(Ordering::Acquire), 1);

            let image = payload.call_method0("memoryview").unwrap();
            let exported = PyBuffer::<u8>::get(&image).unwrap();
            assert_eq!(exported.buf_ptr().cast_const().cast::<u8>(), address);
            drop(exported);
            image.call_method0("release").unwrap();

            let invalidator = binding.getattr("invalidate").unwrap().unbind();
            response
                .get()
                .invalidate_then_release(py, invalidator, payload.unbind(), 1)
                .unwrap();
            assert!(response.get().is_released());
            assert_eq!(response.get().exports.load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn detached_request_keeps_recycled_bytes_until_export_ends() {
        Python::initialize();
        Python::attach(|py| {
            let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits::default());
            let len = 512 * 1024;
            let bytes = vec![b'q'; len];
            let ptr = bytes.as_ptr();
            let owner = Bound::new(
                py,
                PyCoreRequestBuffer::new_recycled(RecycledBytes::new(bytes, budget.clone())),
            )
            .unwrap();
            let tracker = tracker();
            owner
                .get()
                .track_retained(&tracker, "request", "detach", "resource_input")
                .unwrap();
            let view = PyBuffer::<u8>::get(owner.as_any()).unwrap();
            owner.get().detach_for_owned();
            assert_counts(&tracker, 0, 0);
            drop(owner);
            assert_eq!(view.buf_ptr().cast_const().cast::<u8>(), ptr);
            assert_eq!(view.to_vec(py).unwrap(), vec![b'q'; len]);
            drop(view);
            let recycled = budget.allocate_vec(len).unwrap();
            assert_eq!(recycled.as_ptr(), ptr);
        });
    }

    #[test]
    fn writable_slab_cannot_be_taken_while_exported() {
        Python::initialize();
        Python::attach(|py| {
            let pool = Arc::new(parking_lot::RwLock::new(
                c2_mem::MemPool::new(c2_mem::PoolConfig::primary_for_budget(8 * 1024 * 1024))
                    .unwrap(),
            ));
            let slab = WritableSlab::try_alloc(pool.clone(), 64 * 1024)
                .unwrap()
                .expect("SHM");
            let owner = Bound::new(py, PyWritableSlab::new(slab)).unwrap();
            let view = PyMemoryView::from(owner.as_any()).unwrap();
            view.set_item(0, 42_u8).unwrap();
            assert_buffer_error(
                py,
                owner.get().take_unexported().map(|_| ()),
                "cannot consume an exported slab",
            );
            view.call_method0("release").unwrap();
            let slab = owner.get().take_unexported().unwrap();
            assert_eq!(slab.as_slice()[0], 42);
            assert_buffer_error(
                py,
                PyMemoryView::from(owner.as_any()).map(|_| ()),
                "slab already consumed",
            );
            drop(slab);
            assert_eq!(pool.read().stats().alloc_count, 0);
        });
    }

    #[test]
    fn response_sink_rejects_shm_coordinates() {
        Python::initialize();
        Python::attach(|py| {
            let sink = PyResponseSink::new(None);
            let coordinates = (1, 0, 32, false).into_pyobject(py).unwrap();
            assert!(sink.write(py, coordinates.as_any()).is_err());
            assert!(sink.close().is_none());
        });
    }

    #[test]
    fn stream_destination_cannot_be_exported_after_the_write() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                r#"
class Plan:
    def _c2_write_slab(self, target):
        self.target = target
        with memoryview(target) as view:
            view[:] = b'data'
        raise ValueError('write failed')
"#,
            );
            let plan = module.getattr("Plan").unwrap().call0().unwrap();
            let mut destination = [0; 4];
            let error = write_deferred_range(&plan, &mut destination).unwrap_err();
            assert!(error.is_instance_of::<PyValueError>(py));
            assert_eq!(&destination, b"data");
            let target = plan.getattr("target").unwrap();
            assert_buffer_error(
                py,
                PyMemoryView::from(&target).map(|_| ()),
                "writable buffer scope has ended",
            );
        });
    }

    #[test]
    fn response_copy_releases_reassembly_before_local_hold() {
        Python::initialize();
        Python::attach(|py| {
            let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits::default());
            let pool = Arc::new(parking_lot::RwLock::new(
                c2_mem::MemPool::new_with_budget(
                    c2_mem::PoolConfig {
                        pool_enabled: false,
                        spill_threshold: 1.0,
                        ..Default::default()
                    },
                    budget.clone(),
                )
                .unwrap(),
            ));
            let mut backing =
                c2_wire::chunk::backing::ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
            backing.write_at(0, &[b'r'; 512]).unwrap();
            assert!(budget.snapshot().memory.used_bytes >= 512);
            let held =
                HeldResponse::from_response_data(c2_ipc::ResponseData::Handle(backing)).unwrap();
            assert_eq!(budget.snapshot().memory.used_bytes, 0);
            assert_eq!(pool.read().stats().alloc_count, 0);
            let owner = PyCoreResponse::new(held);
            let tracker = tracker();
            owner
                .track_retained(&tracker, "local", "copy", "client_response")
                .unwrap();
            assert_counts(&tracker, 1, 512);
            drop(pool);
            assert_eq!(owner.__bytes__(py).unwrap().as_bytes(), &[b'r'; 512]);
            owner.release().unwrap();
            assert!(owner.inner.lock().is_none());
            assert_counts(&tracker, 0, 0);
        });
    }

    #[test]
    fn response_invalidator_error_preserves_original_and_drops_local_bytes_once() {
        Python::initialize();
        Python::attach(|py| {
            let source = module(
                py,
                "calls = []\nerror = ValueError('original')\ndef invalidate(value):\n calls.append(value)\n raise error\n",
            );
            let owner = PyCoreResponse::new(HeldResponse::from_owned_bytes(vec![1; 32]));
            let tracker = tracker();
            owner
                .track_retained(&tracker, "local", "error", "client_response")
                .unwrap();
            let invalidator = source.getattr("invalidate").unwrap().unbind();
            let error = owner
                .invalidate_then_release(py, invalidator.clone_ref(py), py.None(), 0)
                .unwrap_err();
            assert!(error.value(py).is(&source.getattr("error").unwrap()));
            assert!(owner.inner.lock().is_none());
            assert_counts(&tracker, 0, 0);
            owner
                .invalidate_then_release(py, invalidator, py.None(), 0)
                .unwrap();
            owner.release().unwrap();
            assert_eq!(source.getattr("calls").unwrap().len().unwrap(), 1);
        });
    }

    #[test]
    fn response_release_rejects_export_before_inner_lock() {
        response_release_race(false);
    }

    #[test]
    fn response_invalidation_rejects_export_before_inner_lock() {
        response_release_race(true);
    }

    #[test]
    fn request_release_rejects_export_before_released_store() {
        Python::initialize();
        Python::attach(|py| {
            let request = Py::new(py, PyCoreRequestBuffer::new(vec![b'q'; 32])).unwrap();
            let tracker = tracker();
            request
                .borrow(py)
                .track_retained(&tracker, "request", "race", "resource_input")
                .unwrap();
            let (reached, resume) = pause_once(&request.borrow(py).before_release);
            let worker_request = request.clone_ref(py);
            let worker = std::thread::spawn(move || {
                Python::attach(|py| worker_request.borrow(py).release())
            });
            py.detach(move || reached.recv_timeout(Duration::from_secs(10)))
                .unwrap();
            let view = PyMemoryView::from(request.bind(py).as_any());
            resume.send(()).unwrap();
            let result = py.detach(|| worker.join()).unwrap();
            let view = view.unwrap();
            assert_buffer_error(
                py,
                result,
                "cannot release: buffer is currently exported as memoryview",
            );
            let owner = request.borrow(py);
            assert!(owner.inner.lock().is_some());
            assert!(owner.inner.lock().as_ref().unwrap().local_copy.is_some());
            assert_counts(&tracker, 1, 32);
            let exported = PyBuffer::<u8>::get(view.as_any()).unwrap();
            assert_eq!(exported.to_vec(py).unwrap(), vec![b'q'; 32]);
            drop(exported);
            view.call_method0("release").unwrap();
            owner.release().unwrap();
            assert!(owner.inner.lock().is_none());
            assert!(owner.inner.lock().is_none());
            owner.release().unwrap();
            assert_counts(&tracker, 0, 0);
            assert_buffer_error(
                py,
                PyBuffer::<u8>::get(request.bind(py).as_any()).map(|_| ()),
                "buffer already released",
            );
        });
    }

    #[test]
    fn request_tracking_after_release_does_not_revive_local_copy() {
        Python::initialize();
        Python::attach(|py| {
            let request = Py::new(py, PyCoreRequestBuffer::new(vec![b'q'; 32])).unwrap();
            let tracker = tracker();
            request
                .borrow(py)
                .track_retained(&tracker, "request", "initial", "resource_input")
                .unwrap();
            assert_counts(&tracker, 1, 32);
            let (reached, resume) = pause_once(&request.borrow(py).before_track_retained);
            let worker_request = request.clone_ref(py);
            let worker_tracker = tracker.clone();
            let worker = std::thread::spawn(move || {
                Python::attach(|py| {
                    worker_request.borrow(py).track_retained(
                        &worker_tracker,
                        "request",
                        "late",
                        "resource_input",
                    )
                })
            });
            py.detach(move || reached.recv_timeout(Duration::from_secs(10)))
                .unwrap();
            let released = request.borrow(py).release();
            let after_release = tracker.inner.stats();
            resume.send(()).unwrap();
            py.detach(|| worker.join()).unwrap().unwrap();
            released.unwrap();
            assert_eq!(
                (after_release.active_holds, after_release.total_held_bytes),
                (0, 0)
            );
            let owner = request.borrow(py);
            assert!(owner.inner.lock().is_none());
            assert_counts(&tracker, 0, 0);
            assert!(owner.inner.lock().is_none());
            owner
                .track_retained(&tracker, "request", "later", "resource_input")
                .unwrap();
            owner.release().unwrap();
            assert_counts(&tracker, 0, 0);
            assert_buffer_error(
                py,
                PyMemoryView::from(request.bind(py).as_any()).map(|_| ()),
                "buffer already released",
            );
        });
    }

    #[test]
    fn request_snapshot_moves_into_python_and_outlives_service_return() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                r#"
def dispatch(route, method, request, response_sink):
    global view
    view = memoryview(request)
    if method:
        raise ValueError('dispatch failed')
"#,
            );
            let service = PyCoreService::new(
                "request-owner".into(),
                module.getattr("dispatch").unwrap().unbind(),
            );
            for method in [0, 1] {
                let request = vec![b'x'; 32];
                let address = request.as_ptr();
                assert_eq!(service.invoke(method, request).is_err(), method == 1);
                let view = module.getattr("view").unwrap();
                let owner = view.getattr("obj").unwrap();
                let owner = owner.cast::<PyCoreRequestBuffer>().unwrap().borrow();
                let tracker = PyLocalCopyTracker::from_arc(std::sync::Arc::new(
                    c2_core::memory::LocalCopyTracker::new(),
                ));
                owner
                    .track_retained(&tracker, "request-owner", "dispatch", "resource_input")
                    .unwrap();
                assert_eq!(tracker.inner.stats().total_held_bytes, 32);
                assert_eq!(owner.inner.lock().as_ref().unwrap().bytes.as_ptr(), address);
                assert_eq!(
                    view.call_method0("tobytes")
                        .unwrap()
                        .extract::<Vec<u8>>()
                        .unwrap(),
                    vec![b'x'; 32],
                );
                assert!(
                    owner
                        .release()
                        .unwrap_err()
                        .is_instance_of::<PyBufferError>(py)
                );
                assert_eq!(tracker.inner.stats().active_holds, 1);
                view.call_method0("release").unwrap();
                owner.release().unwrap();
                assert_eq!(tracker.inner.stats().active_holds, 0);
                assert_eq!(tracker.inner.stats().total_held_bytes, 0);
                owner.release().unwrap();
                assert!(owner.__len__().is_err());
            }
        });
    }

    #[test]
    fn mutable_and_readonly_alias_snapshots_own_stable_bytes() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                "backing = bytearray(b'original')\nalias = memoryview(backing).toreadonly()\n",
            );
            for name in ["backing", "alias"] {
                let source = module.getattr(name).unwrap();
                let buffer = PyBuffer::<u8>::get(&source).unwrap();
                let snapshot = snapshot_python_buffer(py, &buffer).unwrap();
                drop(buffer);
                module
                    .getattr("backing")
                    .unwrap()
                    .set_item(0, b'X')
                    .unwrap();
                assert_eq!(snapshot, b"original");
                module
                    .getattr("backing")
                    .unwrap()
                    .set_item(0, b'o')
                    .unwrap();
            }
        });
    }

    #[test]
    fn exact_bytes_owner_pins_storage_until_detached_factory_finishes() {
        Python::initialize();
        Python::attach(|py| {
            let source = PyBytes::new(py, b"factory-owned input");
            let owner = pyo3::pybacked::PyBackedBytes::from(source.clone());
            assert_eq!(owner.as_ptr(), source.as_bytes().as_ptr());
            let factory = move || owner.to_vec();
            drop(source);

            let bytes = py.detach(move || std::thread::spawn(factory).join().unwrap());
            assert_eq!(bytes, b"factory-owned input");
        });
    }

    #[test]
    fn bytes_subclass_is_snapshotted_and_finalized_on_caller() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                r#"
import threading
caller = threading.get_ident()
events = []
class CustomBytes(bytes):
    def __del__(self):
        events.append(threading.get_ident())
source = CustomBytes(b'snapshot')
"#,
            );
            let source = module.getattr("source").unwrap();
            assert!(source.cast_exact::<PyBytes>().is_err());
            let buffer = PyBuffer::<u8>::get(&source).unwrap();
            let snapshot = snapshot_python_buffer(py, &buffer).unwrap();
            drop(buffer);
            module.delattr("source").unwrap();
            drop(source);
            assert_eq!(snapshot, b"snapshot");
            let caller: u64 = module.getattr("caller").unwrap().extract().unwrap();
            let events: Vec<u64> = module.getattr("events").unwrap().extract().unwrap();
            assert_eq!(events, vec![caller]);
        });
    }
}
