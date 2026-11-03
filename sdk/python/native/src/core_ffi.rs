use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use c2_core::memory::LocalCopyGuard;
use c2_core::{
    CallOptions, CallTimeout, Client, EncodedService, HeldResponse, LifecycleError, ObservedPath,
    ObservedRoute, PreparedCall, ServerFailure,
};
use c2_error::{C2Error, ErrorCode};

use crate::core_error_ffi::{core_error_to_py, lifecycle_error_to_py};
use crate::local_copy_ffi::PyLocalCopyTracker;

#[pyclass(name = "CoreClient", frozen, skip_from_py_object)]
pub(crate) struct PyCoreClient {
    inner: Mutex<Option<Client>>,
    route_name: String,
    observed_path: ObservedPath,
    observed_route: ObservedRoute,
}

impl PyCoreClient {
    pub(crate) fn new(client: Client) -> Self {
        let route_name = client.expected_route().route_name.clone();
        let observed_path = client.observed_path();
        let observed_route = client.observed_route().clone();
        Self {
            inner: Mutex::new(Some(client)),
            route_name,
            observed_path,
            observed_route,
        }
    }

    // Keep this view lock out of preparation, config, materialization and I/O.
    fn client(&self) -> PyResult<Client> {
        self.inner.lock().clone().ok_or_else(|| {
            lifecycle_error_to_py(LifecycleError::Server(ServerFailure::new(
                "Core client is closed".into(),
            )))
        })
    }
}

#[pyclass(name = "NativeCallOptions", frozen, skip_from_py_object)]
struct PyNativeCallOptions {
    options: CallOptions,
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
}

/// One native scope and finite slot, reserved before caller-side serialization.
/// Taking the Option makes every execution attempt single use, including errors.
#[pyclass(name = "NativePreparedCall", frozen, skip_from_py_object)]
struct PyNativePreparedCall {
    inner: Mutex<Option<PreparedCall>>,
}

impl PyNativePreparedCall {
    fn take(&self) -> PyResult<PreparedCall> {
        self.inner.lock().take().ok_or_else(|| {
            PyValueError::new_err("prepared call has already been consumed or closed")
        })
    }

    fn finish(py: Python<'_>, encoded: c2_core::EncodedCall) -> PyResult<PyCoreResponse> {
        py.detach(move || encoded.call_held())
            .map(PyCoreResponse::new)
            .map_err(core_error_to_py)
    }
}

#[pymethods]
impl PyNativePreparedCall {
    fn charge_input(&self, nbytes: u64) -> PyResult<()> {
        let mut inner = self.inner.lock();
        inner
            .as_mut()
            .ok_or_else(|| {
                PyValueError::new_err("prepared call has already been consumed or closed")
            })?
            .charge_input(nbytes)
            .map_err(core_error_to_py)
    }

    fn call(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<PyCoreResponse> {
        let mut prepared = self.take()?;
        if let Ok(bytes) = data.cast_exact::<PyBytes>() {
            let nbytes = bytes.as_bytes().len();
            // Exact built-in bytes have no user finalizer; their storage is immutable; the strong Py owner is Send and pins its
            // storage until the Core-owned factory finishes. The owner is
            // accessed under the GIL, the native copy detaches, and no Python
            // callback runs on the Core execution worker.
            let owner = bytes.clone().unbind();
            let encoded = prepared
                .encode(nbytes, move || {
                    Ok(Python::attach(|py| {
                        let bytes = owner.bind(py).as_bytes();
                        // Immutable bytes remain pinned by owner while the native
                        // copy releases the GIL. The expired waiter can reacquire
                        // Python independently of a still-running native copy.
                        py.detach(|| bytes.to_vec())
                    }))
                })
                .map_err(core_error_to_py)?;
            return Self::finish(py, encoded);
        }
        // A read-only memoryview can still alias mutable backing. Snapshot all
        // non-bytes buffers on the caller under the GIL after native charging;
        // no raw borrowed pointer or mutable exporter crosses the handoff.
        let buffer = PyBuffer::<u8>::get(data)?;
        prepared
            .charge_input(buffer.len_bytes() as u64)
            .map_err(core_error_to_py)?;
        let request = snapshot_python_buffer(py, &buffer)?;
        let encoded = prepared.encode_vec(request).map_err(core_error_to_py)?;
        Self::finish(py, encoded)
    }

    fn close(&self) {
        let closed = self.inner.lock().take();
        drop(closed);
    }
}

#[pymethods]
impl PyCoreClient {
    fn connection_observation(&self) -> PyResult<crate::connection_ffi::PyConnectionObservation> {
        self.client()?
            .connection_observation()
            .map(|inner| crate::connection_ffi::PyConnectionObservation { inner })
            .map_err(core_error_to_py)
    }

