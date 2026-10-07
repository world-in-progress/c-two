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

mod credential;
pub use credential::{
    ENDPOINT_CREDENTIAL_MAX_BYTES, ENDPOINT_CREDENTIAL_SCHEMA_VERSION, EndpointCredentialError,
    EndpointCredentialErrorKind,
};

pub mod owner;
pub use owner::{OwnerControlKeepalive, OwnerControlReceiver, owner_control_pair};

#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
mod unix_managed;

/// An opaque proof of one exact OS endpoint object created by this runtime.
///
/// Unix credentials contain the socket identity and listener
/// incarnation, so an old credential can never authorize removal of a newer
/// listener that reused the same socket path. Windows credentials describe a
/// kernel-managed named pipe and are not Unix cleanup capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointCredential {
    endpoint: LocalEndpoint,
    #[cfg(unix)]
    identity: UnixSocketIdentity,
    #[cfg(unix)]
    incarnation: Option<[u8; 16]>,
}

impl EndpointCredential {
    pub fn endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
    }

    /// The listener incarnation; this is metadata, not a capability.
    #[cfg(unix)]
    pub fn incarnation(&self) -> Option<[u8; 16]> {
        self.incarnation
    }

    #[cfg(unix)]
    pub(crate) fn unix_managed(
        endpoint: LocalEndpoint,
        identity: UnixSocketIdentity,
        incarnation: [u8; 16],
    ) -> Self {
        Self {
            endpoint,
            identity,
            incarnation: Some(incarnation),
        }
    }

    #[cfg(unix)]
    pub(crate) fn identity(&self) -> Option<UnixSocketIdentity> {
        Some(self.identity)
    }

    #[cfg(windows)]
    pub(crate) fn kernel_managed(endpoint: LocalEndpoint) -> Self {
        Self { endpoint }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnixSocketIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) changed_secs: i64,
    pub(crate) changed_nanos: i64,
}

/// Why the native layer could not prove that an endpoint is safe to reap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointUnverifiedReason {
    UnsafeDirectory,
    Symlink,
    UnexpectedObject,
    ForeignOwner,
    MissingOwnership,
    InvalidOwnership,
    InvalidRecord,
    RecordMismatch,
    /// The managed namespace records a coordinator gate but the gate entry is
    /// gone. Creating a replacement inode would be a second lock.
    CoordinatorMissing,
    /// The coordinator gate entry no longer names the inode the namespace
    /// marker recorded.
    CoordinatorReplaced,
    /// A managed namespace contains endpoint objects but no verifiable first
    /// initialization identity. Nothing in it may be retired automatically.
    InitializationIncomplete,
}

/// A read-only observation of a local endpoint.
#[derive(Debug)]
pub enum EndpointInspection {
    Absent,
    Present(EndpointCredential),
    Unverified(EndpointUnverifiedReason),
    /// The platform owns endpoint lifetime inside a kernel namespace (Windows
    /// named pipes). This observes the platform, not existence: there is no
    /// filesystem entry that could prove one live instance is present.
    KernelManaged,
    IoError(io::Error),
}

/// Result of an identity-constrained, nonblocking endpoint reap.
#[derive(Debug)]
pub enum EndpointReapResult {
    Reaped,
    AlreadyAbsent,
    Busy,
    StaleTarget,
    Unverified(EndpointUnverifiedReason),
    /// There is no Unix socket directory entry to collect on this platform.
    /// This does not claim that a matching live endpoint exists.
    NotApplicable,
    IoError(io::Error),
}

/// Bounded OS error details retained in a sweep batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointIoError {
    pub kind: io::ErrorKind,
    pub raw_os_error: Option<i32>,
}

impl From<&io::Error> for EndpointIoError {
    fn from(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        }
    }
}

/// Bounds one incremental maintenance batch. The default is a scheduling
/// target, not a hard real-time guarantee for filesystem calls.
#[derive(Clone, Copy, Debug)]
pub struct SweepBudget {
    pub max_entries: usize,
    pub max_duration: Duration,
}

impl Default for SweepBudget {
    fn default() -> Self {
        Self {
            max_entries: 64,
            max_duration: Duration::from_millis(10),
        }
    }
}

