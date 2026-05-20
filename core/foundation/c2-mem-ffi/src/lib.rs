//! C ABI substrate for foreign runtime adapters that need C-Two shared memory.

use c2_mem::{MemPool, PoolConfig};
use std::collections::HashSet;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;

const MAX_SHM_PREFIX_LEN: usize = 24;
const MAX_IPC_SHM_SEGMENTS: u16 = 16;
const C2_MEM_FFI_ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C2MemFfiStatus {
    Ok = 0,
    NullPointer = 1,
    InvalidArgument = 2,
    PoolError = 3,
    InsufficientBuffer = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct C2MemFfiRequestBlock {
    pub segment_index: u16,
    pub is_dedicated: u8,
    pub reserved: u8,
    pub offset: u32,
    pub byte_length: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct C2MemFfiResponseBlock {
    pub segment_index: u16,
    pub is_dedicated: u8,
    pub reserved: u8,
    pub offset: u32,
    pub byte_length: u32,
}

pub struct C2MemFfiRequestPool {
    inner: Mutex<C2MemFfiRequestPoolState>,
}

struct C2MemFfiRequestPoolState {
    pool: MemPool,
    local_blocks: HashSet<C2MemFfiBlockKey>,
}

pub struct C2MemFfiResponsePool {
    inner: Mutex<C2MemFfiResponsePoolState>,
}

struct C2MemFfiResponsePoolState {
    prefix: String,
    buddy_segment_size: usize,
    max_segments: usize,
    pool: MemPool,
    read_blocks: HashSet<C2MemFfiBlockKey>,
}

impl Drop for C2MemFfiResponsePool {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.lock() {
            let blocks: Vec<_> = state.read_blocks.drain().collect();
            for block in blocks {
                let _ = state.pool.free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct C2MemFfiBlockKey {
    segment_index: u16,
    offset: u32,
    byte_length: u32,
}

fn guard_status(action: impl FnOnce() -> Result<(), C2MemFfiStatus>) -> C2MemFfiStatus {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(())) => C2MemFfiStatus::Ok,
        Ok(Err(status)) => status,
        Err(_) => C2MemFfiStatus::PoolError,
    }
}

fn pool_ref<'a>(
    pool: *const C2MemFfiRequestPool,
) -> Result<&'a C2MemFfiRequestPool, C2MemFfiStatus> {
    if pool.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    Ok(unsafe { &*pool })
}

fn pool_mut<'a>(pool: *mut C2MemFfiRequestPool) -> Result<&'a C2MemFfiRequestPool, C2MemFfiStatus> {
    pool_ref(pool.cast_const())
}

fn response_pool_ref<'a>(
    pool: *const C2MemFfiResponsePool,
) -> Result<&'a C2MemFfiResponsePool, C2MemFfiStatus> {
    if pool.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    Ok(unsafe { &*pool })
}

fn response_pool_mut<'a>(
    pool: *mut C2MemFfiResponsePool,
) -> Result<&'a C2MemFfiResponsePool, C2MemFfiStatus> {
    response_pool_ref(pool.cast_const())
}

