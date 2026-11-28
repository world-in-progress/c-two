use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use c2_core::memory::LocalCopyGuard;
use c2_core::{
    EncodedResponse, HeldResponse, LocalCopyDirection, LocalCopyMeta, LocalCopyTracker,
    ResponseSink,
};
use c2_mem::{RecycledBytes, WritableSlab};
use jni::JNIEnv;
use jni::errors::Error as JniError;
use jni::objects::{JClass, JObject, JValue};
use jni::sys::{jlong, jlongArray};

use crate::bridge::{Failure, class, get, put, run};

const PAYLOAD_BRIDGE: &str = "io/ctwo/internal/PayloadBridge";

static REQUEST_SLABS: AtomicU64 = AtomicU64::new(0);
static RESPONSE_SLABS: AtomicU64 = AtomicU64::new(0);
static PUBLISHER_SLABS: AtomicU64 = AtomicU64::new(0);

pub(crate) enum SlabPath {
    Request,
    Response,
    Publisher,
}

pub(crate) fn record_slab(path: SlabPath) {
    let counter = match path {
        SlabPath::Request => &REQUEST_SLABS,
        SlabPath::Response => &RESPONSE_SLABS,
        SlabPath::Publisher => &PUBLISHER_SLABS,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

enum NativeBackingBox {
    Held {
        response: HeldResponse,
        _tracking: Option<LocalCopyGuard>,
    },
    Recycled(RecycledBytes),
}

impl NativeBackingBox {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Held { response, .. } => response.bytes(),
            Self::Recycled(bytes) => bytes,
        }
    }
}

pub(crate) fn put_response(
    mut response: HeldResponse,
    held: bool,
    tracker: &LocalCopyTracker,
    route: &str,
    method: &str,
) -> jlong {
    let tracking = if held {
        Some(tracker.track(LocalCopyMeta {
            route_name: route.to_owned(),
            method_name: method.to_owned(),
            direction: LocalCopyDirection::ClientResponse,
            bytes: response.bytes().len(),
        }))
    } else {
        response.detach_budget();
        None
    };
    put(NativeBackingBox::Held {
        response,
        _tracking: tracking,
    })
}

pub(crate) fn put_recycled(bytes: RecycledBytes) -> jlong {
    put(NativeBackingBox::Recycled(bytes))
}

pub(crate) fn put_vec(bytes: Vec<u8>) -> jlong {
    put_recycled(bytes.into())
}

fn retainable_pointer<T>(owner: Arc<T>) -> (*const T, Arc<T>) {
    // Use the documented Arc raw-pointer origin for the C retain callbacks.
    // Return the same reference for the Rust caller to pin across export.
    let pointer = Arc::into_raw(owner);
    // SAFETY: pointer is the unconsumed reference transferred above.
    (pointer, unsafe { Arc::from_raw(pointer) })
}

// The Java bridge keeps the registry's Arc through openExternalNative. After
// retain succeeds, FastDB owns a separate Arc until its last checked pin ends.
unsafe extern "C" fn retain_bytes(_context: *mut c_void, owner: *mut c_void) -> u32 {
    // SAFETY: FastDB calls retain only while an owning reference is live.
    unsafe { Arc::increment_strong_count(owner.cast::<NativeBackingBox>()) };
    0
}

