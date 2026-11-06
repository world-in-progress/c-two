//! Local byte streams. Protocol framing and runtime policy stay in their owners.
//!
//! `write_all` is an inherent method on the stream and its owned write half:
//! cancelling a partially written operation stops local I/O so the next frame
//! can never reuse a truncated stream. Remote read guards retain the actual OS
//! stream until their last read ends. A completed write has drained the
//! transport adapter; peer consumption still requires a protocol acknowledgement.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

pub use c2_config::{LocalEndpoint, LocalEndpointContext};
pub use c2_local_security::{is_peer_identity_mismatch, peer_identity_mismatch};

mod observation;
pub use observation::{
    ConnectionClosed, ConnectionEndReason, ConnectionEvent, ConnectionEventSource,
    ConnectionEvents, ConnectionEventsError, ConnectionObservation, ConnectionPublisher,
    ConnectionSide, is_peer_closed_error,
};

mod credential;
pub use credential::{
    ENDPOINT_CREDENTIAL_MAX_BYTES, ENDPOINT_CREDENTIAL_SCHEMA_VERSION, EndpointCredentialError,
    EndpointCredentialErrorKind,
};

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
    incarnation: [u8; 16],
}

impl EndpointCredential {
    pub fn endpoint(&self) -> &LocalEndpoint {
        &self.endpoint
    }

    /// The listener incarnation; this is metadata, not a capability.
    #[cfg(unix)]
    pub fn incarnation(&self) -> [u8; 16] {
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
            incarnation,
        }
    }

    #[cfg(unix)]
    pub(crate) fn identity(&self) -> UnixSocketIdentity {
        self.identity
    }

    #[cfg(windows)]
    pub(crate) fn kernel_managed(endpoint: LocalEndpoint) -> Self {
        Self { endpoint }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnixSocketIdentity {
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
    /// A recognizable gate or marker belongs to a different namespace disk
    /// format and must not be adopted or rewritten in place.
    UnsupportedNamespaceFormat,
}

impl EndpointUnverifiedReason {
    /// Canonical spelling shared by inspect, bind diagnostics, and CLI output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsafeDirectory => "unsafe-directory",
            Self::Symlink => "symlink",
            Self::UnexpectedObject => "unexpected-object",
            Self::ForeignOwner => "foreign-owner",
            Self::MissingOwnership => "missing-ownership",
            Self::InvalidOwnership => "invalid-ownership",
            Self::InvalidRecord => "invalid-record",
            Self::RecordMismatch => "record-mismatch",
            Self::CoordinatorMissing => "coordinator-missing",
            Self::CoordinatorReplaced => "coordinator-replaced",
            Self::InitializationIncomplete => "initialization-incomplete",
            Self::UnsupportedNamespaceFormat => "unsupported-namespace-format",
        }
    }
}

impl std::fmt::Display for EndpointUnverifiedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for EndpointUnverifiedReason {}

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

/// Native I/O details and an optional named refusal for a listener bind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointBindCause {
    pub error: EndpointIoError,
    pub unverified_reason: Option<EndpointUnverifiedReason>,
}

impl From<&io::Error> for EndpointBindCause {
    fn from(error: &io::Error) -> Self {
        let mut source = error
            .get_ref()
            .map(|source| source as &(dyn std::error::Error + 'static));
        let mut unverified_reason = None;
        while let Some(current) = source {
            if let Some(reason) = current.downcast_ref::<EndpointUnverifiedReason>() {
                unverified_reason = Some(*reason);
                break;
            }
            source = current.source();
        }
        Self {
            error: EndpointIoError::from(error),
            unverified_reason,
        }
    }
}

impl From<&io::Error> for EndpointIoError {
    fn from(error: &io::Error) -> Self {
        let source = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<ContextIoError>())
            .map_or(error, |context| &context.source);
        Self {
            kind: source.kind(),
            raw_os_error: source.raw_os_error(),
        }
    }
}

impl EndpointIoError {
    /// Stable local-I/O category; the original ErrorKind remains available.
    pub fn category(&self) -> &'static str {
        match self.kind {
            io::ErrorKind::PermissionDenied => "permission_denied",
            io::ErrorKind::NotFound => "not_found",
            io::ErrorKind::ConnectionRefused => "connection_refused",
            io::ErrorKind::AddrInUse => "address_in_use",
            _ => "other",
        }
    }
}

