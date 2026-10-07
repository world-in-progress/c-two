use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::process::Stdio;

use c2_local_security::LocalSecurityAttributes;
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::NamedPipeServer;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_MODE_INFORMATION, FILE_SYNCHRONOUS_IO_ALERT, FILE_SYNCHRONOUS_IO_NONALERT,
    FileModeInformation, NtQueryInformationFile,
};
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_NO_DATA, GENERIC_WRITE, GetHandleInformation,
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, RtlNtStatusToDosError,
    SetHandleInformation, SetLastError,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_TYPE_DISK,
    FILE_TYPE_PIPE, GetFileType, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeInfo, PIPE_REJECT_REMOTE_CLIENTS, PIPE_SERVER_END,
    PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_WAIT, PeekNamedPipe,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub(super) struct Keepalive(Option<OwnedHandle>);

/// Receiver endpoint with a two-phase lifecycle.
///
/// `pending` owns the raw, unregistered server end until the first [`Receiver::wait_closed`] call.
/// That call moves this exact handle into `active`, the single Tokio watcher and the only
/// completion-port association the pipe ever gets. Every later wait, including one that follows a
/// cancelled wait, reuses that watcher: the code never duplicates the handle to build a second
/// watcher, and never hands a registered handle to another process. Exactly one field owns the
/// handle at a time, so `shutdown` and `Drop` close it once.
///
/// This capability has no reconnection path, so the *observed remote closure* is a terminal
/// outcome of the receiver, not of the watcher. `peer_closed` records that the peer end went away
/// (EOF, `ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`) so every later wait returns `Ok(())` without
/// touching the completion port again. A second overlapped read on that same handle can instead
/// surface `ERROR_PIPE_NOT_CONNECTED`, which must never be reported as a fresh local failure.
pub(super) struct Receiver {
    pending: Option<OwnedHandle>,
    active: Option<NamedPipeServer>,
    connected: bool,
    peer_closed: bool,
}

pub(super) fn pair() -> io::Result<(Keepalive, Receiver)> {
    let mut random = [0u8; 16];
    // Use the system-preferred cryptographic RNG; the generated name remains private to this
    // construction and is never returned, logged, placed in argv/env, or exposed as an address.
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
    let pipe_name: Vec<u16> = format!(r"\\.\pipe\LOCAL\c-two-owner-{suffix}")
        .encode_utf16()
        .chain(Some(0))
        .collect();

    let mut security = LocalSecurityAttributes::new()?;
    // The server end is created overlapped but deliberately remains a raw, unregistered OS
    // handle in the controller. Tokio attaches it to an IOCP only after explicit child adoption,
    // on the first `wait_closed` call inside the receiving process.
    let raw_receiver = unsafe {
        CreateNamedPipeW(
            pipe_name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            0,
            0,
            security.as_mut_ptr(),
        )
    };
    if raw_receiver.is_null() || raw_receiver == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateNamedPipeW returns one fresh owned handle.
    let receiver = unsafe { OwnedHandle::from_raw_handle(raw_receiver) };
    ensure_noninheritable(receiver.as_raw_handle())?;
    verify_receiver(receiver.as_raw_handle())?;

    // CreateFileW with null SECURITY_ATTRIBUTES returns a non-inheritable client end. This
    // write-only handle is the keepalive; it is never registered for asynchronous I/O.
    let raw_keepalive = unsafe {
        CreateFileW(
            pipe_name.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw_keepalive.is_null() || raw_keepalive == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateFileW returns one fresh owned handle.
    let keepalive = unsafe { OwnedHandle::from_raw_handle(raw_keepalive) };
    ensure_noninheritable(keepalive.as_raw_handle())?;
    Ok((
        Keepalive(Some(keepalive)),
        Receiver {
            pending: Some(receiver),
            active: None,
            connected: false,
            peer_closed: false,
        },
    ))
}

pub(super) unsafe fn adopt(handle: RawHandle) -> io::Result<Receiver> {
    // SAFETY: the public caller promises this borrowed handle stays valid through validation and
    // DuplicateHandle. Validation rejects ordinary files, sockets, client pipe ends, and sync I/O.
    verify_receiver(handle)?;
    let current = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            current,
            handle as HANDLE,
            current,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: DuplicateHandle returned one fresh owned, non-inheritable handle.
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    ensure_noninheritable(duplicate.as_raw_handle())?;
    verify_receiver(duplicate.as_raw_handle())?;
    // The child owns only this duplicate. The inherited source handle (for stdin, the one held by
    // the process standard stream) stays with its original owner and is never closed here. The
    // duplicate is still unregistered; no IOCP association exists in this process until the first
    // `wait_closed` call moves it into the single Tokio watcher.
    Ok(Receiver {
        pending: Some(duplicate),
        active: None,
        connected: false,
        peer_closed: false,
    })
}

fn verify_receiver(handle: RawHandle) -> io::Result<()> {
    let handle = handle as HANDLE;
    // GetFileType uses FILE_TYPE_UNKNOWN both for an invalid handle and for an unknown type.
    unsafe { SetLastError(0) };
    let file_type = unsafe { GetFileType(handle) };
    let last_error = unsafe { GetLastError() };
    if file_type != FILE_TYPE_PIPE {
        if file_type == FILE_TYPE_DISK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited owner control handle is not a pipe",
            ));
        }
        if last_error != 0 {
            return Err(io::Error::from_raw_os_error(last_error as i32));
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited owner control handle has an unsupported OS type",
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner control handle must be a byte-mode server read end",
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner control pipe must support overlapped I/O",
        ));
    }
    Ok(())
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

