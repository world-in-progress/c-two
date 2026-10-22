use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::process::Stdio;

use c2_local_security::LocalSecurityAttributes;
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_ACCESS_INFORMATION, FILE_MODE_INFORMATION, FILE_PIPE_CONNECTED_STATE,
    FILE_PIPE_LOCAL_INFORMATION, FILE_SYNCHRONOUS_IO_ALERT, FILE_SYNCHRONOUS_IO_NONALERT,
    FileAccessInformation, FileModeInformation, FilePipeLocalInformation, NtQueryInformationFile,
};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, GetHandleInformation,
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, RtlNtStatusToDosError,
    SetHandleInformation, SetLastError,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_READ_DATA,
    FILE_TYPE_DISK, FILE_TYPE_PIPE, FILE_WRITE_DATA, GetFileType, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeInfo, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_SERVER_END, PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Controller-side client. The raw handle remains unregistered until activation.
pub(super) struct Controller {
    pending: Option<OwnedHandle>,
    activated: bool,
    closed: bool,
}

/// Server-side bootstrap. A cancelled connect leaves the same registered watcher here.
/// No other alias of its file object may be registered with a reactor.
pub(super) struct Bootstrap {
    pending: Option<OwnedHandle>,
    active: Option<NamedPipeServer>,
    connected: bool,
    activated: bool,
    closed: bool,
}

// Each direction can accept a small control write before the peer is activated.
// Zero quota instead makes write completion wait for a peer read. Larger frames
// still drain through the peer; these buffers request 8 KiB per private pair.
const OWNER_PAIR_BUFFER_BYTES: u32 = 4096;

pub(super) fn pair() -> io::Result<(Controller, Bootstrap)> {
    let mut random = [0u8; 16];
    // The private name is never returned, logged, or sent through argv or the environment.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(nt_status_error(status));
    }
    let mut suffix = String::with_capacity(random.len() * 2);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut suffix, "{byte:02x}").expect("writing to String cannot fail");
    }
    let name: Vec<u16> = format!(r"\\.\pipe\LOCAL\c-two-owner-{suffix}")
        .encode_utf16()
        .chain(Some(0))
        .collect();

    let mut security = LocalSecurityAttributes::new()?;
    // This current-logon-SID, local-only, one-instance pipe is created as an
    // unregistered overlapped handle so it can be transferred as child stdio.
    let raw_server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            OWNER_PAIR_BUFFER_BYTES,
            OWNER_PAIR_BUFFER_BYTES,
            0,
            security.as_mut_ptr(),
        )
    };
    if raw_server.is_null() || raw_server == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateNamedPipeW returned one owned handle.
    let server = unsafe { OwnedHandle::from_raw_handle(raw_server) };
    ensure_noninheritable(server.as_raw_handle())?;

    let raw_client = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if raw_client.is_null() || raw_client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned one owned handle.
    let client = unsafe { OwnedHandle::from_raw_handle(raw_client) };
    ensure_noninheritable(client.as_raw_handle())?;
    Ok((
        Controller {
            pending: Some(client),
            activated: false,
            closed: false,
        },
        Bootstrap {
            pending: Some(server),
            active: None,
            connected: false,
            activated: false,
            closed: false,
        },
    ))
}

/// # Safety
///
/// `handle` must be a valid borrowed handle from the dedicated owner bootstrap
/// supplied by a trusted launcher and remain open through this call. Every alias
/// of its file object must still be unregistered, with no concurrent I/O; after
/// activation the original alias (for example stdin) must never be registered
/// with any reactor. Type and mode checks cannot make arbitrary handles safe to
/// upgrade. This function duplicates the handle and never closes the original.
pub(super) unsafe fn adopt(handle: RawHandle) -> io::Result<Bootstrap> {
    verify_bootstrap(handle)?;
    let duplicate = duplicate_noninheritable(handle)?;
    Ok(Bootstrap {
        pending: Some(duplicate),
        active: None,
        connected: false,
        activated: false,
        closed: false,
    })
}

fn duplicate_noninheritable(handle: RawHandle) -> io::Result<OwnedHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            process,
            handle as HANDLE,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: DuplicateHandle returned a new owned handle.
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    ensure_noninheritable(duplicate.as_raw_handle())?;
    Ok(duplicate)
}

