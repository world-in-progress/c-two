//! C ABI substrate for foreign runtime adapters that need C-Two shared memory.

use c2_config::{ConfigResolver, ConfigSources, LocalEndpointContext, LocalEndpointOptions};
use c2_mem::{MemPool, PoolConfig};
use std::collections::HashSet;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

const MAX_SHM_PREFIX_LEN: usize = 255;
const OWNER_INCARNATION_SUFFIX_LEN: usize = 41;
const MAX_IPC_SHM_SEGMENTS: u16 = 16;
const C2_MEM_FFI_ABI_VERSION: u32 = 3;

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
    pub generation: u32,
    pub offset: u32,
    pub byte_length: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct C2MemFfiResponseBlock {
    pub segment_index: u16,
    pub is_dedicated: u8,
    pub reserved: u8,
    pub generation: u32,
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

/// Owned immutable endpoint context. The C API exposes only an opaque pointer.
pub struct C2MemFfiLocalEndpointContext {
    inner: LocalEndpointContext,
}

struct C2MemFfiResponsePoolState {
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
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated,
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct C2MemFfiBlockKey {
    segment_index: u16,
    generation: u32,
    offset: u32,
    byte_length: u32,
    is_dedicated: bool,
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
    if block.is_dedicated > 1
        || block.byte_length == 0
        || (block.is_dedicated == 1 && (block.generation != 0 || block.offset != 0))
        || (block.is_dedicated == 0 && block.generation == 0)
    {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(())
}

fn block_key(block: C2MemFfiRequestBlock) -> Result<C2MemFfiBlockKey, C2MemFfiStatus> {
    validate_block(block)?;
    Ok(C2MemFfiBlockKey {
        segment_index: block.segment_index,
        generation: block.generation,
        offset: block.offset,
        byte_length: block.byte_length,
        is_dedicated: block.is_dedicated != 0,
    })
}

fn validate_response_block(block: C2MemFfiResponseBlock) -> Result<(), C2MemFfiStatus> {
    if block.is_dedicated > 1
        || block.byte_length == 0
        || (block.is_dedicated == 1 && (block.generation != 0 || block.offset != 0))
        || (block.is_dedicated == 0 && block.generation == 0)
    {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    Ok(())
}

fn response_block_key(block: C2MemFfiResponseBlock) -> Result<C2MemFfiBlockKey, C2MemFfiStatus> {
    validate_response_block(block)?;
    Ok(C2MemFfiBlockKey {
        segment_index: block.segment_index,
        generation: block.generation,
        offset: block.offset,
        byte_length: block.byte_length,
        is_dedicated: block.is_dedicated != 0,
    })
}

fn ensure_response_segment(
    state: &mut C2MemFfiResponsePoolState,
    block: C2MemFfiResponseBlock,
) -> Result<(), C2MemFfiStatus> {
    if block.is_dedicated != 0 {
        return state
            .pool
            .ensure_peer_dedicated(block.segment_index as u32, block.byte_length as usize)
            .map_err(|_| C2MemFfiStatus::PoolError);
    }
    if block.segment_index as usize >= state.max_segments {
        return Err(C2MemFfiStatus::InvalidArgument);
    }
    state
        .pool
        .ensure_peer_segment(
            block.segment_index as u32,
            block.generation,
            state.buddy_segment_size,
        )
        .map_err(|_| C2MemFfiStatus::PoolError)
}

fn validate_response_range(
    state: &C2MemFfiResponsePoolState,
    block: C2MemFfiResponseBlock,
) -> Result<(), C2MemFfiStatus> {
    state
        .pool
        .validate_data_at(
            block.segment_index as u32,
            block.generation,
            block.offset,
            block.byte_length,
            block.is_dedicated != 0,
        )
        .map_err(|_| C2MemFfiStatus::InvalidArgument)
}

#[unsafe(no_mangle)]
pub extern "C" fn c2_mem_ffi_abi_version() -> u32 {
    C2_MEM_FFI_ABI_VERSION
}

fn parse_utf8_c_string(value: *const c_char) -> Result<String, C2MemFfiStatus> {
    if value.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| C2MemFfiStatus::InvalidArgument)
}

fn capture_local_endpoint_context(
    unix_root: Option<PathBuf>,
) -> Result<LocalEndpointContext, C2MemFfiStatus> {
    // A code root fully selects this context; it must not read an irrelevant
    // env file or depend on unrelated process configuration. Inherited roots
    // capture the resolver's process/file/default sources exactly once.
    let sources = if unix_root.is_some() {
        ConfigSources::empty()
    } else {
        ConfigSources::from_process()
    };
    ConfigResolver::resolve_local_endpoint(LocalEndpointOptions { unix_root }, sources)
        .map_err(|_| C2MemFfiStatus::InvalidArgument)
}

fn endpoint_error_status(error: std::io::Error) -> C2MemFfiStatus {
    match error.kind() {
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            C2MemFfiStatus::InvalidArgument
        }
        _ => C2MemFfiStatus::PoolError,
    }
}

fn endpoint_name_in_context(
    context: &LocalEndpointContext,
    address: &str,
) -> Result<String, C2MemFfiStatus> {
    let endpoint = context.endpoint(address).map_err(endpoint_error_status)?;
    endpoint
        .os_name()
        .to_str()
        .map(str::to_owned)
        .ok_or(C2MemFfiStatus::InvalidArgument)
}

/// Resolve the process configuration for this one legacy operation.
fn local_endpoint_name(address: *const c_char) -> Result<String, C2MemFfiStatus> {
    let address = parse_utf8_c_string(address)?;
    endpoint_name_in_context(&capture_local_endpoint_context(None)?, &address)
}

/// Project the native local endpoint name without duplicating platform rules.
/// Each invocation resolves the current configuration independently. Use an
/// owned endpoint context for a stable two-operation length/copy query.
///
/// # Safety
/// `address` must be NUL-terminated and `out_len` valid for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_len(
    address: *const c_char,
    out_len: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| write_len(out_len, local_endpoint_name(address)?.len()))
}

