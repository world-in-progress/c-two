//! PyO3 bindings for the C-Two IPC client (`c2-ipc`).
//!
//! Exposes `RustClient` — a synchronous IPC client for CRM calls, and
//! `RustClientPool` — a process-level pool of shared clients.
//!
//! **GIL handling**: all blocking operations release the GIL via
//! `py.detach()`. `#[pyclass(frozen)]` ensures thread-safety
//! for free-threading builds.

use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pyo3::exceptions::{PyBufferError, PyRuntimeError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::lease_ffi::PyBufferLeaseTracker;
use crate::writable_sink::{prepared_plan_nbytes, write_python_payload_plan};
use c2_config::{BaseIpcConfig, ClientIpcConfig};
use c2_ipc::{ClientPool, IpcError, ResponseData, ServerPoolState, SyncClient};
use c2_mem::{BufferLeaseGuard, MemPool};

// ---------------------------------------------------------------------------
// CrmCallError — custom exception carrying serialised error bytes
// ---------------------------------------------------------------------------

pyo3::create_exception!(c_two._native, CrmCallError, pyo3::exceptions::PyException);

// ---------------------------------------------------------------------------
// PyResponseBuffer — zero-copy response with buffer protocol
// ---------------------------------------------------------------------------

/// Internal storage for response data.
enum ResponseBufferInner {
    Inline(Vec<u8>),
    Shm {
        pool: Arc<Mutex<Option<ServerPoolState>>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    Handle {
        handle: c2_mem::MemHandle,
        pool: Arc<RwLock<MemPool>>,
    },
}

/// Zero-copy response buffer supporting Python buffer protocol.
///
/// For SHM responses, `memoryview(buf)` returns a direct view into
/// shared memory — zero copies. Call `buf.release()` when done to
/// free the SHM allocation.
///
/// For inline responses, data lives in a Rust Vec.
#[pyclass(name = "ResponseBuffer", frozen)]
pub struct PyResponseBuffer {
    inner: Mutex<Option<ResponseBufferInner>>,
    lease: Mutex<Option<BufferLeaseGuard>>,
    data_len: usize,
    exports: AtomicU32,
}

impl PyResponseBuffer {
    fn from_response_data(
        data: ResponseData,
        pool: Arc<Mutex<Option<ServerPoolState>>>,
        reassembly_pool: Arc<RwLock<MemPool>>,
    ) -> Self {
        let data_len = data.len();
        let inner = match data {
            ResponseData::Inline(vec) => ResponseBufferInner::Inline(vec),
            ResponseData::Shm {
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            } => ResponseBufferInner::Shm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            },
            ResponseData::Handle(handle) => ResponseBufferInner::Handle {
                handle,
                pool: reassembly_pool,
            },
        };
        Self {
            inner: Mutex::new(Some(inner)),
            lease: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
        }
    }

    fn storage_label_for_inner(inner: &ResponseBufferInner) -> &'static str {
        match inner {
            ResponseBufferInner::Inline(_) => "inline",
            ResponseBufferInner::Shm { .. } => "shm",
            ResponseBufferInner::Handle { handle, .. } => {
                if handle.is_file_spill() {
                    "file_spill"
                } else {
                    "handle"
                }
            }
        }
    }
}

#[pymethods]
impl PyResponseBuffer {
    /// Length of the response data.
    fn __len__(&self) -> PyResult<usize> {
        let guard = self.inner.lock();
        match guard.as_ref() {
            Some(_) => Ok(self.data_len),
            None => Err(PyValueError::new_err("buffer already released")),
        }
    }

