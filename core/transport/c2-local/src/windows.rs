use super::{
    EndpointCredential, EndpointInspection, EndpointReapResult, LocalEndpoint,
    LocalEndpointContext, SweepBatch, SweepBudget,
};
use c2_local_security::{
    LocalSecurityAttributes, current_session_id, grant_server_identity_query_access,
    verify_current_user_pipe_owner, verify_server_process_identity,
};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{
    AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, IntoRawHandle, OwnedHandle,
};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientSessionId, GetNamedPipeServerProcessId, GetNamedPipeServerSessionId,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};

pub enum Stream {
    // Keep the verified process object pinned as long as this connection lives.
    Client {
        pipe: NamedPipeClient,
        _server_process: OwnedHandle,
    },
    // A private owner pair is a trusted launcher capability, including after
    // transfer to a child. It does not require its creator to stay alive.
    OwnerClient(NamedPipeClient),
    Server(NamedPipeServer),
    OwnerServer(NamedPipeServer),
}

/// An owned, verified overlapped pipe that has never been associated with IOCP,
/// plus the pinned server process. Same TokenUser does not imply equal trust or
/// prevent denial of service by another process running as that user.
pub struct WindowsPipeConnection {
    pipe: OwnedHandle,
    server_process: OwnedHandle,
}

impl WindowsPipeConnection {
    /// Transfer both owners exactly once: pipe first, server process second.
    /// The adopter must keep the process handle until it closes the pipe, and
    /// must register the pipe with at most one event loop/IOCP.
    pub fn into_parts(self) -> (OwnedHandle, OwnedHandle) {
        (self.pipe, self.server_process)
    }
}

pub(super) async fn connect_raw(endpoint: &LocalEndpoint) -> io::Result<WindowsPipeConnection> {
    loop {
        match open_verified_pipe(endpoint.os_name()) {
            Ok(connection) => return Ok(connection),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // The caller's one total deadline bounds pipe-instance contention.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn open_verified_pipe(name: &OsStr) -> io::Result<WindowsPipeConnection> {
    let name: Vec<_> = name.encode_wide().chain(Some(0)).collect();
    // Identification SQOS must be present at open, before trusting the server:
    // a malicious pipe must never receive a token it can impersonate for access.
    // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new, non-inheritable, unregistered handle.
    let pipe = unsafe { OwnedHandle::from_raw_handle(raw) };
    let server_process = verify_server_process(pipe.as_handle())?;
    Ok(WindowsPipeConnection {
        pipe,
        server_process,
    })
}

fn server_process_id(pipe: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

fn verify_server_process(pipe: BorrowedHandle<'_>) -> io::Result<OwnedHandle> {
    // Check the object before the process: a same-user instance must not join
    // a foreign-owned pipe object whose DACL happens to permit this user.
    verify_current_user_pipe_owner(pipe)?;
    let pid = server_process_id(pipe)?;
    // QUERY_LIMITED_INFORMATION permits OpenProcessToken; SYNCHRONIZE permits
    // a nonblocking liveness check and the handle pins this exact process object.
    // https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned one owned, non-inheritable handle.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    verify_server_process_identity(process.as_handle())?;
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_TIMEOUT => {}
        WAIT_OBJECT_0 => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named pipe server process has exited",
            ));
        }
        WAIT_FAILED => return Err(io::Error::last_os_error()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected process wait result",
            ));
        }
    }
    if server_process_id(pipe)? != pid {
        return Err(c2_local_security::peer_identity_mismatch(
            "named pipe server process changed during verification",
        ));
    }
    Ok(process)
}

pub async fn connect(endpoint: &LocalEndpoint) -> io::Result<Stream> {
    let (pipe, process) = connect_raw(endpoint).await?.into_parts();
    // SAFETY: identity was checked on this owned overlapped pipe before its
    // first IOCP registration. Tokio takes ownership, including on failure.
    let client = unsafe { NamedPipeClient::from_raw_handle(pipe.into_raw_handle()) }?;
    Ok(Stream::Client {
        pipe: client,
        _server_process: process,
    })
}

pub fn raw_handle(stream: &Stream) -> usize {
    match stream {
        Stream::Client { pipe: client, .. } | Stream::OwnerClient(client) => {
            client.as_raw_handle() as usize
        }
        Stream::Server(server) | Stream::OwnerServer(server) => server.as_raw_handle() as usize,
    }
}

