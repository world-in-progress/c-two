//! PyO3 bindings for the C-Two IPC client (`c2-ipc`).
//!
//! Exposes `RustClient` — a synchronous IPC client for CRM calls, and
//! `RustClientPool` — a process-level pool of shared clients.
//!
//! **GIL handling**: all blocking operations release the GIL via
//! `py.allow_threads()`.  `#[pyclass(frozen)]` ensures thread-safety
//! for free-threading builds.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use pyo3::exceptions::{PyBufferError, PyRuntimeError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use c2_ipc::{ClientPool, IpcConfig, IpcError, ResponseData, ServerPoolState, SyncClient};

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
        pool: Arc<StdMutex<Option<ServerPoolState>>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
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
    inner: StdMutex<Option<ResponseBufferInner>>,
    data_len: usize,
    exports: AtomicU32,
}

impl PyResponseBuffer {
    fn from_response_data(
        data: ResponseData,
        pool: Arc<StdMutex<Option<ServerPoolState>>>,
    ) -> Self {
        let data_len = data.len();
        let inner = match data {
            ResponseData::Inline(vec) => ResponseBufferInner::Inline(vec),
            ResponseData::Shm { seg_idx, offset, data_size, is_dedicated } => {
                ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated }
            }
        };
        Self {
            inner: StdMutex::new(Some(inner)),
            data_len,
            exports: AtomicU32::new(0),
        }
    }
}

