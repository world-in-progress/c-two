//! PyO3 bindings for the C-Two IPC client (`c2-ipc`).
//!
//! Exposes `RustClient` — a synchronous IPC client for CRM calls, and
//! `RustClientPool` — a process-level pool of shared clients.
//!
//! **GIL handling**: all blocking operations release the GIL via
//! `py.allow_threads()`.  `#[pyclass(frozen)]` ensures thread-safety
//! for free-threading builds.

use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use c2_ipc::{ClientPool, IpcConfig, IpcError, SyncClient};

// ---------------------------------------------------------------------------
// CrmCallError — custom exception carrying serialised error bytes
// ---------------------------------------------------------------------------

pyo3::create_exception!(c_two._native, CrmCallError, pyo3::exceptions::PyException);

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
    ) -> PyResult<Bound<'py, PyBytes>> {
        let inner = Arc::clone(&self.inner);
        let route = route_name.to_string();
        let method = method_name.to_string();
        let payload = data.to_vec();

        let result = py.allow_threads(move || inner.call(&route, &method, &payload));

        match result {
            Ok(bytes) => Ok(PyBytes::new(py, &bytes)),
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
    m.add("CrmCallError", m.py().get_type::<CrmCallError>())?;
    Ok(())
}