pub fn shared_memory_available(stream: &Stream) -> io::Result<bool> {
    let mut peer_session = 0;
    let raw = raw_handle(stream) as HANDLE;
    // Use the connected instance, not endpoint names or cached process IDs.
    // Query errors must reach the caller instead of silently disabling SHM.
    let result = unsafe {
        match stream {
            Stream::Client { .. } | Stream::OwnerClient(..) => {
                GetNamedPipeServerSessionId(raw, &mut peer_session)
            }
            Stream::Server(..) | Stream::OwnerServer(..) => {
                GetNamedPipeClientSessionId(raw, &mut peer_session)
            }
        }
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(peer_session == current_session_id()?)
}

pub fn cancel(raw: usize) {
    // Tokio/mio retain their buffers until cancelled overlapped operations
    // complete. They remain the sole owners that close this handle.
    unsafe {
        CancelIoEx(raw as HANDLE, std::ptr::null());
    }
}

pub fn cancel_stream(stream: &Stream) {
    if let Stream::OwnerServer(server) = stream {
        // The bootstrap may have been adopted from stdin. Closing only the registered
        // duplicate leaves that original server handle open and keeps the client alive.
        let _ = server.disconnect();
    }
    cancel(raw_handle(stream));
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client { pipe: client, .. } | Self::OwnerClient(client) => {
                Pin::new(client).poll_read(cx, bytes)
            }
            Self::Server(server) | Self::OwnerServer(server) => {
                Pin::new(server).poll_read(cx, bytes)
            }
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Client { pipe: client, .. } | Self::OwnerClient(client) => {
                Pin::new(client).poll_write(cx, bytes)
            }
            Self::Server(server) | Self::OwnerServer(server) => {
                Pin::new(server).poll_write(cx, bytes)
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        poll_flush(self.get_mut(), cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub fn poll_flush(stream: &mut Stream, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    // Tokio's pipe flush is a no-op. Mio permits its next write only after the
    // previous overlapped write completes. A zero-byte write therefore drains
    // that adapter without blocking a Tokio worker in FlushFileBuffers (which
    // waits for peer consumption). Byte-mode pipes add no message on this probe.
    Pin::new(stream).poll_write(cx, &[]).map_ok(|_| ())
}

/// Native identity of the current Windows TokenUser scope at listener creation.
#[derive(Clone)]
pub(crate) struct ContextBinding {
    user_scope_id: String,
}

impl ContextBinding {
    pub(crate) fn capture(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let user_scope_id = endpoint
            .context()
            .windows_user_scope_id()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a Windows endpoint"))?
            .to_owned();
        let binding = Self { user_scope_id };
        Ok(binding)
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"c2.owner.windows-user-scope.v1\0");
        hash.update((self.user_scope_id.len() as u64).to_le_bytes());
        hash.update(self.user_scope_id.as_bytes());
        hash.finalize().into()
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        let current = LocalEndpointContext::default_for_platform()?;
        if current.windows_user_scope_id() != Some(self.user_scope_id.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows endpoint user scope changed",
            ));
        }
        Ok(())
    }
}

pub struct Listener {
    // Declaration order matters: close the pending server instance before
    // releasing the exclusive listener lease. Connected streams do not retain it.
    pending: NamedPipeServer,
    endpoint: LocalEndpoint,
    context_binding: Option<ContextBinding>,
    security: LocalSecurityAttributes,
    _ownership: OwnedHandle,
}

impl Listener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        Self::bind_with_capture(endpoint, false)
    }

    pub(crate) fn bind_with_context_binding(endpoint: &LocalEndpoint) -> io::Result<Self> {
        Self::bind_with_capture(endpoint, true)
    }

    fn bind_with_capture(endpoint: &LocalEndpoint, capture_context: bool) -> io::Result<Self> {
        let context_binding = capture_context
            .then(|| ContextBinding::capture(endpoint))
            .transpose()?;
        let mut security = LocalSecurityAttributes::new()?;
        let ownership = claim_listener(endpoint, &mut security)?;
        grant_server_identity_query_access()?;
        let pending = create_instance(endpoint, &mut security)?;
        Ok(Self {
            pending,
            endpoint: endpoint.clone(),
            context_binding,
            security,
            _ownership: ownership,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        // A cancelled connect future leaves this same instance owned by the
        // listener. The next accept resumes it instead of removing the endpoint.
        self.pending.connect().await?;
        // Keep a listening instance present before delivering this connection.
        let next = create_instance(&self.endpoint, &mut self.security)?;
        Ok(Stream::Server(std::mem::replace(&mut self.pending, next)))
    }

    pub fn credential(&self) -> EndpointCredential {
        EndpointCredential::kernel_managed(self.endpoint.clone())
    }

    pub fn context_binding(&self) -> io::Result<ContextBinding> {
        let binding = self.context_binding.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "listener has no owner context pin",
            )
        })?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn close(self) -> EndpointReapResult {
        // Drop disconnects the pending instance before releasing the kernel
        // listener lease, preserving the existing restart boundary.
        drop(self);
        EndpointReapResult::NotApplicable
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Mio keeps a pending ConnectNamedPipe operation and its handle alive
        // until IOCP reports cancellation. Disconnect this unaccepted instance
        // before releasing the listener lease so it cannot capture a client
        // intended for the next listener. Accepted streams are separate owners.
        let _ = self.pending.disconnect();
    }
}

