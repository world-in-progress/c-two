//! PyO3 bindings for the buddy allocator pool.
//!
//! Exposes the pool to Python with a clean, safe API:
//! - BuddyPoolHandle: main pool object
//! - PoolAlloc: allocation result (seg_idx, offset, size, level, is_dedicated)
//! - PoolConfig: configuration dataclass
//! - PoolStats: statistics dataclass

use crate::pool::{BuddyPool, PoolAllocation, PoolConfig};
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::RwLock;

/// Python-visible pool configuration.
#[pyclass(name = "PoolConfig")]
#[derive(Debug, Clone)]
pub struct PyPoolConfig {
    #[pyo3(get, set)]
    pub segment_size: usize,
    #[pyo3(get, set)]
    pub min_block_size: usize,
    #[pyo3(get, set)]
    pub max_segments: usize,
    #[pyo3(get, set)]
    pub max_dedicated_segments: usize,
    #[pyo3(get, set)]
    pub dedicated_gc_delay_secs: f64,
}

#[pymethods]
impl PyPoolConfig {
    #[new]
    #[pyo3(signature = (
        segment_size = 256 * 1024 * 1024,
        min_block_size = 4096,
        max_segments = 8,
        max_dedicated_segments = 4,
        dedicated_gc_delay_secs = 5.0,
    ))]
    fn new(
        segment_size: usize,
        min_block_size: usize,
        max_segments: usize,
        max_dedicated_segments: usize,
        dedicated_gc_delay_secs: f64,
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
        if dedicated_gc_delay_secs.is_nan() {
            return Err(PyValueError::new_err("dedicated_gc_delay_secs must not be NaN"));
        }
        Ok(Self {
            segment_size,
            min_block_size,
            max_segments,
            max_dedicated_segments,
            dedicated_gc_delay_secs,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "PoolConfig(segment_size={}, min_block={}, max_seg={}, max_ded={})",
            self.segment_size, self.min_block_size, self.max_segments, self.max_dedicated_segments
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
            dedicated_gc_delay_secs: py.dedicated_gc_delay_secs,
        }
    }
}

/// Python-visible allocation result.
#[pyclass(name = "PoolAlloc")]
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
#[pyclass(name = "PoolStats")]
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

/// The main buddy pool handle exposed to Python.
///
/// Thread-safe via internal `RwLock`. Read-only operations (`stats`,
/// `segment_name`, `segment_count`, `seg_data_info`, `data_addr`, `read_at`)
/// take a shared read lock; mutating operations (`alloc`, `alloc_ptr`,
/// `free`, `free_at`, `open_segment`, `gc`, `destroy`) take an exclusive
/// write lock.  This allows concurrent readers without blocking.
#[pyclass(name = "BuddyPoolHandle")]
pub struct PyBuddyPoolHandle {
    pool: RwLock<BuddyPool>,
}

#[pymethods]
impl PyBuddyPoolHandle {
    #[new]
    #[pyo3(signature = (config = None))]
    fn new(config: Option<&PyPoolConfig>) -> PyResult<Self> {
        let cfg = config.map(PoolConfig::from).unwrap_or_default();
        Ok(Self {
            pool: RwLock::new(BuddyPool::new(cfg)),
        })
    }

