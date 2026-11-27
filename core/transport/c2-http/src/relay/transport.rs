//! Relay HTTP connection drivers.

use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::ConnectInfo;
use http::{Request, Response, Version};
use http_body::Frame;
use http_body_util::BodyExt;
use hyper_util::rt::{TokioIo, TokioTimer};
use iroh::endpoint::{RecvStream, SendStream, VarInt};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::iroh_transport::AuthenticatedIrohPeer;
use super::stream_control::ResponseStreamControl;

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_FRAME_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ListenerSurface {
    LocalControl,
    PublicEdge,
    IrohMesh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransportKind {
    Http1,
    Http2,
    IrohHttp1,
}

#[derive(Clone)]
pub(super) struct HttpListener {
    pub(super) app: Router,
    pub(super) surface: ListenerSurface,
    pub(super) tls: Option<TlsAcceptor>,
    pub(super) memory_budget: u64,
}

impl HttpListener {
    pub(super) async fn serve(self, listener: TcpListener, stop: CancellationToken) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => {}
                result = listener.accept() => match result {
                    Ok((stream, addr)) => {
                        let driver = self.clone();
                        let stop = stop.clone();
                        connections.spawn(async move {
                            if let Err(error) = driver.serve_connection(stream, addr, stop).await {
                                tracing::debug!(%addr, %error, "relay HTTP connection ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "relay accept failed");
                        tokio::select! {
                            _ = stop.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        }
                    }
                }
            }
        }
        drop(listener);
        connections.join_all().await;
    }

    async fn serve_connection(
        self,
        stream: TcpStream,
        addr: SocketAddr,
        stop: CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        stream.set_nodelay(true)?;
        if let Some(tls) = &self.tls {
            let stream = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                result = tls.accept(stream) => result?,
            };
            if stream.get_ref().1.alpn_protocol() == Some(b"h2") {
                self.serve_h2(stream, addr, stop).await?;
            } else {
                self.serve_h1(stream, addr, stop).await?;
            }
        } else {
            let (stream, is_h2) = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                result = detect_plaintext(stream) => result?,
            };
            if is_h2 {
                self.serve_h2(stream, addr, stop).await?;
            } else {
                self.serve_h1(stream, addr, stop).await?;
            }
        }
        Ok(())
    }

    async fn request<B>(
        &self,
        request: Request<B>,
        addr: SocketAddr,
        kind: TransportKind,
        control: Arc<ResponseStreamControl>,
    ) -> Response<Body>
    where
        B: HttpBody<Data = Bytes> + Send + 'static,
        B::Error: Into<axum::BoxError>,
    {
        let mut request = request.map(Body::new);
        let extensions = request.extensions_mut();
        extensions.insert(ConnectInfo(addr));
        extensions.insert(self.surface);
        extensions.insert(kind);
        extensions.insert(control);
        match self.app.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    async fn serve_h1<T>(
        self,
        stream: T,
        addr: SocketAddr,
        stop: CancellationToken,
    ) -> Result<(), hyper::Error>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let active = Arc::new(Mutex::new(None));
        let stream = H1TransportStream {
            inner: stream,
            active: active.clone(),
        };
        let service = hyper::service::service_fn(move |request| {
            let control: Arc<ResponseStreamControl> = Arc::default();
            if let Some(prev) = active.lock().replace(control.clone()) {
                prev.clear_abort_waker();
            }
            let driver = self.clone();
            async move {
                Ok::<_, std::convert::Infallible>(
                    driver
                        .request(request, addr, TransportKind::Http1, control)
                        .await,
                )
            }
        });
        let mut conn = std::pin::pin!(
            hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
        );
        tokio::select! {
            result = &mut conn => result,
            _ = stop.cancelled() => {
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        }
    }

    async fn serve_h2<T>(
        self,
        stream: T,
        addr: SocketAddr,
        stop: CancellationToken,
    ) -> Result<(), h2::Error>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        // The receive window is bounded per stream, with room for independent
        // sibling streams at connection scope. Frames and socket/TLS buffers
        // are transport overhead outside the accounted payload budget.
        let stream_window = (self.memory_budget / 16).clamp(16 * 1024, 1024 * 1024) as u32;
        let mut builder = h2::server::Builder::new();
        builder
            .initial_window_size(stream_window)
            .initial_connection_window_size(stream_window * 16)
            .max_send_buffer_size(H2_FRAME_BYTES);
        let mut conn = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = builder.handshake(stream) => result?,
        };
        let mut requests = JoinSet::new();
        let mut stopping = false;
        loop {
            tokio::select! {
                _ = stop.cancelled(), if !stopping => {
                    stopping = true;
                    conn.graceful_shutdown();
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
                result = conn.accept() => match result {
                    Some(Ok((request, respond))) => {
                        let driver = self.clone();
                        requests.spawn(async move {
                            if let Err(error) = driver.serve_h2_request(request, respond, addr).await {
                                tracing::debug!(%addr, %error, "relay HTTP/2 stream ended");
                            }
                        });
                    }
                    Some(Err(error)) => {
                        drop(conn);
                        requests.join_all().await;
                        return Err(error);
                    }
                    None => {
                        drop(conn);
                        requests.join_all().await;
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn serve_h2_request(
        &self,
        request: Request<h2::RecvStream>,
        mut respond: h2::server::SendResponse<Bytes>,
        addr: SocketAddr,
    ) -> Result<(), h2::Error> {
        let control: Arc<ResponseStreamControl> = Arc::default();
        let response = tokio::select! {
            _ = poll_fn(|cx| respond.poll_reset(cx)) => return Ok(()),
            response = self.request(request.map(H2RequestBody), addr, TransportKind::Http2, control.clone()) => response,
        };
        let (mut parts, mut body) = response.into_parts();
        parts.version = Version::HTTP_2;
        let mut send =
            respond.send_response(Response::from_parts(parts, ()), body.is_end_stream())?;
        if body.is_end_stream() {
            return Ok(());
        }
        let result = tokio::select! {
            biased;
            _ = control.aborted() => Ok(()),
            result = send_h2_body(&mut send, &mut body, &control) => result,
        };
        if control.is_aborted() {
            send.send_reset(h2::Reason::CANCEL);
            return Ok(());
        }

        // Dropping the last stream handle after queuing END_STREAM lets h2
        // schedule NO_ERROR behind the complete response if the request is
        // still open. Explicit send_reset would discard the queued frames.
        result
    }
}

async fn send_h2_body(
    send: &mut h2::SendStream<Bytes>,
    body: &mut Body,
    control: &ResponseStreamControl,
) -> Result<(), h2::Error> {
    loop {
        let frame = tokio::select! {
            reset = poll_fn(|cx| send.poll_reset(cx)) => return Err(reset?.into()),
            frame = body.frame() => frame,
        };
        // The body can signal abort from within its own poll.
        if control.is_aborted() {
            return Ok(());
        }
        let Some(frame) = frame else {
            return send.send_data(Bytes::new(), true);
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) => {
                send.send_reset(h2::Reason::INTERNAL_ERROR);
                return Ok(());
            }
        };
        match frame.into_data() {
            Ok(mut data) => {
                while !data.is_empty() {
                    send.reserve_capacity(data.len().min(H2_FRAME_BYTES));
                    let capacity = poll_fn(|cx| {
                        let result = if send.capacity() > 0 {
                            Poll::Ready(Some(Ok(send.capacity())))
                        } else {
                            send.poll_capacity(cx)
                        };
                        control.set_blocked(result.is_pending());
                        result
                    })
                    .await;
                    if control.is_aborted() {
                        return Ok(());
                    }
                    let Some(capacity) = capacity else {
                        return Ok(());
                    };
                    let len = capacity?.min(data.len()).min(H2_FRAME_BYTES);
                    send.send_data(data.split_to(len), false)?;
                }
            }
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    return send.send_trailers(trailers);
                }
            }
        }
    }
}

struct H2RequestBody(h2::RecvStream);

impl HttpBody for H2RequestBody {
    type Data = Bytes;
    type Error = h2::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, h2::Error>>> {
        if let Some(data) = ready!(self.0.poll_data(cx)) {
            let data = data?;
            self.0.flow_control().release_capacity(data.len())?;
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        Poll::Ready(
            ready!(self.0.poll_trailers(cx))?
                .map(Frame::trailers)
                .map(Ok),
        )
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }
}

async fn detect_plaintext(mut stream: TcpStream) -> io::Result<(PlaintextStream, bool)> {
    let mut prefix = [0; H2_PREFACE.len()];
    let peeked = stream.peek(&mut prefix).await?;
    if peeked == 0 || peeked == prefix.len() || !H2_PREFACE.starts_with(&prefix[..peeked]) {
        let is_h2 = peeked == prefix.len() && prefix == H2_PREFACE;
        return Ok((
            PlaintextStream {
                stream,
                prefix,
                replay: 0..0,
            },
            is_h2,
        ));
    }

    // Consume a fragmented preface so an unchanged peek cannot busy-loop.
    // Both protocol drivers still receive every byte in its original order.
    let mut len = 0;
    while len < prefix.len() {
        let n = stream.read(&mut prefix[len..]).await?;
        len += n;
        if n == 0 || prefix[..len] != H2_PREFACE[..len] {
            break;
        }
    }
    let is_h2 = len == prefix.len() && prefix == H2_PREFACE;
    Ok((
        PlaintextStream {
            stream,
            prefix,
            replay: 0..len,
        },
        is_h2,
    ))
}

struct PlaintextStream {
    stream: TcpStream,
    prefix: [u8; H2_PREFACE.len()],
    replay: std::ops::Range<usize>,
}

impl AsyncRead for PlaintextStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.replay.is_empty() {
            return Pin::new(&mut self.stream).poll_read(cx, buf);
        }
        let end = self.replay.end.min(self.replay.start + buf.remaining());
        buf.put_slice(&self.prefix[self.replay.start..end]);
        self.replay.start = end;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for PlaintextStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }
}

struct H1TransportStream<T> {
    inner: T,
    active: Arc<Mutex<Option<Arc<ResponseStreamControl>>>>,
}

impl<T> H1TransportStream<T> {
    fn poll_aborted(&self, cx: &mut Context<'_>) -> bool {
        self.active.lock().as_ref().is_some_and(|control| {
            control.register_abort_waker(cx.waker());
            control.is_aborted()
        })
    }

    fn set_blocked(&self, blocked: bool) {
        if let Some(control) = self.active.lock().as_ref() {
            control.set_blocked(blocked);
        }
    }
}

impl<T> Drop for H1TransportStream<T> {
    fn drop(&mut self) {
        if let Some(control) = self.active.lock().take() {
            control.clear_abort_waker();
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for H1TransportStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.poll_aborted(cx) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for H1TransportStream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.poll_aborted(cx) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.set_blocked(result.is_pending());
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.poll_aborted(cx) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.set_blocked(result.is_pending());
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.poll_aborted(cx) {
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
        if self.poll_aborted(cx) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.set_blocked(result.is_pending());
        result
    }
}

const ERROR_STREAM_EVICTED: VarInt = VarInt::from_u32(1);

/// A single HTTP exchange owns both halves of one QUIC stream.
pub(super) struct IrohQuicBiStream {
    send: SendStream,
    recv: RecvStream,
    control: Option<Arc<ResponseStreamControl>>,
    send_finished: bool,
}

impl IrohQuicBiStream {
    pub(super) fn new(
        send: SendStream,
        recv: RecvStream,
        control: Option<Arc<ResponseStreamControl>>,
    ) -> Self {
        Self {
            send,
            recv,
            control,
            send_finished: false,
        }
    }

    pub(super) async fn serve(
        send: SendStream,
        recv: RecvStream,
        app: Router,
        peer: AuthenticatedIrohPeer,
    ) {
        let control = Arc::<ResponseStreamControl>::default();
        let mut io = Self::new(send, recv, Some(control.clone()));
        let run = {
            let request_control = control.clone();
            let service =
                hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                    let mut request = request.map(Body::new);
                    let extensions = request.extensions_mut();
                    extensions.insert(peer);
                    extensions.insert(ListenerSurface::IrohMesh);
                    extensions.insert(TransportKind::IrohHttp1);
                    extensions.insert(request_control.clone());
                    app.clone().oneshot(request)
                });
            // Response production may be asleep waiting on a Body. Wake and
            // reset the send half even if Hyper is not polling socket writes.
            tokio::select! {
                biased;
                _ = control.aborted() => None,
                result = hyper::server::conn::http1::Builder::new()
                    .half_close(true)
                    .keep_alive(false)
                    .serve_connection(TokioIo::new(&mut io), service) => Some(result),
            }
        };
        match run {
            None => {
                io.evict();
            }
            Some(Ok(())) => {
                let _ = io.finish_send();
            }
            Some(Err(error)) => {
                tracing::debug!(%error, "iroh HTTP stream ended");
            }
        }
        // Send the response FIN before stopping any unread request bytes.
        let _ = io.recv.stop(VarInt::from_u32(0));
    }

    fn evict(&mut self) -> io::Error {
        let _ = self.send.reset(ERROR_STREAM_EVICTED);
        io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "relay response stream evicted",
        )
    }

    fn finish_send(&mut self) -> io::Result<()> {
        if !self.send_finished {
            self.send.finish()?;
            self.send_finished = true;
        }
        Ok(())
    }

    fn poll_send<T>(
        &mut self,
        cx: &mut Context<'_>,
        poll: impl FnOnce(Pin<&mut SendStream>, &mut Context<'_>) -> Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if let Some(control) = self.control.as_ref() {
            control.register_abort_waker(cx.waker());
            if control.is_aborted() {
                return Poll::Ready(Err(self.evict()));
            }
        }
        let result = poll(Pin::new(&mut self.send), cx);
        if let Some(control) = self.control.as_ref() {
            control.set_blocked(result.is_pending());
            if result.is_ready() {
                control.clear_abort_waker();
            }
        }
        result
    }
}

impl AsyncRead for IrohQuicBiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buffer)
    }
}

impl AsyncWrite for IrohQuicBiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_send(cx, |stream, cx| AsyncWrite::poll_write(stream, cx, bytes))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_send(cx, AsyncWrite::poll_flush)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.finish_send())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_wakes_a_pending_read_and_refuses_later_writes() {
        use tokio::io::AsyncWriteExt;
        let (inner, _peer) = tokio::io::duplex(16);
        let control: Arc<ResponseStreamControl> = Arc::default();
        let mut stream = H1TransportStream {
            inner,
            active: Arc::new(Mutex::new(Some(control.clone()))),
        };
        let mut byte = [0];
        let mut read = Box::pin(stream.read(&mut byte));
        assert!(futures::poll!(read.as_mut()).is_pending());
        control.abort();
        let error = tokio::time::timeout(Duration::from_secs(5), read)
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
    }
}