    #[getter]
    fn mode(&self) -> &'static str {
        match self.observed_path {
            ObservedPath::DirectIpc | ObservedPath::RelayAwareLocalIpc => "ipc",
            ObservedPath::ExplicitRelay | ObservedPath::RelayAwareRelay => "http",
        }
    }

    #[getter]
    fn route_name(&self) -> &str {
        &self.route_name
    }

    #[getter]
    fn observed_path(&self) -> &'static str {
        match self.observed_path {
            ObservedPath::DirectIpc => "direct_ipc",
            ObservedPath::ExplicitRelay => "explicit_relay",
            ObservedPath::RelayAwareLocalIpc => "relay_aware_local_ipc",
            ObservedPath::RelayAwareRelay => "relay_aware_relay",
        }
    }

    #[getter]
    fn route_uid(&self) -> &str {
        &self.observed_route.route_uid
    }

    #[pyo3(signature = (*, timeout=inherit_timeout()))]
    fn with_call_options(&self, py: Python<'_>, timeout: Py<PyAny>) -> PyResult<Self> {
        let client = self.client()?;
        Ok(Self::new(
            client.with_call_options(call_options(py, &timeout)?),
        ))
    }

    fn begin_call(&self, py: Python<'_>, method_name: &str) -> PyResult<PyNativePreparedCall> {
        let client = self.client()?;
        let prepared = py
            .detach(move || client.begin_call(method_name))
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
        self.begin_call(py, method_name)?.call(py, data)
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
    local_copy: Mutex<Option<LocalCopyGuard>>,
    data_len: usize,
    exports: AtomicU32,
    #[cfg(test)]
    before_release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PyCoreResponse {
    fn new(held: HeldResponse) -> Self {
        let data_len = held.bytes().len();
        Self {
            inner: Mutex::new(Some(held)),
            local_copy: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
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

    fn invalidate_then_release(
        &self,
        py: Python<'_>,
        invalidator: Py<PyAny>,
        value: Py<PyAny>,
    ) -> PyResult<()> {
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
        let mut python_error = None;
        let result = held.invalidate_then_release(|| {
            invalidator
                .call1(py, (value,))
                .map(|_| ())
                .map_err(|error| {
                    let message = error.to_string();
                    python_error = Some(error);
                    message
                })
        });
        if let Some(error) = python_error {
            return Err(error);
        }
        result.map_err(lifecycle_error_to_py)
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
            (*view).buf = bytes.as_ptr().cast_mut().cast();
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = bytes.len() as isize;
            (*view).readonly = 1;
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
            (*view).strides = if flags & ffi::PyBUF_STRIDES != 0 {
                &mut (*view).itemsize
            } else {
                std::ptr::null_mut()
            };
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
        }
        this.exports.fetch_add(1, Ordering::Release);
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

struct LocalRequestBuffer {
    bytes: Vec<u8>,
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
    fn new(bytes: Vec<u8>) -> Self {
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
            (*view).buf = state.bytes.as_ptr().cast_mut().cast();
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = state.bytes.len() as isize;
            (*view).readonly = 1;
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
            (*view).strides = if flags & ffi::PyBUF_STRIDES != 0 {
                &mut (*view).itemsize
            } else {
                std::ptr::null_mut()
            };
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
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

impl EncodedService for PyCoreService {
    fn invoke(&self, method_index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Python::attach(|py| {
            let request = Py::new(py, PyCoreRequestBuffer::new(request))
                .map_err(|error| python_error_to_c2(py, error))?;
            let result = self
                .dispatcher
                .call1(py, (self.route_name.as_str(), method_index, request))
                .map_err(|error| python_error_to_c2(py, error))?;
            let result = result.bind(py);
            if result.is_none() {
                return Ok(Vec::new());
            }
            materialize_python_bytes(py, result).map_err(|error| python_error_to_c2(py, error))
        })
    }
}

// Caller/GIL snapshot for mutable exporters, read-only aliases and bytes
// subclasses. Only the returned native Vec crosses execution handoff.
fn snapshot_python_buffer(py: Python<'_>, buffer: &PyBuffer<u8>) -> PyResult<Vec<u8>> {
    let mut bytes = vec![0_u8; buffer.len_bytes()];
    buffer.copy_to_slice(py, &mut bytes)?;
    Ok(bytes)
}

fn materialize_python_bytes(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(buffer) = PyBuffer::<u8>::get(value) {
        return snapshot_python_buffer(py, &buffer);
    }
    if let Ok(to_bytes) = value.getattr("to_bytes")
        && to_bytes.is_callable()
    {
        let materialized = to_bytes.call0()?;
        return materialize_python_bytes(py, &materialized);
    }
    Err(PyValueError::new_err(
        "Core service/client payload must expose the buffer protocol or to_bytes()",
    ))
}

fn python_error_to_c2(py: Python<'_>, error: PyErr) -> C2Error {
    if let Ok(error_bytes) = error.value(py).getattr("error_bytes")
        && let Ok(buffer) = PyBuffer::<u8>::get(&error_bytes)
    {
        let mut bytes = vec![0_u8; buffer.len_bytes()];
        if buffer.copy_to_slice(py, &mut bytes).is_ok()
            && let Ok(Some(error)) = C2Error::from_wire_bytes(&bytes)
        {
            return error;
        }
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
                        owner.invalidate_then_release(py, worker_invalidator, py.None())
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
                    .invalidate_then_release(py, invalidator.clone_ref(py), py.None())
                    .unwrap();
            } else {
                owner.release().unwrap();
            }
            assert!(owner.is_released());
            assert!(owner.local_copy.lock().is_none());
            owner
                .invalidate_then_release(py, invalidator, py.None())
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
    fn response_copy_releases_reassembly_before_local_hold() {
        Python::initialize();
        Python::attach(|py| {
            let pool = Arc::new(parking_lot::RwLock::new(
                c2_mem::MemPool::new(c2_mem::PoolConfig {
                    segment_size: 65536,
                    min_block_size: 4096,
                    ..Default::default()
                })
                .unwrap(),
            ));
            let budget = pool.read().budget().unwrap().clone();
            let mut backing =
                c2_wire::chunk::backing::ReassemblyBacking::admit(pool.clone(), 1, 512).unwrap();
            backing.write_at(0, &[b'r'; 512]).unwrap();
            assert_eq!(budget.snapshot().reassembly.used_bytes, 512);
            let held =
                HeldResponse::from_response_data(c2_ipc::ResponseData::Handle(backing)).unwrap();
            assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
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
                .invalidate_then_release(py, invalidator.clone_ref(py), py.None())
                .unwrap_err();
            assert!(error.value(py).is(&source.getattr("error").unwrap()));
            assert!(owner.inner.lock().is_none());
            assert_counts(&tracker, 0, 0);
            owner
                .invalidate_then_release(py, invalidator, py.None())
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
def dispatch(route, method, request):
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