#[derive(Debug)]
struct ContextIoError {
    message: String,
    source: io::Error,
}

impl std::fmt::Display for ContextIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ContextIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Copy a local transport I/O error, retaining its native cause and markers.
///
/// OS errors are rebuilt from their native code. Local context and typed
/// refusals retain their payload; errors carrying only a diagnostic retain
/// that diagnostic and their kind.
pub fn clone_io_error(error: &io::Error) -> io::Error {
    if let Some(code) = error.raw_os_error() {
        return io::Error::from_raw_os_error(code);
    }
    if let Some(context) = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<ContextIoError>())
    {
        return io::Error::new(
            error.kind(),
            ContextIoError {
                message: context.message.clone(),
                source: clone_io_error(&context.source),
            },
        );
    }
    if let Some(reason) = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<EndpointUnverifiedReason>())
    {
        return io::Error::new(error.kind(), *reason);
    }
    if let Some(copy) = c2_local_security::clone_peer_identity_mismatch(error) {
        return copy;
    }
    io::Error::new(error.kind(), error.to_string())
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

/// Caller-selected logical addresses in one canonical protocol namespace.
/// Constructing a scope performs no filesystem access and takes no sweep lease.
#[derive(Clone)]
pub struct EndpointSweepScope {
    endpoint: LocalEndpoint,
    targets: Vec<LocalEndpoint>,
}

