//! ABI 7: one memory context for one verified physical connection. No I/O loop.
use super::*;
use c2_mem::{MemPool, PoolConfig};
use c2_wire::connection::{
    ConnectionCore, ConnectionSide, PreparedLoan, RemoteLoan, WriteBatch, WriteRequest,
};
use c2_wire::frame::decode_frame;
use parking_lot::RwLock;
use std::sync::Arc;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct C2MemFfiBlock {
    pub segment_index: u16,
    pub is_dedicated: u8,
    pub reserved: u8,
    pub generation: u32,
    pub offset: u32,
    pub byte_length: u32,
    pub loan_id: u64,
}

#[cfg(unix)]
type ConnectionHandle = std::os::fd::OwnedFd;
#[cfg(windows)]
struct ConnectionHandle {
    _stream: std::os::windows::io::OwnedHandle,
    _process: std::os::windows::io::OwnedHandle,
}

pub struct C2MemFfiMemoryContext {
    core: ConnectionCore,
    pool: Arc<RwLock<MemPool>>,
    peer: RwLock<Option<PeerMappings>>,
    connection: Arc<ConnectionHandle>,
}

impl Drop for C2MemFfiMemoryContext {
    fn drop(&mut self) {
        // Closing the driver is deactivation, never a fabricated peer end.
        // Admitted responses and in-progress writes keep their original core.
        drop(self.core.deactivate());
    }
}

struct PeerMappings {
    prefix: String,
    // Each mapping owner is immutable after admission. A new generation may
    // replace the cache entry, but cannot unmap an outstanding read's backing.
    slots: std::collections::HashMap<(u16, bool), (u32, Arc<MemPool>)>,
}

/// Unpublished allocation authority. Coordinates alone cannot cancel a loan.
pub struct C2MemFfiRequest {
    loan: PreparedLoan,
}

/// Admitted remote read. Consuming or discarding it settles the same node.
pub struct C2MemFfiResponse {
    _mapping: Arc<MemPool>,
    _loan: RemoteLoan,
    pointer: usize,
    len: usize,
}

pub struct C2MemFfiWrite {
    batch: WriteBatch,
}

fn context<'a>(
    value: *const C2MemFfiMemoryContext,
) -> Result<&'a C2MemFfiMemoryContext, C2MemFfiStatus> {
    if value.is_null() {
        return Err(C2MemFfiStatus::NullPointer);
    }
    Ok(unsafe { &*value })
}

/// # Safety
/// `connection` is a live OS socket/pipe already verified by c2-local, borrowed
/// throughout this call. On Windows it is a HANDLE, never a foreign CRT fd;
/// `process` is its verified server process pin. Unix requires process=0.
/// `out` points to writable pointer storage. The context duplicates the OS
/// reference; neither closing Node's socket nor reconnecting can redirect it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_new(
    connection: usize,
    process: usize,
    out: *mut *mut C2MemFfiMemoryContext,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = ptr::null_mut();
        }
        #[cfg(unix)]
        let connection = {
            use std::os::fd::{BorrowedFd, RawFd};
            if process != 0 {
                return Err(C2MemFfiStatus::InvalidArgument);
            }
            let fd = RawFd::try_from(connection).map_err(|_| C2MemFfiStatus::InvalidArgument)?;
            unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()
        };
        #[cfg(windows)]
        let connection = {
            use std::os::windows::io::BorrowedHandle;
            if connection == 0 || connection == usize::MAX || process == 0 || process == usize::MAX
            {
                return Err(C2MemFfiStatus::InvalidArgument);
            }
            let stream =
                unsafe { BorrowedHandle::borrow_raw(connection as *mut _) }.try_clone_to_owned();
            stream.and_then(|stream| {
                unsafe { BorrowedHandle::borrow_raw(process as *mut _) }
                    .try_clone_to_owned()
                    .map(|process| ConnectionHandle {
                        _stream: stream,
                        _process: process,
                    })
            })
        };
        let connection = connection.map_err(|_| C2MemFfiStatus::IoError)?;
        let config = c2_config::ClientIpcConfig::default()
            .primary_pool_config(&c2_config::PoolRoleTuning::default());
        let pool = MemPool::new(config).map_err(|_| C2MemFfiStatus::PoolError)?;
        let value = C2MemFfiMemoryContext {
            core: ConnectionCore::new(ConnectionSide::Client, || {}),
            pool: Arc::new(RwLock::new(pool)),
            peer: RwLock::new(None),
            connection: Arc::new(connection),
        };
        unsafe {
            *out = Box::into_raw(Box::new(value));
        }
        Ok(())
    })
}