fn parse_prefix(prefix: *const c_char) -> Result<String, C2MemFfiStatus> {
    if prefix.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    let prefix = unsafe { CStr::from_ptr(prefix) }
        .to_str()
        .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
    if prefix.len() <= 1
        || !prefix.starts_with('/')
        || prefix[1..].contains('/')
        || prefix.len() > MAX_SHM_PREFIX_LEN
    {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(prefix.to_string())
}

fn request_pool_config(
    segment_size: u32,
    max_segments: u16,
    min_block_size: u32,
) -> Result<PoolConfig, C2MemFfiStatus> {
    if segment_size == 0
        || !(1..=MAX_IPC_SHM_SEGMENTS).contains(&max_segments)
        || min_block_size == 0
    {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    let config = PoolConfig {
        segment_size: segment_size as usize,
        min_block_size: min_block_size as usize,
        max_segments: max_segments as usize,
        max_dedicated_segments: 0,
        ..PoolConfig::default()
    };
    MemPool::validate_config(&config).map_err(|_| C2MemFfiStatus::InvalidArgument)?;
    Ok(config)
}

fn write_len(out_len: *mut usize, value: usize) -> Result<(), C2MemFfiStatus> {
    if out_len.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    unsafe {
        *out_len = value;
    }
    Ok(())
}

fn copy_c_string(
    value: &str,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> Result<(), C2MemFfiStatus> {
    if dst.is_null() || out_written.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    unsafe {
        *out_written = 0;
    }
    let needed = value.len() + 1;
    if dst_len < needed {
        return Err(C2MemFfiStatus::InsufficientBuffer);
    }
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr(), dst.cast::<u8>(), value.len());
        *dst.add(value.len()) = 0;
        *out_written = value.len();
    }
    Ok(())
}

fn validate_block(block: C2MemFfiRequestBlock) -> Result<(), C2MemFfiStatus> {
    if block.is_dedicated != 0 || block.byte_length == 0 {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(())
}

fn block_key(block: C2MemFfiRequestBlock) -> Result<C2MemFfiBlockKey, C2MemFfiStatus> {
    validate_block(block)?;
    Ok(C2MemFfiBlockKey {
        segment_index: block.segment_index,
        offset: block.offset,
        byte_length: block.byte_length,
    })
}

fn validate_response_block(block: C2MemFfiResponseBlock) -> Result<(), C2MemFfiStatus> {
    if block.is_dedicated != 0 || block.byte_length == 0 {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(())
}

fn response_block_key(block: C2MemFfiResponseBlock) -> Result<C2MemFfiBlockKey, C2MemFfiStatus> {
    validate_response_block(block)?;
    Ok(C2MemFfiBlockKey {
        segment_index: block.segment_index,
        offset: block.offset,
        byte_length: block.byte_length,
    })
}

fn buddy_segment_name(prefix: &str, idx: usize) -> String {
    format!("{prefix}_b{idx:04x}")
}

fn ensure_response_buddy_segment(
    state: &mut C2MemFfiResponsePoolState,
    segment_index: u16,
) -> Result<(), C2MemFfiStatus> {
    let target = segment_index as usize;
    if target >= state.max_segments {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    while state.pool.segment_count() <= target {
        let idx = state.pool.segment_count();
        let name = buddy_segment_name(&state.prefix, idx);
        state
            .pool
            .open_segment(&name, state.buddy_segment_size)
            .map_err(|_| C2MemFfiStatus::PoolError)?;
    }
    Ok(())
}

fn validate_response_range(
    state: &C2MemFfiResponsePoolState,
    block: C2MemFfiResponseBlock,
) -> Result<(), C2MemFfiStatus> {
    let (_, data_size) = state
        .pool
        .seg_data_info(block.segment_index as u32)
        .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
    let end = (block.offset as usize)
        .checked_add(block.byte_length as usize)
        .ok_or(C2MemFfiStatus::InvalidArgument)?;
    if end > data_size {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn c2_mem_ffi_abi_version() -> u32 {
    C2_MEM_FFI_ABI_VERSION
}

/// # Safety
///
/// `prefix` must point to a valid NUL-terminated C string, and `out_pool` must
/// be valid for writing one pool pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_new(
    prefix: *const c_char,
    segment_size: u32,
    max_segments: u16,
    min_block_size: u32,
    out_pool: *mut *mut C2MemFfiRequestPool,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out_pool.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_pool = ptr::null_mut();
        }
        let prefix = parse_prefix(prefix)?;
        let config = request_pool_config(segment_size, max_segments, min_block_size)?;
        let mut pool = MemPool::new_with_prefix(config, prefix);
        pool.ensure_buddy_segments(max_segments as usize)
            .map_err(|_| C2MemFfiStatus::PoolError)?;
        let handle = Box::new(C2MemFfiRequestPool {
            inner: Mutex::new(C2MemFfiRequestPoolState {
                pool,
                local_blocks: HashSet::new(),
            }),
        });
        unsafe {
            *out_pool = Box::into_raw(handle);
        }
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be null or a pointer returned by `c2_mem_ffi_request_pool_new`
/// that has not already been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_destroy(pool: *mut C2MemFfiRequestPool) {
    if !pool.is_null() {
        unsafe {
            drop(Box::from_raw(pool));
        }
    }
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer and `out_len` must be valid for
/// writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_prefix_len(
    pool: *const C2MemFfiRequestPool,
    out_len: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        write_len(out_len, state.pool.prefix().len())
    })
}

/// # Safety
///
/// `pool` must be valid, `dst` must be valid for `dst_len` bytes, and
/// `out_written` must be valid for writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_prefix_copy(
    pool: *const C2MemFfiRequestPool,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        copy_c_string(state.pool.prefix(), dst, dst_len, out_written)
    })
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer and `out_count` must be valid
/// for writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_segment_count(
    pool: *const C2MemFfiRequestPool,
    out_count: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        write_len(out_count, state.pool.segment_count())
    })
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer and `out_len` must be valid for
/// writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_segment_name_len(
    pool: *const C2MemFfiRequestPool,
    segment_index: usize,
    out_len: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        let name = state
            .pool
            .segment_name(segment_index)
            .ok_or(C2MemFfiStatus::InvalidArgument)?;
        write_len(out_len, name.len())
    })
}