    /// Convert to bytes (copies data — use memoryview for zero-copy).
    fn __bytes__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let guard = self.inner.lock();
        match guard.as_ref() {
            Some(ResponseBufferInner::Inline(vec)) => Ok(PyBytes::new(py, vec)),
            Some(ResponseBufferInner::Shm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            }) => {
                let mut pool_guard = pool.lock();
                let state = pool_guard
                    .as_mut()
                    .ok_or_else(|| PyRuntimeError::new_err("server pool not initialised"))?;
                state
                    .ensure_segment(*seg_idx, *data_size, *is_dedicated)
                    .map_err(|e| PyRuntimeError::new_err(format!("SHM lazy-open: {e}")))?;
                let ptr = state
                    .pool
                    .data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyRuntimeError::new_err(format!("SHM read: {e}")))?;
                let slice = unsafe { std::slice::from_raw_parts(ptr, *data_size as usize) };
                Ok(PyBytes::new(py, slice))
            }
            Some(ResponseBufferInner::Handle { handle, pool }) => {
                let pool_guard = pool.read();
                let slice = pool_guard.handle_slice(handle);
                Ok(PyBytes::new(py, slice))
            }
            None => Err(PyValueError::new_err("buffer already released")),
        }
    }

    /// Explicitly release the underlying SHM allocation.
    ///
    /// Must be called after all memoryviews are released.
    /// For inline responses, this is a no-op (Vec is dropped).
    fn release(&self) -> PyResult<()> {
        // Fast-path: avoid locking if exports are obviously active
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ));
        }
        let mut guard = self.inner.lock();
        // Re-check under lock to close the TOCTOU window
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ));
        }
        let inner = guard.take();
        self.lease.lock().take();
        drop(guard);

        match inner {
            Some(ResponseBufferInner::Shm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            }) => {
                let mut pool_guard = pool.lock();
                if let Some(state) = pool_guard.as_mut() {
                    let _ = state.ensure_segment(seg_idx, data_size, is_dedicated);
                    let _ = state
                        .pool
                        .free_at(seg_idx as u32, offset, data_size, is_dedicated);
                }
                Ok(())
            }
            Some(ResponseBufferInner::Handle { handle, pool }) => {
                let mut p = pool.write();
                let _ = p.release_handle(handle);
                Ok(())
            }
            Some(ResponseBufferInner::Inline(_)) => Ok(()),
            None => Ok(()), // already released, idempotent
        }
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="client_response"))]
    fn track_retained(
        &self,
        tracker: &PyBufferLeaseTracker,
        route_name: &str,
        method_name: &str,
        direction: &str,
    ) -> PyResult<()> {
        let guard = self.inner.lock();
        let Some(inner) = guard.as_ref() else {
            return Ok(());
        };
        let storage = Self::storage_label_for_inner(inner);
        let lease_guard = tracker.track_retained_guard(
            route_name,
            method_name,
            direction,
            storage,
            self.data_len,
        )?;
        let mut current = self.lease.lock();
        current.take();
        *current = Some(lease_guard);
        Ok(())
    }

    /// Buffer protocol — enables memoryview(response).
    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let guard = this.inner.lock();
        let inner = guard
            .as_ref()
            .ok_or_else(|| PyBufferError::new_err("buffer already released"))?;

        let (ptr, len) = match inner {
            ResponseBufferInner::Inline(vec) => (vec.as_ptr(), vec.len()),
            ResponseBufferInner::Shm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            } => {
                let mut pool_guard = pool.lock();
                let state = pool_guard
                    .as_mut()
                    .ok_or_else(|| PyBufferError::new_err("server pool not initialised"))?;
                state
                    .ensure_segment(*seg_idx, *data_size, *is_dedicated)
                    .map_err(|e| PyBufferError::new_err(format!("SHM lazy-open: {e}")))?;
                let raw_ptr = state
                    .pool
                    .data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyBufferError::new_err(format!("SHM access: {e}")))?;
                (raw_ptr as *const u8, *data_size as usize)
            }
            ResponseBufferInner::Handle { handle, pool } => {
                let pool_guard = pool.read();
                let slice = pool_guard.handle_slice(handle);
                (slice.as_ptr(), slice.len())
            }
        };

        // Fill the buffer view
        unsafe {
            (*view).buf = ptr as *mut std::os::raw::c_void;
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = len as isize;
            (*view).readonly = 1;
            (*view).itemsize = 1;
            (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
                // "B" = unsigned byte
                b"B\0".as_ptr() as *mut std::os::raw::c_char
            } else {
                std::ptr::null_mut()
            };
            (*view).ndim = 1;
            (*view).shape = if flags & ffi::PyBUF_ND != 0 {
                &mut (*view).len as *mut isize
            } else {
                std::ptr::null_mut()
            };
            (*view).strides = if flags & ffi::PyBUF_STRIDES != 0 {
                &mut (*view).itemsize as *mut isize
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

impl Drop for PyResponseBuffer {
    fn drop(&mut self) {
        // Auto-release SHM/Handle if Python forgot to call release()
        let mut guard = self.inner.lock();
        let inner = guard.take();
        self.lease.lock().take();
        drop(guard);

        match inner {
            Some(ResponseBufferInner::Shm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            }) => {
                let mut pool_guard = pool.lock();
                if let Some(state) = pool_guard.as_mut() {
                    let _ = state.ensure_segment(seg_idx, data_size, is_dedicated);
                    let _ = state
                        .pool
                        .free_at(seg_idx as u32, offset, data_size, is_dedicated);
                }
            }
            Some(ResponseBufferInner::Handle { handle, pool }) => {
                let mut p = pool.write();
                let _ = p.release_handle(handle);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// PyRustClient
// ---------------------------------------------------------------------------

/// A Rust-native IPC client for synchronous CRM calls.
///
/// ```python
/// from c_two._native import RustClient
/// client = RustClient("ipc://my_server")
/// routes = client.route_names()
/// client.close()
/// ```
#[pyclass(name = "RustClient", frozen)]
pub struct PyRustClient {
    inner: Arc<SyncClient>,
    bound_route_name: Option<String>,
}

impl PyRustClient {
    pub(crate) fn from_arc_bound(inner: Arc<SyncClient>, route_name: String) -> Self {
        Self {
            inner,
            bound_route_name: Some(route_name),
        }
    }
}

pub(crate) fn call_sync_client<'py>(
    py: Python<'py>,
    inner: &Arc<SyncClient>,
    route_name: &str,
    method_name: &str,
    data: &[u8],
) -> PyResult<PyResponseBuffer> {
    let client = Arc::clone(inner);
    let route = route_name.to_string();
    let method = method_name.to_string();

    // Fast path: direct SHM write for large payloads.
    // While GIL is held, `data` is a zero-copy &[u8] from Python.
    // Write directly to SHM (single copy), then release GIL to send
    // buddy frame with just coordinates (no data movement).
    if client.should_use_shm(data.len()) {
        match client.pool_alloc_and_write(data) {
            Ok(alloc) => {
                let data_size = data.len();
                let result =
                    py.detach(move || client.call_prealloc(&route, &method, &alloc, data_size));
                return match result {
                    Ok(response_data) => {
                        let pool = inner.server_pool_arc();
                        let reassembly = inner.reassembly_pool_arc();
                        Ok(PyResponseBuffer::from_response_data(
                            response_data,
                            pool,
                            reassembly,
                        ))
                    }
                    Err(IpcError::CrmError(err_bytes)) => {
                        let exc = PyErr::new::<CrmCallError, _>("CRM method error");
                        exc.value(py)
                            .setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
                        Err(exc)
                    }
                    Err(e) => {
                        // call_with_prealloc already freed on send failure.
                        // For receive failures (Closed), server already consumed.
                        Err(PyRuntimeError::new_err(format!("{e}")))
                    }
                };
            }
            Err(_) => {
                // Pool alloc failed; fall through to canonical IPC selection.
            }
        }
    }

    // Fallback: inline/chunked path (small payloads or pool unavailable).
    // to_vec() is needed because detach requires owned data.
    let payload = data.to_vec();
    let result = py.detach(move || client.call(&route, &method, &payload));

    match result {
        Ok(response_data) => {
            let pool = inner.server_pool_arc();
            let reassembly = inner.reassembly_pool_arc();
            Ok(PyResponseBuffer::from_response_data(
                response_data,
                pool,
                reassembly,
            ))
        }
        Err(IpcError::CrmError(err_bytes)) => {
            let exc = PyErr::new::<CrmCallError, _>("CRM method error");
            exc.value(py)
                .setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
            Err(exc)
        }
        Err(e) => Err(PyRuntimeError::new_err(format!("{e}"))),
    }
}

#[pymethods]
impl PyRustClient {
    /// Create and connect to the given IPC address.
    #[new]
    #[pyo3(signature = (
        address, shm_threshold, pool_enabled, pool_segment_size,
        max_pool_segments, reassembly_segment_size,
        reassembly_max_segments, max_total_chunks, chunk_gc_interval,
        chunk_threshold_ratio, chunk_assembler_timeout,
        max_reassembly_bytes, chunk_size,
    ))]
    fn new(
        py: Python<'_>,
        address: &str,
        shm_threshold: u64,
        pool_enabled: bool,
        pool_segment_size: u64,
        max_pool_segments: u32,
        reassembly_segment_size: u64,
        reassembly_max_segments: u32,
        max_total_chunks: u32,
        chunk_gc_interval: f64,
        chunk_threshold_ratio: f64,
        chunk_assembler_timeout: f64,
        max_reassembly_bytes: u64,
        chunk_size: u64,
    ) -> PyResult<Self> {
        let config = ClientIpcConfig {
            base: BaseIpcConfig {
                pool_enabled,
                pool_segment_size,
                max_pool_segments,
                max_pool_memory: pool_segment_size
                    .checked_mul(u64::from(max_pool_segments))
                    .ok_or_else(|| {
                        PyRuntimeError::new_err("max_pool_memory derived value overflowed")
                    })?,
                reassembly_segment_size,
                reassembly_max_segments,
                max_total_chunks,
                chunk_gc_interval_secs: chunk_gc_interval,
                chunk_threshold_ratio,
                chunk_assembler_timeout_secs: chunk_assembler_timeout,
                max_reassembly_bytes,
                chunk_size,
            },
            shm_threshold,
        };
        config
            .validate()
            .map_err(|e| PyValueError::new_err(format!("invalid IPC client config: {e}")))?;
        let seg_size = config.pool_segment_size as usize;
        let max_segs = config.max_pool_segments as usize;
        let addr = address.to_string();
        let client = py.detach(move || {
            let mut pc = c2_mem::PoolConfig::default();
            pc.segment_size = seg_size;
            pc.max_segments = max_segs;
            let pool = Arc::new(parking_lot::Mutex::new(c2_mem::MemPool::new(pc)));
            SyncClient::connect(&addr, Some(pool), config)
                .map_err(|e| PyRuntimeError::new_err(format!("{e}")))
        })?;
        Ok(Self {
            inner: Arc::new(client),
            bound_route_name: None,
        })
    }

    /// Call a CRM method synchronously.
    ///
    /// Returns the serialised result bytes.  On CRM-level errors the
    /// raw error bytes are attached to a `CrmCallError` exception as
    /// the `.error_bytes` attribute so the Python layer can decode the
    /// original `CCError`.
    fn call<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        data: &[u8],
    ) -> PyResult<PyResponseBuffer> {
        let Some(bound_route_name) = self.bound_route_name.as_ref() else {
            return Err(PyRuntimeError::new_err(
                "RustClient CRM calls require a route-bound client from RuntimeSession.acquire_ipc_client()",
            ));
        };
        call_sync_client(py, &self.inner, bound_route_name, method_name, data)
    }

    /// Call a CRM method with a prepared payload write plan.
    fn call_prepared<'py>(
        &self,
        py: Python<'py>,
        method_name: &str,
        plan: &Bound<'py, PyAny>,
    ) -> PyResult<PyResponseBuffer> {
        let Some(bound_route_name) = self.bound_route_name.as_ref() else {
            return Err(PyRuntimeError::new_err(
                "RustClient CRM calls require a route-bound client from RuntimeSession.acquire_ipc_client()",
            ));
        };
        let data_size = prepared_plan_nbytes(plan)?.ok_or_else(|| {
            PyValueError::new_err("prepared payload plan must expose nbytes or byte_length")
        })?;
        if data_size > u32::MAX as usize {
            return Err(PyValueError::new_err(format!(
                "prepared request payload size {data_size} exceeds buddy wire limit {}",
                u32::MAX
            )));
        }
        if !self.inner.should_use_shm(data_size) {
            let payload = plan.call_method0("to_bytes")?;
            let bytes = payload.cast::<PyBytes>()?;
            return call_sync_client(
                py,
                &self.inner,
                bound_route_name,
                method_name,
                bytes.as_bytes(),
            );
        }

        let alloc = self
            .inner
            .pool_alloc_and_fill(data_size, |destination| {
                write_python_payload_plan(py, plan, destination)
                    .map_err(|err| format!("{}", err.value(py)))
            })
            .map_err(|err| PyRuntimeError::new_err(format!("{err}")))?;

        let client = Arc::clone(&self.inner);
        let route = bound_route_name.to_string();
        let method = method_name.to_string();
        let result = py.detach(move || client.call_prealloc(&route, &method, &alloc, data_size));
        match result {
            Ok(response_data) => {
                let pool = self.inner.server_pool_arc();
                let reassembly = self.inner.reassembly_pool_arc();
                Ok(PyResponseBuffer::from_response_data(
                    response_data,
                    pool,
                    reassembly,
                ))
            }
            Err(IpcError::CrmError(err_bytes)) => {
                let exc = PyErr::new::<CrmCallError, _>("CRM method error");
                exc.value(py)
                    .setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
                Err(exc)
            }
            Err(err) => Err(PyRuntimeError::new_err(format!("{err}"))),
        }
    }

    /// Whether the client is connected.
    #[getter]
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Route name validated by the runtime session for CRM calls.
    #[getter]
    fn route_name(&self) -> Option<&str> {
        self.bound_route_name.as_deref()
    }

    /// Get all route names advertised by the server.
    fn route_names(&self) -> Vec<String> {
        self.inner
            .route_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// CRM tag advertised for a route by the IPC handshake.
    fn route_contract(
        &self,
        route_name: &str,
    ) -> Option<(String, String, String, String, String, String)> {
        self.inner.route_contract(route_name).map(|contract| {
            (
                contract.route_name,
                contract.crm_ns,
                contract.crm_name,
                contract.crm_ver,
                contract.abi_hash,
                contract.signature_hash,
            )
        })
    }

    /// Stable logical server identity advertised by the IPC handshake.
    #[getter]
    fn server_id(&self) -> Option<String> {
        self.inner.server_id().map(ToOwned::to_owned)
    }

    /// Per-process server incarnation identity advertised by the IPC handshake.
    #[getter]
    fn server_instance_id(&self) -> Option<String> {
        self.inner.server_instance_id().map(ToOwned::to_owned)
    }

    /// Close the connection.
    ///
    /// `SyncClient::close` takes `&mut self` which is incompatible with
    /// `Arc`; the connection will be cleaned up when the last reference
    /// is dropped.  This method is kept for API symmetry with Python.
    fn close(&self) -> PyResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PyRustClientPool
// ---------------------------------------------------------------------------

/// Process-level pool of `RustClient` instances.
///
/// ```python
/// from c_two._native import RustClientPool
/// pool = RustClientPool.instance()
/// client = pool.acquire("ipc://my_server")
/// # ... use client ...
/// pool.release("ipc://my_server")
/// ```
#[pyclass(name = "RustClientPool", frozen)]
pub struct PyRustClientPool {
    pub(crate) inner: &'static ClientPool,
}

impl PyRustClientPool {
    pub(crate) fn global() -> Self {
        Self {
            inner: ClientPool::instance(),
        }
    }
}

#[pymethods]
impl PyRustClientPool {
    /// Get the process-level singleton pool.
    #[staticmethod]
    fn instance() -> Self {
        Self::global()
    }

    /// Acquire (or create) a client for the given address.
    ///
    /// Increments the internal reference count.  Call `release()` when
    /// the client is no longer needed.
    #[pyo3(signature = (address,))]
    fn acquire(&self, py: Python<'_>, address: &str) -> PyResult<PyRustClient> {
        let addr = address.to_string();
        let pool = self.inner;
        let client = py
            .detach(move || pool.acquire(&addr, None))
            .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        Ok(PyRustClient {
            inner: client,
            bound_route_name: None,
        })
    }

    /// Release a client reference for the given address.
    fn release(&self, address: &str) {
        self.inner.release(address);
    }

    /// Set the default IPC config for newly created clients.
    #[pyo3(signature = (
        shm_threshold, pool_enabled, pool_segment_size, max_pool_segments,
        reassembly_segment_size, reassembly_max_segments,
        max_total_chunks, chunk_gc_interval, chunk_threshold_ratio,
        chunk_assembler_timeout, max_reassembly_bytes, chunk_size,
    ))]
    fn set_default_config(
        &self,
        shm_threshold: u64,
        pool_enabled: bool,
        pool_segment_size: u64,
        max_pool_segments: u32,
        reassembly_segment_size: u64,
        reassembly_max_segments: u32,
        max_total_chunks: u32,
        chunk_gc_interval: f64,
        chunk_threshold_ratio: f64,
        chunk_assembler_timeout: f64,
        max_reassembly_bytes: u64,
        chunk_size: u64,
    ) -> PyResult<()> {
        let config = ClientIpcConfig {
            base: BaseIpcConfig {
                pool_enabled,
                pool_segment_size,
                max_pool_segments,
                max_pool_memory: pool_segment_size
                    .checked_mul(u64::from(max_pool_segments))
                    .ok_or_else(|| {
                        PyRuntimeError::new_err("max_pool_memory derived value overflowed")
                    })?,
                reassembly_segment_size,
                reassembly_max_segments,
                max_total_chunks,
                chunk_gc_interval_secs: chunk_gc_interval,
                chunk_threshold_ratio,
                chunk_assembler_timeout_secs: chunk_assembler_timeout,
                max_reassembly_bytes,
                chunk_size,
            },
            shm_threshold,
        };
        config
            .validate()
            .map_err(|e| PyValueError::new_err(format!("invalid IPC client config: {e}")))?;
        self.inner.set_default_config(config);
        Ok(())
    }

    /// Sweep entries that have been unreferenced beyond the grace period.
    fn sweep_expired(&self) {
        self.inner.sweep_expired();
    }

    /// Shut down all pooled clients immediately.
    fn shutdown_all(&self, py: Python<'_>) {
        py.detach(|| self.inner.shutdown_all());
    }

    /// Number of active entries in the pool.
    fn active_count(&self) -> usize {
        self.inner.active_count()
    }

    /// Reference count for a specific address.
    fn refcount(&self, address: &str) -> usize {
        self.inner.refcount(address)
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register client classes and exceptions on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRustClient>()?;
    m.add_class::<PyRustClientPool>()?;
    m.add_class::<PyResponseBuffer>()?;
    m.add("CrmCallError", m.py().get_type::<CrmCallError>())?;
    Ok(())
}
