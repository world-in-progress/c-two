//! Per-connection transport control for a blocked topic subscriber.
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use futures::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

pub(crate) struct ConnectionControl {
    blocked: AtomicBool,
    aborted: AtomicBool,
    read_waker: AtomicWaker,
    write_waker: AtomicWaker,
    pub(crate) changed: Notify,
}
impl ConnectionControl {
    pub(crate) fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::Acquire)
    }
    /// A control handle without a live stream, for tests that drive blocked
    /// or aborted states without a real connection.
    #[cfg(test)]
    pub(crate) fn for_tests() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            blocked: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            read_waker: AtomicWaker::new(),
            write_waker: AtomicWaker::new(),
            changed: Notify::new(),
        })
    }
    /// End this HTTP/1 connection without appending another record to a
    /// response whose current write is blocked partway through a frame.
    pub(crate) fn abort(&self) {
        if !self.aborted.swap(true, Ordering::AcqRel) {
            self.read_waker.wake();
            self.write_waker.wake();
            self.changed.notify_waiters();
            self.changed.notify_one();
        }
    }
    fn poll_aborted(&self, cx: &mut Context<'_>, waker: &AtomicWaker) -> bool {
        if self.aborted.load(Ordering::Acquire) {
            return true;
        }
        waker.register(cx.waker());
        self.aborted.load(Ordering::Acquire)
    }
    pub(crate) fn set_blocked(&self, blocked: bool) {
        if self.blocked.swap(blocked, Ordering::AcqRel) != blocked {
            self.changed.notify_waiters();
            self.changed.notify_one();
        }
    }
}

#[derive(Clone)]
pub(crate) struct ConnectionInfo {
    pub(crate) address: SocketAddr,
    pub(crate) control: Arc<ConnectionControl>,
}

pub(crate) struct RelayListener(pub(crate) TcpListener);
pub(crate) struct RelayStream {
    inner: TcpStream,
    control: Arc<ConnectionControl>,
}

impl Listener for RelayListener {
    type Io = RelayStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.0.accept().await {
                Ok(value) => value,
                Err(error) => {
                    // Persistent OS errors such as EMFILE must back off rather
                    // than spin: a bare yield would busy-loop and flood the
                    // log while the condition holds.
                    tracing::warn!(%error, "relay accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let control = Arc::new(ConnectionControl {
                blocked: AtomicBool::new(false),
                aborted: AtomicBool::new(false),
                read_waker: AtomicWaker::new(),
                write_waker: AtomicWaker::new(),
                changed: Notify::new(),
            });
            return (
                RelayStream {
                    inner: stream,
                    control,
                },
                address,
            );
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
impl Connected<IncomingStream<'_, RelayListener>> for ConnectionInfo {
    fn connect_info(stream: IncomingStream<'_, RelayListener>) -> Self {
        Self {
            address: *stream.remote_addr(),
            control: stream.io().control.clone(),
        }
    }
}
impl AsyncRead for RelayStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.control.poll_aborted(cx, &self.control.read_waker) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for RelayStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.control.poll_aborted(cx, &self.control.write_waker) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.control.set_blocked(result.is_pending());
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.control.poll_aborted(cx, &self.control.write_waker) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.control.set_blocked(result.is_pending());
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.control.poll_aborted(cx, &self.control.write_waker) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.control.poll_aborted(cx, &self.control.write_waker) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.control.set_blocked(result.is_pending());
        result
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        self.control.read_waker.take();
        self.control.write_waker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn abort_wakes_a_pending_read_and_refuses_later_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = TcpStream::connect(address).await.unwrap();
        let (mut stream, _) = RelayListener(listener).accept().await;
        let control = stream.control.clone();
        let mut byte = [0];
        let mut read = Box::pin(stream.read(&mut byte));
        assert!(futures::poll!(read.as_mut()).is_pending());
        control.abort();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            stream.write(b"later frame").await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            stream.flush().await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        drop((stream, peer));
    }
}