    /// Allocate `size` bytes and return (PoolAlloc, raw_address).
    ///
    /// The raw_address is a usize pointer into SHM that Python can use with
    /// ctypes.memmove or (ctypes.c_char * size).from_address(addr) to write
    /// directly into the SHM block — zero intermediate copies.
    fn alloc_ptr(&self, size: usize) -> PyResult<(PyPoolAlloc, usize)> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        let ptr = pool.data_ptr_at(seg_idx, offset, is_dedicated)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(ptr as usize)
    }

    /// Allocate `size` bytes from the pool.
    /// Returns a PoolAlloc with segment index, offset, actual size, level, and dedicated flag.
    fn alloc(&self, size: usize) -> PyResult<PyPoolAlloc> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        pool.alloc(size)
            .map(PyPoolAlloc::from)
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Free a previously allocated block.
    fn free(&self, alloc: &PyPoolAlloc) -> PyResult<()> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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

    /// Get a memoryview into the allocated SHM block (zero-copy).
    ///
    /// Returns a Python memoryview pointing directly at the SHM data.
    /// The caller MUST free the allocation AFTER releasing the memoryview.
    fn get_memoryview<'py>(
        &self,
        py: Python<'py>,
        alloc: &PyPoolAlloc,
        size: usize,
    ) -> PyResult<Bound<'py, pyo3::types::PyMemoryView>> {
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        let pa = PoolAllocation {
            seg_idx: alloc.seg_idx,
            offset: alloc.offset,
            actual_size: alloc.actual_size,
            level: alloc.level,
            is_dedicated: alloc.is_dedicated,
        };
        let ptr = pool.data_ptr(&pa).map_err(|e| PyRuntimeError::new_err(e))?;
        if size > alloc.actual_size as usize {
            return Err(PyValueError::new_err("size exceeds allocation"));
        }

        // Create a bytes-like view using PyBytes as backing and then slicing.
        // For true zero-copy, we use the buffer protocol via a custom wrapper.
        // For now, create a memoryview from a bytes object (1 copy).
        // TODO: Phase 2 — implement buffer protocol for zero-copy.
        let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
        let bytes = PyBytes::new(py, slice);
        let mv = pyo3::types::PyMemoryView::from(&bytes)?;
        Ok(mv.unbind().into_bound(py))
    }

    /// Write data from a Python buffer (bytes-like) into an allocated block.
    /// Uses buffer protocol for zero-copy from Python side.
    fn write_from_buffer(&self, py: Python<'_>, data: PyBuffer<u8>, alloc: &PyPoolAlloc) -> PyResult<()> {
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        pool.free_at(seg_idx, offset, data_size, is_dedicated)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok(())
    }

    /// Get pool statistics.
    fn stats(&self) -> PyResult<PyPoolStats> {
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
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
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        Ok(pool.segment_count())
    }

    /// Run garbage collection on freed dedicated segments and idle buddy segments.
    fn gc(&self) -> PyResult<()> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        pool.gc_dedicated();
        pool.gc_buddy();
        Ok(())
    }

    /// Get the SHM name for a buddy segment.
    fn segment_name(&self, idx: usize) -> PyResult<Option<String>> {
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        Ok(pool.segment_name(idx).map(|s| s.to_string()))
    }

    /// Get the data region base address and size for a buddy segment.
    ///
    /// Returns (data_base_addr, data_region_size).  Python creates a persistent
    /// memoryview from this instead of per-request ctypes arrays.
    fn seg_data_info(&self, seg_idx: u32) -> PyResult<(usize, usize)> {
        let pool = self.pool.read().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        let (ptr, size) = pool.seg_data_info(seg_idx)
            .map_err(|e| PyRuntimeError::new_err(e))?;
        Ok((ptr as usize, size))
    }

    /// Open a remote buddy segment (for the other side of a connection).
    fn open_segment(&self, name: &str, size: usize) -> PyResult<usize> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        pool.open_segment(name, size)
            .map_err(|e| PyRuntimeError::new_err(e))
    }

    /// Destroy the pool and all its SHM segments.
    fn destroy(&self) -> PyResult<()> {
        let mut pool = self.pool.write().map_err(|e| {
            PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
        })?;
        pool.destroy();
        Ok(())
    }
}

/// Register the c2_buddy Python module.
pub fn register_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPoolConfig>()?;
    m.add_class::<PyPoolAlloc>()?;
    m.add_class::<PyPoolStats>()?;
    m.add_class::<PyBuddyPoolHandle>()?;
    Ok(())
}
