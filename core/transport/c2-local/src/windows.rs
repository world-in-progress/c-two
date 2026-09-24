use super::LocalEndpoint;
use c2_local_security::LocalSecurityAttributes;
use sha2::{Digest, Sha256};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, GetLastError, HANDLE};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Threading::CreateMutexW;

pub enum Stream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

pub async fn connect(endpoint: &LocalEndpoint) -> io::Result<Stream> {
    loop {
        match ClientOptions::new().open(endpoint.os_name()) {
            Ok(client) => return Ok(Stream::Client(client)),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // The common connect deadline bounds pipe-instance contention.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn raw_handle(stream: &Stream) -> usize {
    match stream {
        Stream::Client(client) => client.as_raw_handle() as usize,
        Stream::Server(server) => server.as_raw_handle() as usize,
    }
}

pub fn cancel(raw: usize) {
    // Tokio/mio retain their buffers until cancelled overlapped operations
    // complete. They remain the sole owners that close this handle.
    unsafe {
        CancelIoEx(raw as HANDLE, std::ptr::null());
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(client) => Pin::new(client).poll_read(cx, bytes),
            Self::Server(server) => Pin::new(server).poll_read(cx, bytes),
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
            Self::Client(client) => Pin::new(client).poll_write(cx, bytes),
            Self::Server(server) => Pin::new(server).poll_write(cx, bytes),
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

pub struct Listener {
    // Declaration order matters: close the pending server instance before
    // releasing the exclusive listener lease. Connected streams do not retain it.
    pending: NamedPipeServer,
    endpoint: LocalEndpoint,
    security: LocalSecurityAttributes,
    _ownership: OwnedHandle,
}

impl Listener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let mut security = LocalSecurityAttributes::new()?;
        let ownership = claim_listener(endpoint, &mut security)?;
        let pending = create_instance(endpoint, &mut security)?;
        Ok(Self {
            pending,
            endpoint: endpoint.clone(),
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
}

fn claim_listener(
    endpoint: &LocalEndpoint,
    security: &mut LocalSecurityAttributes,
) -> io::Result<OwnedHandle> {
    let mut digest = Sha256::new();
    for unit in endpoint.os_name().encode_wide() {
        digest.update(unit.to_le_bytes());
    }
    let name: Vec<u16> = format!(r"Local\c_two_listener-{:x}", digest.finalize())
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
    unsafe {
        options
            .create_with_security_attributes_raw(endpoint.os_name(), security.as_mut_ptr().cast())
    }
}