fn verify_bootstrap(handle: RawHandle) -> io::Result<()> {
    let handle = handle as HANDLE;
    unsafe { SetLastError(0) };
    let file_type = unsafe { GetFileType(handle) };
    let last_error = unsafe { GetLastError() };
    if file_type != FILE_TYPE_PIPE {
        if file_type == FILE_TYPE_DISK {
            return Err(invalid_bootstrap("owner bootstrap handle is not a pipe"));
        }
        if last_error != 0 {
            return Err(io::Error::from_raw_os_error(last_error as i32));
        }
        return Err(invalid_bootstrap(
            "owner bootstrap handle has an unsupported OS type",
        ));
    }

    let mut pipe_flags = 0;
    if unsafe {
        GetNamedPipeInfo(
            handle,
            &mut pipe_flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if pipe_flags & PIPE_SERVER_END == 0 || pipe_flags & PIPE_TYPE_MESSAGE != 0 {
        return Err(invalid_bootstrap(
            "owner bootstrap must be a byte-mode server pipe",
        ));
    }

    let mut status_block = IO_STATUS_BLOCK::default();
    let mut mode = FILE_MODE_INFORMATION::default();
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut status_block,
            (&mut mode as *mut FILE_MODE_INFORMATION).cast(),
            size_of::<FILE_MODE_INFORMATION>() as u32,
            FileModeInformation,
        )
    };
    if status < 0 {
        return Err(nt_status_error(status));
    }
    if mode.Mode & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT) != 0 {
        return Err(invalid_bootstrap(
            "owner bootstrap pipe must support overlapped I/O",
        ));
    }

    let mut access = FILE_ACCESS_INFORMATION::default();
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut status_block,
            (&mut access as *mut FILE_ACCESS_INFORMATION).cast(),
            size_of::<FILE_ACCESS_INFORMATION>() as u32,
            FileAccessInformation,
        )
    };
    if status < 0 {
        return Err(nt_status_error(status));
    }
    if access.AccessFlags & (FILE_READ_DATA | FILE_WRITE_DATA) != FILE_READ_DATA | FILE_WRITE_DATA {
        return Err(invalid_bootstrap(
            "owner bootstrap pipe must permit both reading and writing",
        ));
    }
    Ok(())
}

fn invalid_bootstrap(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn ensure_noninheritable(handle: RawHandle) -> io::Result<()> {
    let handle = handle as HANDLE;
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut flags = 0;
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & HANDLE_FLAG_INHERIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "owner control handle could not be made non-inheritable",
        ));
    }
    Ok(())
}

fn nt_status_error(status: i32) -> io::Error {
    let error = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(error as i32)
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "owner endpoint is closed")
}

fn already_activated() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "owner endpoint is already activated",
    )
}

fn probe_live(handle: RawHandle) -> io::Result<()> {
    // PeekNamedPipe can succeed with unread bytes in FILE_PIPE_CLOSING_STATE. The local
    // connection state is a non-consuming probe even when legal control bytes are buffered.
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut information = FILE_PIPE_LOCAL_INFORMATION::default();
    let status = unsafe {
        NtQueryInformationFile(
            handle as HANDLE,
            &mut status_block,
            (&mut information as *mut FILE_PIPE_LOCAL_INFORMATION).cast(),
            size_of::<FILE_PIPE_LOCAL_INFORMATION>() as u32,
            FilePipeLocalInformation,
        )
    };
    if status < 0 {
        return Err(nt_status_error(status));
    }
    if information.NamedPipeState != FILE_PIPE_CONNECTED_STATE {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "owner pipe peer is already closed",
        ));
    }
    Ok(())
}

