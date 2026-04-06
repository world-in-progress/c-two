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
use std::sync::Arc;
use parking_lot::Mutex;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};

use c2_mem::MemPool;
use c2_config::{BaseIpcConfig, ServerIpcConfig};
use c2_server::dispatcher::{CrmCallback, CrmError, CrmRoute, RequestData, ResponseMeta};
use c2_server::scheduler::{AccessLevel, ConcurrencyMode, Scheduler};
use c2_server::Server;

use crate::mem_ffi::PyMemPool;
use crate::shm_buffer::PyShmBuffer;

// ---------------------------------------------------------------------------
// PyCrmCallback — bridges a Python callable to the Rust CrmCallback trait
// ---------------------------------------------------------------------------

struct PyCrmCallback {
    py_callable: PyObject,
    response_pool_obj: PyObject,
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
        request: RequestData,
        // Unused: cached as self.response_pool_obj at registration time to avoid
        // re-creating PyMemPool on every call. Trait requires it for non-FFI impls.
        _response_pool: Arc<parking_lot::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        Python::with_gil(|py| {
            // Convert RequestData → PyShmBuffer
            let shm_buf = match request {
                RequestData::Inline(data) => PyShmBuffer::from_inline(data),
                RequestData::Shm {
                    pool,
                    seg_idx,
                    offset,
                    data_size,
                    is_dedicated,
                } => PyShmBuffer::from_peer_shm(pool, seg_idx, offset, data_size, is_dedicated),
                RequestData::Handle { handle, pool } => PyShmBuffer::from_handle(handle, pool),
            };
            let buf_obj = Py::new(py, shm_buf)
                .map_err(|e| CrmError::InternalError(format!("failed to create ShmBuffer: {e}")))?;

            // Call Python: dispatcher(route_name, method_idx, shm_buffer, response_pool)
            let args = (route_name, method_idx, buf_obj, &self.response_pool_obj);
            match self.py_callable.call1(py, args) {
                Ok(result) => parse_response_meta(py, result),
                Err(e) => {
                    // Check for .error_bytes attribute (CrmCallError from Python).
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
/// srv = RustServer("ipc://my_server")
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
        address, shm_threshold, pool_enabled, pool_segment_size,
        max_pool_segments, max_pool_memory, reassembly_segment_size,
        reassembly_max_segments, max_frame_size, max_payload_size,
        max_pending_requests, pool_decay_seconds, heartbeat_interval,
        heartbeat_timeout, max_total_chunks, chunk_gc_interval,
        chunk_threshold_ratio, chunk_assembler_timeout,
        max_reassembly_bytes, chunk_size,
    ))]
    fn new(
        address: &str,
        shm_threshold: u64, pool_enabled: bool,
        pool_segment_size: u64, max_pool_segments: u32, max_pool_memory: u64,
        reassembly_segment_size: u64, reassembly_max_segments: u32,
        max_frame_size: u64, max_payload_size: u64, max_pending_requests: u32,
        pool_decay_seconds: f64, heartbeat_interval: f64, heartbeat_timeout: f64,
        max_total_chunks: u32, chunk_gc_interval: f64, chunk_threshold_ratio: f64,
        chunk_assembler_timeout: f64, max_reassembly_bytes: u64, chunk_size: u64,
    ) -> PyResult<Self> {
        let config = ServerIpcConfig {
            base: BaseIpcConfig {
                pool_enabled, pool_segment_size, max_pool_segments, max_pool_memory,
                reassembly_segment_size, reassembly_max_segments, max_total_chunks,
                chunk_gc_interval_secs: chunk_gc_interval,
                chunk_threshold_ratio,
                chunk_assembler_timeout_secs: chunk_assembler_timeout,
                max_reassembly_bytes, chunk_size,
            },
            shm_threshold, max_frame_size, max_payload_size, max_pending_requests,
            pool_decay_seconds,
            heartbeat_interval_secs: heartbeat_interval,
            heartbeat_timeout_secs: heartbeat_timeout,
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
    /// `dispatcher` is a Python callable:
    /// `(route_name: str, method_idx: int, request_buffer: ShmBuffer, response_pool: MemPool) -> None | bytes | tuple`
    ///
    /// Return value conventions:
    /// - `None` → empty response
    /// - `bytes` → inline response data
    /// - `(seg_idx, offset, data_size, is_dedicated)` → SHM-allocated response
    ///
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

        // Create PyMemPool wrapping the server's response pool
        let response_pool_obj: PyObject = {
            let pool_arc = self.inner.response_pool_arc();
            let py_pool = PyMemPool::from_arc(pool_arc);
            Py::new(py, py_pool)?.into_any()
        };

        let scheduler = Arc::new(Scheduler::new(ConcurrencyMode::ReadParallel, map));
        let callback = Arc::new(PyCrmCallback {
            py_callable: dispatcher,
            response_pool_obj,
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
            let rt_guard = self.rt.lock();
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
                let mut rt_guard = self.rt.lock();
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
                let mut rt_guard = self.rt.lock();
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
            let rt_guard = self.rt.lock();
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
        self.rt.lock().is_some()
    }

    /// The filesystem path of the UDS socket.
    #[getter]
    fn socket_path(&self) -> String {
        self.inner.socket_path().to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// parse_response_meta — convert Python return value to ResponseMeta
// ---------------------------------------------------------------------------

/// Convert Python dispatcher return value into ResponseMeta.
///
/// Expected return types:
/// - `None` → `ResponseMeta::Empty`
/// - `bytes` → `ResponseMeta::Inline(vec)`
/// - `(seg_idx: int, offset: int, data_size: int, is_dedicated: bool)` → `ResponseMeta::ShmAlloc`
fn parse_response_meta(py: Python<'_>, result: PyObject) -> Result<ResponseMeta, CrmError> {
    // None → Empty
    if result.is_none(py) {
        return Ok(ResponseMeta::Empty);
    }

    // bytes → Inline
    if let Ok(bytes) = result.downcast_bound::<PyBytes>(py) {
        return Ok(ResponseMeta::Inline(bytes.as_bytes().to_vec()));
    }

    // tuple (seg_idx, offset, data_size, is_dedicated) → ShmAlloc
    if let Ok(tup) = result.downcast_bound::<PyTuple>(py) {
        if tup.len() != 4 {
            return Err(CrmError::InternalError(format!(
                "expected 4-element tuple (seg_idx, offset, data_size, is_dedicated), got {}-element tuple",
                tup.len()
            )));
        }
        let seg_idx: u16 = tup
                .get_item(0)
                .map_err(|e| CrmError::InternalError(format!("bad seg_idx: {e}")))?
                .extract()
                .map_err(|e| CrmError::InternalError(format!("bad seg_idx: {e}")))?;
            let offset: u32 = tup
                .get_item(1)
                .map_err(|e| CrmError::InternalError(format!("bad offset: {e}")))?
                .extract()
                .map_err(|e| CrmError::InternalError(format!("bad offset: {e}")))?;
            let data_size: u32 = tup
                .get_item(2)
                .map_err(|e| CrmError::InternalError(format!("bad data_size: {e}")))?
                .extract()
                .map_err(|e| CrmError::InternalError(format!("bad data_size: {e}")))?;
            let is_dedicated: bool = tup
                .get_item(3)
                .map_err(|e| CrmError::InternalError(format!("bad is_dedicated: {e}")))?
                .extract()
                .map_err(|e| CrmError::InternalError(format!("bad is_dedicated: {e}")))?;
        return Ok(ResponseMeta::ShmAlloc {
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            });
    }

    Err(CrmError::InternalError(
        "dispatcher must return None, bytes, or (seg_idx, offset, data_size, is_dedicated) tuple"
            .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register server classes on the parent module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyServer>()?;
    Ok(())
}