/// # Safety
/// `address` must be NUL-terminated; `dst` and `out_written` must be writable for `dst_len`
/// bytes and one `usize`, respectively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_copy(
    address: *const c_char,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| copy_c_string(&local_endpoint_name(address)?, dst, dst_len, out_written))
}

/// Capture code/process/`.env`/platform endpoint configuration exactly once.
/// Deriving names from the returned context never creates endpoint directories.
///
/// # Safety
/// A non-null `unix_root` must be a valid NUL-terminated UTF-8 C string, and
/// `out_context` must be writable for one pointer without aliasing that string.
/// A successful context is owned by the caller and must be freed exactly once
/// after its last query.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_capture(
    unix_root: *const c_char,
    out_context: *mut *mut C2MemFfiLocalEndpointContext,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out_context.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out_context = ptr::null_mut();
        }
        let unix_root = if unix_root.is_null() {
            None
        } else {
            Some(PathBuf::from(parse_utf8_c_string(unix_root)?))
        };
        let inner = capture_local_endpoint_context(unix_root)?;
        unsafe {
            *out_context = Box::into_raw(Box::new(C2MemFfiLocalEndpointContext { inner }));
        }
        Ok(())
    })
}

/// # Safety
/// `context` must be null or a pointer returned by a successful capture that
/// has not been freed. No queries may still be using it when it is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_free(
    context: *mut C2MemFfiLocalEndpointContext,
) {
    if !context.is_null() {
        unsafe {
            drop(Box::from_raw(context));
        }
    }
}

/// # Safety
/// `context` must be a live captured context. `address` must be a valid
/// NUL-terminated UTF-8 C string, and `out_len` writable for one `usize` without
/// aliasing either input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_name_len(
    context: *const C2MemFfiLocalEndpointContext,
    address: *const c_char,
    out_len: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        write_len(out_len, 0)?;
        if context.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let address = parse_utf8_c_string(address)?;
        let name = endpoint_name_in_context(unsafe { &(*context).inner }, &address)?;
        write_len(out_len, name.len())
    })
}

/// # Safety
/// `context` must be a live captured context and `address` a valid
/// NUL-terminated UTF-8 C string. `dst` must be writable for `dst_len` bytes,
/// and `out_written` for one `usize`. Outputs must not alias each other or
/// the context/address inputs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_name_copy(
    context: *const C2MemFfiLocalEndpointContext,
    address: *const c_char,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        write_len(out_written, 0)?;
        if context.is_null() || dst.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let address = parse_utf8_c_string(address)?;
        let name = endpoint_name_in_context(unsafe { &(*context).inner }, &address)?;
        copy_c_string(&name, dst, dst_len, out_written)
    })
}

/// # Safety
/// `context` must be a live captured context and `out_len` writable for one
/// `usize`, without aliasing the context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_namespace_id_len(
    context: *const C2MemFfiLocalEndpointContext,
    out_len: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        write_len(out_len, 0)?;
        if context.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        write_len(out_len, unsafe { (*context).inner.namespace_id().len() })
    })
}

