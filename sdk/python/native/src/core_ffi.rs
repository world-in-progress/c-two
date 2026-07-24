use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use c2_core::{
    Client, EncodedClient, EncodedService, HeldResponse, LifecycleError, ObservedPath,
    ObservedRoute,
};
use c2_error::{C2Error, ErrorCode};
use c2_mem::BufferLeaseGuard;

use crate::core_error_ffi::{core_error_to_py, lifecycle_error_to_py};
use crate::lease_ffi::PyBufferLeaseTracker;
use crate::writable_sink::{prepared_plan_nbytes, write_python_payload_plan};

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

    fn call_bytes<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        request: Vec<u8>,
    ) -> PyResult<Py<PyAny>> {
        let result = py.detach(|| {
            let client = self.inner.lock();
            let client = client
                .as_ref()
                .ok_or_else(|| LifecycleError::Server("Core client is closed".to_string()))?;
            client
                .call_held(method_name, &request)
                .map_err(CoreCallFailure::Core)
        });
        let held = match result {
            Ok(held) => held,
            Err(CoreCallFailure::Core(error)) => return Err(core_error_to_py(error)),
            Err(CoreCallFailure::Lifecycle(error)) => {
                return Err(lifecycle_error_to_py(error));
            }
        };
        Ok(Py::new(py, PyCoreResponse::new(held))?.into_any())
    }
}

enum CoreCallFailure {
    Core(c2_core::Error),
    Lifecycle(LifecycleError),
}

impl From<LifecycleError> for CoreCallFailure {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
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

    fn call<'py>(&self, py: Python<'py>, method_name: &str, data: &[u8]) -> PyResult<Py<PyAny>> {
        self.call_bytes(py, method_name, data.to_vec())
    }

    fn call_prepared<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        plan: &Bound<'py, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let request = materialize_python_bytes(py, plan)?;
        self.call_bytes(py, method_name, request)
    }

    fn close(&self) {
        self.inner.lock().take();
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

fn materialize_python_bytes(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(buffer) = PyBuffer::<u8>::get(value) {
        let mut bytes = vec![0_u8; buffer.len_bytes()];
        buffer.copy_to_slice(py, &mut bytes)?;
        return Ok(bytes);
    }
    if let Ok(to_bytes) = value.getattr("to_bytes")
        && to_bytes.is_callable()
    {
        let materialized = to_bytes.call0()?;
        return materialize_python_bytes(py, &materialized);
    }
    if let Some(nbytes) = prepared_plan_nbytes(value)? {
        let mut bytes = vec![0_u8; nbytes];
        write_python_payload_plan(py, value, &mut bytes)?;
        return Ok(bytes);
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
    module.add_class::<PyCoreResponse>()?;
    module.add_class::<PyCoreRequestBuffer>()?;
    module.add_function(wrap_pyfunction!(core_capability_receipt, module)?)?;
    Ok(())
}