impl Controller {
    pub(super) async fn activate(&mut self) -> io::Result<crate::LocalStream> {
        if self.closed {
            return Err(closed());
        }
        if self.activated {
            return Err(already_activated());
        }
        if let Err(error) = probe_live(self.pending.as_ref().ok_or_else(closed)?.as_raw_handle()) {
            self.close();
            return Err(error);
        }
        let pending = self.pending.take().ok_or_else(closed)?;
        // SAFETY: this is the owned, overlapped client end made by CreateFileW.
        let client = match unsafe { NamedPipeClient::from_raw_handle(pending.into_raw_handle()) } {
            Ok(client) => client,
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        self.activated = true;
        Ok(crate::LocalStream::from_inner(
            crate::platform::Stream::Client(client),
        ))
    }

    pub(super) fn close(&mut self) {
        self.closed = true;
        self.pending.take();
    }
}

impl Bootstrap {
    pub(super) async fn activate(&mut self) -> io::Result<crate::LocalStream> {
        if self.closed {
            return Err(closed());
        }
        if self.activated {
            return Err(already_activated());
        }
        if self.active.is_none() {
            let pending = self.pending.as_ref().ok_or_else(closed)?;
            // Keep the unregistered handle until registration succeeds. If Tokio
            // registration fails, close() can still disconnect this pipe even
            // when the inherited stdin alias remains open.
            let duplicate = duplicate_noninheritable(pending.as_raw_handle())?;
            // SAFETY: this is a fresh, unregistered duplicate of the validated
            // overlapped server end; Tokio owns it after this call.
            let watcher =
                match unsafe { NamedPipeServer::from_raw_handle(duplicate.into_raw_handle()) } {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        self.close();
                        return Err(error);
                    }
                };
            self.active = Some(watcher);
            self.pending.take();
        }
        // The watcher is in self before the first await. Tokio connect is cancel
        // safe, so a later call resumes this same watcher and pending operation.
        if !self.connected {
            self.active.as_mut().ok_or_else(closed)?.connect().await?;
            self.connected = true;
        }
        let server = self.active.as_ref().ok_or_else(closed)?;
        probe_live(server.as_raw_handle())?;
        let server = self.active.take().ok_or_else(closed)?;
        self.activated = true;
        Ok(crate::LocalStream::from_inner(
            crate::platform::Stream::OwnerServer(server),
        ))
    }

    pub(super) fn take_stdio(&mut self) -> io::Result<Stdio> {
        if self.closed {
            return Err(closed());
        }
        if self.active.is_some() || self.activated {
            return Err(already_activated());
        }
        let pending = self.pending.take().ok_or_else(closed)?;
        self.closed = true;
        // Ownership moves to Stdio. Drop of this wrapper must not disconnect it.
        Ok(Stdio::from(pending))
    }

