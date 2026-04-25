//! PyO3 bindings for the C-Two HTTP client (`c2-http`).
//!
//! Exposes `RustHttpClient` and `RustHttpClientPool` — mirroring the
//! IPC client FFI pattern from `client_ffi.rs`.

use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use c2_http::client::{HttpClient, HttpClientPool, HttpError};

// ---------------------------------------------------------------------------
// HttpCrmCallError — custom exception carrying serialised error bytes
// ---------------------------------------------------------------------------

pyo3::create_exception!(c_two._native, HttpCrmCallError, pyo3::exceptions::PyException);

// ---------------------------------------------------------------------------
// PyRustHttpClient
// ---------------------------------------------------------------------------

/// A Rust-native HTTP client for CRM calls through a relay server.
///
/// ```python
/// from c_two._native import RustHttpClient
/// client = RustHttpClient("http://localhost:8080", 30.0, 100)
/// result = client.call("grid", "step", payload)
/// client.close()
/// ```
#[pyclass(name = "RustHttpClient", frozen)]
pub struct PyRustHttpClient {
    inner: Arc<HttpClient>,
}

#[pymethods]
impl PyRustHttpClient {
    /// Call a CRM method via HTTP relay.
    ///
    /// Returns serialised result bytes.  On CRM-level errors (HTTP 500)
    /// the raw error bytes are attached to an `HttpCrmCallError`
    /// exception as the `.error_bytes` attribute.
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

        let result = py.detach(move || inner.call(&route, &method, &payload));

        match result {
            Ok(bytes) => Ok(PyBytes::new(py, &bytes)),
            Err(HttpError::CrmError(err_bytes)) => {
                let exc =
                    PyErr::new::<HttpCrmCallError, _>("CRM method error");
                exc.value(py)
                    .setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
                Err(exc)
            }
            Err(e) => Err(PyRuntimeError::new_err(format!("{e}"))),
        }
    }

    /// Health check — GET /health on the relay.
    fn health(&self, py: Python<'_>) -> PyResult<bool> {
        let inner = Arc::clone(&self.inner);
        py.detach(move || inner.health())
            .map_err(|e| PyRuntimeError::new_err(format!("{e}")))
    }

    /// No-op close (Arc cleanup on drop).
    fn close(&self) -> PyResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PyRustHttpClientPool
// ---------------------------------------------------------------------------

/// Process-level pool of `RustHttpClient` instances.
///
/// ```python
/// from c_two._native import RustHttpClientPool
/// pool = RustHttpClientPool.instance()
/// client = pool.acquire("http://relay:8080")
/// # ... use client ...
/// pool.release("http://relay:8080")
/// ```
#[pyclass(name = "RustHttpClientPool", frozen)]
pub struct PyRustHttpClientPool {
    inner: &'static HttpClientPool,
}

#[pymethods]
impl PyRustHttpClientPool {
    /// Get the process-level singleton pool.
    #[staticmethod]
    fn instance() -> Self {
        Self {
            inner: HttpClientPool::instance(),
        }
    }

    /// Acquire (or create) an HTTP client for the given relay URL.
    fn acquire(
        &self,
        py: Python<'_>,
        base_url: &str,
    ) -> PyResult<PyRustHttpClient> {
        let url = base_url.to_string();
        let pool = self.inner;
        let client = py
            .detach(move || pool.acquire(&url))
            .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        Ok(PyRustHttpClient { inner: client })
    }

    /// Release a client reference for the given relay URL.
    fn release(&self, base_url: &str) {
        self.inner.release(base_url);
    }

    /// Shut down all pooled HTTP clients immediately.
    fn shutdown_all(&self, py: Python<'_>) {
        py.detach(|| self.inner.shutdown_all());
    }

    /// Number of active entries in the pool.
    fn active_count(&self) -> usize {
        self.inner.active_count()
    }

    /// Reference count for a specific relay URL.
    fn refcount(&self, base_url: &str) -> usize {
        self.inner.refcount(base_url)
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register HTTP client classes and exceptions on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRustHttpClient>()?;
    m.add_class::<PyRustHttpClientPool>()?;
    m.add(
        "HttpCrmCallError",
        m.py().get_type::<HttpCrmCallError>(),
    )?;
    Ok(())
}
