use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use c2_core::{
    CallOptions, CallTimeout, Client, EncodedService, HeldResponse, LifecycleError, ObservedPath,
    ObservedRoute, PreparedCall,
};
use c2_error::{C2Error, ErrorCode};
use c2_mem::BufferLeaseGuard;

use crate::core_error_ffi::{core_error_to_py, lifecycle_error_to_py};
use crate::lease_ffi::PyBufferLeaseTracker;
use crate::writable_sink::{materialize_python_payload_plan, prepared_plan_nbytes};

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
            lifecycle_error_to_py(LifecycleError::Server("Core client is closed".into()))
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
    // Validate the monotonic clock range through Core as well as Duration.
    let options = CallOptions::with_timeout(timeout);
    c2_core::CallScope::new(options, None)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(options)
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

    fn call_prepared(&self, py: Python<'_>, plan: &Bound<'_, PyAny>) -> PyResult<PyCoreResponse> {
        let mut prepared = self.take()?;
        if let Some(nbytes) = prepared_plan_nbytes(plan)? {
            prepared
                .charge_input(nbytes as u64)
                .map_err(core_error_to_py)?;
            // write_into is user code and stays on this caller/GIL thread.
            // Its destination owns its memory, even if a view escapes.
            let request = materialize_python_payload_plan(py, plan, nbytes)?;
            return Self::finish(py, prepared.encode_vec(request).map_err(core_error_to_py)?);
        }
        if let Ok(bytes) = plan.cast_exact::<PyBytes>() {
            let owner = bytes.clone().unbind();
            let encoded = prepared
                .encode(bytes.as_bytes().len(), move || {
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
        let continuation = Self {
            inner: Mutex::new(Some(prepared)),
        };
        if PyBuffer::<u8>::get(plan).is_ok() {
            return continuation.call(py, plan);
        }
        // Unknown-size Python serialization remains on the caller. The same
        // preparation is charged against its original D before native copying.
        let materialized = plan.call_method0("to_bytes")?;
        continuation.call(py, &materialized)
    }

    fn close(&self) {
        let closed = self.inner.lock().take();
        drop(closed);
    }
}

#[pymethods]
impl PyCoreClient {
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

    #[getter]
    fn route_revision(&self) -> u64 {
        self.observed_route.route_revision
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

    fn call_prepared(
        &self,
        py: Python<'_>,
        method_name: &str,
        plan: &Bound<'_, PyAny>,
    ) -> PyResult<PyCoreResponse> {
        self.begin_call(py, method_name)?.call_prepared(py, plan)
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
    lease: Mutex<Option<BufferLeaseGuard>>,
    data_len: usize,
    exports: AtomicU32,
}

impl PyCoreResponse {
    fn new(held: HeldResponse) -> Self {
        let data_len = held.bytes().len();
        Self {
            inner: Mutex::new(Some(held)),
            lease: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
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
        self.ensure_not_exported()?;
        let mut inner = self.inner.lock();
        let Some(held) = inner.as_mut() else {
            return Ok(());
        };
        held.invalidate_then_release(|| Ok(()))
            .map_err(lifecycle_error_to_py)?;
        self.lease.lock().take();
        Ok(())
    }

    fn invalidate_then_release(
        &self,
        py: Python<'_>,
        invalidator: Py<PyAny>,
        value: Py<PyAny>,
    ) -> PyResult<()> {
        self.ensure_not_exported()?;
        let mut inner = self.inner.lock();
        let Some(held) = inner.as_mut() else {
            return Ok(());
        };

        let mut python_error = None;
        let release_result =
            held.invalidate_then_release(|| match invalidator.call1(py, (value.clone_ref(py),)) {
                Ok(_) => Ok(()),
                Err(error) => {
                    let message = error.to_string();
                    python_error = Some(error);
                    Err(message)
                }
            });
        self.lease.lock().take();

        if let Some(error) = python_error {
            if let Err(release_error) = release_result {
                let note = format!("C-Two Core release also reported: {release_error}");
                let _ = error.value(py).call_method1("add_note", (note,));
            }
            return Err(error);
        }
        release_result.map_err(lifecycle_error_to_py)
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="client_response"))]
    fn track_retained(
        &self,
        tracker: &PyBufferLeaseTracker,
        route_name: &str,
        method_name: &str,
        direction: &str,
    ) -> PyResult<()> {
        let inner = self.inner.lock();
        if inner.as_ref().is_none_or(HeldResponse::is_released) {
            return Ok(());
        }
        let lease = tracker.track_retained_guard(
            route_name,
            method_name,
            direction,
            "inline",
            self.data_len,
        )?;
        *self.lease.lock() = Some(lease);
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

#[pyclass(name = "CoreRequestBuffer", frozen, skip_from_py_object)]
struct PyCoreRequestBuffer {
    bytes: Vec<u8>,
    released: AtomicBool,
    exports: AtomicU32,
    lease: Mutex<Option<BufferLeaseGuard>>,
}

impl PyCoreRequestBuffer {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            released: AtomicBool::new(false),
            exports: AtomicU32::new(0),
            lease: Mutex::new(None),
        }
    }
}

#[pymethods]
impl PyCoreRequestBuffer {
    fn __len__(&self) -> PyResult<usize> {
        if self.released.load(Ordering::Acquire) {
            Err(PyValueError::new_err("buffer already released"))
        } else {
            Ok(self.bytes.len())
        }
    }

    fn release(&self) -> PyResult<()> {
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ));
        }
        self.released.store(true, Ordering::Release);
        self.lease.lock().take();
        Ok(())
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="resource_input"))]
    fn track_retained(
        &self,
        tracker: &PyBufferLeaseTracker,
        route_name: &str,
        method_name: &str,
        direction: &str,
    ) -> PyResult<()> {
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        let lease = tracker.track_retained_guard(
            route_name,
            method_name,
            direction,
            "inline",
            self.bytes.len(),
        )?;
        *self.lease.lock() = Some(lease);
        Ok(())
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        if this.released.load(Ordering::Acquire) {
            return Err(PyBufferError::new_err("buffer already released"));
        }
        unsafe {
            (*view).buf = this.bytes.as_ptr().cast_mut().cast();
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = this.bytes.len() as isize;
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
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        Python::attach(|py| {
            let request = Py::new(py, PyCoreRequestBuffer::new(request))
                .map_err(|error| python_error_to_c2(py, error))?;
            let result = self
                .dispatcher
                .call1(
                    py,
                    (self.route_name.as_str(), method_index, request, py.None()),
                )
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
    if let Some(nbytes) = prepared_plan_nbytes(value)? {
        return materialize_python_payload_plan(py, value, nbytes);
    }
    Err(PyValueError::new_err(
        "Core service/client payload must expose the buffer protocol, to_bytes(), or write_into()",
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
    use pyo3::types::PyModule;
    use std::ffi::CString;

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

    #[test]
    fn response_writer_transfers_its_only_vec_and_escaped_failure_stays_safe() {
        Python::initialize();
        Python::attach(|py| {
            let module = module(
                py,
                r#"
import ctypes
class Plan:
    nbytes = 32
    def write_into(self, sink):
        self.address = ctypes.addressof(ctypes.c_char.from_buffer(sink))
        with memoryview(sink) as view:
            view[:] = b'a' * self.nbytes
plan = Plan()
class EscapingPlan:
    nbytes = 32
    def __init__(self, fail):
        self.fail = fail
    def write_into(self, sink):
        self.view = memoryview(sink)
        self.view[:] = b'b' * self.nbytes
        if self.fail:
            raise ValueError('writer failed')
"#,
            );
            let plan = module.getattr("plan").unwrap();
            let bytes = materialize_python_bytes(py, &plan).unwrap();
            let address: usize = plan.getattr("address").unwrap().extract().unwrap();
            assert_eq!(address, bytes.as_ptr() as usize);
            assert_eq!(bytes, vec![b'a'; 32]);
            for fail in [false, true] {
                let plan = module
                    .getattr("EscapingPlan")
                    .unwrap()
                    .call1((fail,))
                    .unwrap();
                let error = materialize_python_bytes(py, &plan).unwrap_err();
                if fail {
                    assert!(error.is_instance_of::<PyValueError>(py));
                } else {
                    assert!(error.is_instance_of::<PyBufferError>(py));
                }
                // A Python exception traceback can also retain callback
                // locals. Release it before proving export-only ownership.
                drop(error);
                let view = plan.getattr("view").unwrap();
                // Drop the plan/sink callback owners before using the escaped
                // view: its buffer export alone keeps the native Vec alive.
                drop(plan);
                assert_eq!(
                    view.call_method0("tobytes")
                        .unwrap()
                        .extract::<Vec<u8>>()
                        .unwrap(),
                    vec![b'b'; 32]
                );
                view.set_item(0, b'c').unwrap();
                view.call_method0("release").unwrap();
            }
        });
    }
}