    pub(super) fn close(&mut self) {
        if let Some(server) = self.active.take() {
            let _ = server.disconnect();
            crate::platform::cancel(server.as_raw_handle() as usize);
            drop(server);
        }
        if let Some(pending) = self.pending.take() {
            unsafe { DisconnectNamedPipe(pending.as_raw_handle() as HANDLE) };
            drop(pending);
        }
        self.closed = true;
    }
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::io::Write;
    use std::task::Context;
    use tokio::io::AsyncReadExt;
    use windows_sys::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND};
    use windows_sys::Win32::System::Pipes::PIPE_READMODE_MESSAGE;

    fn raw_server(access: u32, mode: u32) -> (OwnedHandle, Vec<u16>) {
        let name: Vec<_> = format!(
            r"\\.\pipe\LOCAL\c2-owner-test-{}",
            uuid::Uuid::new_v4().simple()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let mut security = LocalSecurityAttributes::new().unwrap();
        let raw = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                access | FILE_FLAG_FIRST_PIPE_INSTANCE,
                mode | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                security.as_mut_ptr(),
            )
        };
        assert!(!raw.is_null() && raw != INVALID_HANDLE_VALUE);
        // SAFETY: this is a fresh owned, unregistered pipe for this test only.
        (unsafe { OwnedHandle::from_raw_handle(raw) }, name)
    }

    #[tokio::test]
    async fn activation_rejects_a_closed_peer_even_with_buffered_control_bytes() {
        for close_server in [false, true] {
            // Keep the endpoint under test overlapped, but use an unregistered
            // synchronous peer. Dropping a Tokio stream only requests IOCP
            // cancellation; dropping this writer closes its native handle now.
            let (server, name) = raw_server(
                PIPE_ACCESS_DUPLEX
                    | if close_server {
                        0
                    } else {
                        FILE_FLAG_OVERLAPPED
                    },
                PIPE_TYPE_BYTE | PIPE_WAIT,
            );
            let raw_client = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    if close_server {
                        FILE_FLAG_OVERLAPPED
                    } else {
                        0
                    },
                    std::ptr::null_mut(),
                )
            };
            assert!(!raw_client.is_null() && raw_client != INVALID_HANDLE_VALUE);
            // SAFETY: CreateFileW returned this test's new, unregistered handle.
            let client = unsafe { OwnedHandle::from_raw_handle(raw_client) };
            if close_server {
                let mut peer = std::fs::File::from(server);
                peer.write_all(b"buffered reply").unwrap();
                drop(peer);
                let mut controller = Controller {
                    pending: Some(client),
                    activated: false,
                    closed: false,
                };
                assert!(controller.activate().await.is_err());
            } else {
                // SAFETY: server is dedicated and unregistered; its idle alias
                // stays open while only the bootstrap registers and performs I/O.
                let mut bootstrap = unsafe { adopt(server.as_raw_handle()) }.unwrap();
                let mut peer = std::fs::File::from(client);
                peer.write_all(b"buffered request").unwrap();
                drop(peer);
                assert!(bootstrap.activate().await.is_err());
            }
        }
    }

    #[test]
    fn inherited_wrong_type_access_and_mode_are_rejected() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        // SAFETY: dedicated, valid unregistered handles intentionally exercise type checks.
        assert!(unsafe { adopt(file.as_raw_handle()) }.is_err());
        for (access, mode) in [
            (
                PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
            ),
            (
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
            ),
            (PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT),
            (
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            ),
        ] {
            let (source, _) = raw_server(access, mode);
            assert!(unsafe { adopt(source.as_raw_handle()) }.is_err());
        }
        let (controller, _bootstrap) = pair().unwrap();
        assert!(unsafe { adopt(controller.pending.as_ref().unwrap().as_raw_handle()) }.is_err());
        assert!(unsafe { adopt(INVALID_HANDLE_VALUE) }.is_err());
    }

    #[tokio::test]
    async fn cancelled_activation_keeps_the_exact_iocp_watcher_until_connection() {
        let (source, name) = raw_server(
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_WAIT,
        );
        // SAFETY: source is dedicated, never registered, and remains an idle alias. Only the
        // adopted bootstrap may register or perform I/O for this pipe's server file object.
        let mut bootstrap = unsafe { adopt(source.as_raw_handle()) }.unwrap();
        let waker = futures_util::task::noop_waker();
        for _ in 0..2 {
            let mut activation = Box::pin(bootstrap.activate());
            assert!(
                activation
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            drop(activation);
            assert!(bootstrap.pending.is_none());
            assert!(bootstrap.active.is_some());
        }
        let registered = bootstrap.active.as_ref().unwrap().as_raw_handle();
        assert_eq!(
            bootstrap.take_stdio().err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut activation = Box::pin(bootstrap.activate());
        assert!(
            activation
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(activation);
        assert_eq!(
            bootstrap.active.as_ref().unwrap().as_raw_handle(),
            registered
        );
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        assert!(!raw.is_null() && raw != INVALID_HANDLE_VALUE);
        // SAFETY: CreateFileW returned a new, unregistered owned client handle.
        let mut controller = Controller {
            pending: Some(unsafe { OwnedHandle::from_raw_handle(raw) }),
            activated: false,
            closed: false,
        };
        let (mut server, mut client) =
            tokio::try_join!(bootstrap.activate(), controller.activate()).unwrap();
        server.write_all(b"same watcher").await.unwrap();
        let mut bytes = [0; 12];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"same watcher");
        drop(server);
        let mut byte = [0; 1];
        // Dropping the activated owner server end disconnects its pipe, so the
        // surviving client end observes ERROR_PIPE_NOT_CONNECTED instead of the
        // zero-length EOF a Unix socketpair read would report.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut byte))
            .await
            .expect("owner server drop must end the pipe within the deadline");
        match ended {
            Err(error) => assert_eq!(
                error.raw_os_error(),
                Some(windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED as i32)
            ),
            other => panic!("owner server drop must end the pipe: {other:?}"),
        }
        // The original source is physically open, but owner-specific disconnect ended the pipe.
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(source.as_raw_handle(), &mut flags) },
            0
        );
    }
}