#[pymethods]
impl PyResponseBuffer {
    /// Length of the response data.
    fn __len__(&self) -> PyResult<usize> {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(_) => Ok(self.data_len),
            None => Err(PyValueError::new_err("buffer already released")),
        }
    }

    /// Convert to bytes (copies data — use memoryview for zero-copy).
    fn __bytes__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(ResponseBufferInner::Inline(vec)) => Ok(PyBytes::new(py, vec)),
            Some(ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated }) => {
                let pool_guard = pool.lock().unwrap();
                let state = pool_guard.as_ref().ok_or_else(|| {
                    PyRuntimeError::new_err("server pool not initialised")
                })?;
                let ptr = state.pool.data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyRuntimeError::new_err(format!("SHM read: {e}")))?;
                let slice = unsafe { std::slice::from_raw_parts(ptr, *data_size as usize) };
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
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: buffer is currently exported as memoryview",
            ));
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.take() {
            Some(ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated }) => {
                if let Ok(mut pool_guard) = pool.lock() {
                    if let Some(state) = pool_guard.as_mut() {
                        let _ = state.pool.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                    }
                }
                Ok(())
            }
            Some(ResponseBufferInner::Inline(_)) => Ok(()),
            None => Ok(()), // already released, idempotent
        }
    }

    /// Buffer protocol — enables memoryview(response).
    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let guard = this.inner.lock().unwrap();
        let inner = guard.as_ref().ok_or_else(|| {
            PyBufferError::new_err("buffer already released")
        })?;

        let (ptr, len) = match inner {
            ResponseBufferInner::Inline(vec) => (vec.as_ptr(), vec.len()),
            ResponseBufferInner::Shm { pool, seg_idx, offset, data_size, is_dedicated } => {
                let pool_guard = pool.lock().unwrap();
                let state = pool_guard.as_ref().ok_or_else(|| {
                    PyBufferError::new_err("server pool not initialised")
                })?;
                let raw_ptr = state.pool.data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyBufferError::new_err(format!("SHM access: {e}")))?;
                (raw_ptr as *const u8, *data_size as usize)
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
        // Auto-release SHM if Python forgot to call release()
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(ResponseBufferInner::Shm {
                pool, seg_idx, offset, data_size, is_dedicated,
            }) = guard.take()
            {
                if let Ok(mut pool_guard) = pool.lock() {
                    if let Some(state) = pool_guard.as_mut() {
                        let _ = state.pool.free_at(
                            seg_idx as u32, offset, data_size, is_dedicated,
                        );
                    }
                }
            }
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
/// result = client.call("grid", "step", payload)
/// client.close()
/// ```
#[pyclass(name = "RustClient", frozen)]
pub struct PyRustClient {
    inner: Arc<SyncClient>,
}

#[pymethods]
impl PyRustClient {
    /// Create and connect to the given IPC address.
    #[new]
    #[pyo3(signature = (address, shm_threshold=4096, chunk_size=131072))]
    fn new(
        py: Python<'_>,
        address: &str,
        shm_threshold: usize,
        chunk_size: usize,
    ) -> PyResult<Self> {
        let config = IpcConfig {
            shm_threshold,
            chunk_size,
        };
        let addr = address.to_string();
        let client = py.allow_threads(move || {
            let pool = Arc::new(std::sync::Mutex::new(
                c2_mem::MemPool::new(c2_mem::PoolConfig::default()),
            ));
            SyncClient::connect(&addr, Some(pool), config)
                .map_err(|e| PyRuntimeError::new_err(format!("{e}")))
        })?;
        Ok(Self {
            inner: Arc::new(client),
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
        route_name: &str,
        method_name: &str,
        data: &[u8],
    ) -> PyResult<PyResponseBuffer> {
        let inner = Arc::clone(&self.inner);
        let route = route_name.to_string();
        let method = method_name.to_string();
        let payload = data.to_vec();

        let result = py.allow_threads(move || inner.call(&route, &method, &payload));

        match result {
            Ok(response_data) => {
                let pool = self.inner.server_pool_arc();
                Ok(PyResponseBuffer::from_response_data(response_data, pool))
            }
            Err(IpcError::CrmError(err_bytes)) => {
                let exc = PyErr::new::<CrmCallError, _>("CRM method error");
                exc.value(py).setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
                Err(exc)
            }
            Err(e) => Err(PyRuntimeError::new_err(format!("{e}"))),
        }
    }

    /// Whether the client is connected.
    #[getter]
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Get all route names advertised by the server.
    fn route_names(&self) -> Vec<String> {
        self.inner
            .route_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
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
    inner: &'static ClientPool,
}

#[pymethods]
impl PyRustClientPool {
    /// Get the process-level singleton pool.
    #[staticmethod]
    fn instance() -> Self {
        Self {
            inner: ClientPool::instance(),
        }
    }

    /// Acquire (or create) a client for the given address.
    ///
    /// Increments the internal reference count.  Call `release()` when
    /// the client is no longer needed.
    #[pyo3(signature = (address, shm_threshold=None, chunk_size=None))]
    fn acquire(
        &self,
        py: Python<'_>,
        address: &str,
        shm_threshold: Option<usize>,
        chunk_size: Option<usize>,
    ) -> PyResult<PyRustClient> {
        let config = if shm_threshold.is_some() || chunk_size.is_some() {
            Some(IpcConfig {
                shm_threshold: shm_threshold.unwrap_or(4096),
                chunk_size: chunk_size.unwrap_or(131072),
            })
        } else {
            None
        };

        let addr = address.to_string();
        let pool = self.inner;
        let client = py
            .allow_threads(move || pool.acquire(&addr, config.as_ref()))
            .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;

        Ok(PyRustClient { inner: client })
    }

    /// Release a client reference for the given address.
    fn release(&self, address: &str) {
        self.inner.release(address);
    }

    /// Set the default IPC config for newly created clients.
    #[pyo3(signature = (shm_threshold=4096, chunk_size=131072))]
    fn set_default_config(&self, shm_threshold: usize, chunk_size: usize) {
        self.inner.set_default_config(IpcConfig {
            shm_threshold,
            chunk_size,
        });
    }

    /// Sweep entries that have been unreferenced beyond the grace period.
    fn sweep_expired(&self) {
        self.inner.sweep_expired();
    }

    /// Shut down all pooled clients immediately.
    fn shutdown_all(&self, py: Python<'_>) {
        py.allow_threads(|| self.inner.shutdown_all());
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
