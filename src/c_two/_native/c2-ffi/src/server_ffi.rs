//! PyO3 bindings for the C-Two IPC server (`c2-server`).
//!
//! Exposes `RustServer` — a UDS server that hosts CRM routes and dispatches
//! method calls through Python callables.  The server runs on a background
//! OS thread with its own tokio runtime.
//!
//! **GIL handling**: `PyCrmCallback::invoke()` acquires the GIL internally
//! (called from `spawn_blocking` inside tokio).  All blocking `PyServer`
//! methods release the GIL via `py.allow_threads()`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use c2_server::config::IpcConfig;
use c2_server::dispatcher::{CrmCallback, CrmError, CrmRoute};
use c2_server::scheduler::{AccessLevel, ConcurrencyMode, Scheduler};
use c2_server::Server;

// ---------------------------------------------------------------------------
// PyCrmCallback — bridges a Python callable to the Rust CrmCallback trait
// ---------------------------------------------------------------------------

struct PyCrmCallback {
    py_callable: PyObject,
}

// SAFETY: PyObject is Send when accessed only under the GIL.
// `invoke()` always acquires the GIL via `Python::with_gil` before
// touching `py_callable`.
unsafe impl Send for PyCrmCallback {}
unsafe impl Sync for PyCrmCallback {}

