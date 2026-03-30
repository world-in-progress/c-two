//! PyO3 bindings for the C-Two HTTP relay server.
//!
//! Exposes `NativeRelay` — an embedded axum HTTP server that bridges
//! HTTP requests to IPC upstreams. The server runs on a background
//! OS thread with its own tokio runtime.
//!
//! **GIL handling**: All methods that block (start, stop, register, etc.)
//! release the Python GIL via `py.allow_threads()` so the Python ServerV2
//! asyncio loop can continue processing connections during handshake.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use c2_relay::server::RelayServer;

/// An embedded HTTP relay server backed by Rust (axum + tokio).
///
/// The relay runs in a background thread and provides dynamic upstream
/// registration: CRM processes register themselves at runtime.
///
/// ```python
/// from c_two._native import NativeRelay
/// relay = NativeRelay("0.0.0.0:8080")
/// relay.start()
/// relay.register_upstream("grid", "ipc://my_server")
/// relay.list_routes()  # [{"name": "grid", "address": "ipc://my_server"}]
/// relay.stop()
/// ```
#[pyclass(name = "NativeRelay")]
pub struct PyNativeRelay {
    bind: String,
    idle_timeout_secs: u64,
    server: Option<RelayServer>,
}

#[pymethods]
impl PyNativeRelay {
    /// Create a new relay targeting the given bind address.
    ///
    /// `idle_timeout_secs` controls how long an upstream can be idle
    /// before its connection is evicted. Set to `0` to disable (default).
    #[new]
    #[pyo3(signature = (bind = "0.0.0.0:8080", idle_timeout_secs = 0))]
    fn new(bind: &str, idle_timeout_secs: u64) -> Self {
        Self {
            bind: bind.to_string(),
            idle_timeout_secs,
            server: None,
        }
    }

    /// Start the relay HTTP server on a background thread.
    ///
    /// Releases the GIL while waiting for the listener to bind.
    fn start(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.server.is_some() {
            return Err(PyRuntimeError::new_err("Relay is already running"));
        }
        let bind = self.bind.clone();
        let idle_timeout_secs = self.idle_timeout_secs;
        let server = py
            .allow_threads(|| RelayServer::start(&bind, idle_timeout_secs))
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to start relay: {e}")))?;
        self.server = Some(server);
        Ok(())
    }

    /// Stop the relay gracefully.
    ///
    /// Releases the GIL while waiting for shutdown.
    fn stop(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.server.as_mut() {
            Some(s) => {
                py.allow_threads(|| s.stop())
                    .map_err(|e| PyRuntimeError::new_err(format!("Stop failed: {e}")))?;
                self.server = None;
                Ok(())
            }
            None => Err(PyRuntimeError::new_err("Relay is not running")),
        }
    }

    /// Register a new upstream IPC connection.
    ///
    /// Releases the GIL while connecting to the upstream (UDS + handshake).
    fn register_upstream(&self, py: Python<'_>, name: &str, address: &str) -> PyResult<()> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Relay is not running"))?;
        let name = name.to_string();
        let address = address.to_string();
        py.allow_threads(|| server.register_upstream(&name, &address))
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Remove a registered upstream.
    ///
    /// Releases the GIL while waiting for the command to complete.
    fn unregister_upstream(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Relay is not running"))?;
        let name = name.to_string();
        py.allow_threads(|| server.unregister_upstream(&name))
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// List all registered routes.
    ///
    /// Returns a list of dicts with "name" and "address" keys.
    fn list_routes(&self, py: Python<'_>) -> PyResult<PyObject> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("Relay is not running"))?;
        let routes = py
            .allow_threads(|| server.list_routes())
            .map_err(|e| PyRuntimeError::new_err(e))?;

        let list = pyo3::types::PyList::empty(py);
        for (name, address) in routes {
            let dict = pyo3::types::PyDict::new(py);
            dict.set_item("name", name)?;
            dict.set_item("address", address)?;
            list.append(dict)?;
        }
        Ok(list.into_any().unbind())
    }

    /// Check if the relay is currently running.
    #[getter]
    fn is_running(&self) -> bool {
        self.server.is_some()
    }

    /// The configured bind address.
    #[getter]
    fn bind_address(&self) -> &str {
        &self.bind
    }
}

/// Register relay classes on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyNativeRelay>()?;
    Ok(())
}
