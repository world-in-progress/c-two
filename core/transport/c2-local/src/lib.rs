//! Local byte streams. Protocol framing and runtime policy stay in their owners.
//!
//! `write_all` is an inherent method on the stream and its owned write half:
//! cancelling a partially written operation aborts the connection so the next
//! frame can never reuse a truncated stream. A completed write has drained the
//! transport adapter; peer consumption still requires a protocol acknowledgement.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

pub use c2_config::LocalEndpoint;

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

struct AbortState {
    stream: Mutex<Option<platform::Stream>>,
    closed: AtomicBool,
    reader: AtomicWaker,
    writer: AtomicWaker,
}

/// Abort pending local I/O without outliving or double-closing its OS handle.
#[derive(Clone)]
pub struct AbortHandle(Arc<AbortState>);

impl AbortHandle {
    pub fn abort(&self) {
        self.0.closed.store(true, Ordering::Release);
        let stream = self
            .0
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(stream) = stream {
            platform::cancel(platform::raw_handle(&stream));
            drop(stream);
        }
        self.0.reader.wake();
        self.0.writer.wake();
    }

    pub fn is_aborted(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
}

struct AbortOnDrop(Option<AbortHandle>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub struct LocalStream {
    abort: AbortHandle,
}

impl LocalStream {
    fn from_inner(inner: platform::Stream) -> Self {
        Self {
            abort: AbortHandle(Arc::new(AbortState {
                stream: Mutex::new(Some(inner)),
                closed: AtomicBool::new(false),
                reader: AtomicWaker::new(),
                writer: AtomicWaker::new(),
            })),
        }
    }

    pub async fn connect(endpoint: &LocalEndpoint, timeout: Duration) -> io::Result<Self> {
        let stream = tokio::time::timeout(timeout, platform::connect(endpoint))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "local connection deadline expired")
            })??;
        Ok(Self::from_inner(stream))
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    pub fn into_split(self) -> (LocalReadHalf, LocalWriteHalf) {
        let abort = self.abort_handle();
        let (reader, writer) = tokio::io::split(self);
        (
            LocalReadHalf(reader),
            LocalWriteHalf {
                inner: writer,
                abort,
            },
        )
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all_with_timeout(bytes, DEFAULT_WRITE_TIMEOUT)
            .await
    }

    pub async fn write_all_with_timeout(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> io::Result<()> {
        let abort = self.abort_handle();
        write_complete(self, abort, bytes, timeout).await
    }

    /// A connected pair for local-transport tests, using the real OS backend.
    pub async fn pair() -> io::Result<(Self, Self)> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let address = format!(
            "ipc://c2-pair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let endpoint = LocalEndpoint::from_address(&address)?;
        let mut listener = LocalListener::bind(&endpoint)?;
        let (client, server) = tokio::try_join!(
            Self::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
            listener.accept()
        )?;
        Ok((client, server))
    }
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

fn aborted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "local connection aborted")
}

impl AsyncRead for LocalStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.abort.0.reader.register(cx.waker());
        let mut stream = this
            .abort
            .0
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        match stream.as_mut() {
            Some(stream) => Pin::new(stream).poll_read(cx, buf),
            None => Poll::Ready(Err(aborted())),
        }
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.abort.0.writer.register(cx.waker());
        let mut stream = this
            .abort
            .0
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        match stream.as_mut() {
            Some(stream) => Pin::new(stream).poll_write(cx, bytes),
            None => Poll::Ready(Err(aborted())),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.abort.0.writer.register(cx.waker());
        let mut stream = this
            .abort
            .0
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        match stream.as_mut() {
            Some(stream) => platform::poll_flush(stream, cx),
            None => Poll::Ready(Err(aborted())),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub struct LocalReadHalf(tokio::io::ReadHalf<LocalStream>);
impl AsyncRead for LocalReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

pub struct LocalWriteHalf {
    inner: tokio::io::WriteHalf<LocalStream>,
    abort: AbortHandle,
}
impl LocalWriteHalf {
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all_with_timeout(bytes, DEFAULT_WRITE_TIMEOUT)
            .await
    }
    pub async fn write_all_with_timeout(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> io::Result<()> {
        let abort = self.abort_handle();
        write_complete(self, abort, bytes, timeout).await
    }
}
impl AsyncWrite for LocalWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

async fn write_complete<W: AsyncWrite + Unpin>(
    writer: &mut W,
    abort: AbortHandle,
    bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let mut guard = AbortOnDrop(Some(abort));
    let result = tokio::time::timeout(timeout, async {
        AsyncWriteExt::write_all(writer, bytes).await?;
        AsyncWriteExt::flush(writer).await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local write deadline expired"))?;
    if result.is_ok() {
        guard.0 = None;
    }
    result
}

pub struct LocalListener(platform::Listener);
impl LocalListener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        platform::Listener::bind(endpoint).map(Self)
    }
    /// Cancelling accept keeps its pending listener instance available.
    pub async fn accept(&mut self) -> io::Result<LocalStream> {
        self.0.accept().await.map(LocalStream::from_inner)
    }
}

#[cfg(test)]
mod tests;