impl EndpointSweepScope {
    pub fn from_addresses(endpoint: &LocalEndpoint, addresses: &[String]) -> io::Result<Self> {
        let mut targets = Vec::with_capacity(addresses.len());
        for address in addresses {
            let target = endpoint.context().endpoint(address)?;
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
        Self::for_context(&LocalEndpointContext::default_for_platform()?)
    }

    /// Opens the namespace of a captured context without resolving environment
    /// configuration again. Derivation is pure; native directory checks still
    /// govern every maintenance operation.
    pub fn for_context(context: &LocalEndpointContext) -> io::Result<Self> {
        Self::for_endpoint(&context.endpoint("ipc://c2-endpoint-sweep")?)
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

struct AbortState {
    closed: AtomicBool,
    peer_ended: AtomicBool,
    reader: AtomicWaker,
    writer: AtomicWaker,
}

/// Stop pending local I/O without closing a stream retained by a remote reader.
/// This metadata-only handle does not extend the OS stream's lifetime.
#[derive(Clone)]
pub struct AbortHandle(Arc<AbortState>);

impl AbortHandle {
    fn new() -> Self {
        Self(Arc::new(AbortState {
            closed: AtomicBool::new(false),
            peer_ended: AtomicBool::new(false),
            reader: AtomicWaker::new(),
            writer: AtomicWaker::new(),
        }))
    }

    /// Wake local I/O waiters with ConnectionAborted. The OS stream closes
    /// only when its I/O owners and all remote read guards have been dropped.
    pub fn abort(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.reader.wake();
        self.0.writer.wake();
    }

    pub fn is_aborted(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
}

struct StreamOwner {
    stream: Mutex<platform::Stream>,
    abort: AbortHandle,
}

impl Drop for StreamOwner {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Retains the original OS stream while an admitted payload reads peer memory.
/// Drop this guard when that remote read interval ends, before handing the
/// local copy to an application. It provides no I/O or cancellation authority.
pub struct RemoteReadGuard {
    _owner: Arc<StreamOwner>,
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
    owner: Arc<StreamOwner>,
}

async fn within_connect_deadline<T>(
    deadline: Option<Instant>,
    connect: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let deadline = deadline
        .map(tokio::time::Instant::from_std)
        .unwrap_or_else(|| tokio::time::Instant::now() + DEFAULT_CONNECT_TIMEOUT);
    let expired = || io::Error::new(io::ErrorKind::TimedOut, "local connection deadline expired");
    if tokio::time::Instant::now() >= deadline {
        return Err(expired());
    }
    let connected = tokio::time::timeout_at(deadline, connect)
        .await
        .map_err(|_| expired())??;
    // Native identity and directory validation can complete synchronously.
    if tokio::time::Instant::now() >= deadline {
        return Err(expired());
    }
    Ok(connected)
}

impl LocalStream {
    fn from_inner(inner: platform::Stream) -> Self {
        let abort = AbortHandle::new();
        Self {
            owner: Arc::new(StreamOwner {
                stream: Mutex::new(inner),
                abort,
            }),
        }
    }

    pub async fn connect(endpoint: &LocalEndpoint, timeout: Duration) -> io::Result<Self> {
        Self::connect_until(endpoint, Some(Instant::now() + timeout)).await
    }

    /// Establish the stream under one caller-owned deadline on both platforms.
    /// With no deadline, retain the finite default connection phase guard.
    pub async fn connect_until(
        endpoint: &LocalEndpoint,
        deadline: Option<Instant>,
    ) -> io::Result<Self> {
        let stream = within_connect_deadline(deadline, platform::connect(endpoint)).await?;
        Ok(Self::from_inner(stream))
    }

    /// Whether this connected stream can use session-local shared memory.
    /// Call after connect/accept and before splitting. Windows compares the
    /// peer session from this pipe handle with this process's TokenSessionId;
    /// query failures remain errors. Unix connected streams return true.
    pub fn shared_memory_available(&self) -> io::Result<bool> {
        let stream = self
            .owner
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.owner.abort.is_aborted() {
            return Err(aborted());
        }
        platform::shared_memory_available(&stream)
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.owner.abort.clone()
    }

    /// Retain this physical stream for one already-admitted remote read.
    /// The connection core seals new admission when deactivating the connection.
    pub fn retain_for_remote_read(&self) -> RemoteReadGuard {
        RemoteReadGuard {
            _owner: self.owner.clone(),
        }
    }

    /// Native read-side proof, available before this stream physically closes.
    /// Local cancellation and write failures never set this fact.
    pub fn peer_ended(&self) -> bool {
        self.owner.abort.0.peer_ended.load(Ordering::Acquire)
    }

    pub fn into_split(self) -> (LocalReadHalf, LocalWriteHalf) {
        let abort = self.abort_handle();
        let writer = Self {
            owner: self.owner.clone(),
        };
        (
            LocalReadHalf(self),
            LocalWriteHalf {
                inner: writer,
                abort,
            },
        )
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all_with_timeout(bytes, c2_config::BaseIpcConfig::default().write_timeout())
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
        let connected = tokio::try_join!(
            Self::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
            listener.accept()
        );
        let closed = tokio::task::spawn_blocking(move || listener.close()).await?;
        match closed {
            EndpointReapResult::Reaped
            | EndpointReapResult::AlreadyAbsent
            | EndpointReapResult::NotApplicable => connected,
            other => Err(io::Error::other(format!(
                "pair listener cleanup failed: {other:?}"
            ))),
        }
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
        this.owner.abort.0.reader.register(cx.waker());
        let mut stream = this
            .owner
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.owner.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        let before = buf.filled().len();
        let had_capacity = buf.remaining() != 0;
        let result = Pin::new(&mut *stream).poll_read(cx, buf);
        match &result {
            Poll::Ready(Ok(())) if had_capacity && buf.filled().len() == before => {
                this.owner.abort.0.peer_ended.store(true, Ordering::Release);
            }
            Poll::Ready(Err(error)) => {
                if observation::ConnectionEnd::read_error("read", error).is_peer_end() {
                    this.owner.abort.0.peer_ended.store(true, Ordering::Release);
                }
            }
            _ => {}
        }
        result
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.owner.abort.0.writer.register(cx.waker());
        let mut stream = this
            .owner
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.owner.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        Pin::new(&mut *stream).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.owner.abort.0.writer.register(cx.waker());
        let mut stream = this
            .owner
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if this.owner.abort.is_aborted() {
            return Poll::Ready(Err(aborted()));
        }
        platform::poll_flush(&mut stream, cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub struct LocalReadHalf(LocalStream);
impl LocalReadHalf {
    pub fn retain_for_remote_read(&self) -> RemoteReadGuard {
        self.0.retain_for_remote_read()
    }

    pub fn peer_ended(&self) -> bool {
        self.0.peer_ended()
    }
}
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
    inner: LocalStream,
    abort: AbortHandle,
}
impl LocalWriteHalf {
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all_with_timeout(bytes, c2_config::BaseIpcConfig::default().write_timeout())
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
    let mut guard = AbortOnDrop(Some(abort.clone()));
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
        Self::bind_until(endpoint, None)
    }

    /// Wait for listener ownership within the caller's startup deadline.
    /// With no deadline, use the existing bounded coordinator wait.
    pub fn bind_until(endpoint: &LocalEndpoint, deadline: Option<Instant>) -> io::Result<Self> {
        platform::Listener::bind_until(endpoint, deadline).map(Self)
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
    if credential.endpoint() != endpoint {
        return EndpointReapResult::StaleTarget;
    }
    platform::reap_endpoint(credential.endpoint(), credential)
}

#[cfg(test)]
mod credential_tests;
#[cfg(test)]
mod error_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod write_completion_tests {
    use super::*;

    struct FaultWriter {
        bytes: Vec<u8>,
        limit: usize,
        flush: u8,
    }
    impl AsyncWrite for FaultWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let count = bytes.len().min(this.limit.saturating_sub(this.bytes.len()));
            if count == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "partial write",
                )));
            }
            this.bytes.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.flush {
                1 => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "flush after full frame",
                ))),
                2 => Poll::Pending,
                _ => Poll::Ready(Ok(())),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    fn abort() -> AbortHandle {
        AbortHandle::new()
    }

    #[tokio::test(start_paused = true)]
    async fn complete_write_aborts_on_flush_error_and_configured_timeout() {
        for fault in [1, 2] {
            let mut writer = FaultWriter {
                bytes: Vec::new(),
                limit: 100,
                flush: fault,
            };
            let abort = abort();
            let start = tokio::time::Instant::now();
            let error = write_complete(
                &mut writer,
                abort.clone(),
                b"complete frame",
                Duration::from_millis(175),
            )
            .await
            .unwrap_err();
            assert_eq!(writer.bytes, b"complete frame");
            assert!(abort.is_aborted());
            assert_eq!(
                error.kind(),
                if fault == 1 {
                    io::ErrorKind::BrokenPipe
                } else {
                    io::ErrorKind::TimedOut
                }
            );
            if fault == 2 {
                assert_eq!(start.elapsed(), Duration::from_millis(175));
            }
        }
    }

    #[tokio::test]
    async fn partial_write_error_aborts_local_io() {
        let mut writer = FaultWriter {
            bytes: Vec::new(),
            limit: 3,
            flush: 0,
        };
        let abort = abort();
        write_complete(
            &mut writer,
            abort.clone(),
            b"complete frame",
            c2_config::BaseIpcConfig::default().write_timeout(),
        )
        .await
        .unwrap_err();
        assert_eq!(writer.bytes, b"com");
        assert!(abort.is_aborted());
    }
}