/// Bounded counters for one sweep batch. No directory-wide entry list is
/// retained or returned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepBatch {
    pub entries_visited: usize,
    pub endpoints_examined: usize,
    pub reaped: usize,
    pub already_absent: usize,
    pub busy: usize,
    pub stale_target: usize,
    pub unverified: usize,
    pub io_errors: usize,
    pub last_io_error: Option<EndpointIoError>,
    pub not_applicable: usize,
    /// Managed-v2 lease entries retired whose socket was already absent.
    pub leases_retired: usize,
    /// True only on the batch of a round that reached directory EOF.
    pub round_complete: bool,
    /// True when the round ended before EOF because the verified namespace
    /// directory was replaced. Every later batch of the same sweep keeps this
    /// set and never reports `round_complete`, so a caller that only checks
    /// `round_complete` cannot mistake an interrupted round for full coverage.
    pub round_interrupted: bool,
    /// True when the managed namespace no longer names the verified directory
    /// this sweep opened, including the terminal batches after that detection.
    pub namespace_changed: bool,
}

/// Stateful, explicitly driven maintenance over the current user's local
/// endpoint namespace. Dropping this value interrupts the round; a new sweep
/// always starts at the beginning and does not inherit completion state.
pub struct EndpointSweep(platform::EndpointSweep);

/// A bounded set of logical addresses in one canonical protocol namespace.
/// Constructing a scope performs no filesystem access and takes no sweep lease.
#[derive(Clone)]
pub struct EndpointSweepScope {
    endpoint: LocalEndpoint,
    targets: Vec<LocalEndpoint>,
}

impl EndpointSweepScope {
    pub const MAX_ADDRESSES: usize = 4096;

    pub fn from_addresses(endpoint: &LocalEndpoint, addresses: &[String]) -> io::Result<Self> {
        if addresses.len() > Self::MAX_ADDRESSES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many endpoint sweep addresses",
            ));
        }
        let root = std::path::Path::new(endpoint.os_name()).parent();
        let mut targets = Vec::with_capacity(addresses.len());
        for address in addresses {
            let target = LocalEndpoint::from_address(address)?;
            if std::path::Path::new(target.os_name()).parent() != root {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "endpoint sweep target is outside its namespace",
                ));
            }
            targets.push(target);
        }
        Ok(Self {
            endpoint: endpoint.clone(),
            targets,
        })
    }
}

impl EndpointSweep {
    /// Validates an address scope before any iterator or process lease exists.
    pub fn scope_for_addresses(
        endpoint: &LocalEndpoint,
        addresses: &[String],
    ) -> io::Result<EndpointSweepScope> {
        EndpointSweepScope::from_addresses(endpoint, addresses)
    }

    /// Opens the default managed namespace derived from `LocalEndpoint`
    /// authority. It is not a second hardcoded directory.
    pub fn open() -> io::Result<Self> {
        platform::EndpointSweep::open().map(Self)
    }

    /// Opens the managed namespace that contains `endpoint`.
    pub fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        platform::EndpointSweep::for_endpoint(endpoint).map(Self)
    }

    /// Opens a sweep restricted to the validated logical-address slots.
    /// An empty scope selects no endpoint or ownership entries.
    pub fn for_scope(scope: &EndpointSweepScope) -> io::Result<Self> {
        platform::EndpointSweep::for_scope(&scope.endpoint, &scope.targets).map(Self)
    }

    pub fn next_batch(&mut self, budget: SweepBudget) -> SweepBatch {
        self.0.next_batch(budget)
    }
}

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

    /// Returns the exact endpoint object credential for explicit lifecycle
    /// management by a caller that already owns this listener.
    pub fn credential(&self) -> EndpointCredential {
        self.0.credential()
    }

    /// Closes the listener and reports the native socket cleanup result.
    /// Unix listeners retire their lease entry under the namespace gate.
    pub fn close(self) -> EndpointReapResult {
        self.0.close()
    }
}

/// Inspects an endpoint without creating ownership metadata or contacting a
/// business listener.
pub fn inspect_endpoint(endpoint: &LocalEndpoint) -> EndpointInspection {
    platform::inspect_endpoint(endpoint)
}

/// Reaps only the exact endpoint object represented by `credential`.
pub fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    platform::reap_endpoint(endpoint, credential)
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod credential_tests;