/// # Safety
/// `value` is null or a live context, freed once after all driver operations.
/// Unconfirmed loans are never reclaimed into a surviving allocator on Drop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_free(value: *mut C2MemFfiMemoryContext) {
    if !value.is_null() {
        drop(unsafe { Box::from_raw(value) });
    }
}

/// # Safety
/// Context and output storage must be live. Counts exclude the final NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_prefix_len(
    value: *const C2MemFfiMemoryContext,
    out: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| write_len(out, context(value)?.pool.read().prefix().len()))
}
/// # Safety
/// `dst` is writable for `len` bytes; output storage must not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_prefix_copy(
    value: *const C2MemFfiMemoryContext,
    dst: *mut c_char,
    len: usize,
    out: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| copy_c_string(context(value)?.pool.read().prefix(), dst, len, out))
}

/// # Safety
/// Call only after the verified connection's final metadata ACK. `prefix` is
/// its validated SHM response prefix, or NULL when responses use owned bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_finish_handshake(
    value: *const C2MemFfiMemoryContext,
    prefix: *const c_char,
) -> C2MemFfiStatus {
    guard_status(|| {
        let value = context(value)?;
        let peer = if prefix.is_null() {
            None
        } else {
            let prefix = parse_prefix(prefix)?;
            Some(PeerMappings {
                prefix,
                slots: Default::default(),
            })
        };
        value
            .core
            .finish_handshake()
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
        *value.peer.write() = peer;
        Ok(())
    })
}

/// # Safety
/// Payload is readable for `len`; outputs are writable and do not overlap.
/// The returned opaque owner must be discarded or moved to write_begin once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_prepare(
    value: *const C2MemFfiMemoryContext,
    data: *const u8,
    len: usize,
    out: *mut *mut C2MemFfiRequest,
    block: *mut C2MemFfiBlock,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out.is_null() || block.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = ptr::null_mut();
            *block = C2MemFfiBlock::default();
        }
        if data.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        if len == 0 || len > u32::MAX as usize {
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        let value = context(value)?;
        let mut pool = value.pool.write();
        let mut handle = pool
            .alloc_shm_handle(len)
            .map_err(|_| C2MemFfiStatus::PoolError)?;
        let alloc = handle.shm_allocation().expect("SHM allocation");
        let pointer = pool.handle_slice_mut(&mut handle).as_mut_ptr();
        unsafe {
            ptr::copy_nonoverlapping(data, pointer, len);
        }
        drop(pool);
        let loan = value
            .core
            .prepare_loan(value.pool.clone(), handle)
            .map_err(|_| C2MemFfiStatus::PoolError)?;
        let descriptor = C2MemFfiBlock {
            segment_index: alloc.seg_idx as u16,
            is_dedicated: u8::from(alloc.is_dedicated),
            reserved: 0,
            generation: alloc.generation,
            offset: alloc.offset,
            byte_length: len as u32,
            loan_id: loan.id(),
        };
        unsafe {
            *block = descriptor;
            *out = Box::into_raw(Box::new(C2MemFfiRequest { loan }));
        }
        Ok(())
    })
}

/// # Safety
/// `request` is null or an unpublished owner, discarded exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_request_discard(request: *mut C2MemFfiRequest) {
    if !request.is_null() {
        drop(unsafe { Box::from_raw(request) });
    }
}

