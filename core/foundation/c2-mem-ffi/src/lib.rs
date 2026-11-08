//! C ABI for native client domains, IPC endpoints, calls and local buffers.

use c2_config::{ConfigResolver, ConfigSources, LocalEndpointContext, LocalEndpointOptions};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;

const C2_MEM_FFI_ABI_VERSION: u32 = 8;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C2MemFfiStatus {
    Ok = 0,
    NullPointer = 1,
    InvalidArgument = 2,
    PoolError = 3,
    InsufficientBuffer = 4,
    IoError = 5,
    PermissionDenied = 6,
    IdentityMismatch = 7,
}

mod client;
pub use client::*;

/// Owned immutable endpoint context. The C API exposes only an opaque pointer.
pub struct C2MemFfiLocalEndpointContext {
    inner: LocalEndpointContext,
}

fn guard_status(action: impl FnOnce() -> Result<(), C2MemFfiStatus>) -> C2MemFfiStatus {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(())) => C2MemFfiStatus::Ok,
        Ok(Err(status)) => status,
        Err(_) => C2MemFfiStatus::PoolError,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::fs;
    use std::ptr;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);
    // These tests share process environment and create native handles while
    // other tests launch a compiler. Serialize those boundaries, including
    // socketpair's non-atomic CLOEXEC setup on macOS.
    pub(super) static PROCESS_STATE_LOCK: Mutex<()> = Mutex::new(());

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
            // Endpoint tests hold PROCESS_STATE_LOCK across every configuration
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

    #[cfg(unix)]
    #[test]
    fn endpoint_context_freezes_sources_and_keeps_resolution_pure() {
        let _lock = PROCESS_STATE_LOCK.lock().unwrap();
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

        // A descriptive path may exceed socket address capacity. Native IO
        // addresses its directory descriptor and short name separately.
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
            C2MemFfiStatus::Ok
        );
        assert_eq!(failed_len, overlong.name(&address).len());
        assert!(failed_len > 108);
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
        let _lock = PROCESS_STATE_LOCK.lock().unwrap();
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
        let _lock = PROCESS_STATE_LOCK.lock().unwrap();
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
    fn public_abi_version_is_exported() {
        assert_eq!(c2_mem_ffi_abi_version(), 8);
    }
}