/// # Safety
/// `context` must be a live captured context. `dst` must be writable for
/// `dst_len` bytes, and `out_written` for one `usize`, without aliasing each
/// other or the context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_local_endpoint_context_namespace_id_copy(
    context: *const C2MemFfiLocalEndpointContext,
    dst: *mut c_char,
    dst_len: usize,
    out_written: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        write_len(out_written, 0)?;
        if context.is_null() || dst.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        copy_c_string(
            unsafe { (*context).inner.namespace_id() },
            dst,
            dst_len,
            out_written,
        )
    })
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
        if prefix.len() > MAX_SHM_PREFIX_LEN - OWNER_INCARNATION_SUFFIX_LEN {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
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
            .seg_data_info(
                segment_index,
                state
                    .pool
                    .segment_generation(segment_index as usize)
                    .ok_or(C2MemFfiStatus::InvalidArgument)?,
            )
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
                        is_dedicated: u8::from(alloc.is_dedicated),
                        reserved: 0,
                        generation: alloc.generation,
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
                generation: alloc.generation,
                offset: alloc.offset,
                byte_length: data_len as u32,
                is_dedicated: alloc.is_dedicated,
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
            .data_ptr_at(
                block.segment_index as u32,
                block.generation,
                block.offset,
                block.is_dedicated != 0,
            )
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
/// allocation still owned by the local writer.
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
                block.generation,
                block.offset,
                block.byte_length,
                block.is_dedicated != 0,
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
/// allocation accepted by the Rust server. Buddy ownership transfers to the
/// reader; a dedicated creator records completion and waits for read_done.
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
        if block.is_dedicated != 0 {
            if state
                .pool
                .free_at(
                    block.segment_index as u32,
                    block.generation,
                    block.offset,
                    block.byte_length,
                    true,
                )
                .is_err()
            {
                state.local_blocks.insert(key);
                return Err(C2MemFfiStatus::InvalidArgument);
            }
            state.pool.gc_dedicated();
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
        let pool = MemPool::open_peer(config, prefix);
        let handle = Box::new(C2MemFfiResponsePool {
            inner: Mutex::new(C2MemFfiResponsePoolState {
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
        ensure_response_segment(&mut state, block)?;
        validate_response_range(&state, block)?;
        let ptr = state
            .pool
            .data_ptr_at(
                block.segment_index as u32,
                block.generation,
                block.offset,
                block.is_dedicated != 0,
            )
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
/// response block from this pool's server prefix that has not
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
            ensure_response_segment(&mut state, block)?;
            validate_response_range(&state, block)?;
            if state
                .pool
                .free_at(
                    block.segment_index as u32,
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated != 0,
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
        if result.is_ok() {
            state.pool.gc_dedicated();
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
    static ENDPOINT_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct SavedEndpointEnv {
        root: Option<std::ffi::OsString>,
        env_file: Option<std::ffi::OsString>,
    }

    impl SavedEndpointEnv {
        fn new() -> Self {
            Self {
                root: std::env::var_os("C2_IPC_ROOT"),
                env_file: std::env::var_os("C2_ENV_FILE"),
            }
        }

        fn set_root(root: Option<&str>) {
            // Endpoint tests hold ENDPOINT_ENV_LOCK across every configuration
            // capture/read as well as mutation of these test-owned variables.
            unsafe {
                match root {
                    Some(root) => std::env::set_var("C2_IPC_ROOT", root),
                    None => std::env::remove_var("C2_IPC_ROOT"),
                }
            }
        }
    }

    impl Drop for SavedEndpointEnv {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in [
                    ("C2_IPC_ROOT", self.root.take()),
                    ("C2_ENV_FILE", self.env_file.take()),
                ] {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    struct EndpointContextHandle(*mut C2MemFfiLocalEndpointContext);

    impl EndpointContextHandle {
        fn capture(root: Option<&CStr>) -> Self {
            let mut context = ptr::null_mut();
            assert_eq!(
                unsafe {
                    c2_mem_ffi_local_endpoint_context_capture(
                        root.map_or(ptr::null(), CStr::as_ptr),
                        &mut context,
                    )
                },
                C2MemFfiStatus::Ok
            );
            assert!(!context.is_null());
            Self(context)
        }

        fn name(&self, address: &CStr) -> String {
            copy_string(
                |out| unsafe {
                    c2_mem_ffi_local_endpoint_context_name_len(self.0, address.as_ptr(), out)
                },
                |dst, len, out| unsafe {
                    c2_mem_ffi_local_endpoint_context_name_copy(
                        self.0,
                        address.as_ptr(),
                        dst,
                        len,
                        out,
                    )
                },
            )
        }

        fn namespace_id(&self) -> String {
            copy_string(
                |out| unsafe { c2_mem_ffi_local_endpoint_context_namespace_id_len(self.0, out) },
                |dst, len, out| unsafe {
                    c2_mem_ffi_local_endpoint_context_namespace_id_copy(self.0, dst, len, out)
                },
            )
        }
    }

    impl Drop for EndpointContextHandle {
        fn drop(&mut self) {
            unsafe {
                c2_mem_ffi_local_endpoint_context_free(self.0);
            }
        }
    }

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
    fn c_endpoint_projection_uses_the_same_platform_authority() {
        let _lock = ENDPOINT_ENV_LOCK.lock().unwrap();
        let address = CString::new("ipc://c-ffi-endpoint").unwrap();
        let context = ConfigResolver::resolve_local_endpoint(
            LocalEndpointOptions::default(),
            ConfigSources::from_process(),
        )
        .unwrap();
        let expected = context.endpoint(address.to_str().unwrap()).unwrap();
        let expected = expected.os_name().to_str().unwrap();
        let mut length = 0;
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_len(address.as_ptr(), &mut length) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(length, expected.len());
        let mut buffer = vec![0_i8; length + 1];
        let mut written = 0;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_copy(
                    address.as_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut written,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(written, length);
        assert_eq!(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap(),
            expected
        );
        let invalid = CString::new("tcp://not-ipc").unwrap();
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_len(invalid.as_ptr(), &mut length) },
            C2MemFfiStatus::InvalidArgument
        );
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_len(std::ptr::null(), &mut length) },
            C2MemFfiStatus::NullPointer
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_context_freezes_sources_and_keeps_resolution_pure() {
        let _lock = ENDPOINT_ENV_LOCK.lock().unwrap();
        let _saved_env = SavedEndpointEnv::new();
        unsafe {
            std::env::set_var("C2_ENV_FILE", "");
        }
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root_a = format!("/tmp/c2f{}a{id}", std::process::id());
        let root_b = format!("/tmp/c2f{}beta{id}", std::process::id());
        let root_code = format!("/tmp/c2f{}code{id}", std::process::id());
        for root in [&root_a, &root_b, &root_code] {
            assert!(!std::path::Path::new(root).exists());
        }
        let address = CString::new("ipc://frozen-ffi").unwrap();
        SavedEndpointEnv::set_root(Some(&root_a));
        let frozen = EndpointContextHandle::capture(None);
        let namespace_a = frozen.namespace_id();
        let mut length_a = 0;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_len(
                    frozen.0,
                    address.as_ptr(),
                    &mut length_a,
                )
            },
            C2MemFfiStatus::Ok
        );

        SavedEndpointEnv::set_root(Some(&root_b));
        let mut buffer = vec![0_i8; length_a + 1];
        let mut written = 0;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_copy(
                    frozen.0,
                    address.as_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut written,
                )
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(written, length_a);
        let name_a = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap();
        assert!(name_a.starts_with(&format!("{root_a}/")));
        assert_eq!(frozen.name(&address), name_a);
        assert_eq!(frozen.namespace_id(), namespace_a);
        let fresh = EndpointContextHandle::capture(None);
        assert!(fresh.name(&address).starts_with(&format!("{root_b}/")));
        assert_ne!(fresh.namespace_id(), namespace_a);
        let legacy_name = copy_string(
            |out| unsafe { c2_mem_ffi_local_endpoint_len(address.as_ptr(), out) },
            |dst, len, out| unsafe {
                c2_mem_ffi_local_endpoint_copy(address.as_ptr(), dst, len, out)
            },
        );
        assert_eq!(legacy_name, fresh.name(&address));

        let code_root = CString::new(root_code.as_str()).unwrap();
        let explicit = EndpointContextHandle::capture(Some(&code_root));
        assert!(
            explicit
                .name(&address)
                .starts_with(&format!("{root_code}/"))
        );
        assert_eq!(
            explicit.namespace_id(),
            LocalEndpointContext::with_unix_root(std::path::Path::new(&root_code))
                .unwrap()
                .namespace_id()
        );
        SavedEndpointEnv::set_root(Some(&root_code));
        let mut fresh_legacy_len = 0;
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_len(address.as_ptr(), &mut fresh_legacy_len) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(fresh_legacy_len, explicit.name(&address).len());

        // A root may be syntactically valid while its derived socket exceeds
        // the native path capacity. Derivation rejects it without creating it.
        let overlong_root = format!("/tmp/c2f{}{}", std::process::id(), "x".repeat(110));
        let root = CString::new(overlong_root.as_str()).unwrap();
        let overlong = EndpointContextHandle::capture(Some(&root));
        let mut failed_len = usize::MAX;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_len(
                    overlong.0,
                    address.as_ptr(),
                    &mut failed_len,
                )
            },
            C2MemFfiStatus::InvalidArgument
        );
        assert_eq!(failed_len, 0);
        assert!(!std::path::Path::new(&overlong_root).exists());

        for invalid_root in ["", "relative/root", "/tmp/../c2-invalid-root"] {
            let root = CString::new(invalid_root).unwrap();
            let mut failed = std::ptr::NonNull::dangling().as_ptr();
            assert_eq!(
                unsafe { c2_mem_ffi_local_endpoint_context_capture(root.as_ptr(), &mut failed) },
                C2MemFfiStatus::InvalidArgument
            );
            assert!(failed.is_null());
        }
        let invalid_utf8 = CStr::from_bytes_with_nul(b"/tmp/\xff\0").unwrap();
        let mut failed = ptr::null_mut();
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_capture(invalid_utf8.as_ptr(), &mut failed)
            },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(failed.is_null());
        SavedEndpointEnv::set_root(Some("relative/from-env"));
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_context_capture(ptr::null(), &mut failed) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(failed.is_null());
        let explicit_over_invalid_env = EndpointContextHandle::capture(Some(&code_root));
        assert_eq!(
            explicit_over_invalid_env.name(&address),
            explicit.name(&address)
        );

        // A directory cannot be loaded as an env file. Explicit-root capture
        // remains pure and succeeds without consulting it; inherited capture
        // must report the resolver's configuration error.
        SavedEndpointEnv::set_root(None);
        unsafe {
            std::env::set_var("C2_ENV_FILE", std::env::temp_dir());
        }
        let explicit_over_invalid_file = EndpointContextHandle::capture(Some(&code_root));
        assert_eq!(
            explicit_over_invalid_file.name(&address),
            explicit.name(&address)
        );
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_context_capture(ptr::null(), &mut failed) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(failed.is_null());

        let env_file = std::env::temp_dir().join(format!("c2f-{}-{id}.env", std::process::id()));
        fs::write(&env_file, format!("C2_IPC_ROOT={root_a}\n")).unwrap();
        SavedEndpointEnv::set_root(None);
        unsafe {
            std::env::set_var("C2_ENV_FILE", &env_file);
        }
        let from_file = EndpointContextHandle::capture(None);
        assert_eq!(from_file.name(&address), name_a);
        fs::write(&env_file, format!("C2_IPC_ROOT={root_b}\n")).unwrap();
        assert_eq!(from_file.name(&address), name_a);
        assert_eq!(from_file.namespace_id(), namespace_a);
        assert_eq!(
            EndpointContextHandle::capture(None).name(&address),
            fresh.name(&address)
        );
        fs::remove_file(env_file).unwrap();

        // Parsing a nonexistent but otherwise valid root succeeds. No bind,
        // directory probe, or directory creation is performed by this API.
        for root in [&root_a, &root_b, &root_code] {
            assert!(!std::path::Path::new(root).exists());
        }
        unsafe {
            c2_mem_ffi_local_endpoint_context_free(ptr::null_mut());
        }
    }

    #[cfg(windows)]
    #[test]
    fn endpoint_context_uses_current_sid_and_rejects_root_overrides() {
        let _lock = ENDPOINT_ENV_LOCK.lock().unwrap();
        let _saved_env = SavedEndpointEnv::new();
        unsafe {
            std::env::set_var("C2_ENV_FILE", "");
        }
        SavedEndpointEnv::set_root(None);
        let address = CString::new("ipc://ffi-current-sid").unwrap();
        let captured = EndpointContextHandle::capture(None);
        let expected = LocalEndpointContext::default_for_platform().unwrap();
        assert_eq!(captured.namespace_id(), expected.namespace_id());
        assert_eq!(
            captured.name(&address),
            expected
                .endpoint(address.to_str().unwrap())
                .unwrap()
                .os_name()
                .to_str()
                .unwrap()
        );
        assert!(captured.name(&address).starts_with(r"\\.\pipe\c_two-"));
        let root = CString::new(r"C:\arbitrary-pipe-root").unwrap();
        let mut failed = std::ptr::NonNull::dangling().as_ptr();
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_context_capture(root.as_ptr(), &mut failed) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(failed.is_null());
        SavedEndpointEnv::set_root(Some(r"C:\arbitrary-pipe-root"));
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_context_capture(ptr::null(), &mut failed) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(failed.is_null());
        assert_eq!(captured.namespace_id(), expected.namespace_id());
    }

    #[test]
    fn endpoint_context_query_failures_reset_outputs_and_preserve_buffers() {
        let _lock = ENDPOINT_ENV_LOCK.lock().unwrap();
        // Explicit Unix options remain independent of ambient C2_IPC_ROOT;
        // Windows uses its native SID-only default.
        let inner = LocalEndpointContext::default_for_platform().unwrap();
        let context =
            EndpointContextHandle(Box::into_raw(Box::new(C2MemFfiLocalEndpointContext {
                inner,
            })));
        let address = CString::new("ipc://ffi-output-shape").unwrap();
        let invalid = CString::new("tcp://not-ipc").unwrap();
        let invalid_utf8 = CStr::from_bytes_with_nul(b"ipc://\xff\0").unwrap();
        let mut len = usize::MAX;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_len(ptr::null(), address.as_ptr(), &mut len)
            },
            C2MemFfiStatus::NullPointer
        );
        assert_eq!(len, 0);
        for (input, status) in [
            (ptr::null(), C2MemFfiStatus::NullPointer),
            (invalid.as_ptr(), C2MemFfiStatus::InvalidArgument),
            (invalid_utf8.as_ptr(), C2MemFfiStatus::InvalidArgument),
        ] {
            len = usize::MAX;
            assert_eq!(
                unsafe { c2_mem_ffi_local_endpoint_context_name_len(context.0, input, &mut len) },
                status
            );
            assert_eq!(len, 0);
            let mut written = usize::MAX;
            let mut buffer = [42_i8; 256];
            assert_eq!(
                unsafe {
                    c2_mem_ffi_local_endpoint_context_name_copy(
                        context.0,
                        input,
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        &mut written,
                    )
                },
                status
            );
            assert_eq!(written, 0);
            assert_eq!(buffer, [42_i8; 256]);
        }
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_len(
                    context.0,
                    address.as_ptr(),
                    ptr::null_mut(),
                )
            },
            C2MemFfiStatus::NullPointer
        );
        let name = context.name(&address);
        let namespace = context.namespace_id();
        for short_size in [0, name.len()] {
            let mut buffer = vec![42_i8; name.len() + 1];
            let mut written = usize::MAX;
            assert_eq!(
                unsafe {
                    c2_mem_ffi_local_endpoint_context_name_copy(
                        context.0,
                        address.as_ptr(),
                        buffer.as_mut_ptr(),
                        short_size,
                        &mut written,
                    )
                },
                C2MemFfiStatus::InsufficientBuffer
            );
            assert_eq!(written, 0);
            assert!(buffer.iter().all(|byte| *byte == 42));
        }
        let mut written = usize::MAX;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_copy(
                    context.0,
                    address.as_ptr(),
                    ptr::null_mut(),
                    name.len() + 1,
                    &mut written,
                )
            },
            C2MemFfiStatus::NullPointer
        );
        assert_eq!(written, 0);
        let mut buffer = [42_i8; 256];
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_name_copy(
                    context.0,
                    address.as_ptr(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    ptr::null_mut(),
                )
            },
            C2MemFfiStatus::NullPointer
        );
        assert_eq!(buffer, [42_i8; 256]);
        for ctx in [ptr::null(), context.0.cast_const()] {
            len = usize::MAX;
            let status = if ctx.is_null() {
                C2MemFfiStatus::NullPointer
            } else {
                C2MemFfiStatus::Ok
            };
            assert_eq!(
                unsafe { c2_mem_ffi_local_endpoint_context_namespace_id_len(ctx, &mut len) },
                status
            );
            assert_eq!(len, if ctx.is_null() { 0 } else { namespace.len() });
        }
        written = usize::MAX;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_namespace_id_copy(
                    context.0,
                    buffer.as_mut_ptr(),
                    namespace.len(),
                    &mut written,
                )
            },
            C2MemFfiStatus::InsufficientBuffer
        );
        assert_eq!(written, 0);
        assert_eq!(buffer, [42_i8; 256]);
        written = usize::MAX;
        assert_eq!(
            unsafe {
                c2_mem_ffi_local_endpoint_context_namespace_id_copy(
                    ptr::null(),
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut written,
                )
            },
            C2MemFfiStatus::NullPointer
        );
        assert_eq!(written, 0);
        assert_eq!(buffer, [42_i8; 256]);
        assert_eq!(
            unsafe { c2_mem_ffi_local_endpoint_context_capture(ptr::null(), ptr::null_mut()) },
            C2MemFfiStatus::NullPointer
        );
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
        assert_eq!(name, MemPool::buddy_segment_name(&prefix, 0, 1));

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
    fn dedicated_completion_waits_for_reader_ack_and_reclaims_owner_mapping() {
        for release_by_drop in [false, true] {
            let owner = PoolHandle::new();
            let payload = vec![7_u8; 128 * 1024 + 3];
            let mut block = C2MemFfiRequestBlock::default();
            assert_eq!(
                unsafe {
                    c2_mem_ffi_request_pool_write(
                        owner.0,
                        payload.as_ptr(),
                        payload.len(),
                        &mut block,
                    )
                },
                C2MemFfiStatus::Ok
            );
            assert_eq!(block.is_dedicated, 1);
            assert_eq!(block.generation, 0);
            let prefix = copy_string(
                |out| unsafe { c2_mem_ffi_request_pool_prefix_len(owner.0, out) },
                |dst, len, out| unsafe {
                    c2_mem_ffi_request_pool_prefix_copy(owner.0, dst, len, out)
                },
            );
            assert_eq!(
                unsafe { c2_mem_ffi_request_pool_forget_consumed(owner.0, block) },
                C2MemFfiStatus::Ok
            );
            {
                let mut state = pool_ref(owner.0).unwrap().inner.lock().unwrap();
                state.pool.gc_dedicated();
                assert_eq!(
                    state.pool.stats().dedicated_segments,
                    1,
                    "creator must retain the unacknowledged mapping"
                );
            }
            let response = C2MemFfiResponseBlock {
                segment_index: block.segment_index,
                is_dedicated: 1,
                reserved: 0,
                generation: block.generation,
                offset: block.offset,
                byte_length: block.byte_length,
            };
            {
                let peer = ResponsePoolHandle::new(&CString::new(prefix).unwrap());
                let mut out = vec![0; payload.len()];
                let mut read = 0;
                assert_eq!(
                    unsafe {
                        c2_mem_ffi_response_pool_read(
                            peer.0,
                            response,
                            out.as_mut_ptr(),
                            out.len(),
                            &mut read,
                        )
                    },
                    C2MemFfiStatus::Ok
                );
                assert_eq!(out, payload);
                assert_eq!(read, payload.len());
                if !release_by_drop {
                    assert_eq!(
                        unsafe { c2_mem_ffi_response_pool_release(peer.0, response) },
                        C2MemFfiStatus::Ok
                    );
                    assert_ne!(
                        unsafe { c2_mem_ffi_response_pool_release(peer.0, response) },
                        C2MemFfiStatus::Ok
                    );
                    assert_ne!(
                        unsafe {
                            c2_mem_ffi_response_pool_read(
                                peer.0,
                                response,
                                out.as_mut_ptr(),
                                out.len(),
                                &mut read,
                            )
                        },
                        C2MemFfiStatus::Ok
                    );
                }
            }
            let mut state = pool_ref(owner.0).unwrap().inner.lock().unwrap();
            state.pool.gc_dedicated();
            assert_eq!(
                state.pool.stats().dedicated_segments,
                0,
                "read_done must make the creator mapping reclaimable"
            );
        }
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
            CString::new(format!(
                "/{}",
                "x".repeat(MAX_SHM_PREFIX_LEN - OWNER_INCARNATION_SUFFIX_LEN)
            ))
            .unwrap(),
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
        assert_eq!(c2_mem_ffi_abi_version(), 3);
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
_Static_assert(C2_MEM_FFI_MAX_SHM_PREFIX_LEN == 255u, "prefix length limit");
_Static_assert(C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS == 16u, "segment count limit");
_Static_assert(C2_MEM_FFI_ABI_VERSION == 3u, "abi version");
_Static_assert(sizeof(C2MemFfiRequestBlock) == 16, "request block size");
_Static_assert(offsetof(C2MemFfiRequestBlock, segment_index) == 0, "request segment_index offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, is_dedicated) == 2, "request dedicated offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, generation) == 4, "generation offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, offset) == 8, "request offset offset");
_Static_assert(offsetof(C2MemFfiRequestBlock, byte_length) == 12, "request byte_length offset");
_Static_assert(sizeof(C2MemFfiResponseBlock) == 16, "response block size");
_Static_assert(offsetof(C2MemFfiResponseBlock, segment_index) == 0, "response segment_index offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, is_dedicated) == 2, "response dedicated offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, generation) == 4, "generation offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, offset) == 8, "response offset offset");
_Static_assert(offsetof(C2MemFfiResponseBlock, byte_length) == 12, "response byte_length offset");

static void use_endpoint_api(void) {
    C2MemFfiLocalEndpointContext *context = NULL;
    size_t length = 0;
    char name[256];
    (void)c2_mem_ffi_local_endpoint_len("ipc://header-check", &length);
    (void)c2_mem_ffi_local_endpoint_copy("ipc://header-check", name, sizeof(name), &length);
    (void)c2_mem_ffi_local_endpoint_context_capture(NULL, &context);
    (void)c2_mem_ffi_local_endpoint_context_name_len(context, "ipc://header-check", &length);
    (void)c2_mem_ffi_local_endpoint_context_name_copy(context, "ipc://header-check", name, sizeof(name), &length);
    (void)c2_mem_ffi_local_endpoint_context_namespace_id_len(context, &length);
    (void)c2_mem_ffi_local_endpoint_context_namespace_id_copy(context, name, sizeof(name), &length);
    c2_mem_ffi_local_endpoint_context_free(context);
}

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

        let compiler = std::env::var("CC").unwrap_or_else(|_| {
            if cfg!(target_env = "msvc") {
                "cl.exe"
            } else {
                "cc"
            }
            .to_string()
        });
        let mut command = Command::new(&compiler);
        if std::path::Path::new(&compiler)
            .file_stem()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("cl"))
        {
            command.args(["/nologo", "/std:c11", "/Zs", "/I"]);
        } else {
            command.args(["-std=c11", "-fsyntax-only", "-I"]);
        }
        let output = command
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
                generation: alloc.generation,
                offset: alloc.offset,
                byte_length: payload.len() as u32,
            },
        )
    }

    /// Owner pool that creates its buddy backing only on the first allocation,
    /// like a lazy IPC server pool whose handshake advertised no segments.
    fn lazy_server_pool(prefix: &str, segment_size: usize) -> MemPool {
        MemPool::new_with_prefix(
            PoolConfig {
                segment_size,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 0,
                min_retained_segments: 0,
                buddy_idle_decay_secs: -1.0,
                ..PoolConfig::default()
            },
            prefix.to_string(),
        )
    }

    /// Allocate and fill one buddy block, returning its response coordinates.
    fn lazy_server_block(server: &mut MemPool, payload: &[u8]) -> C2MemFfiResponseBlock {
        let alloc = server.alloc(payload.len()).unwrap();
        assert!(!alloc.is_dedicated);
        let ptr = server.data_ptr(&alloc).unwrap();
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), ptr, payload.len());
        }
        C2MemFfiResponseBlock {
            segment_index: alloc.seg_idx as u16,
            is_dedicated: 0,
            reserved: 0,
            generation: alloc.generation,
            offset: alloc.offset,
            byte_length: payload.len() as u32,
        }
    }

    fn read_response_block(
        pool: *mut C2MemFfiResponsePool,
        block: C2MemFfiResponseBlock,
        out: &mut [u8],
    ) -> (C2MemFfiStatus, usize) {
        let mut read = usize::MAX;
        let status = unsafe {
            c2_mem_ffi_response_pool_read(pool, block, out.as_mut_ptr(), out.len(), &mut read)
        };
        (status, read)
    }

    struct ResponsePoolHandle(*mut C2MemFfiResponsePool);

    impl ResponsePoolHandle {
        fn new(prefix: &CString) -> Self {
            Self::with_bootstrap_config(prefix, 65_536, 2, 4096)
        }

        /// Create a peer pool whose configured capacity is only a bootstrap
        /// floor for lazily opened backings.
        fn with_bootstrap_config(
            prefix: &CString,
            segment_size: u32,
            max_segments: u16,
            min_block_size: u32,
        ) -> Self {
            let mut pool = ptr::null_mut();
            let status = unsafe {
                c2_mem_ffi_response_pool_new(
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
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());

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
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated != 0,
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
            let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated != 0,
                )
                .is_err()
        );
    }

    #[test]
    fn response_pool_releases_server_buddy_block_without_read() {
        let prefix = test_prefix();
        let payload = b"unread response";
        let (mut server, block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());

        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
        assert!(
            server
                .free_at(
                    block.segment_index as u32,
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated != 0,
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
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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
                    block.generation,
                    block.offset,
                    block.byte_length,
                    block.is_dedicated != 0,
                )
                .is_err()
        );
    }

    #[test]
    fn response_pool_rejects_dedicated_blocks_with_buddy_generation() {
        let prefix = test_prefix();
        let payload = b"dedicated generation must be zero";
        let (server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.is_dedicated = 1;
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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
        let (server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.offset = 65_536;
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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
        let (server, mut block) = server_pool_with_payload(prefix.to_str().unwrap(), payload);
        block.segment_index = 2;
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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
            generation: second.generation,
            offset: second.offset,
            byte_length: payload.len() as u32,
        };
        let handle = ResponsePoolHandle::new(&CString::new(server.prefix()).unwrap());
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

    #[test]
    fn response_pool_bootstrap_floor_reads_and_releases_late_server_segment() {
        let prefix = test_prefix();
        let payload = b"late lazy server response";
        // The reader's configured capacity is the minimum legal peer geometry,
        // while the server lazily creates a much larger backing after the
        // handshake advertised none. Only the mapped backing is authoritative.
        let bootstrap_floor = 2 * 4096;
        let mut server = lazy_server_pool(prefix.to_str().unwrap(), 1 << 20);
        assert_eq!(
            server.stats().total_segments,
            0,
            "lazy owner must not pre-create a backing"
        );
        let block = lazy_server_block(&mut server, payload);
        assert_eq!(block.segment_index, 0);
        assert_eq!(block.generation, 1);
        assert!(
            server.stats().buddy_data_bytes > bootstrap_floor as u64,
            "server backing must differ from the reader's bootstrap floor"
        );
        let server_prefix = CString::new(server.prefix()).unwrap();

        let handle =
            ResponsePoolHandle::with_bootstrap_config(&server_prefix, bootstrap_floor, 1, 4096);
        {
            let state = response_pool_ref(handle.0).unwrap().inner.lock().unwrap();
            assert_eq!(state.buddy_segment_size, bootstrap_floor as usize);
        }
        let mut out = vec![0_u8; payload.len()];
        let (status, read) = read_response_block(handle.0, block, &mut out);
        assert_eq!(status, C2MemFfiStatus::Ok);
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
                    block.generation,
                    block.offset,
                    block.byte_length,
                    false,
                )
                .is_err(),
            "peer release must reach the owner allocator"
        );
    }

    #[test]
    fn response_pool_rejects_unbacked_generation_before_late_open() {
        let prefix = test_prefix();
        let payload = b"generation gated late open";
        let mut server = lazy_server_pool(prefix.to_str().unwrap(), 1 << 20);
        let current = lazy_server_block(&mut server, payload);
        let server_prefix = CString::new(server.prefix()).unwrap();
        let handle = ResponsePoolHandle::with_bootstrap_config(&server_prefix, 2 * 4096, 1, 4096);

        let mut future = current;
        future.generation = current.generation + 1;
        let mut out = vec![0_u8; payload.len()];
        let (status, read) = read_response_block(handle.0, future, &mut out);
        assert_eq!(
            status,
            C2MemFfiStatus::PoolError,
            "an unbacked generation must never be fabricated"
        );
        assert_eq!(read, 0);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, future) },
            C2MemFfiStatus::PoolError
        );

        // The rejected generation must not poison the real coordinates.
        let (status, read) = read_response_block(handle.0, current, &mut out);
        assert_eq!(status, C2MemFfiStatus::Ok);
        assert_eq!(read, payload.len());
        assert_eq!(out, payload);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, current) },
            C2MemFfiStatus::Ok
        );
    }

    #[test]
    fn response_pool_reopens_late_generation_after_owner_retires_backing() {
        let prefix = test_prefix();
        let payload = b"retired and recreated backing";
        let mut server = lazy_server_pool(prefix.to_str().unwrap(), 1 << 20);
        let first = lazy_server_block(&mut server, payload);
        let server_prefix = CString::new(server.prefix()).unwrap();
        let handle = ResponsePoolHandle::with_bootstrap_config(&server_prefix, 2 * 4096, 1, 4096);

        let mut out = vec![0_u8; payload.len()];
        let (status, read) = read_response_block(handle.0, first, &mut out);
        assert_eq!(status, C2MemFfiStatus::Ok);
        assert_eq!(read, payload.len());
        assert_eq!(out, payload);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, first) },
            C2MemFfiStatus::Ok
        );

        assert_eq!(server.gc_buddy(), 1, "idle owner backing must retire");
        assert_eq!(server.stats().total_segments, 0);
        let second = lazy_server_block(&mut server, payload);
        assert_eq!(second.segment_index, 0);
        assert_eq!(
            second.generation,
            first.generation + 1,
            "a recreated slot must carry a fresh generation"
        );

        out.fill(0);
        let (status, read) = read_response_block(handle.0, second, &mut out);
        assert_eq!(
            status,
            C2MemFfiStatus::Ok,
            "the peer must lazy-open the recreated backing by generation"
        );
        assert_eq!(read, payload.len());
        assert_eq!(out, payload);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, second) },
            C2MemFfiStatus::Ok
        );
    }

    #[test]
    fn response_pool_rejects_out_of_range_bootstrap_geometry() {
        let prefix = test_prefix();
        let payload = b"range guarded lazy response";
        let mut server = lazy_server_pool(prefix.to_str().unwrap(), 1 << 20);
        let block = lazy_server_block(&mut server, payload);
        let data_size = server.stats().buddy_data_bytes as u32;
        let server_prefix = CString::new(server.prefix()).unwrap();
        let handle = ResponsePoolHandle::with_bootstrap_config(&server_prefix, 2 * 4096, 1, 4096);

        let mut oversized = block;
        oversized.offset = data_size;
        oversized.byte_length = 4096;
        let mut out = vec![0_u8; oversized.byte_length as usize];
        let (status, read) = read_response_block(handle.0, oversized, &mut out);
        assert_eq!(status, C2MemFfiStatus::InvalidArgument);
        assert_eq!(read, 0);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, oversized) },
            C2MemFfiStatus::InvalidArgument
        );

        let mut actual = vec![0_u8; payload.len()];
        let (status, read) = read_response_block(handle.0, block, &mut actual);
        assert_eq!(status, C2MemFfiStatus::Ok);
        assert_eq!(read, payload.len());
        assert_eq!(actual, payload);
        assert_eq!(
            unsafe { c2_mem_ffi_response_pool_release(handle.0, block) },
            C2MemFfiStatus::Ok
        );
    }
}