unsafe extern "C" fn release_bytes(_context: *mut c_void, owner: *mut c_void) {
    // SAFETY: this balances the reference transferred by one successful retain.
    unsafe { Arc::decrement_strong_count(owner.cast::<NativeBackingBox>()) };
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_backing(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlongArray {
    run(&mut env, std::ptr::null_mut(), |env| {
        let (owner, bytes) = retainable_pointer(get::<NativeBackingBox>(handle)?);
        let slice = bytes.as_slice();
        let values = [
            slice.as_ptr() as usize as jlong,
            slice.len() as jlong,
            retain_bytes as *const () as usize as jlong,
            release_bytes as *const () as usize as jlong,
            owner as usize as jlong,
        ];
        let result = env.new_long_array(values.len() as i32)?;
        env.set_long_array_region(&result, 0, &values)?;
        Ok(result.into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_ctwo_internal_NativeCalls_directSlabCounts(
    mut env: JNIEnv,
    _class: JClass,
) -> jlongArray {
    run(&mut env, std::ptr::null_mut(), |env| {
        let counts = [
            REQUEST_SLABS.load(Ordering::Relaxed),
            RESPONSE_SLABS.load(Ordering::Relaxed),
            PUBLISHER_SLABS.load(Ordering::Relaxed),
        ];
        let result = env.new_long_array(counts.len() as i32)?;
        env.set_long_array_region(&result, 0, &counts.map(|count| count as jlong))?;
        Ok(result.into_raw())
    })
}

// The public fdb_payload_v1_backing_v1_t layout from FastDB's ABI-118 header.
// These callbacks pass owners and storage only. FastDB performs all builds,
// external opens and invalidations through its own Java binding.
type Reserve = unsafe extern "C" fn(
    *mut c_void,
    u32,
    u64,
    u32,
    *mut *mut c_void,
    *mut *mut u8,
    *mut u64,
) -> u32;
type Write = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *const u8, u64) -> u32;
type Commit = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *mut *const u8, *mut u64) -> u32;
type Status = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type Release = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[repr(C)]
struct BackingTable {
    struct_size: u32,
    flags: u32,
    context: *mut c_void,
    reserve: Option<Reserve>,
    write: Option<Write>,
    commit: Option<Commit>,
    rollback: Option<Status>,
    retain: Option<Status>,
    release: Option<Release>,
    reserved: [u64; 4],
}

const RESERVE_DIRECT: u32 = 1;
const DIRECT_UNAVAILABLE: u32 = 2007;
const ALLOCATION_FAILED: u32 = 5002;
const COMMIT_FAILED: u32 = 5003;

struct SlabBacking {
    slab: WritableSlab,
    data: usize,
    claimed: AtomicBool,
    committed: AtomicBool,
}

unsafe extern "C" fn reserve_slab(
    context: *mut c_void,
    mode: u32,
    capacity: u64,
    alignment: u32,
    out_owner: *mut *mut c_void,
    out_data: *mut *mut u8,
    out_capacity: *mut u64,
) -> u32 {
    if mode != RESERVE_DIRECT {
        return ALLOCATION_FAILED;
    }
    // SAFETY: the caller owns its Arc throughout the synchronous execution.
    let state = unsafe { &*context.cast::<SlabBacking>() };
    if capacity != state.slab.len() as u64
        || !alignment.is_power_of_two()
        || !state.data.is_multiple_of(alignment as usize)
        || state.claimed.swap(true, Ordering::Relaxed)
    {
        return DIRECT_UNAVAILABLE;
    }
    // SAFETY: outputs are ABI-provided and this reference is transferred to
    // FastDB's one rollback or release. The slab is exclusively writable here.
    unsafe {
        Arc::increment_strong_count(context.cast::<SlabBacking>());
        out_owner.write(context);
        out_data.write(state.data as *mut u8);
        out_capacity.write(capacity);
    }
    0
}

unsafe extern "C" fn commit_slab(
    _context: *mut c_void,
    owner: *mut c_void,
    size: u64,
    out_data: *mut *const u8,
    out_size: *mut u64,
) -> u32 {
    // SAFETY: FastDB holds the reference transferred by reserve.
    let state = unsafe { &*owner.cast::<SlabBacking>() };
    if size != state.slab.len() as u64 || state.committed.swap(true, Ordering::Release) {
        return COMMIT_FAILED;
    }
    // SAFETY: the ABI supplies these outputs during commit.
    unsafe {
        out_data.write(state.data as *const u8);
        out_size.write(size);
    }
    0
}

unsafe extern "C" fn rollback_slab(context: *mut c_void, owner: *mut c_void) -> u32 {
    // SAFETY: rollback consumes the uncommitted FastDB owner exactly once.
    unsafe { release_slab(context, owner) };
    0
}

unsafe extern "C" fn retain_slab(_context: *mut c_void, owner: *mut c_void) -> u32 {
    // SAFETY: the existing FastDB owner pins the slab through retain.
    unsafe { Arc::increment_strong_count(owner.cast::<SlabBacking>()) };
    0
}

unsafe extern "C" fn release_slab(_context: *mut c_void, owner: *mut c_void) {
    // SAFETY: FastDB balances each transferred or retained reference.
    unsafe { Arc::decrement_strong_count(owner.cast::<SlabBacking>()) };
}

pub(crate) fn plan_size(env: &mut JNIEnv, input: &JObject) -> Result<Option<usize>, Failure> {
    let bridge = class(env, PAYLOAD_BRIDGE)?;
    let length = env
        .call_static_method(
            bridge,
            "planSize",
            "(Ljava/lang/Object;)J",
            &[JValue::Object(input)],
        )?
        .j()?;
    if length == -1 {
        return Ok(None);
    }
    Ok(Some(byte_length(length)?))
}

pub(crate) fn can_direct(env: &mut JNIEnv, input: &JObject) -> Result<bool, Failure> {
    let bridge = class(env, PAYLOAD_BRIDGE)?;
    Ok(env
        .call_static_method(
            bridge,
            "canDirect",
            "(Ljava/lang/Object;)Z",
            &[JValue::Object(input)],
        )?
        .z()?)
}

pub(crate) fn direct(
    env: &mut JNIEnv,
    input: &JObject,
    mut slab: WritableSlab,
) -> Result<Option<WritableSlab>, Failure> {
    let data = slab.as_mut_ptr() as usize;
    let (owner, state) = retainable_pointer(Arc::new(SlabBacking {
        slab,
        data,
        claimed: AtomicBool::new(false),
        committed: AtomicBool::new(false),
    }));
    let table = BackingTable {
        struct_size: std::mem::size_of::<BackingTable>() as u32,
        flags: 0,
        context: owner.cast_mut().cast(),
        reserve: Some(reserve_slab),
        write: None,
        commit: Some(commit_slab),
        rollback: Some(rollback_slab),
        retain: Some(retain_slab),
        release: Some(release_slab),
        reserved: [0; 4],
    };
    let bridge = class(env, PAYLOAD_BRIDGE)?;
    let built = env
        .call_static_method(
            bridge,
            "direct",
            "(Ljava/lang/Object;J)Z",
            &[
                JValue::Object(input),
                JValue::Long(&raw const table as usize as jlong),
            ],
        )?
        .z()?;
    if !built {
        return Ok(None);
    }
    let state = Arc::try_unwrap(state)
        .map_err(|_| Failure::argument("FastDB kept a reference to the writable slab"))?;
    if !state.committed.load(Ordering::Acquire) {
        return Err(Failure::argument("FastDB did not commit the writable slab"));
    }
    Ok(Some(state.slab))
}

fn byte_length(length: jlong) -> Result<usize, Failure> {
    if length < 0 || length as u64 > isize::MAX as u64 {
        return Err(Failure::argument(
            "FastDB payload exceeds the native address range",
        ));
    }
    Ok(length as usize)
}

pub(crate) fn with_payload_bytes<T>(
    env: &mut JNIEnv,
    input: &JObject,
    read: impl FnOnce(&[u8]) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let bridge = class(env, PAYLOAD_BRIDGE)?;
    let access = env
        .call_static_method(
            bridge,
            "access",
            "(Ljava/lang/Object;)Lio/fastdb/payload/Access;",
            &[JValue::Object(input)],
        )?
        .l()?;
    let result = (|| {
        let size = byte_length(env.call_method(&access, "byteLength", "()J", &[])?.j()?)?;
        let address = env.call_method(&access, "dataAddress", "()J", &[])?.j()? as usize;
        if size == 0 {
            return read(&[]);
        }
        if address == 0 {
            return Err(Failure::argument("FastDB returned no payload address"));
        }
        // SAFETY: Access owns the stable FastDB bytes until the closure returns
        // and the unique pin is closed below. The slice never crosses JNI.
        read(unsafe { std::slice::from_raw_parts(address as *const u8, size) })
    })();
    if env.exception_check()? {
        // Closing the unique access pin is mandatory, including when a FastDB
        // method failed. Preserve the original Java throwable across cleanup.
        let original = env.exception_occurred()?;
        env.exception_clear()?;
        let _ = env.call_method(&access, "close", "()V", &[]);
        if env.exception_check()? {
            env.exception_clear()?;
        }
        env.throw(original)?;
        return Err(JniError::JavaException.into());
    }
    env.call_method(&access, "close", "()V", &[])?;
    result
}

pub(crate) fn copy_payload(env: &mut JNIEnv, input: &JObject) -> Result<Vec<u8>, Failure> {
    if input.is_null() {
        return Ok(Vec::new());
    }
    with_payload_bytes(env, input, |bytes| Ok(bytes.to_vec()))
}

pub(crate) fn encode_response(
    env: &mut JNIEnv,
    input: &JObject,
    sink: &ResponseSink,
) -> Result<EncodedResponse, Failure> {
    if input.is_null() {
        return Ok(EncodedResponse::Inline(Vec::new()));
    }
    if let Some(len) = plan_size(env, input)?
        && can_direct(env, input)?
        && let Some(slab) = sink.try_alloc_slab(len).map_err(Failure::argument)?
        && let Some(slab) = direct(env, input, slab)?
    {
        let response = sink.finish_slab(slab).map_err(Failure::argument)?;
        record_slab(SlabPath::Response);
        return Ok(response);
    }
    with_payload_bytes(env, input, |bytes| {
        if let Some(mut slab) = sink
            .try_alloc_slab(bytes.len())
            .map_err(Failure::argument)?
        {
            slab.as_mut_slice().copy_from_slice(bytes);
            sink.finish_slab(slab).map_err(Failure::argument)
        } else {
            Ok(EncodedResponse::Inline(bytes.to_vec()))
        }
    })
}