/// # Safety
///
/// `pool` must be valid, `dst` must be valid for `dst_len` bytes, and
/// `out_written` must be valid for writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_segment_name_copy(
    pool: *const C2MemFfiRequestPool,
    segment_index: usize,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        let name = state
            .pool
            .segment_name(segment_index)
            .ok_or(C2MemFfiStatus::InvalidArgument)?;
        copy_c_string(name, dst, dst_len, out_written)
    })
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer and `out_size` must be valid
/// for writing one `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_segment_data_size(
    pool: *const C2MemFfiRequestPool,
    segment_index: usize,
    out_size: *mut u32,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out_size.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        let segment_index =
            u32::try_from(segment_index).map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        let (_, size) = state
            .pool
            .seg_data_info(segment_index)
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        let size = u32::try_from(size).map_err(|_| C2MemFfiStatus::PoolError)?;
        unsafe {
            *out_size = size;
        }
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be valid, `data` must be readable for `data_len` bytes, and
/// `out_block` must be valid for writing one block descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_write(
    pool: *mut C2MemFfiRequestPool,
    data: *const u8,
    data_len: usize,
    out_block: *mut C2MemFfiRequestBlock,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out_block.is_null() || data.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_block = C2MemFfiRequestBlock::default();
        }
        if data_len == 0 || data_len > u32::MAX as usize {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        let pool = pool_mut(pool)?;
        let mut state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        let alloc = state
            .pool
            .alloc(data_len)
            .map_err(|_| C2MemFfiStatus::PoolError)?;
        if alloc.is_dedicated {
            let _ = state.pool.free(&alloc);
            return Err(C2MemFfiStatus::PoolError);
        }
        let copy_result = state
            .pool
            .data_ptr(&alloc)
            .map_err(|_| C2MemFfiStatus::PoolError)
            .and_then(|ptr| {
                unsafe {
                    ptr::copy_nonoverlapping(data, ptr, data_len);
                }
                let segment_index =
                    u16::try_from(alloc.seg_idx).map_err(|_| C2MemFfiStatus::PoolError)?;
                unsafe {
                    *out_block = C2MemFfiRequestBlock {
                        segment_index,
                        is_dedicated: 0,
                        reserved: 0,
                        offset: alloc.offset,
                        byte_length: data_len as u32,
                    };
                }
                Ok(())
            });
        if copy_result.is_err() {
            let _ = state.pool.free(&alloc);
        } else {
            let segment_index =
                u16::try_from(alloc.seg_idx).map_err(|_| C2MemFfiStatus::PoolError)?;
            let inserted = state.local_blocks.insert(C2MemFfiBlockKey {
                segment_index,
                offset: alloc.offset,
                byte_length: data_len as u32,
            });
            if !inserted {
                let _ = state.pool.free(&alloc);
                return Err(C2MemFfiStatus::PoolError);
            }
        }
        copy_result
    })
}

