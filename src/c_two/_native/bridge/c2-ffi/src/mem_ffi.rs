//! PyO3 bindings for the unified memory pool.
//!
//! Exposes the pool to Python with a clean, safe API:
//! - MemPool: main pool object (was MemPoolHandle)
//! - PoolAlloc: allocation result (seg_idx, offset, size, level, is_dedicated)
//! - PoolConfig: configuration dataclass
//! - PoolStats: statistics dataclass

use c2_mem::handle::MemHandle;
use c2_mem::{MemPool, PoolAllocation, PoolConfig};
use c2_wire::assembler::ChunkAssembler;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};

/// Python-visible pool configuration.
#[pyclass(name = "PoolConfig", frozen)]
#[derive(Debug, Clone)]
pub struct PyPoolConfig {
    #[pyo3(get)]
    pub segment_size: usize,
    #[pyo3(get)]
    pub min_block_size: usize,
    #[pyo3(get)]
    pub max_segments: usize,
    #[pyo3(get)]
    pub max_dedicated_segments: usize,
    #[pyo3(get)]
    pub dedicated_crash_timeout_secs: f64,
    #[pyo3(get)]
    pub buddy_idle_decay_secs: f64,
    #[pyo3(get)]
    pub spill_threshold: f64,
    #[pyo3(get)]
    pub spill_dir: String,
}

#[pymethods]
impl PyPoolConfig {
    #[new]
    #[pyo3(signature = (
        segment_size = 256 * 1024 * 1024,
        min_block_size = 4096,
        max_segments = 8,
        max_dedicated_segments = 4,
        dedicated_crash_timeout_secs = 60.0,
        buddy_idle_decay_secs = 60.0,
        spill_threshold = 0.8,
        spill_dir = String::from("/tmp/c_two_spill/"),
    ))]
    fn new(
        segment_size: usize,
        min_block_size: usize,
        max_segments: usize,
        max_dedicated_segments: usize,
        dedicated_crash_timeout_secs: f64,
        buddy_idle_decay_secs: f64,
        spill_threshold: f64,
        spill_dir: String,
    ) -> PyResult<Self> {
        if !min_block_size.is_power_of_two() {
            return Err(PyValueError::new_err("min_block_size must be a power of 2"));
        }
        if segment_size < min_block_size * 2 {
            return Err(PyValueError::new_err(
                "segment_size must be at least 2x min_block_size",
            ));
        }
        if !segment_size.is_power_of_two() {
            return Err(PyValueError::new_err("segment_size must be a power of 2"));
        }
        if dedicated_crash_timeout_secs.is_nan() {
            return Err(PyValueError::new_err("dedicated_crash_timeout_secs must not be NaN"));
        }
        if buddy_idle_decay_secs.is_nan() {
            return Err(PyValueError::new_err("buddy_idle_decay_secs must not be NaN"));
        }
        if spill_threshold < 0.0 || spill_threshold > 1.0 || spill_threshold.is_nan() {
            return Err(PyValueError::new_err("spill_threshold must be in [0.0, 1.0]"));
        }
        Ok(Self {
            segment_size,
            min_block_size,
            max_segments,
            max_dedicated_segments,
            dedicated_crash_timeout_secs,
            buddy_idle_decay_secs,
            spill_threshold,
            spill_dir,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "PoolConfig(segment_size={}, min_block={}, max_seg={}, max_ded={}, spill_threshold={}, spill_dir='{}')",
            self.segment_size, self.min_block_size, self.max_segments, self.max_dedicated_segments,
            self.spill_threshold, self.spill_dir
        )
    }
}

impl From<&PyPoolConfig> for PoolConfig {
    fn from(py: &PyPoolConfig) -> Self {
        PoolConfig {
            segment_size: py.segment_size,
            min_block_size: py.min_block_size,
            max_segments: py.max_segments,
            max_dedicated_segments: py.max_dedicated_segments,
            dedicated_crash_timeout_secs: py.dedicated_crash_timeout_secs,
            buddy_idle_decay_secs: py.buddy_idle_decay_secs,
            spill_threshold: py.spill_threshold,
            spill_dir: std::path::PathBuf::from(&py.spill_dir),
        }
    }
}