impl Keepalive {
    pub(super) fn shutdown(&mut self) {
        self.0.take();
    }
}

impl Receiver {
    pub(super) async fn prepare(&mut self) -> io::Result<bool> {
        if self.peer_closed {
            return Ok(false);
        }
        if self.active.is_none() {
            let pending = self.pending.take().ok_or_else(closed_receiver)?;
            // The first wait activates the endpoint. `NamedPipeServer` takes ownership of this
            // exact handle and registers it on the current process's completion port exactly
            // once; it is the only watcher, and the handle is never duplicated. A registration
            // failure is terminal: the handle is closed and later waits report the receiver as
            // closed.
            //
            // SAFETY: `pending` is the validated, overlapped, byte-mode server end of a private
            // one-instance pipe, and it is transferred, not borrowed.
            let watcher = unsafe { NamedPipeServer::from_raw_handle(pending.into_raw_handle()) }?;
            self.active = Some(watcher);
        }
        let connected = self.connected;
        let watcher = self.active.as_mut().ok_or_else(closed_receiver)?;
        if !connected {
            // Tokio documents `connect` as cancel safe: a cancelled wait leaves the pending
            // `ConnectNamedPipe` on the same completion port, and this call resumes it without a
            // new registration. `connected` is only set after a full success.
            match watcher.connect().await {
                Ok(()) => self.connected = true,
                Err(error) if is_peer_closed(&error) => {
                    self.peer_closed = true;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
        let watcher = self.active.as_ref().ok_or_else(closed_receiver)?;
        let mut available = 0u32;
        // This handle is validated FILE_FLAG_OVERLAPPED. Peek does not create another
        // IOCP registration or pending read, and detects an already broken peer.
        let ok = unsafe {
            PeekNamedPipe(
                watcher.as_raw_handle() as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if is_peer_closed(&error) {
                self.peer_closed = true;
                return Ok(false);
            }
            return Err(error);
        }
        if available != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner control pipe unexpectedly carried data",
            ));
        }
        Ok(true)
    }

    pub(super) async fn wait_closed(&mut self) -> io::Result<()> {
        // The first observed remote closure is terminal. Later waits must report the same stable
        // `Ok(())` and must not issue another overlapped operation, which on Windows can return
        // ERROR_PIPE_NOT_CONNECTED instead of the original EOF.
        if self.peer_closed {
            return Ok(());
        }
        if !self.prepare().await? {
            return Ok(());
        }
        let mut byte = [0u8; 1];
        match self
            .active
            .as_mut()
            .ok_or_else(closed_receiver)?
            .read(&mut byte)
            .await
        {
            Ok(0) => self.record_peer_closed(),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner control pipe unexpectedly carried data",
            )),
            Err(error) if is_peer_closed(&error) => self.record_peer_closed(),
            // Local I/O failures (including ERROR_PIPE_NOT_CONNECTED without a recorded peer EOF)
            // are returned to the caller and never laundered into a "peer closed" fact.
            Err(error) => Err(error),
        }
    }

    /// Record the observed remote closure once and return the terminal `Ok(())` for this wait.
    fn record_peer_closed(&mut self) -> io::Result<()> {
        self.peer_closed = true;
        Ok(())
    }

    pub(super) fn take_stdio(&mut self) -> io::Result<Stdio> {
        if self.active.is_some() {
            return Err(super::already_activated());
        }
        let pending = self.pending.take().ok_or_else(closed_receiver)?;
        Ok(Stdio::from(pending))
    }

    pub(super) fn is_activated(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn shutdown(&mut self) {
        // Dropping the watcher deregisters the handle and closes it once; the pending raw handle,
        // when the receiver was never activated, is closed by the same take. The recorded peer
        // closure is cleared together with the watcher: a shut-down receiver reports
        // `BrokenPipe` on both later waits and later transfers.
        self.peer_closed = false;
        self.active.take();
        self.pending.take();
    }
}

/// Windows reports a peer that closed its pipe end as either `ERROR_BROKEN_PIPE` or
/// `ERROR_NO_DATA` depending on the operation and timing.
fn is_peer_closed(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)
    )
}

fn closed_receiver() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "owner control receiver is closed",
    )
}