/// # Safety
///
/// `pool` must be valid, `dst` must be valid for `dst_len` bytes, and
/// `out_read` must be valid for writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_read_local(
    pool: *const C2MemFfiRequestPool,
    block: C2MemFfiRequestBlock,
    dst: *mut u8,
    dst_len: usize,
    out_read: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        if dst.is_null() || out_read.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_read = 0;
        }
        let key = block_key(block)?;
        let data_len = block.byte_length as usize;
        if dst_len < data_len {
            return Err(C2MemFfiStatus::InsufficientBuffer);
        }
        let pool = pool_ref(pool)?;
        let state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        if !state.local_blocks.contains(&key) {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        let ptr = state
            .pool
            .data_ptr_at(block.segment_index as u32, block.offset, false)
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        unsafe {
            ptr::copy_nonoverlapping(ptr.cast_const(), dst, data_len);
            *out_read = data_len;
        }
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer. `block` must describe a live
/// non-dedicated allocation still owned by the local writer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_release(
    pool: *mut C2MemFfiRequestPool,
    block: C2MemFfiRequestBlock,
) -> C2MemFfiStatus {
    guard_status(|| {
        let key = block_key(block)?;
        let pool = pool_mut(pool)?;
        let mut state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        if !state.local_blocks.remove(&key) {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        if state
            .pool
            .free_at(
                block.segment_index as u32,
                block.offset,
                block.byte_length,
                false,
            )
            .is_err()
        {
            state.local_blocks.insert(key);
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be a valid request-pool pointer. `block` must describe a live
/// non-dedicated allocation that has been accepted by the Rust server, so local
/// writer release authority must be dropped without freeing the block locally.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_pool_forget_consumed(
    pool: *mut C2MemFfiRequestPool,
    block: C2MemFfiRequestBlock,
) -> C2MemFfiStatus {
    guard_status(|| {
        let key = block_key(block)?;
        let pool = pool_mut(pool)?;
        let mut state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        if !state.local_blocks.remove(&key) {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        Ok(())
    })
}

/// # Safety
///
/// `prefix` must point to a valid NUL-terminated C string, and `out_pool` must
/// be valid for writing one pool pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_pool_new(
    prefix: *const c_char,
    segment_size: u32,
    max_segments: u16,
    min_block_size: u32,
    out_pool: *mut *mut C2MemFfiResponsePool,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out_pool.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_pool = ptr::null_mut();
        }
        let prefix = parse_prefix(prefix)?;
        let config = request_pool_config(segment_size, max_segments, min_block_size)?;
        let pool = MemPool::new_with_prefix(config, prefix.clone());
        let handle = Box::new(C2MemFfiResponsePool {
            inner: Mutex::new(C2MemFfiResponsePoolState {
                prefix,
                buddy_segment_size: segment_size as usize,
                max_segments: max_segments as usize,
                pool,
                read_blocks: HashSet::new(),
            }),
        });
        unsafe {
            *out_pool = Box::into_raw(handle);
        }
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be null or a pointer returned by `c2_mem_ffi_response_pool_new`
/// that has not already been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_pool_destroy(pool: *mut C2MemFfiResponsePool) {
    if !pool.is_null() {
        unsafe {
            drop(Box::from_raw(pool));
        }
    }
}

/// # Safety
///
/// `pool` must be valid, `dst` must be valid for `dst_len` bytes, and
/// `out_read` must be valid for writing one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_pool_read(
    pool: *mut C2MemFfiResponsePool,
    block: C2MemFfiResponseBlock,
    dst: *mut u8,
    dst_len: usize,
    out_read: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        if dst.is_null() || out_read.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_read = 0;
        }
        let key = response_block_key(block)?;
        let data_len = block.byte_length as usize;
        if dst_len < data_len {
            return Err(C2MemFfiStatus::InsufficientBuffer);
        }
        let pool = response_pool_mut(pool)?;
        let mut state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        if state.read_blocks.contains(&key) {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        ensure_response_buddy_segment(&mut state, block.segment_index)?;
        validate_response_range(&state, block)?;
        let ptr = state
            .pool
            .data_ptr_at(block.segment_index as u32, block.offset, false)
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        unsafe {
            ptr::copy_nonoverlapping(ptr.cast_const(), dst, data_len);
            *out_read = data_len;
        }
        state.read_blocks.insert(key);
        Ok(())
    })
}