impl CrmCallback for PyCrmCallback {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        payload: &[u8],
    ) -> Result<Vec<u8>, CrmError> {
        Python::with_gil(|py| {
            let args = (route_name, method_idx, PyBytes::new(py, payload));
            match self.py_callable.call1(py, args) {
                Ok(result) => {
                    let bytes: &Bound<'_, PyBytes> = result
                        .downcast_bound::<PyBytes>(py)
                        .map_err(|e| {
                            CrmError::InternalError(format!("expected bytes return: {e}"))
                        })?;
                    Ok(bytes.as_bytes().to_vec())
                }
                Err(e) => {
                    // Check for .error_bytes attribute (CrmCallError from Python).
                    // This preserves serialized CCError bytes for the client.
                    let val = e.value(py);
                    if let Ok(attr) = val.getattr("error_bytes") {
                        if let Ok(b) = attr.downcast::<PyBytes>() {
                            return Err(CrmError::UserError(b.as_bytes().to_vec()));
                        }
                    }
                    Err(CrmError::InternalError(format!("{e}")))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// PyServer — main pyclass
// ---------------------------------------------------------------------------

/// A Rust-native IPC server for hosting CRM routes.
///
/// ```python
/// from c_two._native import RustServer
/// srv = RustServer("ipc-v3://my_server")
/// srv.register_route("grid", dispatcher_fn, ["step", "query"], {0: "read", 1: "write"})
/// srv.start()
/// # ... serve requests ...
/// srv.shutdown()
/// ```
#[pyclass(name = "RustServer", frozen)]
pub struct PyServer {
    inner: Arc<Server>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
}

#[pymethods]
impl PyServer {
    /// Create a new server targeting the given IPC address.
    #[new]
    #[pyo3(signature = (
        address,
        max_frame_size = 268_435_456,
        max_payload_size = 134_217_728,
        max_pool_segments = 4,
        segment_size = 268_435_456,
        chunked_threshold = 67_108_864,
        heartbeat_interval = 10.0,
        heartbeat_timeout = 30.0,
    ))]
    fn new(
        address: &str,
        max_frame_size: u64,
        max_payload_size: u64,
        max_pool_segments: u32,
        segment_size: u64,
        chunked_threshold: u64,
        heartbeat_interval: f64,
        heartbeat_timeout: f64,
    ) -> PyResult<Self> {
        let config = IpcConfig {
            max_frame_size,
            max_payload_size,
            max_pool_segments,
            pool_segment_size: segment_size,
            heartbeat_interval,
            heartbeat_timeout,
            // Derive chunked threshold ratio from absolute value and max_payload_size
            chunk_threshold_ratio: if max_payload_size > 0 {
                chunked_threshold as f64 / max_payload_size as f64
            } else {
                0.9
            },
            ..IpcConfig::default()
        };
        let server = Server::new(address, config)
            .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
        Ok(Self {
            inner: Arc::new(server),
            rt: Mutex::new(None),
        })
    }

    /// Register a CRM route.
    ///
    /// `dispatcher` is a Python callable `(route_name: str, method_idx: int, payload: bytes) -> bytes`.
    /// `method_names` lists the CRM method names indexed by method_idx.
    /// `access_map` maps method_idx → "read" or "write".
    #[pyo3(signature = (name, dispatcher, method_names, access_map))]
    fn register_route(
        &self,
        py: Python<'_>,
        name: &str,
        dispatcher: PyObject,
        method_names: Vec<String>,
        access_map: &Bound<'_, PyDict>,
    ) -> PyResult<()> {
        // Parse access_map: {int: "read"|"write"} → HashMap<u16, AccessLevel>
        let mut map = HashMap::new();
        for (key, value) in access_map.iter() {
            let idx: u16 = key.extract()?;
            let level_str: String = value.extract()?;
            let level = match level_str.as_str() {
                "read" => AccessLevel::Read,
                "write" => AccessLevel::Write,
                other => {
                    return Err(PyRuntimeError::new_err(format!(
                        "invalid access level '{other}', expected 'read' or 'write'"
                    )));
                }
            };
            map.insert(idx, level);
        }

        let scheduler = Arc::new(Scheduler::new(ConcurrencyMode::ReadParallel, map));
        let callback = Arc::new(PyCrmCallback {
            py_callable: dispatcher,
        });

        let route = CrmRoute {
            name: name.to_string(),
            scheduler,
            callback,
            method_names,
        };

        let server = Arc::clone(&self.inner);

        // Register requires async; use a short-lived runtime if the main
        // one hasn't started, or block_on the existing runtime's handle.
        py.allow_threads(move || {
            let rt_guard = self.rt.lock().unwrap();
            if let Some(rt) = rt_guard.as_ref() {
                rt.block_on(server.register_route(route));
            } else {
                // Server not started yet — create a temporary runtime for registration.
                let tmp_rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("failed to create runtime: {e}"))
                    .unwrap();
                tmp_rt.block_on(server.register_route(route));
            }
        });

        Ok(())
    }

    /// Start the server on a background thread with a tokio runtime.
    ///
    /// The GIL is released while the runtime spins up.
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        // Build the runtime while NOT holding the Mutex (avoid GIL↔Mutex deadlock).
        let server = Arc::clone(&self.inner);

        py.allow_threads(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(format!("failed to create runtime: {e}")))?;

            {
                let mut rt_guard = self.rt.lock().unwrap();
                if rt_guard.is_some() {
                    return Err(PyRuntimeError::new_err("server is already running"));
                }

                // Spawn the accept loop on the runtime.
                rt.spawn(async move {
                    if let Err(e) = server.run().await {
                        eprintln!("c2-server error: {e}");
                    }
                });

                *rt_guard = Some(rt);
            }

            Ok(())
        })
    }

    /// Gracefully shut down the server.
    ///
    /// Signals the accept loop to stop, then drops the tokio runtime
    /// (joining all worker threads). The GIL is released during shutdown.
    fn shutdown(&self, py: Python<'_>) -> PyResult<()> {
        let server = Arc::clone(&self.inner);

        py.allow_threads(|| {
            server.shutdown();

            let rt = {
                let mut rt_guard = self.rt.lock().unwrap();
                rt_guard.take()
            };

            if let Some(rt) = rt {
                rt.shutdown_background();
            }
        });

        Ok(())
    }

    /// Remove a CRM route. Returns `True` if it existed.
    fn unregister_route(&self, py: Python<'_>, name: &str) -> PyResult<bool> {
        let server = Arc::clone(&self.inner);
        let name = name.to_string();

        py.allow_threads(move || {
            let rt_guard = self.rt.lock().unwrap();
            if let Some(rt) = rt_guard.as_ref() {
                Ok(rt.block_on(server.unregister_route(&name)))
            } else {
                let tmp_rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
                Ok(tmp_rt.block_on(server.unregister_route(&name)))
            }
        })
    }

    /// Whether the server is currently running.
    #[getter]
    fn is_running(&self) -> bool {
        self.rt.lock().unwrap().is_some()
    }

    /// The filesystem path of the UDS socket.
    #[getter]
    fn socket_path(&self) -> String {
        self.inner.socket_path().to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register server classes on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyServer>()?;
    Ok(())
}