/// Python-visible allocation result.
#[pyclass(name = "PoolAlloc", frozen)]
#[derive(Debug, Clone)]
pub struct PyPoolAlloc {
    #[pyo3(get)]
    pub seg_idx: u32,
    #[pyo3(get)]
    pub offset: u32,
    #[pyo3(get)]
    pub actual_size: u32,
    #[pyo3(get)]
    pub level: u16,
    #[pyo3(get)]
    pub is_dedicated: bool,
}

#[pymethods]
impl PyPoolAlloc {
    fn __repr__(&self) -> String {
        format!(
            "PoolAlloc(seg={}, off={}, size={}, lvl={}, ded={})",
            self.seg_idx, self.offset, self.actual_size, self.level, self.is_dedicated
        )
    }
}

impl From<PoolAllocation> for PyPoolAlloc {
    fn from(a: PoolAllocation) -> Self {
        Self {
            seg_idx: a.seg_idx,
            offset: a.offset,
            actual_size: a.actual_size,
            level: a.level,
            is_dedicated: a.is_dedicated,
        }
    }
}

/// Python-visible pool statistics.
#[pyclass(name = "PoolStats", frozen)]
#[derive(Debug, Clone)]
pub struct PyPoolStats {
    #[pyo3(get)]
    pub total_segments: usize,
    #[pyo3(get)]
    pub dedicated_segments: usize,
    #[pyo3(get)]
    pub total_bytes: u64,
    #[pyo3(get)]
    pub free_bytes: u64,
    #[pyo3(get)]
    pub alloc_count: u32,
    #[pyo3(get)]
    pub fragmentation_ratio: f64,
}

#[pymethods]
impl PyPoolStats {
    fn __repr__(&self) -> String {
        format!(
            "PoolStats(segs={}, ded={}, total={}MB, free={}MB, allocs={}, frag={:.2}%)",
            self.total_segments,
            self.dedicated_segments,
            self.total_bytes / (1024 * 1024),
            self.free_bytes / (1024 * 1024),
            self.alloc_count,
            self.fragmentation_ratio * 100.0
        )
    }
}

/// The main memory pool handle exposed to Python.
///
/// Thread-safe via internal `RwLock`. Read-only operations (`stats`,
/// `segment_name`, `segment_count`, `seg_data_info`, `data_addr`, `read_at`)
/// take a shared read lock; mutating operations (`alloc`, `alloc_ptr`,
/// `free`, `free_at`, `open_segment`, `gc`, `destroy`) take an exclusive
/// write lock.  This allows concurrent readers without blocking.
#[pyclass(name = "MemPool", frozen)]
pub struct PyMemPool {
    pool: Arc<RwLock<MemPool>>,
}

impl PyMemPool {
    pub(crate) fn pool_arc(&self) -> Arc<RwLock<MemPool>> {
        Arc::clone(&self.pool)
    }

    /// Create from an existing shared pool (used by server_ffi to expose response_pool).
    pub(crate) fn from_arc(pool: Arc<RwLock<MemPool>>) -> Self {
        Self { pool }
    }
}

#[pymethods]
impl PyMemPool {
    #[new]
    #[pyo3(signature = (config = None))]
    fn new(config: Option<&PyPoolConfig>) -> PyResult<Self> {
        let cfg = config.map(PoolConfig::from).unwrap_or_default();
        Ok(Self {
            pool: Arc::new(RwLock::new(MemPool::new(cfg))),
        })
    }