/// # Safety
///
/// `pool` must be a valid response-pool pointer. `block` must describe a valid
/// non-dedicated response block from this pool's server prefix that has not
/// already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_pool_release(
    pool: *mut C2MemFfiResponsePool,
    block: C2MemFfiResponseBlock,
) -> C2MemFfiStatus {
    guard_status(|| {
        let key = response_block_key(block)?;
        let pool = response_pool_mut(pool)?;
        let mut state = pool.inner.lock().map_err(|_| C2MemFfiStatus::PoolError)?;
        let was_read = state.read_blocks.remove(&key);
        let result = (|| {
            ensure_response_buddy_segment(&mut state, block.segment_index)?;
            validate_response_range(&state, block)?;
            if state
                .pool
                .free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err()
            {
                return Err(C2MemFfiStatus::InvalidArgument);
            }
            Ok(())
        })();
        if result.is_err() && was_read {
            state.read_blocks.insert(key);
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::fs;
    use std::process::Command;
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn test_prefix() -> CString {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        CString::new(format!("/cc2ffi{id:04x}")).unwrap()
    }

    struct PoolHandle(*mut C2MemFfiRequestPool);

    impl PoolHandle {
        fn new() -> Self {
            Self::new_with_config(65_536, 2, 4096)
        }

        fn new_with_config(segment_size: u32, max_segments: u16, min_block_size: u32) -> Self {
            let prefix = test_prefix();
            let mut pool = ptr::null_mut();
            let status = unsafe {
                c2_mem_ffi_request_pool_new(
                    prefix.as_ptr(),
                    segment_size,
                    max_segments,
                    min_block_size,
                    &mut pool,
                )
            };
            assert_eq!(status, C2MemFfiStatus::Ok);
            assert!(!pool.is_null());
            Self(pool)
        }
    }

    impl Drop for PoolHandle {
        fn drop(&mut self) {
            unsafe {
                c2_mem_ffi_request_pool_destroy(self.0);
            }
        }
    }

    fn copy_string(
        len_fn: impl FnOnce(*mut usize) -> C2MemFfiStatus,
        copy_fn: impl FnOnce(*mut c_char, usize, *mut usize) -> C2MemFfiStatus,
    ) -> String {
        let mut len = 0usize;
        assert_eq!(len_fn(&mut len), C2MemFfiStatus::Ok);
        let mut buf = vec![0_i8; len + 1];
        let mut written = 0usize;
        assert_eq!(
            copy_fn(buf.as_mut_ptr(), buf.len(), &mut written),
            C2MemFfiStatus::Ok
        );
        assert_eq!(written, len);
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn request_pool_advertises_handshake_metadata() {
        let handle = PoolHandle::new();
        let prefix = copy_string(
            |out| unsafe { c2_mem_ffi_request_pool_prefix_len(handle.0, out) },
            |dst, len, out| unsafe { c2_mem_ffi_request_pool_prefix_copy(handle.0, dst, len, out) },
        );
        assert!(prefix.starts_with("/cc2ffi"));

        let mut count = 0usize;
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_segment_count(handle.0, &mut count) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(count, 2);

        let name = copy_string(
            |out| unsafe { c2_mem_ffi_request_pool_segment_name_len(handle.0, 0, out) },
            |dst, len, out| unsafe {
                c2_mem_ffi_request_pool_segment_name_copy(handle.0, 0, dst, len, out)
            },
        );
        assert_eq!(name, format!("{prefix}_b0000"));

        let mut data_size = 0u32;
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_segment_data_size(handle.0, 0, &mut data_size) },
            C2MemFfiStatus::Ok
        );
        assert!(data_size >= 65_536);
    }

    #[test]
    fn request_pool_advertises_all_configured_segments_before_writes() {
        let prefix = test_prefix();
        let mut pool = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_new(prefix.as_ptr(), 65_536, 2, 4096, &mut pool) },
            C2MemFfiStatus::Ok
        );
        let handle = PoolHandle(pool);

        let mut count = 0usize;
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_segment_count(handle.0, &mut count) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(count, 2);

        let first_segment_size = {
            let mut data_size = 0u32;
            assert_eq!(
                unsafe { c2_mem_ffi_request_pool_segment_data_size(handle.0, 0, &mut data_size) },
                C2MemFfiStatus::Ok
            );
            data_size as usize
        };
        let first_payload = vec![1_u8; first_segment_size];
        let second_payload = b"second segment payload";
        let mut first = C2MemFfiRequestBlock::default();
        let mut second = C2MemFfiRequestBlock::default();

        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(
                    handle.0,
                    first_payload.as_ptr(),
                    first_payload.len(),
                    &mut first,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(
                    handle.0,
                    second_payload.as_ptr(),
                    second_payload.len(),
                    &mut second,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(second.segment_index, 1);

        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, second) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, first) },
            C2MemFfiStatus::Ok
        );
    }

    #[test]
    fn request_pool_never_allocates_unadvertised_segments() {
        let handle = PoolHandle::new_with_config(65_536, 2, 4096);
        let mut advertised_count = 0usize;
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_segment_count(handle.0, &mut advertised_count) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(advertised_count, 2);

        let full_segment = vec![1_u8; 65_536];
        let mut first = C2MemFfiRequestBlock::default();
        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(
                    handle.0,
                    full_segment.as_ptr(),
                    full_segment.len(),
                    &mut first,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(first.segment_index, 0);

        let payload = vec![2_u8; 4096];
        let mut second = C2MemFfiRequestBlock::default();
        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(
                    handle.0,
                    payload.as_ptr(),
                    payload.len(),
                    &mut second,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(second.segment_index, 1);
        assert!((second.segment_index as usize) < advertised_count);

        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, second) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, first) },
            C2MemFfiStatus::Ok
        );
    }

    #[test]
    fn request_pool_write_read_and_release_round_trip() {
        let handle = PoolHandle::new();
        let payload = b"native request payload";
        let mut block = C2MemFfiRequestBlock::default();

        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(handle.0, payload.as_ptr(), payload.len(), &mut block)
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(block.segment_index, 0);
        assert_eq!(block.is_dedicated, 0);
        assert_eq!(block.byte_length, payload.len() as u32);

        let mut out = vec![0_u8; payload.len()];
        let mut read = 0usize;
        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_read_local(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(read, payload.len());
        assert_eq!(out, payload);

        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
    }

    #[test]
    fn request_pool_forget_consumed_drops_local_release_authority() {
        let handle = PoolHandle::new();
        let payload = b"server consumed request";
        let mut block = C2MemFfiRequestBlock::default();

        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(handle.0, payload.as_ptr(), payload.len(), &mut block)
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_forget_consumed(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_release(handle.0, block) },
            C2MemFfiStatus::InvalidArgument
        );
    }

    #[test]
    fn request_pool_rejects_oversized_non_dedicated_payloads() {
        let handle = PoolHandle::new();
        let payload = vec![7_u8; 128 * 1024];
        let mut block = C2MemFfiRequestBlock::default();

        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_write(handle.0, payload.as_ptr(), payload.len(), &mut block)
            },
            C2MemFfiStatus::PoolError
        );
    }

    #[test]
    fn request_pool_rejects_short_copy_buffers() {
        let handle = PoolHandle::new();
        let mut len = 0usize;
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_segment_name_len(handle.0, 0, &mut len) },
            C2MemFfiStatus::Ok
        );
        let mut written = usize::MAX;
        let mut dst = vec![0_i8; len];

        assert_eq!(
            unsafe {
                c2_mem_ffi_request_pool_segment_name_copy(
                    handle.0,
                    0,
                    dst.as_mut_ptr(),
                    dst.len(),
                    &mut written,
                )
            },
            C2MemFfiStatus::InsufficientBuffer
        );
        assert_eq!(written, 0);
    }

    #[test]
    fn request_pool_rejects_invalid_prefixes() {
        for prefix in [
            CString::new("/").unwrap(),
            CString::new("cc2ffi_no_slash").unwrap(),
            CString::new("/cc2ffi/extra").unwrap(),
            CString::new("/cc2ffi_prefix_that_is_way_too_long").unwrap(),
        ] {
            let mut pool = ptr::null_mut();
            assert_eq!(
                unsafe { c2_mem_ffi_request_pool_new(prefix.as_ptr(), 65_536, 2, 4096, &mut pool) },
                C2MemFfiStatus::InvalidArgument
            );
            assert!(pool.is_null());
        }
    }

    #[test]
    fn request_pool_rejects_segment_counts_outside_ipc_wire_range() {
        let prefix = test_prefix();
        let mut pool = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_new(prefix.as_ptr(), 65_536, 17, 4096, &mut pool) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(pool.is_null());
        assert_eq!(
            unsafe { c2_mem_ffi_request_pool_new(prefix.as_ptr(), 65_536, 256, 4096, &mut pool) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(pool.is_null());
    }

    #[test]
    fn public_abi_version_is_exported() {
        assert_eq!(c2_mem_ffi_abi_version(), 1);
    }

    #[test]
    fn public_c_header_compiles_and_matches_block_layout() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let header = manifest_dir.join("include/c2_mem_ffi.h");
        assert!(
            header.exists(),
            "missing public C header: {}",
            header.display()
        );

        let source = std::env::temp_dir().join(format!(
            "c2_mem_ffi_header_check_{}.c",
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(
            &source,
            r#"
#include <stddef.h>
#include <stdint.h>
#include "c2_mem_ffi.h"

_Static_assert(C2_MEM_FFI_STATUS_OK == 0, "status ok value");
_Static_assert(C2_MEM_FFI_STATUS_INSUFFICIENT_BUFFER == 4, "status buffer value");
_Static_assert(C2_MEM_FFI_MAX_SHM_PREFIX_LEN == 24u, "prefix length limit");
_Static_assert(C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS == 16u, "segment count limit");
_Static_assert(C2_MEM_FFI_ABI_VERSION == 1u, "abi version");
_Static_assert(sizeof(C2MemFfiRequestBlock) == 12, "request block size");
_Static_assert(offsetof(C2MemFfiRequestBlock, segment_index) == 0, "request segment_index offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, is_dedicated) == 2, "request dedicated offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, offset) == 4, "request offset offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, byte_length) == 8, "request byte_length offset");
_Static_assert(sizeof(C2MemFfiResponseBlock) == 12, "response block size");
_Static_assert(offsetof(C2MemFfiResponseBlock, segment_index) == 0, "response segment_index offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, is_dedicated) == 2, "response dedicated offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, offset) == 4, "response offset offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, byte_length) == 8, "response byte_length offset");

static void use_request_api(void) {
    C2MemFfiRequestPool *pool = NULL;
    C2MemFfiRequestBlock block = {0};
    size_t count = 0;
    uint32_t size = 0;
    unsigned char byte = 0;
    uint32_t version = c2_mem_ffi_abi_version();
    (void)version;
    (void)c2_mem_ffi_request_pool_new("/cc2ffihead", 65536u, 1u, 4096u, &pool);
    (void)c2_mem_ffi_request_pool_prefix_len(pool, &count);
    (void)c2_mem_ffi_request_pool_segment_count(pool, &count);
    (void)c2_mem_ffi_request_pool_segment_name_len(pool, 0, &count);
    (void)c2_mem_ffi_request_pool_segment_data_size(pool, 0, &size);
    (void)c2_mem_ffi_request_pool_write(pool, &byte, 1, &block);
    (void)c2_mem_ffi_request_pool_read_local(pool, block, &byte, 1, &count);
    (void)c2_mem_ffi_request_pool_forget_consumed(pool, block);
    (void)c2_mem_ffi_request_pool_release(pool, block);
    c2_mem_ffi_request_pool_destroy(pool);
}

static void use_response_api(void) {
    C2MemFfiResponsePool *pool = NULL;
    C2MemFfiResponseBlock block = {0};
    size_t read = 0;
    unsigned char byte = 0;
    (void)c2_mem_ffi_response_pool_new("/cc2ffiresp", 65536u, 1u, 4096u, &pool);
    (void)c2_mem_ffi_response_pool_read(pool, block, &byte, 1, &read);
    (void)c2_mem_ffi_response_pool_release(pool, block);
    c2_mem_ffi_response_pool_destroy(pool);
}
"#,
        )
        .unwrap();

        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let output = Command::new(&compiler)
            .arg("-std=c11")
            .arg("-fsyntax-only")
            .arg("-I")
            .arg(manifest_dir.join("include"))
            .arg(&source)
            .output()
            .unwrap_or_else(|err| panic!("failed to run C compiler `{compiler}`: {err}"));
        assert!(
            output.status.success(),
            "public C header did not compile\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(source);
    }

    fn server_pool_with_payload(prefix: &str, payload: &[u8]) -> (MemPool, C2MemFfiResponseBlock) {
        let mut server = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 65_536,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 0,
                ..PoolConfig::default()
            },
            prefix.to_string(),
        );
        server.ensure_buddy_segments(2).unwrap();
        let alloc = server.alloc(payload.len()).unwrap();
        assert!(!alloc.is_dedicated);
        let ptr = server.data_ptr(&alloc).unwrap();
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
        }
        (
            server,
            C2MemFfiResponseBlock {
                segment_index: alloc.seg_idx as u16,
                is_dedicated: 0,
                reserved: 0,
                offset: alloc.offset,
                byte_length: payload.len() as u32,
            },
        )
    }

    struct ResponsePoolHandle(*mut C2MemFfiResponsePool);

    impl ResponsePoolHandle {
        fn new(prefix: &CString) -> Self {
            let mut pool = ptr::null_mut();
            let status = unsafe {
                c2_mem_ffi_response_pool_new(prefix.as_ptr(), 65_536, 2, 4096, &mut pool)
            };
            assert_eq!(status, C2MemFfiStatus::Ok);
            assert!(!pool.is_null());
            Self(pool)
        }
    }

    impl Drop for ResponsePoolHandle {
        fn drop(&mut self) {
            unsafe {
                c2_mem_ffi_response_pool_destroy(self.0);
            }
        }
    }

    #[test]
    fn response_pool_reads_and_releases_server_buddy_block() {
        let prefix = test_prefix();
        let payload = b"server response payload";
        let (mut server, block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        let handle = ResponsePoolHandle::new(&prefix);

        let mut out = vec![0_u8; payload.len()];
        let mut read = 0usize;
        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(read, payload.len());
        assert_eq!(out, payload);

        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        assert!(
            server
                .free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err()
        );
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::InvalidArgument
        );
    }

    #[test]
    fn response_pool_destroy_releases_unreleased_reads() {
        let prefix = test_prefix();
        let payload = b"drop releases response";
        let (mut server, block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);

        {
            let handle = ResponsePoolHandle::new(&prefix);
            let mut out = vec![0_u8; payload.len()];
            let mut read = 0usize;
            assert_eq!(
                unsafe {
                    c2_mem_ffi_response_pool_read(
                        handle.0,
                        block,
                        out.as_mut_ptr(),
                        out.len(),
                        &mut read,
                    )
                },
                C2MemFfiStatus::Ok
            );
            assert_eq!(read, payload.len());
            assert_eq!(out, payload);
        }

        assert!(
            server
                .free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err()
        );
    }

    #[test]
    fn response_pool_releases_server_buddy_block_without_read() {
        let prefix = test_prefix();
        let payload = b"unread response";
        let (mut server, block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        let handle = ResponsePoolHandle::new(&prefix);

        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        assert!(
            server
                .free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err()
        );
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::InvalidArgument
        );
    }

    #[test]
    fn response_pool_short_destination_can_release_unread_block() {
        let prefix = test_prefix();
        let payload = b"short destination response";
        let (mut server, block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        let handle = ResponsePoolHandle::new(&prefix);
        let mut out = vec![0_u8; payload.len() - 1];
        let mut read = usize::MAX;

        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::InsufficientBuffer
        );
        assert_eq!(read, 0);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        assert!(
            server
                .free_at(
                    block.segment_index as u32,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err()
        );
    }

    #[test]
    fn response_pool_rejects_dedicated_blocks() {
        let prefix = test_prefix();
        let payload = b"dedicated is not native buddy";
        let (_server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.is_dedicated = 1;
        let handle = ResponsePoolHandle::new(&prefix);
        let mut out = vec![0_u8; payload.len()];
        let mut read = usize::MAX;

        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::InvalidArgument
        );
        assert_eq!(read, 0);
    }

    #[test]
    fn response_pool_rejects_out_of_range_blocks_without_release_authority() {
        let prefix = test_prefix();
        let payload = b"range guarded response";
        let (_server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.offset = 65_536;
        let handle = ResponsePoolHandle::new(&prefix);
        let mut out = vec![0_u8; payload.len()];
        let mut read = usize::MAX;

        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::InvalidArgument
        );
        assert_eq!(read, 0);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::InvalidArgument
        );
    }

    #[test]
    fn response_pool_rejects_segment_indexes_beyond_configured_limit() {
        let prefix = test_prefix();
        let payload = b"segment guard response";
        let (_server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.segment_index = 2;
        let handle = ResponsePoolHandle::new(&prefix);
        let mut out = vec![0_u8; payload.len()];
        let mut read = usize::MAX;

        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::InvalidArgument
        );
        assert_eq!(read, 0);
    }

    #[test]
    fn response_pool_lazily_opens_unadvertised_deterministic_segments() {
        let prefix = test_prefix();
        let mut server = MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 65_536,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 0,
                ..PoolConfig::default()
            },
            prefix.to_str().unwrap().to_string(),
        );
        server.ensure_buddy_segments(2).unwrap();
        let full_segment = vec![1_u8; 65_536];
        let first = server.alloc(full_segment.len()).unwrap();
        assert_eq!(first.seg_idx, 0);
        let payload = b"second segment response";
        let second = server.alloc(payload.len()).unwrap();
        assert_eq!(second.seg_idx, 1);
        let ptr = server.data_ptr(&second).unwrap();
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
        }
        let block = C2MemFfiResponseBlock {
            segment_index: 1,
            is_dedicated: 0,
            reserved: 0,
            offset: second.offset,
            byte_length: payload.len() as u32,
        };
        let handle = ResponsePoolHandle::new(&prefix);
        let mut out = vec![0_u8; payload.len()];
        let mut read = 0usize;

        assert_eq!(
            unsafe {
                c2_mem_ffi_response_pool_read(
                    handle.0,
                    block,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut read,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(out, payload);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        server.free(&first).unwrap();
    }
}
