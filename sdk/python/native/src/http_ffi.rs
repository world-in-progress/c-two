//! PyO3 bindings for the C-Two HTTP client (`c2-http`).
//!
//! Exposes `RustHttpClient` and `RustHttpClientPool` — mirroring the
//! IPC client FFI pattern from `client_ffi.rs`.

use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use c2_config::{ConfigResolver, ConfigSources};
use c2_http::client::{
    HttpClient, HttpClientPool, HttpError, RelayAwareHttpClient, RelayControlClient,
    RelayRegistration,
};

// ---------------------------------------------------------------------------
// HttpCrmCallError — custom exception carrying serialised error bytes
// ---------------------------------------------------------------------------

pyo3::create_exception!(
    c_two._native,
    HttpCrmCallError,
    pyo3::exceptions::PyException
);
pyo3::create_exception!(
    c_two._native,
    RelayControlError,
    pyo3::exceptions::PyException
);

// ---------------------------------------------------------------------------
// PyRustHttpClient
// ---------------------------------------------------------------------------

/// A Rust-native HTTP client for relay health and low-level diagnostics.
///
/// ```python
/// from c_two._native import RustHttpClientPool
/// client = RustHttpClientPool.instance().acquire("http://localhost:8080")
/// assert client.health()
/// ```
#[pyclass(name = "RustHttpClient", frozen)]
pub struct PyRustHttpClient {
    inner: Arc<HttpClient>,
}

impl PyRustHttpClient {
    pub(crate) fn from_arc(inner: Arc<HttpClient>) -> Self {
        Self { inner }
    }
}

pub(crate) fn acquire_http_client_from_global_pool(
    base_url: &str,
    use_proxy: bool,
    call_timeout_secs: f64,
    remote_payload_chunk_size: u64,
) -> Result<Arc<HttpClient>, HttpError> {
    HttpClientPool::instance().acquire_with_options(
        base_url,
        use_proxy,
        call_timeout_secs,
        remote_payload_chunk_size,
    )
}

pub(crate) fn call_relay_aware_http_client<'py>(
    py: Python<'py>,
    inner: &Arc<RelayAwareHttpClient>,
    method_name: &str,
    data: &[u8],
) -> PyResult<Bound<'py, PyBytes>> {
    let client = Arc::clone(inner);
    let method = method_name.to_string();
    let payload = data.to_vec();

    let result = py.detach(move || client.call(&method, &payload));
    http_call_result_to_py(py, result)
}

fn http_call_result_to_py<'py>(
    py: Python<'py>,
    result: Result<Vec<u8>, HttpError>,
) -> PyResult<Bound<'py, PyBytes>> {
    match result {
        Ok(bytes) => Ok(PyBytes::new(py, &bytes)),
        Err(HttpError::CrmError(err_bytes)) => {
            let exc = PyErr::new::<HttpCrmCallError, _>("CRM method error");
            exc.value(py)
                .setattr("error_bytes", PyBytes::new(py, &err_bytes))?;
            Err(exc)
        }
        Err(HttpError::ServerError(code, body)) => {
            let error_bytes = c2_error_wire_bytes_from_http_body(&body);
            let exc = PyErr::new::<HttpCrmCallError, _>(format!("HTTP {code}: {body}"));
            let value = exc.value(py);
            value.setattr("status_code", code)?;
            value.setattr("body", body)?;
            value.setattr("error_bytes", PyBytes::new(py, &error_bytes))?;
            Err(exc)
        }
        Err(e) => Err(PyRuntimeError::new_err(format!("{e}"))),
    }
}

pub(crate) fn c2_error_wire_bytes_from_http_body(body: &str) -> Vec<u8> {
    c2_core::semantic_error_from_http_body(body).to_wire_bytes()
}

pub(crate) fn release_http_client_from_global_pool(base_url: &str) {
    HttpClientPool::instance().release(base_url);
}

pub(crate) fn http_client_refcount_from_global_pool(base_url: &str) -> usize {
    HttpClientPool::instance().refcount(base_url)
}

pub(crate) fn shutdown_http_clients_from_global_pool() {
    HttpClientPool::instance().shutdown_all();
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
        method_name: &str,
        data: &[u8],
    ) -> PyResult<Bound<'py, PyBytes>> {
        let _ = (py, method_name, data);
        Err(PyRuntimeError::new_err(
            "RustHttpClient CRM calls require a route-bound relay-aware client from RuntimeSession.connect_explicit_relay_http()",
        ))
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