    /// Allocate `size` bytes and return (PoolAlloc, raw_address).
    ///
    /// The raw_address is a usize pointer into SHM that Python can use with
    /// ctypes.memmove or (ctypes.c_char * size).from_address(addr) to write
    /// directly into the SHM block — zero intermediate copies.
    fn alloc_ptr(&self, size: usize) -> PyResult<(PyPoolAlloc, usize)> {
        let mut pool = self.pool.write();
        let alloc = pool.alloc(size)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        let ptr = pool.data_ptr(&alloc)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok((PyPoolAlloc::from(alloc), ptr as usize))
    }

    /// Get the raw address for a (seg_idx, offset) pair — for remote reading.
    ///
    /// Returns the usize pointer that Python can pass to ctypes.memmove.
    fn data_addr(&self, seg_idx: u32, offset: u32, is_dedicated: bool) -> PyResult<usize> {
        let pool = self.pool.read();
        let ptr = pool.data_ptr_at(seg_idx, offset, is_dedicated)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(ptr as usize)
    }

    /// Allocate `size` bytes from the pool.
    /// Returns a PoolAlloc with segment index, offset, actual size, level, and dedicated flag.
    fn alloc(&self, size: usize) -> PyResult<PyPoolAlloc> {
        let mut pool = self.pool.write();
        pool.alloc(size)
            .map(PyPoolAlloc::from)
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Free a previously allocated block.
    fn free(&self, alloc: &PyPoolAlloc) -> PyResult<()> {
        let mut pool = self.pool.write();
        let pa = PoolAllocation {
            seg_idx: alloc.seg_idx,
            offset: alloc.offset,
            actual_size: alloc.actual_size,
            level: alloc.level,
            is_dedicated: alloc.is_dedicated,
        };
        pool.free(&pa)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(())
    }

    /// Write data into an allocated block.
    fn write(&self, alloc: &PyPoolAlloc, data: &[u8]) -> PyResult<()> {
        let pool = self.pool.read();
        let pa = PoolAllocation {
            seg_idx: alloc.seg_idx,
            offset: alloc.offset,
            actual_size: alloc.actual_size,
            level: alloc.level,
            is_dedicated: alloc.is_dedicated,
        };
        let ptr = pool.data_ptr(&pa).map_err(|e| PyRuntimeError::new_err(e))?;
        if data.len() > alloc.actual_size as usize {
            return Err(PyValueError::new_err("data exceeds allocation size"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }
        Ok(())
    }

    /// Read data from an allocated block as bytes.
    fn read<'py>(&self, py: Python<'py>, alloc: &PyPoolAlloc, size: usize) -> PyResult<Bound<'py, PyBytes>> {
        let pool = self.pool.read();
        let pa = PoolAllocation {
            seg_idx: alloc.seg_idx,
            offset: alloc.offset,
            actual_size: alloc.actual_size,
            level: alloc.level,
            is_dedicated: alloc.is_dedicated,
        };
        let ptr = pool.data_ptr(&pa).map_err(|e| PyRuntimeError::new_err(e))?;
        if size > alloc.actual_size as usize {
            return Err(PyValueError::new_err("read size exceeds allocation"));
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
        Ok(PyBytes::new(py, slice))
    }

    /// Write data from a Python buffer (bytes-like) into an allocated block.
    /// Uses buffer protocol for zero-copy from Python side.
    fn write_from_buffer(&self, py: Python<'_>, data: PyBuffer<u8>, alloc: &PyPoolAlloc) -> PyResult<()> {
        let pool = self.pool.read();
        let pa = PoolAllocation {
            seg_idx: alloc.seg_idx,
            offset: alloc.offset,
            actual_size: alloc.actual_size,
            level: alloc.level,
            is_dedicated: alloc.is_dedicated,
        };
        let ptr = pool.data_ptr(&pa).map_err(|e| PyRuntimeError::new_err(e))?;
        let len = data.len_bytes();
        if len > alloc.actual_size as usize {
            return Err(PyValueError::new_err("buffer exceeds allocation size"));
        }
        // Copy from Python buffer to SHM.
        unsafe {
            data.copy_to_slice(py, &mut std::slice::from_raw_parts_mut(ptr, len))?;
        }
        Ok(())
    }

    /// Read data from a specific (seg_idx, offset) without a PoolAlloc object.
    ///
    /// Used by the remote side to read from SHM blocks allocated by the peer.
    #[pyo3(name = "read_at")]
    fn read_at<'py>(
        &self,
        py: Python<'py>,
        seg_idx: u32,
        offset: u32,
        size: usize,
        is_dedicated: bool,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let pool = self.pool.read();
        let ptr = pool.data_ptr_at(seg_idx, offset, is_dedicated)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
        Ok(PyBytes::new(py, slice))
    }

    /// Free a block given (seg_idx, offset, data_size) without a PoolAlloc object.
    ///
    /// Used by the remote side to free SHM blocks after reading.
    /// Recomputes the buddy level from data_size internally.
    #[pyo3(name = "free_at")]
    fn free_at(
        &self,
        seg_idx: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> PyResult<()> {
        let mut pool = self.pool.write();
        let _ = pool.free_at(seg_idx, offset, data_size, is_dedicated)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(())
    }

    /// Get pool statistics.
    fn stats(&self) -> PyResult<PyPoolStats> {
        let pool = self.pool.read();
        let s = pool.stats();
        Ok(PyPoolStats {
            total_segments: s.total_segments,
            dedicated_segments: s.dedicated_segments,
            total_bytes: s.total_bytes,
            free_bytes: s.free_bytes,
            alloc_count: s.alloc_count,
            fragmentation_ratio: s.fragmentation_ratio,
        })
    }

    /// Number of buddy segments currently alive.
    fn segment_count(&self) -> PyResult<usize> {
        let pool = self.pool.read();
        Ok(pool.segment_count())
    }

    /// Run garbage collection on freed dedicated segments and idle buddy segments.
    fn gc(&self) -> PyResult<()> {
        let mut pool = self.pool.write();
        pool.gc_dedicated();
        pool.gc_buddy();
        Ok(())
    }

    /// Get the SHM name for a buddy segment.
    fn segment_name(&self, idx: usize) -> PyResult<Option<String>> {
        let pool = self.pool.read();
        Ok(pool.segment_name(idx).map(|s| s.to_string()))
    }

    /// Get the pool name prefix (for handshake exchange / name derivation).
    fn prefix(&self) -> PyResult<String> {
        let pool = self.pool.read();
        Ok(pool.prefix().to_string())
    }

    /// Derive the SHM name for a segment given its index and tier.
    ///
    /// Buddy segments: `{prefix}_b{seg_idx:04x}`
    /// Dedicated segments: `{prefix}_d{seg_idx:04x}`
    #[pyo3(signature = (seg_idx, is_dedicated = false))]
    fn derive_segment_name(&self, seg_idx: u32, is_dedicated: bool) -> PyResult<String> {
        let pool = self.pool.read();
        let tag = if is_dedicated { "d" } else { "b" };
        Ok(format!("{}_{}{:04x}", pool.prefix(), tag, seg_idx))
    }

    /// Get the data region base address and size for a buddy segment.
    ///
    /// Returns (data_base_addr, data_region_size).  Python creates a persistent
    /// memoryview from this instead of per-request ctypes arrays.
    fn seg_data_info(&self, seg_idx: u32) -> PyResult<(usize, usize)> {
        let pool = self.pool.read();
        let (ptr, size) = pool.seg_data_info(seg_idx)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok((ptr as usize, size))
    }

    /// Open a remote buddy segment (for the other side of a connection).
    fn open_segment(&self, name: &str, size: usize) -> PyResult<usize> {
        let mut pool = self.pool.write();
        pool.open_segment(name, size)
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Destroy the pool and all its SHM segments.
    fn destroy(&self) -> PyResult<()> {
        let mut pool = self.pool.write();
        pool.destroy();
        Ok(())
    }
}

/// Python-visible handle to a memory region (buddy SHM, dedicated SHM,
/// or file-backed mmap). Supports write_at and buffer_info for zero-copy.
///
/// Thread-safe via `frozen` + interior `Mutex`.
#[pyclass(name = "MemHandle", frozen)]
pub struct PyMemHandle {
    state: Mutex<MemHandleInner>,
}

struct MemHandleInner {
    handle: Option<MemHandle>,
    pool: Arc<RwLock<MemPool>>,
}

#[pymethods]
impl PyMemHandle {
    #[getter]
    fn len(&self) -> PyResult<usize> {
        let state = self.state.lock();
        state
            .handle
            .as_ref()
            .map(|h| h.len())
            .ok_or_else(|| PyRuntimeError::new_err("handle released"))
    }

    #[getter]
    fn is_file_spill(&self) -> bool {
        let state = self.state.lock();
        state
            .handle
            .as_ref()
            .map(|h| h.is_file_spill())
            .unwrap_or(false)
    }

    #[getter]
    fn is_buddy(&self) -> bool {
        let state = self.state.lock();
        state
            .handle
            .as_ref()
            .map(|h| h.is_buddy())
            .unwrap_or(false)
    }

    #[getter]
    fn is_dedicated(&self) -> bool {
        let state = self.state.lock();
        state
            .handle
            .as_ref()
            .map(|h| h.is_dedicated())
            .unwrap_or(false)
    }

    /// Write `data` at byte `offset` within the handle.
    #[pyo3(signature = (data, offset = 0))]
    fn write_at(&self, data: &[u8], offset: usize) -> PyResult<()> {
        let mut state = self.state.lock();
        let inner = &mut *state; // reborrow for split field access
        let h = inner
            .handle
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("handle released"))?;
        if offset + data.len() > h.len() {
            return Err(PyRuntimeError::new_err("write_at out of bounds"));
        }
        let pool = inner
            .pool
            .read();
        pool.handle_slice_mut(h)[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Get raw pointer + length for memoryview construction.
    /// Returns (address, length) tuple.
    fn buffer_info(&self) -> PyResult<(usize, usize)> {
        let state = self.state.lock();
        let h = state
            .handle
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("handle released"))?;
        let pool = state
            .pool
            .read();
        let slice = pool.handle_slice(h);
        Ok((slice.as_ptr() as usize, slice.len()))
    }

    /// Release the underlying memory. Idempotent.
    fn release(&self) -> PyResult<()> {
        let mut state = self.state.lock();
        if let Some(h) = state.handle.take() {
            let mut pool = state
                .pool
                .write();
            pool.release_handle(h);
        }
        Ok(())
    }

    fn __len__(&self) -> PyResult<usize> {
        self.len()
    }

    fn __repr__(&self) -> String {
        let state = self.state.lock();
        match &state.handle {
            Some(h) => format!(
                "MemHandle(len={}, type={})",
                h.len(),
                if h.is_buddy() {
                    "buddy"
                } else if h.is_dedicated() {
                    "dedicated"
                } else {
                    "file_spill"
                }
            ),
            None => "MemHandle(released)".into(),
        }
    }
}

impl Drop for PyMemHandle {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if let Some(h) = state.handle.take() {
            let mut pool = state.pool.write();
            pool.release_handle(h);
        }
    }
}

/// Python wrapper for Rust ChunkAssembler.
///
/// Thread-safe via `frozen` + interior `Mutex`.
#[pyclass(name = "ChunkAssembler", frozen)]
pub struct PyChunkAssembler {
    state: Mutex<AssemblerInner>,
}

struct AssemblerInner {
    inner: Option<ChunkAssembler>,
    pool: Arc<RwLock<MemPool>>,
}

#[pymethods]
impl PyChunkAssembler {
    #[new]
    fn new(
        pool_handle: &PyMemPool,
        total_chunks: usize,
        chunk_size: usize,
    ) -> PyResult<Self> {
        let pool_arc = pool_handle.pool_arc();
        let asm = {
            let mut pool = pool_arc
                .write();
            ChunkAssembler::new(
                &mut pool, total_chunks, chunk_size,
                512, // TODO: pass from config
                8 * (1 << 30), // TODO: pass from config
            )
                .map_err(|e| PyRuntimeError::new_err(e))?
        };
        Ok(Self {
            state: Mutex::new(AssemblerInner {
                inner: Some(asm),
                pool: pool_arc,
            }),
        })
    }

    #[setter]
    fn set_route_name(&self, name: String) -> PyResult<()> {
        let mut state = self.state.lock();
        state
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("consumed"))?
            .route_name = Some(name);
        Ok(())
    }

    #[setter]
    fn set_method_idx(&self, idx: u16) -> PyResult<()> {
        let mut state = self.state.lock();
        state
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("consumed"))?
            .method_idx = Some(idx);
        Ok(())
    }