/// # Safety
/// `block` is received exactly once on this connection after metadata ACK.
/// This admits a node before mapping: failed admission/mapping also settles it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_admit(
    value: *const C2MemFfiMemoryContext,
    block: C2MemFfiBlock,
    out: *mut *mut C2MemFfiResponse,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = ptr::null_mut();
        }
        let value = context(value)?;
        let result = (|| {
            if block.reserved != 0
                || block.is_dedicated > 1
                || block.generation == 0
                || block.byte_length == 0
                || block.loan_id == 0
                || (block.is_dedicated != 0 && block.offset != 0)
            {
                return Err(C2MemFfiStatus::InvalidArgument);
            }
            let mut peers = value.peer.write();
            let peer = peers.as_mut().ok_or(C2MemFfiStatus::InvalidArgument)?;
            // Admission owns the connection and process pin even if mapping fails.
            let loan = value
                .core
                .admit_remote_loan(block.loan_id, Box::new(value.connection.clone()))
                .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
            let key = (block.segment_index, block.is_dedicated != 0);
            if peer
                .slots
                .get(&key)
                .is_none_or(|(generation, _)| *generation != block.generation)
            {
                let config = PoolConfig {
                    segment_size: 8192,
                    max_segments: c2_config::MAX_IPC_POOL_SEGMENTS as usize,
                    ..PoolConfig::default()
                };
                let mut pool = MemPool::open_peer(config, peer.prefix.clone())
                    .map_err(|_| C2MemFfiStatus::PoolError)?;
                if block.is_dedicated != 0 {
                    pool.ensure_peer_dedicated(
                        block.segment_index.into(),
                        block.generation,
                        block.byte_length as usize,
                    )
                } else {
                    pool.ensure_peer_segment(block.segment_index.into(), block.generation, 1)
                }
                .map_err(|_| C2MemFfiStatus::PoolError)?;
                peer.slots.insert(key, (block.generation, Arc::new(pool)));
            }
            let pool = peer.slots[&key].1.clone();
            pool.validate_data_at(
                block.segment_index.into(),
                block.generation,
                block.offset,
                block.byte_length,
                block.is_dedicated != 0,
            )
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?;
            let pointer = pool
                .data_ptr_at(
                    block.segment_index.into(),
                    block.generation,
                    block.offset,
                    block.is_dedicated != 0,
                )
                .expect("admitted span");
            drop(peers);
            unsafe {
                *out = Box::into_raw(Box::new(C2MemFfiResponse {
                    _mapping: pool,
                    _loan: loan,
                    pointer: pointer as usize,
                    len: block.byte_length as usize,
                }));
            }
            Ok(())
        })();
        if result.is_err() {
            drop(value.core.deactivate());
        }
        result
    })
}

/// # Safety
/// Consumes a live response exactly once, even if dst/out is null or too short.
/// Copy ends before RemoteLoan Drop queues release. No remote writes occur.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_consume(
    response: *mut C2MemFfiResponse,
    dst: *mut u8,
    len: usize,
    out: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        if response.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let response = unsafe { Box::from_raw(response) };
        if out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = 0;
        }
        if len < response.len {
            return Err(C2MemFfiStatus::InsufficientBuffer);
        }
        if dst.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            ptr::copy_nonoverlapping(response.pointer as *const u8, dst, response.len);
            *out = response.len;
        }
        drop(response);
        Ok(())
    })
}
/// # Safety
/// Consumes an unread response once, including on caller allocation failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_response_discard(response: *mut C2MemFfiResponse) {
    if !response.is_null() {
        drop(unsafe { Box::from_raw(response) });
    }
}

/// # Safety
/// `bytes` is one complete received release frame, readable for len bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_receive_release(
    value: *const C2MemFfiMemoryContext,
    bytes: *const u8,
    len: usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        let value = context(value)?;
        if bytes.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        let (header, payload) = decode_frame(bytes).map_err(|_| {
            drop(value.core.deactivate());
            C2MemFfiStatus::InvalidArgument
        })?;
        if header.total_len as usize + 4 != len {
            drop(value.core.deactivate());
            return Err(C2MemFfiStatus::InvalidArgument);
        }
        let result = value.core.receive_release(&header, payload);
        drop(result.reclaims);
        result
            .error
            .map_or(Ok(()), |_| Err(C2MemFfiStatus::InvalidArgument))
    })
}