// ---------------------------------------------------------------------------
// PyRustRelayControlClient
// ---------------------------------------------------------------------------

#[pyclass(name = "RustRelayControlClient", frozen)]
pub struct PyRustRelayControlClient {
    inner: Arc<RelayControlClient>,
}

#[pymethods]
impl PyRustRelayControlClient {
    #[new]
    fn new(base_url: &str) -> PyResult<Self> {
        let use_proxy = ConfigResolver::resolve_relay_use_proxy(ConfigSources::from_process())
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        let client = RelayControlClient::new(base_url, use_proxy).map_err(py_http_error)?;
        Ok(Self {
            inner: Arc::new(client),
        })
    }

    #[pyo3(signature = (name, server_id, server_instance_id, ipc_address, crm_ns, crm_name, crm_ver, abi_hash, signature_hash, max_payload_size))]
    fn register(
        &self,
        py: Python<'_>,
        name: &str,
        server_id: &str,
        server_instance_id: &str,
        ipc_address: &str,
        crm_ns: &str,
        crm_name: &str,
        crm_ver: &str,
        abi_hash: &str,
        signature_hash: &str,
        max_payload_size: u64,
    ) -> PyResult<()> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let server_id = server_id.to_string();
        let server_instance_id = server_instance_id.to_string();
        let ipc_address = ipc_address.to_string();
        let crm_ns = crm_ns.to_string();
        let crm_name = crm_name.to_string();
        let crm_ver = crm_ver.to_string();
        let abi_hash = abi_hash.to_string();
        let signature_hash = signature_hash.to_string();
        py.detach(move || {
            let expected = c2_contract::ExpectedRouteContract {
                route_name: name,
                crm_ns,
                crm_name,
                crm_ver,
                abi_hash,
                signature_hash,
            };
            inner.register(RelayRegistration {
                expected: &expected,
                server_id: &server_id,
                server_instance_id: &server_instance_id,
                address: &ipc_address,
                max_payload_size,
            })
        })
        .map_err(py_http_error)
    }

    fn unregister(&self, py: Python<'_>, name: &str, server_id: &str) -> PyResult<()> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let server_id = server_id.to_string();
        py.detach(move || inner.unregister(&name, &server_id))
            .map_err(py_http_error)
    }

    fn invalidate(&self, name: &str) {
        self.inner.invalidate(name);
    }

    fn clear_cache(&self) {
        self.inner.clear_cache();
    }
}

fn py_http_error(err: HttpError) -> PyErr {
    match err {
        HttpError::ServerError(code, body) => {
            let error_bytes = c2_error_wire_bytes_from_http_body(&body);
            let exc = PyErr::new::<RelayControlError, _>(format!("HTTP {code}: {body}"));
            Python::attach(|py| {
                let value = exc.value(py);
                value.setattr("status_code", code).ok();
                value.setattr("body", body.as_str()).ok();
                value
                    .setattr("error_bytes", PyBytes::new(py, &error_bytes))
                    .ok();
            });
            exc
        }
        other => PyRuntimeError::new_err(other.to_string()),
    }
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
    fn acquire(&self, py: Python<'_>, base_url: &str) -> PyResult<PyRustHttpClient> {
        let url = base_url.to_string();
        let pool = self.inner;
        let client = py
            .detach(move || {
                let use_proxy =
                    ConfigResolver::resolve_relay_use_proxy(ConfigSources::from_process())
                        .map_err(|e| HttpError::Transport(e.to_string()))?;
                let call_timeout_secs =
                    ConfigResolver::resolve_relay_call_timeout_secs(ConfigSources::from_process())
                        .map_err(|e| HttpError::Transport(e.to_string()))?;
                let remote_payload_chunk_size = ConfigResolver::resolve_remote_payload_chunk_size(
                    None,
                    ConfigSources::from_process(),
                )
                .map_err(|e| HttpError::Transport(e.to_string()))?;
                pool.acquire_with_options(
                    &url,
                    use_proxy,
                    call_timeout_secs,
                    remote_payload_chunk_size,
                )
            })
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
    m.add_class::<PyRustRelayControlClient>()?;
    m.add("HttpCrmCallError", m.py().get_type::<HttpCrmCallError>())?;
    m.add("RelayControlError", m.py().get_type::<RelayControlError>())?;
    Ok(())
}
