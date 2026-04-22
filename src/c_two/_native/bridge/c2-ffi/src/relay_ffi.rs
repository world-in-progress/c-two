//! PyO3 bindings for the C-Two HTTP relay server.
//!
//! Exposes `NativeRelay` — an embedded axum HTTP server that bridges
//! HTTP requests to IPC upstreams. The server runs on a background
//! OS thread with its own tokio runtime.
//!
//! **GIL handling**: All methods that block (start, stop, register, etc.)
//! release the Python GIL via `py.detach()` so the Python ServerV2
//! asyncio loop can continue processing connections during handshake.

use parking_lot::Mutex;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyAny;

use c2_http::relay::server::RelayServer;
use c2_config::RelayConfig;

struct ServerState {
    config: RelayConfig,
    server: Option<RelayServer>,
}

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
#[pyclass(name = "NativeRelay", frozen)]
pub struct PyNativeRelay {
    state: Mutex<ServerState>,
}

#[pymethods]
impl PyNativeRelay {
    /// Create a new relay targeting the given bind address.
    ///
    /// `idle_timeout` controls how long an upstream can be idle
    /// before its connection is evicted. Set to `0` to disable (default).
    #[new]
    #[pyo3(signature = (bind = "0.0.0.0:8080", relay_id=None, advertise_url=None, seeds=None, idle_timeout=0, skip_ipc_validation=false))]
    fn new(
        bind: &str,
        relay_id: Option<String>,
        advertise_url: Option<String>,
        seeds: Option<Vec<String>>,
        idle_timeout: u64,
        skip_ipc_validation: bool,
    ) -> Self {
        let config = RelayConfig {
            bind: bind.to_string(),
            relay_id: relay_id.unwrap_or_else(|| RelayConfig::generate_relay_id()),
            advertise_url: advertise_url.unwrap_or_default(),
            seeds: seeds.unwrap_or_default(),
            idle_timeout_secs: idle_timeout,
            skip_ipc_validation,
            ..Default::default()
        };
        Self {
            state: Mutex::new(ServerState { config, server: None }),
        }
    }

    /// Start the relay HTTP server on a background thread.
    ///
    /// Releases the GIL while waiting for the listener to bind.
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        let config = {
            let state = self.state.lock();
            if state.server.is_some() {
                return Err(PyRuntimeError::new_err("Relay is already running"));
            }
            state.config.clone()
        }; // lock dropped before detach
        let server = py
            .detach(|| RelayServer::start(config))
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to start relay: {e}")))?;
        self.state.lock().server = Some(server);
        Ok(())
    }

    /// Stop the relay gracefully.
    ///
    /// Releases the GIL while waiting for shutdown.
    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        let mut server = {
            let mut state = self.state.lock();
            state
                .server
                .take()
                .ok_or_else(|| PyRuntimeError::new_err("Relay is not running"))?
        }; // lock dropped before detach
        py.detach(|| server.stop())
            .map_err(|e| PyRuntimeError::new_err(format!("Stop failed: {e}")))?;
        Ok(())
    }

    /// Register a new upstream IPC connection.
    ///
    /// Releases the GIL while connecting to the upstream (UDS + handshake).
    /// The Mutex is acquired inside `detach` to prevent GIL↔Mutex
    /// deadlock.
    fn register_upstream(&self, py: Python<'_>, name: &str, address: &str) -> PyResult<()> {
        let name = name.to_string();
        let address = address.to_string();
        py.detach(|| {
            let state = self.state.lock();
            let server = state
                .server
                .as_ref()
                .ok_or_else(|| "Relay is not running".to_string())?;
            server.register_upstream(&name, &address)
        })
        .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Remove a registered upstream.
    ///
    /// Releases the GIL while waiting for the command to complete.
    fn unregister_upstream(&self, py: Python<'_>, name: &str) -> PyResult<()> {
        let name = name.to_string();
        py.detach(|| {
            let state = self.state.lock();
            let server = state
                .server
                .as_ref()
                .ok_or_else(|| "Relay is not running".to_string())?;
            server.unregister_upstream(&name)
        })
        .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// List all registered routes.
    ///
    /// Returns a list of dicts with "name" and "address" keys.
    fn list_routes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let routes = py
            .detach(|| {
                let state = self.state.lock();
                let server = state
                    .server
                    .as_ref()
                    .ok_or_else(|| "Relay is not running".to_string())?;
                server.list_routes()
            })
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
        self.state.lock().server.is_some()
    }

    /// The configured bind address.
    #[getter]
    fn bind_address(&self) -> String {
        self.state.lock().config.bind.clone()
    }
}

/// Register relay classes on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyNativeRelay>()?;
    Ok(())
}