    #[getter]
    fn route_name(&self) -> Option<String> {
        let state = self.state.lock();
        state.inner.as_ref().and_then(|a| a.route_name.clone())
    }

    #[getter]
    fn method_idx(&self) -> Option<u16> {
        let state = self.state.lock();
        state.inner.as_ref().and_then(|a| a.method_idx)
    }

    /// Feed a chunk. Returns True when all chunks received.
    fn feed_chunk(&self, chunk_idx: usize, data: &[u8]) -> PyResult<bool> {
        let mut state = self.state.lock();
        let inner = &mut *state; // reborrow for split field access
        let asm = inner
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("consumed"))?;
        let pool = inner
            .pool
            .read();
        asm.feed_chunk(&pool, chunk_idx, data)
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    #[getter]
    fn is_complete(&self) -> bool {
        let state = self.state.lock();
        state
            .inner
            .as_ref()
            .map(|a| a.is_complete())
            .unwrap_or(false)
    }

    #[getter]
    fn received(&self) -> usize {
        let state = self.state.lock();
        state.inner.as_ref().map(|a| a.received()).unwrap_or(0)
    }

    /// Finish reassembly → PyMemHandle.
    fn finish(&self) -> PyResult<PyMemHandle> {
        let mut state = self.state.lock();
        let asm = state
            .inner
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("consumed"))?;
        let handle = asm.finish().map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(PyMemHandle {
            state: Mutex::new(MemHandleInner {
                handle: Some(handle),
                pool: Arc::clone(&state.pool),
            }),
        })
    }

    /// Abort reassembly, releasing buffer.
    fn abort(&self) -> PyResult<()> {
        let mut state = self.state.lock();
        if let Some(asm) = state.inner.take() {
            let mut pool = state
                .pool
                .write();
            asm.abort(&mut pool);
        }
        Ok(())
    }
}

impl Drop for PyChunkAssembler {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if let Some(asm) = state.inner.take() {
            let mut pool = state.pool.write();
            asm.abort(&mut pool);
        }
    }
}

/// Clean up stale SHM segments left by crashed processes.
///
/// Scans for SHM segments matching the "cc3b" prefix pattern, extracts the
/// PID from each name, and unlinks segments whose owner is no longer alive.
/// Returns the number of segments removed.
///
/// On macOS, returns 0 (POSIX SHM cannot be enumerated without /dev/shm).
#[pyfunction]
#[pyo3(signature = (prefix="cc3b"))]
fn cleanup_stale_shm(prefix: &str) -> usize {
    use c2_mem::MemPool;
    MemPool::cleanup_stale_segments(prefix)
}

/// Register the memory pool Python module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPoolConfig>()?;
    m.add_class::<PyPoolAlloc>()?;
    m.add_class::<PyPoolStats>()?;
    m.add_class::<PyMemPool>()?;
    m.add_class::<PyMemHandle>()?;
    m.add_class::<PyChunkAssembler>()?;
    m.add_function(wrap_pyfunction!(cleanup_stale_shm, m)?)?;
    Ok(())
}