pub(crate) fn inspect_endpoint(_endpoint: &LocalEndpoint) -> EndpointInspection {
    // KernelManaged describes who owns endpoint lifetime, not whether one live
    // instance exists: named-pipe existence is not probed and cannot be proven
    // from the address alone.
    EndpointInspection::KernelManaged
}

pub(crate) fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    // A credential for another logical endpoint is a stale target on every
    // platform, so callers can separate a mismatch from "nothing to collect".
    if credential.endpoint() != endpoint {
        return EndpointReapResult::StaleTarget;
    }
    EndpointReapResult::NotApplicable
}

pub(crate) struct EndpointSweep;

impl EndpointSweep {
    pub(crate) fn for_endpoint(_endpoint: &LocalEndpoint) -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn for_scope(
        _endpoint: &LocalEndpoint,
        _targets: &[LocalEndpoint],
    ) -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn next_batch(&mut self, _budget: SweepBudget) -> SweepBatch {
        SweepBatch {
            not_applicable: 1,
            round_complete: true,
            ..SweepBatch::default()
        }
    }
}

fn listener_lease_name(endpoint_name: &OsStr) -> String {
    let mut digest = Sha256::new();
    for unit in endpoint_name.encode_wide() {
        digest.update(unit.to_le_bytes());
    }
    let mut name = String::from(r"Global\c_two_listener-");
    for byte in digest.finalize() {
        write!(&mut name, "{byte:02x}").expect("writing to String cannot fail");
    }
    name
}

fn claim_listener(
    endpoint: &LocalEndpoint,
    security: &mut LocalSecurityAttributes,
) -> io::Result<OwnedHandle> {
    let name: Vec<u16> = listener_lease_name(endpoint.os_name())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // This is an existence lease, not a mutex lock: no thread owns or waits on
    // it. Tokio may move/drop Listener on another thread. Clients never open it,
    // so old pipe handles cannot keep a stopped listener's lease alive.
    let raw = unsafe { CreateMutexW(security.as_mut_ptr(), 0, name.as_ptr()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: CreateMutexW returned one fresh owned handle, including when it
    // opened an existing object. Every error path closes that handle as well.
    let ownership = unsafe { OwnedHandle::from_raw_handle(raw) };
    if existed {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "IPC endpoint already has an active listener",
        ));
    }
    Ok(ownership)
}