/// # Safety
/// Consumes request (if non-null) on every path. kind: 0 business, 1 control,
/// 2 releases only. A null write means no releases pending for kind 2.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_begin(
    value: *const C2MemFfiMemoryContext,
    request: *mut C2MemFfiRequest,
    kind: u8,
    out: *mut *mut C2MemFfiWrite,
) -> C2MemFfiStatus {
    guard_status(|| {
        let request = if request.is_null() {
            None
        } else {
            Some(unsafe { Box::from_raw(request) })
        };
        if out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = ptr::null_mut();
        }
        let value = context(value)?;
        let action = match (kind, request) {
            (0, request) => WriteRequest::Business(request.into_iter().map(|r| r.loan).collect()),
            (1, None) => WriteRequest::Control(
                value
                    .core
                    .prepare_control_frame()
                    .map_err(|_| C2MemFfiStatus::InvalidArgument)?,
            ),
            (2, None) => WriteRequest::Releases,
            _ => return Err(C2MemFfiStatus::InvalidArgument),
        };
        if let Some(batch) = value
            .core
            .begin_write(action)
            .map_err(|_| C2MemFfiStatus::InvalidArgument)?
        {
            unsafe {
                *out = Box::into_raw(Box::new(C2MemFfiWrite { batch }));
            }
        }
        Ok(())
    })
}
/// # Safety
/// `write` is live; length excludes the driver-owned business/control frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_prefix_len(
    write: *const C2MemFfiWrite,
    out: *mut usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        if write.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        write_len(out, unsafe { &*write }.batch.release_bytes().len())
    })
}
/// # Safety
/// `dst` is writable for len bytes. Empty prefixes allow NULL dst.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_prefix_copy(
    write: *const C2MemFfiWrite,
    dst: *mut u8,
    len: usize,
) -> C2MemFfiStatus {
    guard_status(|| {
        if write.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let bytes = unsafe { &*write }.batch.release_bytes();
        if len < bytes.len() {
            return Err(C2MemFfiStatus::InsufficientBuffer);
        }
        if !bytes.is_empty() {
            if dst.is_null() {
                return Err(C2MemFfiStatus::NullPointer);
            }
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            }
        }
        Ok(())
    })
}
/// # Safety
/// Call just before submitting the first byte, exactly once on a live write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_start(write: *mut C2MemFfiWrite) -> C2MemFfiStatus {
    guard_status(|| {
        if write.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe { &mut *write }
            .batch
            .begin_io()
            .map_err(|_| C2MemFfiStatus::InvalidArgument)
    })
}
/// # Safety
/// Consumes write once. success=1 means all frames and flush completed; 0 is
/// failed/uncertain I/O. Never replay a failed prefix or reclaim its loans.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_complete(
    write: *mut C2MemFfiWrite,
    success: u8,
) -> C2MemFfiStatus {
    guard_status(|| {
        if write.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        let mut write = unsafe { Box::from_raw(write) };
        if success == 1 {
            for _ in 0..write.batch.frame_count() {
                write.batch.frame_completed();
            }
            write.batch.flush_completed();
        } else {
            write.batch.write_failed();
        }
        Ok(())
    })
}
/// # Safety
/// Cancels an unstarted write once; Drop restores an unsent release prefix.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_write_discard(write: *mut C2MemFfiWrite) {
    if !write.is_null() {
        drop(unsafe { Box::from_raw(write) });
    }
}
/// # Safety
/// `value` is live. This is deactivation, never proof of peer end.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_deactivate(
    value: *const C2MemFfiMemoryContext,
) -> C2MemFfiStatus {
    guard_status(|| {
        drop(context(value)?.core.deactivate());
        Ok(())
    })
}
/// # Safety
/// Call only on read EOF, or Unix read ECONNRESET; never on local close/write
/// failure/deadline/DISCONNECT. Incoming carriers retain their actual OS guard.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_peer_end(
    value: *const C2MemFfiMemoryContext,
) -> C2MemFfiStatus {
    guard_status(|| {
        drop(context(value)?.core.peer_end());
        Ok(())
    })
}
/// # Safety
/// Context and output storage are live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c2_mem_ffi_memory_context_try_close(
    value: *const C2MemFfiMemoryContext,
    out: *mut u8,
) -> C2MemFfiStatus {
    guard_status(|| {
        if out.is_null() {
            return Err(C2MemFfiStatus::NullPointer);
        }
        unsafe {
            *out = u8::from(context(value)?.core.try_close());
        }
        Ok(())
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use c2_wire::flags;
    use c2_wire::frame::decode_frame;
    use c2_wire::release::{ReleaseBatch, decode_release_frame, encode_release_frame};
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    struct Context(*mut C2MemFfiMemoryContext, UnixStream);
    impl Context {
        fn new() -> Self {
            let (stream, peer) = UnixStream::pair().unwrap();
            let mut context = ptr::null_mut();
            assert_eq!(
                unsafe {
                    c2_mem_ffi_memory_context_new(stream.as_raw_fd() as usize, 0, &mut context)
                },
                C2MemFfiStatus::Ok
            );
            drop(stream);
            Self(context, peer)
        }
        fn value(&self) -> &C2MemFfiMemoryContext {
            unsafe { &*self.0 }
        }
        fn handshake(&self, prefix: Option<&str>) {
            let prefix = prefix.map(|s| CString::new(s).unwrap());
            assert_eq!(
                unsafe {
                    c2_mem_ffi_memory_context_finish_handshake(
                        self.0,
                        prefix.as_ref().map_or(ptr::null(), |p| p.as_ptr()),
                    )
                },
                C2MemFfiStatus::Ok
            );
        }
        fn prepare(&self, data: &[u8]) -> (*mut C2MemFfiRequest, C2MemFfiBlock) {
            let (mut owner, mut block) = (ptr::null_mut(), C2MemFfiBlock::default());
            assert_eq!(
                unsafe {
                    c2_mem_ffi_request_prepare(
                        self.0,
                        data.as_ptr(),
                        data.len(),
                        &mut owner,
                        &mut block,
                    )
                },
                C2MemFfiStatus::Ok
            );
            (owner, block)
        }
        fn write(&self, request: *mut C2MemFfiRequest, kind: u8, success: bool) -> Vec<u8> {
            let mut write = ptr::null_mut();
            assert_eq!(
                unsafe { c2_mem_ffi_write_begin(self.0, request, kind, &mut write) },
                C2MemFfiStatus::Ok
            );
            if write.is_null() {
                return Vec::new();
            }
            let mut len = 0;
            assert_eq!(
                unsafe { c2_mem_ffi_write_prefix_len(write, &mut len) },
                C2MemFfiStatus::Ok
            );
            let mut prefix = vec![0; len];
            assert_eq!(
                unsafe { c2_mem_ffi_write_prefix_copy(write, prefix.as_mut_ptr(), len) },
                C2MemFfiStatus::Ok
            );
            assert_eq!(unsafe { c2_mem_ffi_write_start(write) }, C2MemFfiStatus::Ok);
            assert_eq!(
                unsafe { c2_mem_ffi_write_complete(write, u8::from(success)) },
                C2MemFfiStatus::Ok
            );
            prefix
        }
        fn receive(&self, bytes: &[u8]) -> C2MemFfiStatus {
            unsafe {
                c2_mem_ffi_memory_context_receive_release(self.0, bytes.as_ptr(), bytes.len())
            }
        }
    }
    impl Drop for Context {
        fn drop(&mut self) {
            unsafe {
                c2_mem_ffi_memory_context_free(self.0);
            }
        }
    }
    fn release_frame(id: u64, response: bool) -> Vec<u8> {
        encode_release_frame(&ReleaseBatch::new(&[id]).unwrap(), response)
    }
    fn prefix_ids(bytes: &[u8]) -> Vec<u64> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let (header, payload) = decode_frame(bytes).unwrap();
        assert_eq!(header.flags, flags::FLAG_CTRL);
        decode_release_frame(&header, payload, false, true)
            .unwrap()
            .ids()
            .to_vec()
    }
    fn response(
        context: &Context,
        len: usize,
    ) -> (Arc<RwLock<MemPool>>, c2_mem::MemHandle, C2MemFfiBlock) {
        let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default()).unwrap()));
        context.handshake(Some(pool.read().prefix()));
        let mut handle = pool.write().alloc_shm_handle(len).unwrap();
        pool.read().handle_slice_mut(&mut handle).fill(42);
        let a = handle.shm_allocation().unwrap();
        let block = C2MemFfiBlock {
            segment_index: a.seg_idx as u16,
            generation: a.generation,
            offset: a.offset,
            byte_length: len as u32,
            is_dedicated: u8::from(a.is_dedicated),
            reserved: 0,
            loan_id: (1u64 << 60) + 37,
        };
        (pool, handle, block)
    }
    fn admit(context: &Context, block: C2MemFfiBlock) -> *mut C2MemFfiResponse {
        let mut owner = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_response_admit(context.0, block, &mut owner) },
            C2MemFfiStatus::Ok
        );
        owner
    }

    #[test]
    fn peer_prefix_rejects_invalid_shape_length_and_utf8_before_handshake() {
        let cases = [
            Vec::new(),
            b"/".to_vec(),
            b"a".to_vec(),
            b"/a/b".to_vec(),
            [b"/".as_slice(), &[b'a'; 255]].concat(),
            vec![b'/', 0xff],
        ];
        for bytes in cases {
            let context = Context::new();
            let prefix = CString::new(bytes).unwrap();
            assert_eq!(
                parse_prefix(prefix.as_ptr()),
                Err(C2MemFfiStatus::InvalidArgument)
            );
            assert_eq!(
                unsafe { c2_mem_ffi_memory_context_finish_handshake(context.0, prefix.as_ptr()) },
                C2MemFfiStatus::InvalidArgument
            );
            context.handshake(None);
        }
    }
    #[test]
    fn malformed_dedicated_coordinates_deactivate_without_mapping_or_release() {
        for (dedicated, offset, reserved) in [(1, 1, 0), (2, 0, 0), (1, 0, 1)] {
            let context = Context::new();
            context.handshake(Some("/p8a_unopened"));
            let block = C2MemFfiBlock {
                is_dedicated: dedicated,
                offset,
                reserved,
                generation: 1,
                byte_length: 8,
                loan_id: 41,
                ..Default::default()
            };
            let mut owner = ptr::null_mut();
            assert_eq!(
                unsafe { c2_mem_ffi_response_admit(context.0, block, &mut owner) },
                C2MemFfiStatus::InvalidArgument
            );
            assert!(owner.is_null());
            assert!(context.value().core.is_deactivated());
            assert!(
                context
                    .value()
                    .peer
                    .read()
                    .as_ref()
                    .unwrap()
                    .slots
                    .is_empty()
            );
            assert!(context.write(ptr::null_mut(), 2, true).is_empty());
        }
    }
    #[test]
    fn missing_mapping_deactivates_and_queues_release_once() {
        let context = Context::new();
        context.handshake(Some(&format!("/p8a_absent_{}", std::process::id())));
        let block = C2MemFfiBlock {
            generation: 1,
            byte_length: 8,
            loan_id: 41,
            ..Default::default()
        };
        let mut owner = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_response_admit(context.0, block, &mut owner) },
            C2MemFfiStatus::PoolError
        );
        assert!(owner.is_null());
        assert!(context.value().core.is_deactivated());
        assert_eq!(prefix_ids(&context.write(ptr::null_mut(), 2, true)), [41]);
        assert!(context.write(ptr::null_mut(), 2, true).is_empty());
    }
    #[test]
    fn mapped_out_of_bounds_span_deactivates_without_reading_and_releases_once() {
        let context = Context::new();
        let (pool, handle, mut block) = response(&context, 8);
        block.offset = u32::MAX;
        let mut owner = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_response_admit(context.0, block, &mut owner) },
            C2MemFfiStatus::InvalidArgument
        );
        assert!(owner.is_null());
        assert!(
            context
                .value()
                .peer
                .read()
                .as_ref()
                .unwrap()
                .slots
                .contains_key(&(block.segment_index, false)),
            "the real mapping must succeed before the span fails"
        );
        assert!(context.value().core.is_deactivated());
        assert_eq!(
            prefix_ids(&context.write(ptr::null_mut(), 2, true)),
            [block.loan_id]
        );
        assert!(context.write(ptr::null_mut(), 2, true).is_empty());
        assert_eq!(pool.read().stats().alloc_count, 1);
        pool.write().release_handle(handle);
    }

    #[test]
    fn opaque_unpublished_owner_cancels_without_coordinate_release() {
        let context = Context::new();
        context.handshake(None);
        let (request, block) = context.prepare(&[1, 2, 3]);
        assert!(block.generation > 0 && block.loan_id > 0);
        unsafe {
            c2_mem_ffi_request_discard(request);
        }
        assert_eq!(context.value().pool.read().stats().alloc_count, 0);
    }
    #[test]
    fn received_release_settles_published_owner_once() {
        let context = Context::new();
        context.handshake(None);
        let (request, block) = context.prepare(&[7; 8]);
        context.write(request, 0, true);
        assert_eq!(context.value().pool.read().stats().alloc_count, 1);
        let frame = release_frame(block.loan_id, true);
        assert_eq!(context.receive(&frame), C2MemFfiStatus::Ok);
        assert_eq!(context.value().pool.read().stats().alloc_count, 0);
        assert_eq!(context.receive(&frame), C2MemFfiStatus::InvalidArgument);
    }
    #[test]
    fn write_failure_keeps_owner_until_read_side_peer_end() {
        let context = Context::new();
        context.handshake(None);
        let (request, _) = context.prepare(&[7; 8]);
        context.write(request, 0, false);
        assert_eq!(context.value().pool.read().stats().alloc_count, 1);
        unsafe {
            c2_mem_ffi_memory_context_deactivate(context.0);
        }
        assert!(!context.value().core.try_close());
        assert_eq!(
            unsafe { c2_mem_ffi_memory_context_peer_end(context.0) },
            C2MemFfiStatus::Ok
        );
        assert_eq!(context.value().pool.read().stats().alloc_count, 0);
        assert!(context.value().core.try_close());
    }
    #[test]
    fn consume_copies_ends_remote_read_and_queues_bigint_release_immediately() {
        let context = Context::new();
        let (pool, handle, block) = response(&context, 12);
        let owner = admit(&context, block);
        let mut bytes = [0; 12];
        let mut read = 0;
        assert_eq!(
            unsafe {
                c2_mem_ffi_response_consume(owner, bytes.as_mut_ptr(), bytes.len(), &mut read)
            },
            C2MemFfiStatus::Ok
        );
        assert_eq!(bytes, [42; 12]);
        assert_eq!(read, 12);
        // No next call or SDK hold release is needed to get the control prefix.
        assert_eq!(
            prefix_ids(&context.write(ptr::null_mut(), 2, true)),
            vec![block.loan_id]
        );
        assert_eq!(pool.read().stats().alloc_count, 1); // peer cannot mutate allocator
        pool.write().release_handle(handle);
        assert!(context.write(ptr::null_mut(), 2, true).is_empty());
    }
    #[test]
    fn short_copy_and_unread_discard_each_settle_one_response() {
        for discard in [false, true] {
            let context = Context::new();
            let (pool, handle, block) = response(&context, 9);
            let owner = admit(&context, block);
            if discard {
                unsafe {
                    c2_mem_ffi_response_discard(owner);
                }
            } else {
                let mut dst = [0; 1];
                let mut read = 99;
                assert_eq!(
                    unsafe {
                        c2_mem_ffi_response_consume(owner, dst.as_mut_ptr(), dst.len(), &mut read)
                    },
                    C2MemFfiStatus::InsufficientBuffer
                );
                assert_eq!(read, 0);
            }
            assert_eq!(
                prefix_ids(&context.write(ptr::null_mut(), 2, true)),
                vec![block.loan_id]
            );
            pool.write().release_handle(handle);
        }
    }
    #[test]
    fn failed_mapping_settles_the_admitted_loan() {
        let context = Context::new();
        let (pool, handle, mut block) = response(&context, 9);
        block.generation += 1;
        let mut owner = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_response_admit(context.0, block, &mut owner) },
            C2MemFfiStatus::PoolError
        );
        assert!(owner.is_null());
        assert!(context.value().core.is_deactivated());
        assert_eq!(
            prefix_ids(&context.write(ptr::null_mut(), 2, true)),
            vec![block.loan_id]
        );
        assert!(context.write(ptr::null_mut(), 2, true).is_empty());
        pool.write().release_handle(handle);
    }
    #[test]
    fn remote_read_keeps_original_os_connection_after_context_drop() {
        use std::io::{ErrorKind, Read};
        let context = Context::new();
        let (pool, handle, block) = response(&context, 9);
        let response = admit(&context, block);
        let mut peer = context.1.try_clone().unwrap();
        peer.set_nonblocking(true).unwrap();
        drop(context);
        assert_eq!(
            peer.read(&mut [0]).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        unsafe {
            c2_mem_ffi_response_discard(response);
        }
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        pool.write().release_handle(handle);
    }
    #[test]
    fn lazy_buddy_and_dedicated_generations_consume_without_segment_advertisements() {
        for size in [4096, 16384] {
            let context = Context::new();
            let config = PoolConfig {
                segment_size: 8192,
                max_segments: 1,
                max_dedicated_segments: 1,
                min_retained_segments: 0,
                buddy_idle_decay_secs: 0.0,
                spill_threshold: 1.0,
                ..PoolConfig::default()
            };
            let mut pool = MemPool::new(config).unwrap();
            context.handshake(Some(pool.prefix()));
            let mut previous_generation = 0;
            for value in 1..4u8 {
                let mut handle = pool.alloc_shm_handle(size).unwrap();
                pool.handle_slice_mut(&mut handle).fill(value);
                let a = handle.shm_allocation().unwrap();
                assert!(a.generation > previous_generation);
                previous_generation = a.generation;
                let block = C2MemFfiBlock {
                    segment_index: a.seg_idx as u16,
                    generation: a.generation,
                    offset: a.offset,
                    byte_length: size as u32,
                    is_dedicated: u8::from(a.is_dedicated),
                    reserved: 0,
                    loan_id: u64::MAX - u64::from(value),
                };
                let response = admit(&context, block);
                let mut bytes = vec![0; size];
                let mut read = 0;
                assert_eq!(
                    unsafe {
                        c2_mem_ffi_response_consume(response, bytes.as_mut_ptr(), size, &mut read)
                    },
                    C2MemFfiStatus::Ok
                );
                assert!(bytes.iter().all(|byte| *byte == value));
                assert_eq!(
                    prefix_ids(&context.write(ptr::null_mut(), 2, true)),
                    [block.loan_id]
                );
                pool.release_handle(handle);
                pool.gc_buddy();
            }
        }
    }

    #[test]
    fn consuming_copy_boundary_settles_success_failure_and_discard_without_shm() {
        // Exercise the actual C consume/discard functions with an owned test
        // backing. Native SHM mapping is covered separately, never substituted.
        for mode in 0..4 {
            let context = Context::new();
            context.handshake(None);
            let bytes = Arc::new(vec![11u8; 8]);
            let pointer = bytes.as_ptr() as usize;
            let loan = context
                .value()
                .core
                .admit_remote_loan(
                    0xfffffffffffffff1,
                    Box::new((bytes, context.value().connection.clone())),
                )
                .unwrap();
            let response = Box::into_raw(Box::new(C2MemFfiResponse {
                _mapping: Arc::new(MemPool::new(PoolConfig::default()).unwrap()),
                _loan: loan,
                pointer,
                len: 8,
            }));
            let mut dst = [0; 8];
            let mut read = 99;
            let status = match mode {
                0 => unsafe {
                    c2_mem_ffi_response_consume(response, dst.as_mut_ptr(), dst.len(), &mut read)
                },
                1 => unsafe {
                    c2_mem_ffi_response_consume(response, dst.as_mut_ptr(), 1, &mut read)
                },
                2 => unsafe {
                    c2_mem_ffi_response_consume(response, ptr::null_mut(), 8, &mut read)
                },
                _ => {
                    unsafe {
                        c2_mem_ffi_response_discard(response);
                    }
                    C2MemFfiStatus::Ok
                }
            };
            assert_eq!(
                status,
                [
                    C2MemFfiStatus::Ok,
                    C2MemFfiStatus::InsufficientBuffer,
                    C2MemFfiStatus::NullPointer,
                    C2MemFfiStatus::Ok
                ][mode]
            );
            if mode == 0 {
                assert_eq!(dst, [11; 8]);
                assert_eq!(read, 8);
            }
            assert_eq!(
                prefix_ids(&context.write(ptr::null_mut(), 2, true)),
                [0xfffffffffffffff1]
            );
            assert!(context.write(ptr::null_mut(), 2, true).is_empty());
        }
    }

    #[test]
    fn unstarted_write_discard_ends_the_original_control_reservation() {
        let context = Context::new();
        context.handshake(None);
        let mut write = ptr::null_mut();
        assert_eq!(
            unsafe { c2_mem_ffi_write_begin(context.0, ptr::null_mut(), 1, &mut write) },
            C2MemFfiStatus::Ok
        );
        assert!(!write.is_null());
        unsafe {
            c2_mem_ffi_memory_context_deactivate(context.0);
        }
        assert!(!context.value().core.try_close());
        unsafe {
            c2_mem_ffi_write_discard(write);
        }
        assert!(context.value().core.try_close());
    }

    #[test]
    fn c_header_matches_abi7_block_layout_and_compiles() {
        use std::process::Command;
        assert_eq!(std::mem::size_of::<C2MemFfiBlock>(), 24);
        let header = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include");
        let output = Command::new("cc").args(["-std=c11", "-x", "c", "-fsyntax-only", "-I"])
            .arg(header).arg("-").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn().map(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(b"#include <c2_mem_ffi.h>\n_Static_assert(C2_MEM_FFI_ABI_VERSION == 7, \"ABI\");\n_Static_assert(sizeof(C2MemFfiBlock) == 24, \"layout\");\n_Static_assert(offsetof(C2MemFfiBlock, loan_id) == 16, \"loan\");\n").unwrap();
                child.wait_with_output().unwrap()
            }).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
