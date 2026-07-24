//! `ShmBuffer` — zero-copy Python buffer backed by shared memory.
//!
//! Wraps SHM coordinates (PeerShm), reassembly handles (Handle),
//! or inline bytes (Inline) and exposes them via `__getbuffer__`
//! for `memoryview()` zero-copy access.

use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;

use crate::lease_ffi::PyBufferLeaseTracker;
use c2_mem::{BufferLeaseGuard, MemHandle, MemPool};

// ---------------------------------------------------------------------------
// Inner enum
// ---------------------------------------------------------------------------

enum ShmBufferInner {
    /// Small payload copied inline (no SHM).
    Inline(Vec<u8>),
    /// Peer-side SHM coordinates — the pool has segments already opened.
    PeerShm {
        pool: Arc<RwLock<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Reassembled handle from the local pool.
    Handle {
        handle: MemHandle,
        pool: Arc<RwLock<MemPool>>,
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
#[pyclass(name = "ShmBuffer", frozen, weakref)]
pub struct PyShmBuffer {
    inner: Mutex<Option<ShmBufferInner>>,
    lease: Mutex<Option<BufferLeaseGuard>>,
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
            lease: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
        }
    }

    /// Wrap peer-side SHM coordinates.
    pub fn from_peer_shm(
        pool: Arc<RwLock<MemPool>>,
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
            lease: Mutex::new(None),
            data_len: data_size as usize,
            exports: AtomicU32::new(0),
        }
    }

    /// Wrap a reassembled `MemHandle`.
    pub fn from_handle(handle: MemHandle, pool: Arc<RwLock<MemPool>>) -> Self {
        let data_len = handle.len();
        Self {
            inner: Mutex::new(Some(ShmBufferInner::Handle { handle, pool })),
            lease: Mutex::new(None),
            data_len,
            exports: AtomicU32::new(0),
        }
    }

    fn storage_label_for_inner(inner: &ShmBufferInner) -> &'static str {
        match inner {
            ShmBufferInner::Inline(_) => "inline",
            ShmBufferInner::PeerShm { .. } => "shm",
            ShmBufferInner::Handle { handle, .. } => {
                if handle.is_file_spill() {
                    "file_spill"
                } else {
                    "handle"
                }
            }
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
        let guard = self.inner.lock();
        match guard.as_ref() {
            Some(_) => Ok(self.data_len),
            None => Err(PyValueError::new_err("buffer already released")),
        }
    }

    /// True if the buffer has not been released and has non-zero length.
    fn __bool__(&self) -> bool {
        let guard = self.inner.lock();
        guard.is_some() && self.data_len > 0
    }

    /// True if the buffer is inline (no SHM backing).
    #[getter]
    fn is_inline(&self) -> bool {
        let guard = self.inner.lock();
        matches!(guard.as_ref(), Some(ShmBufferInner::Inline(_)))
    }

    /// Number of active buffer protocol exports (memoryviews).
    #[getter]
    fn exports(&self) -> u32 {
        self.exports.load(Ordering::Acquire)
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
        let mut guard = self.inner.lock();
        // Re-check under lock to close the TOCTOU window
        if self.exports.load(Ordering::Acquire) > 0 {
            return Err(PyBufferError::new_err(
                "cannot release: active memoryview exports",
            ));
        }
        let inner = guard.take();
        self.lease.lock().take();
        drop(guard);

        match inner {
            Some(ShmBufferInner::PeerShm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            }) => {
                let mut p = pool.write();
                let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                Ok(())
            }
            Some(ShmBufferInner::Handle { handle, pool }) => {
                let mut p = pool.write();
                let _ = p.release_handle(handle);
                Ok(())
            }
            Some(ShmBufferInner::Inline(_)) => Ok(()),
            None => Ok(()), // already released — idempotent
        }
    }

    #[pyo3(signature = (tracker, route_name, method_name, direction="resource_input"))]
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

    /// Buffer protocol — enables `memoryview(shm_buf)`.
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
            ShmBufferInner::Inline(vec) => (vec.as_ptr(), vec.len()),
            ShmBufferInner::PeerShm {
                pool,
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            } => {
                let pool_guard = pool.read();
                let raw_ptr = pool_guard
                    .data_ptr_at(*seg_idx as u32, *offset, *is_dedicated)
                    .map_err(|e| PyBufferError::new_err(format!("SHM access: {e}")))?;
                (raw_ptr as *const u8, *data_size as usize)
            }
            ShmBufferInner::Handle { handle, pool } => {
                let pool_guard = pool.read();
                let slice = pool_guard.handle_slice(handle);
                (slice.as_ptr(), slice.len())
            }
        };

        // SAFETY: `pool_guard` (read lock) is released after extracting the raw pointer,
        // but `ptr` remains valid because:
        // (1) `self.inner` is still `Some` (guarded by the inner Mutex held above),
        // (2) `exports` will be incremented below, preventing `release()` from freeing the block,
        // (3) an un-freed block keeps its SHM segment non-idle, so GC cannot reclaim/unmap it.

        // Fill the Py_buffer fields — same pattern as PyResponseBuffer.
        unsafe {
            (*view).buf = ptr as *mut std::os::raw::c_void;
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = len as isize;
            (*view).readonly = 1;
            (*view).itemsize = 1;
            (*view).format = if flags & ffi::PyBUF_FORMAT != 0 {
                c"B".as_ptr().cast_mut()
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
        let mut guard = self.inner.lock();
        let inner = guard.take();
        self.lease.lock().take();
        drop(guard);

        if let Some(inner) = inner {
            match inner {
                ShmBufferInner::PeerShm {
                    pool,
                    seg_idx,
                    offset,
                    data_size,
                    is_dedicated,
                } => {
                    let mut p = pool.write();
                    let _ = p.free_at(seg_idx as u32, offset, data_size, is_dedicated);
                }
                ShmBufferInner::Handle { handle, pool } => {
                    let mut p = pool.write();
                    let _ = p.release_handle(handle);
                }
                ShmBufferInner::Inline(_) => {}
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
