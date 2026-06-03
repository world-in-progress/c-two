//! Response-pool-backed final backing allocations for FastDB build contexts.

use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pyo3::exceptions::{PyBufferError, PyRuntimeError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyAny;

use c2_mem::{MemPool, PoolAllocation};
use c2_server::ResponseMeta;

enum ResponseBackingStorage {
    Inline(Vec<u8>),
    Shm {
        pool: Arc<RwLock<MemPool>>,
        alloc: PoolAllocation,
        len: usize,
    },
}

enum ResponseBackingState {
    Open(ResponseBackingStorage),
    Committed {
        storage: ResponseBackingStorage,
        used_size: usize,
    },
    RolledBack,
    Taken,
}

#[pyclass(name = "ResponseFinalBackingAllocator", frozen)]
pub(crate) struct PyResponseFinalBackingAllocator {
    pool: Arc<RwLock<MemPool>>,
    shm_threshold: u64,
    max_payload_size: u64,
}

impl PyResponseFinalBackingAllocator {
    pub(crate) fn new(
        pool: Arc<RwLock<MemPool>>,
        shm_threshold: u64,
        max_payload_size: u64,
    ) -> Self {
        Self {
            pool,
            shm_threshold,
            max_payload_size,
        }
    }
}

#[pymethods]
impl PyResponseFinalBackingAllocator {
    fn allocate(&self, nbytes: usize) -> PyResult<PyResponseFinalBackingAllocation> {
        if nbytes == 0 {
            return Err(PyValueError::new_err(
                "FastDB response final backing allocation size must be non-zero",
            ));
        }
        if nbytes as u64 > self.max_payload_size {
            return Err(PyValueError::new_err(format!(
                "FastDB response final backing size {nbytes} exceeds max_payload_size {}",
                self.max_payload_size
            )));
        }

        if nbytes as u64 > self.shm_threshold && u32::try_from(nbytes).is_ok() {
            let alloc = {
                let mut pool = self.pool.write();
                pool.alloc(nbytes).ok()
            };
            if let Some(alloc) = alloc {
                if alloc.seg_idx <= u16::MAX as u32 {
                    return Ok(PyResponseFinalBackingAllocation::from_storage(
                        ResponseBackingStorage::Shm {
                            pool: Arc::clone(&self.pool),
                            alloc,
                            len: nbytes,
                        },
                    ));
                }
                let mut pool = self.pool.write();
                let _ = pool.free(&alloc);
            }
        }

        let mut data = Vec::new();
        data.try_reserve_exact(nbytes).map_err(|err| {
            PyRuntimeError::new_err(format!(
                "failed to allocate inline response final backing: {err}"
            ))
        })?;
        data.resize(nbytes, 0);
        Ok(PyResponseFinalBackingAllocation::from_storage(
            ResponseBackingStorage::Inline(data),
        ))
    }
}

#[pyclass(name = "ResponseFinalBackingAllocation", frozen)]
pub(crate) struct PyResponseFinalBackingAllocation {
    state: Mutex<ResponseBackingState>,
    exports: AtomicU32,
}

impl PyResponseFinalBackingAllocation {
    fn from_storage(storage: ResponseBackingStorage) -> Self {
        Self {
            state: Mutex::new(ResponseBackingState::Open(storage)),
            exports: AtomicU32::new(0),
        }
    }

    pub(crate) fn take_response_meta(&self) -> PyResult<ResponseMeta> {
        let mut state = self.state.lock();
        let current = std::mem::replace(&mut *state, ResponseBackingState::Taken);
        match current {
            ResponseBackingState::Committed { storage, used_size } => {
                response_meta_from_storage(storage, used_size)
            }
            ResponseBackingState::Open(storage) => {
                drop_response_storage(storage);
                *state = ResponseBackingState::RolledBack;
                Err(PyRuntimeError::new_err(
                    "FastDB response final backing allocation was not committed",
                ))
            }
            ResponseBackingState::RolledBack => {
                *state = ResponseBackingState::RolledBack;
                Err(PyRuntimeError::new_err(
                    "FastDB response final backing allocation was rolled back",
                ))
            }
            ResponseBackingState::Taken => {
                *state = ResponseBackingState::Taken;
                Err(PyRuntimeError::new_err(
                    "FastDB response final backing allocation was already consumed",
                ))
            }
        }
    }
}

#[pymethods]
impl PyResponseFinalBackingAllocation {
    fn commit(&self, used_size: usize) -> PyResult<()> {
        let mut state = self.state.lock();
        let current = std::mem::replace(&mut *state, ResponseBackingState::Taken);
        match current {
            ResponseBackingState::Open(storage) => {
                let len = storage_len(&storage);
                if used_size != len {
                    drop_response_storage(storage);
                    *state = ResponseBackingState::RolledBack;
                    return Err(PyValueError::new_err(format!(
                        "FastDB response final backing commit size {used_size} does not match allocation size {len}",
                    )));
                }
                *state = ResponseBackingState::Committed { storage, used_size };
                Ok(())
            }
            other => {
                *state = other;
                Err(PyRuntimeError::new_err(
                    "FastDB response final backing allocation is not open",
                ))
            }
        }
    }

    fn rollback(&self) -> PyResult<()> {
        let mut state = self.state.lock();
        let current = std::mem::replace(&mut *state, ResponseBackingState::RolledBack);
        match current {
            ResponseBackingState::Open(storage)
            | ResponseBackingState::Committed { storage, .. } => {
                drop_response_storage(storage);
            }
            ResponseBackingState::RolledBack | ResponseBackingState::Taken => {}
        }
        Ok(())
    }

    fn __len__(&self) -> PyResult<usize> {
        let state = self.state.lock();
        match &*state {
            ResponseBackingState::Open(storage)
            | ResponseBackingState::Committed { storage, .. } => Ok(storage_len(storage)),
            ResponseBackingState::RolledBack => Err(PyBufferError::new_err(
                "FastDB response final backing allocation is rolled back",
            )),
            ResponseBackingState::Taken => Err(PyBufferError::new_err(
                "FastDB response final backing allocation is consumed",
            )),
        }
    }

    fn storage_kind(&self) -> &'static str {
        let state = self.state.lock();
        match &*state {
            ResponseBackingState::Open(storage)
            | ResponseBackingState::Committed { storage, .. } => storage_kind(storage),
            ResponseBackingState::RolledBack => "rolled_back",
            ResponseBackingState::Taken => "taken",
        }
    }

    unsafe fn __getbuffer__(
        slf: &Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let this = slf.borrow();
        let mut state = this.state.lock();
        let storage = match &mut *state {
            ResponseBackingState::Open(storage) => storage,
            ResponseBackingState::Committed { .. } => {
                return Err(PyBufferError::new_err(
                    "FastDB response final backing allocation is committed",
                ));
            }
            ResponseBackingState::RolledBack => {
                return Err(PyBufferError::new_err(
                    "FastDB response final backing allocation is rolled back",
                ));
            }
            ResponseBackingState::Taken => {
                return Err(PyBufferError::new_err(
                    "FastDB response final backing allocation is consumed",
                ));
            }
        };

        let (ptr, len) = storage_ptr_len(storage)?;
        unsafe {
            (*view).buf = ptr as *mut std::os::raw::c_void;
            (*view).obj = ffi::Py_NewRef(slf.as_ptr());
            (*view).len = len as isize;
            (*view).readonly = 0;
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

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for PyResponseFinalBackingAllocation {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        let current = std::mem::replace(&mut *state, ResponseBackingState::Taken);
        match current {
            ResponseBackingState::Open(storage)
            | ResponseBackingState::Committed { storage, .. } => {
                drop_response_storage(storage);
            }
            ResponseBackingState::RolledBack | ResponseBackingState::Taken => {}
        }
    }
}

fn storage_ptr_len(storage: &mut ResponseBackingStorage) -> PyResult<(*mut u8, usize)> {
    match storage {
        ResponseBackingStorage::Inline(data) => Ok((data.as_mut_ptr(), data.len())),
        ResponseBackingStorage::Shm { pool, alloc, len } => {
            let pool_guard = pool.read();
            let ptr = pool_guard.data_ptr(alloc).map_err(|err| {
                PyBufferError::new_err(format!(
                    "FastDB response final backing SHM access failed: {err}"
                ))
            })?;
            Ok((ptr, *len))
        }
    }
}

fn storage_len(storage: &ResponseBackingStorage) -> usize {
    match storage {
        ResponseBackingStorage::Inline(data) => data.len(),
        ResponseBackingStorage::Shm { len, .. } => *len,
    }
}

fn storage_kind(storage: &ResponseBackingStorage) -> &'static str {
    match storage {
        ResponseBackingStorage::Inline(_) => "inline",
        ResponseBackingStorage::Shm { .. } => "shm",
    }
}

fn response_meta_from_storage(
    storage: ResponseBackingStorage,
    used_size: usize,
) -> PyResult<ResponseMeta> {
    match storage {
        ResponseBackingStorage::Inline(mut data) => {
            data.truncate(used_size);
            if data.is_empty() {
                Ok(ResponseMeta::Empty)
            } else {
                Ok(ResponseMeta::Inline(data))
            }
        }
        ResponseBackingStorage::Shm { pool, alloc, .. } => {
            let data_size = u32::try_from(used_size).map_err(|_| {
                let mut pool_guard = pool.write();
                let _ = pool_guard.free(&alloc);
                PyValueError::new_err(format!(
                    "FastDB response final backing size {used_size} exceeds SHM wire limit {}",
                    u32::MAX
                ))
            })?;
            Ok(ResponseMeta::ShmAlloc {
                seg_idx: alloc.seg_idx as u16,
                offset: alloc.offset,
                data_size,
                is_dedicated: alloc.is_dedicated,
            })
        }
    }
}

fn drop_response_storage(storage: ResponseBackingStorage) {
    if let ResponseBackingStorage::Shm { pool, alloc, .. } = storage {
        let mut pool_guard = pool.write();
        let _ = pool_guard.free(&alloc);
    }
}

pub(crate) fn try_response_final_backing_meta(
    result: &Bound<'_, PyAny>,
) -> PyResult<Option<ResponseMeta>> {
    match result.cast::<PyResponseFinalBackingAllocation>() {
        Ok(allocation) => allocation.borrow().take_response_meta().map(Some),
        Err(_) => Ok(None),
    }
}

pub fn register_module(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    parent.add_class::<PyResponseFinalBackingAllocator>()?;
    parent.add_class::<PyResponseFinalBackingAllocation>()?;
    Ok(())
}