fn create_instance(
    endpoint: &LocalEndpoint,
    security: &mut LocalSecurityAttributes,
) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .pipe_mode(PipeMode::Byte)
        .reject_remote_clients(true)
        // FIRST_PIPE_INSTANCE would prevent restart while an old client still
        // retains the pipe object. The separate kernel lease owns exclusivity.
        .first_pipe_instance(false);
    // SAFETY: the ACL owner lives across the synchronous CreateNamedPipe call;
    // the returned handle is explicitly non-inheritable.
    let server = unsafe {
        options
            .create_with_security_attributes_raw(endpoint.os_name(), security.as_mut_ptr().cast())
    }?;
    // CreateNamedPipe may join an already existing pipe object and ignores
    // our descriptor then. Never accept a foreign owner's permissive DACL.
    verify_current_user_pipe_owner(server.as_handle())?;
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_lease_names_preserve_utf16le_sha256_hex_golden_values() {
        for (name, digest) in [
            (
                r"\\.\pipe\c_two-test",
                "0e3d89fd810afdf674e32748ed63ddd71a6bf0a236499c62f610643f1669a9fe",
            ),
            (
                r"\\.\pipe\c_two-资源",
                "781d479e4484e161fc15ec59e3e623a3faad207938d2c8c4a5c8dd5c9fbdf593",
            ),
        ] {
            assert_eq!(
                listener_lease_name(OsStr::new(name)),
                format!(r"Global\c_two_listener-{digest}")
            );
        }
    }

    #[tokio::test]
    async fn global_listener_lease_is_a_real_user_owned_kernel_object() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{
            EqualSid, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
        };
        use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
        use windows_sys::Win32::System::Threading::OpenMutexW;

        let endpoint = endpoint(&format!("global-lease-{}", uuid::Uuid::new_v4().simple()));
        let listener = Listener::bind(&endpoint).unwrap();
        let name = listener_lease_name(endpoint.os_name());
        let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        let raw = unsafe { OpenMutexW(READ_CONTROL, 0, wide.as_ptr()) };
        assert!(!raw.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: OpenMutexW returned a fresh handle to the live Global lease.
        let lease = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    lease.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let mut expected_security = LocalSecurityAttributes::new().unwrap();
        let mut expected_owner = std::ptr::null_mut();
        let mut defaulted = 0;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorOwner(
                    (*expected_security.as_mut_ptr()).lpSecurityDescriptor,
                    &mut expected_owner,
                    &mut defaulted,
                )
            },
            0
        );
        let same_owner = unsafe { EqualSid(owner, expected_owner) };
        unsafe { LocalFree(descriptor) };
        assert_ne!(same_owner, 0);
        assert_eq!(
            Listener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        drop(lease);
        drop(listener);
        // Session 0 may resolve Local and Global to the same namespace. Check
        // release through the exact Global name used to open the live lease.
        let released = unsafe { OpenMutexW(READ_CONTROL, 0, wide.as_ptr()) };
        if !released.is_null() {
            // Close even on assertion failure; only this test's unique name is touched.
            drop(unsafe { OwnedHandle::from_raw_handle(released) });
            panic!("Global listener lease must be released after its last handle closes");
        }
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32)
        );
    }

    #[tokio::test]
    async fn unconnected_pipe_session_query_preserves_the_real_os_error() {
        let endpoint = endpoint(&format!("session-error-{}", uuid::Uuid::new_v4().simple()));
        let mut security = LocalSecurityAttributes::new().unwrap();
        let server = create_instance(&endpoint, &mut security).unwrap();
        let error = shared_memory_available(&Stream::Server(server)).unwrap_err();
        assert!(error.raw_os_error().is_some(), "{error}");
    }

    fn endpoint(name: &str) -> LocalEndpoint {
        LocalEndpoint::from_address(&format!("ipc://{name}")).unwrap()
    }

    #[test]
    fn credential_for_another_endpoint_is_a_stale_target() {
        let owned = endpoint("windows-credential-owner");
        let other = endpoint("windows-credential-other");
        let credential = EndpointCredential::kernel_managed(owned.clone());
        assert_eq!(credential.endpoint(), &owned);
        assert!(matches!(
            reap_endpoint(&other, &credential),
            EndpointReapResult::StaleTarget
        ));
    }

    #[test]
    fn matching_credential_reports_platform_managed_without_collecting_handles() {
        let target = endpoint("windows-managed-endpoint");
        let credential = EndpointCredential::kernel_managed(target.clone());
        // KernelManaged means the platform owns lifetime; it is not proof that
        // one live instance exists, and reap never fabricates Unix cleanup.
        assert!(matches!(
            inspect_endpoint(&target),
            EndpointInspection::KernelManaged
        ));
        assert!(matches!(
            reap_endpoint(&target, &credential),
            EndpointReapResult::NotApplicable
        ));
    }

    #[test]
    fn sweep_reports_not_applicable_without_claiming_a_completed_unix_round() {
        let mut sweep = EndpointSweep::for_endpoint(&endpoint("windows-sweep")).unwrap();
        let batch = sweep.next_batch(SweepBudget::default());
        assert_eq!(batch.not_applicable, 1);
        assert!(batch.round_complete);
        assert!(!batch.round_interrupted);
        assert!(!batch.namespace_changed);
    }
}
