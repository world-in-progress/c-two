//! `ShmBuffer` — zero-copy Python buffer backed by shared memory.
//!
//! Wraps SHM coordinates (PeerShm), reassembly handles (Handle),
//! or inline bytes (Inline) and exposes them via `__getbuffer__`
//! for `memoryview()` zero-copy access.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;

use c2_mem::{MemHandle, MemPool};

// ---------------------------------------------------------------------------
// Inner enum
// ---------------------------------------------------------------------------

enum ShmBufferInner {
    /// Small payload copied inline (no SHM).
    Inline(Vec<u8>),
    /// Peer-side SHM coordinates — the pool has segments already opened.
    PeerShm {
        pool: Arc<Mutex<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Reassembled handle from the local pool.
    Handle {
        handle: MemHandle,
        pool: Arc<Mutex<MemPool>>,
    },
}

// ---------------------------------------------------------------------------
// PyShmBuffer pyclass
// ---------------------------------------------------------------------------

/// Zero-copy SHM buffer supporting Python buffer protocol.
///
/// For SHM-backed buffers, `memoryview(buf)` returns a direct view
/// into shared memory — zero copies. Call `buf.release()` when done
/// to free the SHM allocation.
#[pyclass(name = "ShmBuffer", frozen)]
pub struct PyShmBuffer {
    inner: Mutex<Option<ShmBufferInner>>,
    data_len: usize,
    exports: AtomicU32,
}

// ---------------------------------------------------------------------------
// Rust-only constructors (called from other FFI code)
// ---------------------------------------------------------------------------

impl PyShmBuffer {
    /// Wrap inline bytes (no SHM involved).
    pub fn from_inline(data: Vec<u8>) -> Self {
        let data_len = data.len();
        Self {
            inner: Mutex::new(Some(ShmBufferInner::Inline(data))),
            data_len,
            exports: AtomicU32::new(0),
        }
    }

    /// Wrap peer-side SHM coordinates.
    pub fn from_peer_shm(
        pool: Arc<Mutex<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(Some(ShmBufferInner::PeerShm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            })),
            data_len: data_size as usize,
            exports: AtomicU32::new(0),
        }
    }

    /// Wrap a reassembled `MemHandle`.
    pub fn from_handle(handle: MemHandle, pool: Arc<Mutex<MemPool>>) -> Self {
        let data_len = handle.len();
        Self {
            inner: Mutex::new(Some(ShmBufferInner::Handle { handle, pool })),
            data_len,
            exports: AtomicU32::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Python methods
// ---------------------------------------------------------------------------

#[pymethods]
impl PyShmBuffer {
    /// Length of the buffer data in bytes.
    fn __len__(&self) -> PyResult<usize> {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(_) => Ok(self.data_len),
            None => Err(PyValueError::new_err("buffer already released")),
        }
    }

    /// True if the buffer has not been released and has non-zero length.
    fn __bool__(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.is_some() && self.data_len > 0
    }

    /// True if the buffer is inline (no SHM backing).
    #[getter]
    fn is_inline(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        matches!(guard.as_ref(), Some(ShmBufferInner::Inline(_)))
    }

    /// Free the underlying SHM block.
    ///
    /// Raises `BufferError` if memoryviews are still active.
    /// Idempotent — calling on an already-released buffer is a no-op.
    fn release(&self) -> PyResult<()> {
        // Fast-path: avoid locking if exports are obviously active
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: active memoryview exports",
            ));
        }
        let mut guard = self.inner.lock().unwrap();
        // Re-check under lock to close the TOCTOU window
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: active memoryview exports",
            ));
        }
        match guard.take() {
            Some(ShmBufferInner::PeerShm {
                pool, seg_idx, offset, data_size, is_dedicated,
            }) => {
                if let Ok(mut p) = pool.lock() {
                    let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                }
                Ok(())
            }
            Some(ShmBufferInner::Handle { handle, pool }) => {
                if let Ok(mut p) = pool.lock() {
                    let _ = p.release_handle(handle);
                }
                Ok(())
            }
            Some(ShmBufferInner::Inline(_)) => Ok(()),
            None => Ok(()), // already released — idempotent
        }
    }

    /// Buffer protocol — enables `memoryview(shm_buf)`.
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
            ShmBufferInner::Inline(vec) => (vec.as_ptr(), vec.len()),
            ShmBufferInner::PeerShm {
                pool, seg_idx, offset, data_size, is_dedicated,
            } => {
                let pool_guard = pool.lock()
                    .map_err(|_| PyBufferError::new_err("pool lock poisoned"))?;
                let raw_ptr = pool_guard
                    .data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyBufferError::new_err(format!("SHM access: {e}")))?;
                (raw_ptr as *const u8, *data_size as usize)
            }
            ShmBufferInner::Handle { handle, pool } => {
                let pool_guard = pool.lock()
                    .map_err(|_| PyBufferError::new_err("pool lock poisoned"))?;
                let slice = pool_guard.handle_slice(handle);
                (slice.as_ptr(), slice.len())
            }
        };

        // Fill the Py_buffer fields — same pattern as PyResponseBuffer.
        unsafe {
            (*view).buf = ptr as *mut std::os::raw::c_void;
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = len as isize;
            (*view).readonly = 1;
            (*view).itemsize = 1;
            (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
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

    /// Buffer protocol — release export.
    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Drop — auto-free if Python forgot to call release()
// ---------------------------------------------------------------------------

impl Drop for PyShmBuffer {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(inner) = guard.take() {
                match inner {
                    ShmBufferInner::PeerShm {
                        pool, seg_idx, offset, data_size, is_dedicated,
                    } => {
                        if let Ok(mut p) = pool.lock() {
                            let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                        }
                    }
                    ShmBufferInner::Handle { handle, pool } => {
                        if let Ok(mut p) = pool.lock() {
                            let _ = p.release_handle(handle);
                        }
                    }
                    ShmBufferInner::Inline(_) => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub fn register_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyShmBuffer>()?;
    Ok(())
}
