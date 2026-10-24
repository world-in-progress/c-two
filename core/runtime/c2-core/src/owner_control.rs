//! Dedicated owner sessions. They share the IPC executor, never a business client.
//!
//! A session has one serialized request/reader. Once I/O is cancelled or fails,
//! its stream is aborted so a late reply cannot satisfy another operation.

use std::fmt;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use c2_config::{ConnectDeadline, ConnectOptions, LocalEndpoint, LocalEndpointContext};
use c2_local::{AbortHandle, EndpointContextBinding, LocalStream, OwnerControllerEndpoint};
use c2_wire::flags::{FLAG_CTRL, FLAG_RESPONSE};
use c2_wire::frame::{HEADER_SIZE, decode_frame_body, decode_total_len, encode_frame};
use c2_wire::msg_type::MsgType;
use c2_wire::owner_control::{
    MAX_OWNER_PAYLOAD_SIZE, OWNER_CONTROL_VERSION, OwnerRejection, OwnerRequest, OwnerResponse,
    OwnerStatusWire, OwnerTicketWire, OwnerWireError, decode_owner_response, encode_owner_request,
    encode_owner_response,
};
use parking_lot::{Mutex, MutexGuard};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

pub use c2_wire::owner_control::OwnerPhaseWire as OwnerPhase;

/// Owner control errors are local to this capability protocol, not CRM errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerControlErrorKind {
    Malformed,
    UnsupportedVersion,
    IdentityMismatch,
    OwnerPresent,
    InvalidGrant,
    GraceExpired,
    ShuttingDown,
    Unavailable,
    /// The operation did not begin sending its mutating request.
    NotSent,
    /// Sending began, but no valid confirmation or explicit rejection arrived.
    Indeterminate,
}

impl OwnerControlErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Malformed => "malformed owner control data",
            Self::UnsupportedVersion => "unsupported owner control version",
            Self::IdentityMismatch => "owner endpoint or host identity does not match",
            Self::OwnerPresent => "the host still has an owner",
            Self::InvalidGrant => "the recovery grant is invalid",
            Self::GraceExpired => "the owner recovery grace period expired",
            Self::ShuttingDown => "the host is shutting down",
            Self::Unavailable => "owner control is unavailable",
            Self::NotSent => "the owner control operation was not sent",
            Self::Indeterminate => "the owner control result is indeterminate",
        }
    }
}

/// Contains only bounded classifications. Ticket bytes and OS diagnostics are
/// deliberately excluded from both Display and Debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerControlError {
    kind: OwnerControlErrorKind,
    cause: Option<OwnerControlErrorKind>,
    /// A complete, correlated rejection is conclusive even after sending.
    rejected: bool,
}

impl OwnerControlError {
    pub(crate) const fn new(kind: OwnerControlErrorKind) -> Self {
        Self {
            kind,
            cause: None,
            rejected: false,
        }
    }

    pub const fn kind(&self) -> OwnerControlErrorKind {
        self.kind
    }

    /// Underlying failure when `kind()` is NotSent or Indeterminate.
    pub const fn cause_kind(&self) -> Option<OwnerControlErrorKind> {
        self.cause
    }

    fn refusal(kind: OwnerControlErrorKind) -> Self {
        Self {
            rejected: true,
            ..Self::new(kind)
        }
    }

    fn submission(self, sent: bool) -> Self {
        if self.rejected {
            return self;
        }
        Self {
            kind: if sent {
                OwnerControlErrorKind::Indeterminate
            } else {
                OwnerControlErrorKind::NotSent
            },
            cause: Some(self.cause.unwrap_or(self.kind)),
            rejected: false,
        }
    }
}

impl fmt::Display for OwnerControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.message())?;
        if let Some(cause) = self.cause {
            write!(f, ": {}", cause.message())?;
        }
        Ok(())
    }
}

impl std::error::Error for OwnerControlError {}

impl From<OwnerWireError> for OwnerControlError {
    fn from(error: OwnerWireError) -> Self {
        Self::new(match error {
            OwnerWireError::UnsupportedVersion => OwnerControlErrorKind::UnsupportedVersion,
            OwnerWireError::Malformed | OwnerWireError::PayloadTooLarge => {
                OwnerControlErrorKind::Malformed
            }
        })
    }
}

fn rejection(reason: OwnerRejection) -> OwnerControlError {
    OwnerControlError::refusal(match reason {
        OwnerRejection::Malformed => OwnerControlErrorKind::Malformed,
        OwnerRejection::UnsupportedVersion => OwnerControlErrorKind::UnsupportedVersion,
        OwnerRejection::IdentityMismatch => OwnerControlErrorKind::IdentityMismatch,
        OwnerRejection::OwnerPresent => OwnerControlErrorKind::OwnerPresent,
        OwnerRejection::InvalidGrant => OwnerControlErrorKind::InvalidGrant,
        OwnerRejection::GraceExpired => OwnerControlErrorKind::GraceExpired,
        OwnerRejection::ShuttingDown => OwnerControlErrorKind::ShuttingDown,
        OwnerRejection::Unavailable => OwnerControlErrorKind::Unavailable,
    })
}

fn unavailable<T>(_: T) -> OwnerControlError {
    OwnerControlError::new(OwnerControlErrorKind::Unavailable)
}

fn connect_error(error: io::Error) -> OwnerControlError {
    OwnerControlError::new(if c2_local::is_peer_identity_mismatch(&error) {
        OwnerControlErrorKind::IdentityMismatch
    } else {
        OwnerControlErrorKind::Unavailable
    })
}

fn malformed() -> OwnerControlError {
    OwnerControlError::new(OwnerControlErrorKind::Malformed)
}

/// Opaque recovery capability. Export explicitly to save it outside the owner
/// process; import explicitly to use it. There is no implicit serialization.
#[derive(Clone)]
pub struct OwnerRecoveryTicket {
    wire: OwnerTicketWire,
}

impl fmt::Debug for OwnerRecoveryTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerRecoveryTicket([REDACTED])")
    }
}

impl OwnerRecoveryTicket {
    /// Export the existing versioned RecoveryTicket payload, including its secret.
    pub fn export(&self) -> Vec<u8> {
        encode_owner_response(&OwnerResponse::RecoveryTicket(self.wire.clone()))
            .expect("a validated recovery ticket fits the owner codec")
    }

    /// Reject oversized, trailing, wrong-operation or invalid-identity input.
    pub fn import(bytes: &[u8]) -> Result<Self, OwnerControlError> {
        match decode_owner_response(bytes)? {
            OwnerResponse::RecoveryTicket(wire) => Self::from_wire(wire),
            _ => Err(malformed()),
        }
    }

    fn from_wire(wire: OwnerTicketWire) -> Result<Self, OwnerControlError> {
        validate_identity(
            &wire.address,
            &wire.server_id,
            &wire.server_instance_id,
            wire.epoch,
        )?;
        Ok(Self { wire })
    }
}

fn validate_identity(
    address: &str,
    server_id: &str,
    instance: &str,
    epoch: u64,
) -> Result<(), OwnerControlError> {
    let logical_id = LocalEndpoint::validate_address(address).map_err(|_| malformed())?;
    c2_config::validate_server_id(server_id).map_err(|_| malformed())?;
    // Server identity also has to fit the unchanged IPC handshake representation.
    if server_id.len() > u8::MAX as usize
        || instance.is_empty()
        || instance.len() > u8::MAX as usize
        || instance.trim() != instance
        || instance.chars().any(char::is_control)
        || epoch == 0
    {
        return Err(malformed());
    }
    if logical_id != server_id {
        return Err(OwnerControlError::new(
            OwnerControlErrorKind::IdentityMismatch,
        ));
    }
    Ok(())
}

/// A status observation contains no grant and is never a takeover receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerStatus {
    pub address: String,
    pub server_id: String,
    pub server_instance_id: String,
    pub epoch: u64,
    pub context_binding: [u8; 32],
    pub phase: OwnerPhase,
}

impl From<OwnerStatusWire> for OwnerStatus {
    fn from(wire: OwnerStatusWire) -> Self {
        Self {
            address: wire.address,
            server_id: wire.server_id,
            server_instance_id: wire.server_instance_id,
            epoch: wire.epoch,
            context_binding: wire.context_binding,
            phase: wire.phase,
        }
    }
}

fn check_status(
    status: &OwnerStatusWire,
    ticket: &OwnerTicketWire,
) -> Result<(), OwnerControlError> {
    check_status_identity(status, ticket)?;
    if status.epoch != ticket.epoch {
        return Err(OwnerControlError::new(
            OwnerControlErrorKind::IdentityMismatch,
        ));
    }
    Ok(())
}

fn check_status_identity(
    status: &OwnerStatusWire,
    ticket: &OwnerTicketWire,
) -> Result<(), OwnerControlError> {
    validate_identity(
        &status.address,
        &status.server_id,
        &status.server_instance_id,
        status.epoch,
    )?;
    if status.address != ticket.address
        || status.server_id != ticket.server_id
        || status.server_instance_id != ticket.server_instance_id
        || status.context_binding != ticket.context_binding
    {
        return Err(OwnerControlError::new(
            OwnerControlErrorKind::IdentityMismatch,
        ));
    }
    Ok(())
}

fn deadline(timeout: Option<Duration>) -> Result<ConnectDeadline, OwnerControlError> {
    let options = timeout.map_or(ConnectOptions::new(), |timeout| {
        ConnectOptions::new().with_timeout(timeout)
    });
    ConnectDeadline::start(options).map_err(|_| malformed())
}

fn check_deadline(deadline: ConnectDeadline) -> Result<(), OwnerControlError> {
    deadline.check("owner_control").map_err(unavailable)
}

async fn within_deadline<T>(
    deadline: ConnectDeadline,
    future: impl Future<Output = Result<T, OwnerControlError>>,
) -> Result<T, OwnerControlError> {
    check_deadline(deadline)?;
    match deadline.instant() {
        Some(instant) => tokio::time::timeout_at(instant.into(), future)
            .await
            .map_err(unavailable)?,
        None => future.await,
    }
}

fn matches_request(request: &OwnerRequest, response: &OwnerResponse) -> bool {
    matches!(
        (request, response),
        (_, OwnerResponse::Rejected(_))
            | (OwnerRequest::Hello, OwnerResponse::Status(_))
            | (
                OwnerRequest::GetRecoveryTicket,
                OwnerResponse::RecoveryTicket(_)
            )
            | (OwnerRequest::RevokeRecovery, OwnerResponse::Revoked)
            | (OwnerRequest::Takeover { .. }, OwnerResponse::TakenOver(_))
            | (
                OwnerRequest::RequestShutdown,
                OwnerResponse::ShutdownInitiated
            )
    )
}

async fn read_response<R: AsyncRead + Unpin>(
    reader: &mut R,
    request_id: u64,
) -> Result<OwnerResponse, OwnerControlError> {
    let mut header_bytes = [0; HEADER_SIZE];
    reader
        .read_exact(&mut header_bytes)
        .await
        .map_err(unavailable)?;
    let (total_len, body) = decode_total_len(&header_bytes).map_err(|_| malformed())?;
    if !(15..=12 + MAX_OWNER_PAYLOAD_SIZE as u32).contains(&total_len) {
        return Err(malformed());
    }
    let (header, _) = decode_frame_body(body, total_len).map_err(|_| malformed())?;
    if header.request_id != request_id || header.flags != (FLAG_CTRL | FLAG_RESPONSE) {
        return Err(malformed());
    }
    // Check the tag/version before allocating any variable-length body.
    let mut prefix = [0; 3];
    reader.read_exact(&mut prefix).await.map_err(unavailable)?;
    if prefix[0] != MsgType::OwnerControl.as_byte() {
        return Err(malformed());
    }
    if prefix[1] != OWNER_CONTROL_VERSION {
        return Err(OwnerControlError::new(
            OwnerControlErrorKind::UnsupportedVersion,
        ));
    }
    let mut payload = vec![0; header.payload_len()];
    payload[..prefix.len()].copy_from_slice(&prefix);
    reader
        .read_exact(&mut payload[prefix.len()..])
        .await
        .map_err(unavailable)?;
    decode_owner_response(&payload).map_err(Into::into)
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    next_request_id: &mut u64,
    request: &OwnerRequest,
    deadline: ConnectDeadline,
    sent: &mut bool,
) -> Result<OwnerResponse, OwnerControlError> {
    let payload = encode_owner_request(request)?;
    let request_id = *next_request_id;
    *next_request_id = request_id.checked_add(1).ok_or_else(|| unavailable(()))?;
    let frame = encode_frame(request_id, FLAG_CTRL, &payload);
    check_deadline(deadline)?;
    *sent = true;
    AsyncWriteExt::write_all(stream, &frame)
        .await
        .map_err(unavailable)?;
    // The correlated response confirms delivery. A Windows zero-byte flush
    // can fail after a rejecting peer closes, even with its full reply buffered.
    let response = read_response(stream, request_id).await?;
    if !matches_request(request, &response) {
        return Err(malformed());
    }
    Ok(response)
}

fn confirmed_response(response: OwnerResponse) -> Result<OwnerResponse, OwnerControlError> {
    match response {
        OwnerResponse::Rejected(reason) => Err(rejection(reason)),
        response => Ok(response),
    }
}

#[derive(Default)]
struct SessionAbortState {
    closed: bool,
    abort: Option<AbortHandle>,
}

#[derive(Default)]
struct SessionLifetime {
    state: Mutex<SessionAbortState>,
    changed: Notify,
}

impl SessionLifetime {
    fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    fn install(&self, abort: AbortHandle) -> Result<(), OwnerControlError> {
        let mut state = self.state.lock();
        if state.closed {
            drop(state);
            abort.abort();
            return Err(unavailable(()));
        }
        state.abort = Some(abort);
        Ok(())
    }

    fn close(&self) {
        let abort = {
            let mut state = self.state.lock();
            state.closed = true;
            state.abort.take()
        };
        if let Some(abort) = abort {
            abort.abort();
        }
        self.changed.notify_waiters();
    }

    async fn run<T>(
        &self,
        deadline: ConnectDeadline,
        future: impl Future<Output = Result<T, OwnerControlError>>,
    ) -> Result<T, OwnerControlError> {
        let closed = self.changed.notified();
        tokio::pin!(closed);
        // Register before testing the flag, so close cannot be missed.
        closed.as_mut().enable();
        if self.is_closed() {
            return Err(unavailable(()));
        }
        tokio::select! {
            biased;
            _ = closed => Err(unavailable(())),
            result = within_deadline(deadline, future) => result,
        }
    }
}

struct OwnerIo {
    raw: Option<OwnerControllerEndpoint>,
    stream: Option<LocalStream>,
    next_request_id: u64,
    identity: Option<OwnerStatusWire>,
}

impl OwnerIo {
    fn close(&mut self) {
        if let Some(mut raw) = self.raw.take() {
            raw.close();
        }
        if let Some(stream) = self.stream.take() {
            stream.abort_handle().abort();
        }
    }

    async fn activate(&mut self, lifetime: &SessionLifetime) -> Result<(), OwnerControlError> {
        if self.stream.is_none() {
            let stream = self
                .raw
                .as_mut()
                .ok_or_else(|| unavailable(()))?
                .activate()
                .await
                .map_err(unavailable)?;
            lifetime.install(stream.abort_handle())?;
            self.stream = Some(stream);
            self.raw = None;
        }
        Ok(())
    }
}

impl Drop for OwnerIo {
    fn drop(&mut self) {
        self.close();
    }
}

/// Current owner capability. This value is Send + Sync, but cannot be cloned.
/// Closing it or dropping it ends the owner relationship, independent of any
/// business calls or the host's drain transaction.
pub struct OwnerControl {
    io: Mutex<OwnerIo>,
    lifetime: SessionLifetime,
}

impl fmt::Debug for OwnerControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnerControl")
            .field("closed", &self.lifetime.is_closed())
            .finish_non_exhaustive()
    }
}

/// Create only raw native endpoints. No executor or reactor is activated here.
pub fn owner_control_pair() -> io::Result<(OwnerControl, c2_local::OwnerBootstrap)> {
    let (controller, bootstrap) = c2_local::owner_endpoint_pair()?;
    Ok((
        OwnerControl {
            io: Mutex::new(OwnerIo {
                raw: Some(controller),
                stream: None,
                next_request_id: 1,
                identity: None,
            }),
            lifetime: SessionLifetime::default(),
        },
        bootstrap,
    ))
}

/// Owns every post-acquisition exit, including expiry before the first write.
/// Drop consumes the slot while holding the lifetime lock, so neither a closer
/// nor a subsequent waiter can miss raw-endpoint cleanup or reuse poisoned I/O.
struct SessionUse<'a> {
    owner: &'a OwnerControl,
    io: Option<MutexGuard<'a, OwnerIo>>,
    pending: bool,
}

impl Drop for SessionUse<'_> {
    fn drop(&mut self) {
        let mut io = self.io.take().expect("transaction owns its I/O slot");
        let closed = {
            let mut state = self.owner.lifetime.state.lock();
            state.closed |= self.pending;
            if state.closed {
                io.close();
                state.abort = None;
            }
            // Keep closed-check and slot release in this same critical section.
            // close() either observes the unlocked slot or is handled above.
            drop(io);
            state.closed
        };
        if closed {
            self.owner.lifetime.changed.notify_waiters();
        }
    }
}

impl OwnerControl {
    fn from_stream(stream: LocalStream, next_request_id: u64, identity: OwnerStatusWire) -> Self {
        let lifetime = SessionLifetime::default();
        // A fresh session is not visible to another caller until this returns.
        lifetime
            .install(stream.abort_handle())
            .expect("new session is open");
        Self {
            io: Mutex::new(OwnerIo {
                raw: None,
                stream: Some(stream),
                next_request_id,
                identity: Some(identity),
            }),
            lifetime,
        }
    }

    fn slot(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<MutexGuard<'_, OwnerIo>, OwnerControlError> {
        check_deadline(deadline)?;
        let io = match deadline.instant() {
            Some(instant) => self
                .io
                .try_lock_until(instant)
                .ok_or_else(|| unavailable(()))?,
            None => self.io.lock(),
        };
        // Post-lock checks belong to SessionUse, including their cleanup.
        Ok(io)
    }

    fn transaction<T>(
        &self,
        request: OwnerRequest,
        timeout: Option<Duration>,
        accept: impl FnOnce(OwnerResponse, &mut OwnerIo) -> Result<T, OwnerControlError>,
    ) -> Result<T, OwnerControlError> {
        let deadline = deadline(timeout).map_err(|error| error.submission(false))?;
        // Initialize before taking the slot: an initializer wait cannot stop
        // close() from releasing a still-raw endpoint synchronously.
        let runtime = c2_ipc::shared_io_handle(deadline)
            .map_err(|error| unavailable(error).submission(false))?;
        let mut transaction = SessionUse {
            owner: self,
            io: Some(
                self.slot(deadline)
                    .map_err(|error| error.submission(false))?,
            ),
            pending: false,
        };
        check_deadline(deadline).map_err(|error| error.submission(false))?;
        if self.lifetime.is_closed() {
            return Err(unavailable(()).submission(false));
        }
        let io = transaction
            .io
            .as_mut()
            .expect("transaction owns its I/O slot");
        let result = runtime.block_on(self.lifetime.run(deadline, async {
            io.activate(&self.lifetime).await?;
            let OwnerIo {
                stream,
                next_request_id,
                ..
            } = &mut **io;
            let response = exchange(
                stream.as_mut().ok_or_else(|| unavailable(()))?,
                next_request_id,
                &request,
                deadline,
                &mut transaction.pending,
            )
            .await?;
            let response = confirmed_response(response)?;
            let value = accept(response, io)?;
            check_deadline(deadline)?;
            Ok(value)
        }));
        match result {
            Ok(value) => {
                transaction.pending = false;
                Ok(value)
            }
            Err(error) => {
                // A correlated negative ACK completes the transaction. It is
                // not a partial write or a lost reply, and does not end ownership.
                let sent = transaction.pending;
                if error.rejected {
                    transaction.pending = false;
                }
                Err(error.submission(sent))
            }
        }
    }

    /// The server issues a grant once; repeated calls return its same current
    /// grant until it is revoked. The caller is responsible for persistence.
    pub fn recovery_ticket(
        &self,
        timeout: Option<Duration>,
    ) -> Result<OwnerRecoveryTicket, OwnerControlError> {
        self.transaction(OwnerRequest::GetRecoveryTicket, timeout, |response, io| {
            let OwnerResponse::RecoveryTicket(wire) = response else {
                return Err(malformed());
            };
            let ticket = OwnerRecoveryTicket::from_wire(wire)?;
            let status = OwnerStatusWire {
                address: ticket.wire.address.clone(),
                server_id: ticket.wire.server_id.clone(),
                server_instance_id: ticket.wire.server_instance_id.clone(),
                epoch: ticket.wire.epoch,
                context_binding: ticket.wire.context_binding,
                phase: OwnerPhase::Armed,
            };
            if let Some(identity) = &io.identity {
                check_status(identity, &ticket.wire)?;
            }
            io.identity = Some(status);
            Ok(ticket)
        })
    }

    /// Idempotently revoke the current recovery grant on the server.
    pub fn revoke_recovery(&self, timeout: Option<Duration>) -> Result<(), OwnerControlError> {
        self.transaction(OwnerRequest::RevokeRecovery, timeout, |_, _| Ok(()))
    }

    /// Acknowledges native shutdown initiation only; it does not wait for drain.
    pub fn request_shutdown(&self, timeout: Option<Duration>) -> Result<(), OwnerControlError> {
        let result = self.transaction(OwnerRequest::RequestShutdown, timeout, |_, _| Ok(()));
        if result.is_ok() {
            self.close();
        }
        result
    }

    fn close_now(&self) {
        // Does not acquire the transaction slot or wait for the active request.
        self.lifetime.close();
        if let Some(mut io) = self.io.try_lock() {
            io.close();
        }
        // Pending raw activation is woken by `lifetime.close`; SessionUse drops
        // the raw endpoint on cancellation. An active stream was aborted above.
    }

    /// Immediately end the owner relationship, without waiting for a control
    /// reply or business drain. Repeated closes are harmless.
    pub fn close(&self) {
        self.close_now();
    }
}

impl Drop for OwnerControl {
    fn drop(&mut self) {
        self.close_now();
    }
}

/// A single takeover attempt and the successor capability that can be saved
/// before it is sent. Failure never consumes access to either stored ticket.
/// There is no automatic retry, old/new-ticket fallback, or success inference.
pub struct PreparedOwnerTakeover {
    ticket: OwnerRecoveryTicket,
    successor: OwnerRecoveryTicket,
    endpoint: LocalEndpoint,
    binding: EndpointContextBinding,
    attempted: AtomicBool,
    #[cfg(test)]
    connect_failure_for_test: Option<fn() -> io::Error>,
}

impl fmt::Debug for PreparedOwnerTakeover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedOwnerTakeover([REDACTED])")
    }
}

impl PreparedOwnerTakeover {
    pub(crate) fn prepare(
        ticket: OwnerRecoveryTicket,
        context: &LocalEndpointContext,
    ) -> Result<Self, OwnerControlError> {
        let epoch = ticket.wire.epoch.checked_add(1).ok_or_else(malformed)?;
        let endpoint = context
            .endpoint(&ticket.wire.address)
            .map_err(|_| malformed())?;
        let binding = EndpointContextBinding::capture(&endpoint).map_err(unavailable)?;
        if binding.digest() != ticket.wire.context_binding {
            return Err(OwnerControlError::new(
                OwnerControlErrorKind::IdentityMismatch,
            ));
        }
        binding.validate().map_err(unavailable)?;
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(unavailable)?;
        let successor = OwnerRecoveryTicket::from_wire(OwnerTicketWire {
            epoch,
            secret,
            ..ticket.wire.clone()
        })?;
        Ok(Self {
            ticket,
            successor,
            endpoint,
            binding,
            attempted: AtomicBool::new(false),
            #[cfg(test)]
            connect_failure_for_test: None,
        })
    }

    /// The original grant remains explicitly exportable after every outcome.
    pub fn recovery_ticket(&self) -> &OwnerRecoveryTicket {
        &self.ticket
    }

    /// Save this before commit if a lost acknowledgement must be recoverable.
    /// It becomes valid only when the server atomically installs the takeover.
    pub fn successor_ticket(&self) -> &OwnerRecoveryTicket {
        &self.successor
    }

    fn validate_binding(&self, deadline: ConnectDeadline) -> Result<(), OwnerControlError> {
        check_deadline(deadline)?;
        self.binding.validate().map_err(|error| {
            OwnerControlError::new(if error.kind() == io::ErrorKind::WouldBlock {
                OwnerControlErrorKind::Unavailable
            } else {
                OwnerControlErrorKind::IdentityMismatch
            })
        })?;
        if self.binding.digest() != self.ticket.wire.context_binding {
            return Err(OwnerControlError::new(
                OwnerControlErrorKind::IdentityMismatch,
            ));
        }
        check_deadline(deadline)
    }

    async fn candidate(
        &self,
        deadline: ConnectDeadline,
    ) -> Result<(OwnerIo, OwnerStatusWire), OwnerControlError> {
        self.validate_binding(deadline)?;
        let connect_timeout = deadline
            .remaining("owner_connect")
            .map_err(unavailable)?
            .map_or(c2_local::DEFAULT_CONNECT_TIMEOUT, |remaining| {
                remaining.min(c2_local::DEFAULT_CONNECT_TIMEOUT)
            });
        let connect = LocalStream::connect(&self.endpoint, connect_timeout);
        #[cfg(test)]
        let connect = async {
            match self.connect_failure_for_test {
                Some(fail) => Err(fail()),
                None => connect.await,
            }
        };
        let stream = connect.await.map_err(connect_error)?;
        // This guard aborts on every error or cancellation, including one that
        // arrives before the first control frame is sent.
        let mut io = OwnerIo {
            raw: None,
            stream: Some(stream),
            next_request_id: 1,
            identity: None,
        };
        self.validate_binding(deadline)?;
        let mut hello_sent = false;
        let response = exchange(
            io.stream.as_mut().expect("candidate owns its stream"),
            &mut io.next_request_id,
            &OwnerRequest::Hello,
            deadline,
            &mut hello_sent,
        )
        .await?;
        let binding_result = self.validate_binding(deadline);
        let response = confirmed_response(response)?;
        binding_result?;
        let OwnerResponse::Status(status) = response else {
            return Err(malformed());
        };
        check_status_identity(&status, &self.ticket.wire)?;
        Ok((io, status))
    }

    /// Observe the actual current epoch and phase. Even a later epoch is only
    /// an observation; it does not establish that this attempt ever succeeded.
    pub fn inspect(&self, timeout: Option<Duration>) -> Result<OwnerStatus, OwnerControlError> {
        let deadline = deadline(timeout)?;
        let runtime = c2_ipc::shared_io_handle(deadline).map_err(unavailable)?;
        runtime.block_on(within_deadline(deadline, async {
            let (_candidate, status) = self.candidate(deadline).await?;
            check_deadline(deadline)?;
            Ok(status.into())
        }))
    }

    /// Attempt exactly once. Hello is not a submission. Before Takeover starts
    /// sending, local failures are NotSent; after that, loss of a valid reply is
    /// Indeterminate. A complete Rejected response remains a definite refusal.
    pub fn commit(&self, timeout: Option<Duration>) -> Result<OwnerControl, OwnerControlError> {
        if self.attempted.swap(true, Ordering::AcqRel) {
            return Err(OwnerControlError::new(OwnerControlErrorKind::NotSent));
        }
        let deadline = deadline(timeout).map_err(|error| error.submission(false))?;
        let runtime = c2_ipc::shared_io_handle(deadline)
            .map_err(|error| unavailable(error).submission(false))?;
        let mut sent = false;
        let result = runtime.block_on(within_deadline(deadline, async {
            let (mut candidate, status) = self.candidate(deadline).await?;
            match status.phase {
                OwnerPhase::Armed => {
                    return Err(OwnerControlError::refusal(
                        OwnerControlErrorKind::OwnerPresent,
                    ));
                }
                OwnerPhase::OwnerMissing => {}
                OwnerPhase::Draining | OwnerPhase::Finished => {
                    return Err(OwnerControlError::refusal(
                        OwnerControlErrorKind::ShuttingDown,
                    ));
                }
                OwnerPhase::WatcherIoError => {
                    return Err(OwnerControlError::refusal(
                        OwnerControlErrorKind::Unavailable,
                    ));
                }
            }
            if status.epoch != self.ticket.wire.epoch {
                return Err(OwnerControlError::refusal(
                    OwnerControlErrorKind::InvalidGrant,
                ));
            }
            self.validate_binding(deadline)?;
            let response = exchange(
                candidate
                    .stream
                    .as_mut()
                    .expect("candidate owns its stream"),
                &mut candidate.next_request_id,
                &OwnerRequest::Takeover {
                    ticket: self.ticket.wire.clone(),
                    successor_secret: self.successor.wire.secret,
                },
                deadline,
                &mut sent,
            )
            .await?;
            let binding_result = self.validate_binding(deadline);
            let response = confirmed_response(response)?;
            binding_result?;
            let OwnerResponse::TakenOver(status) = response else {
                return Err(malformed());
            };
            check_status(&status, &self.successor.wire)?;
            if status.phase != OwnerPhase::Armed {
                return Err(malformed());
            }
            check_deadline(deadline)?;
            // The same stream now supervises the new epoch, even if the caller
            // never requests its (already pre-saved) successor grant.
            Ok(OwnerControl::from_stream(
                candidate.stream.take().expect("candidate owns its stream"),
                candidate.next_request_id,
                status,
            ))
        }));
        result.map_err(|error| error.submission(sent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_wire::frame::decode_frame;
    use c2_wire::owner_control::decode_owner_request;
    use tokio::runtime::Handle;

    fn ticket_wire() -> OwnerTicketWire {
        OwnerTicketWire {
            address: "ipc://owner-test".into(),
            server_id: "owner-test".into(),
            server_instance_id: "owner-instance".into(),
            epoch: 1,
            context_binding: [0x42; 32],
            secret: [b'Q'; 32],
        }
    }

    fn status_wire() -> OwnerStatusWire {
        let ticket = ticket_wire();
        OwnerStatusWire {
            address: ticket.address,
            server_id: ticket.server_id,
            server_instance_id: ticket.server_instance_id,
            epoch: ticket.epoch,
            context_binding: ticket.context_binding,
            phase: OwnerPhase::Armed,
        }
    }

    fn runtime() -> Handle {
        c2_ipc::shared_io_handle(ConnectDeadline::default()).unwrap()
    }

    #[test]
    fn local_connect_identity_refusal_survives_inspect_and_not_sent() {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("c2-owner-kind-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        let context = LocalEndpointContext::with_unix_root(root.path()).unwrap();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let mut wire = ticket_wire();
        wire.server_id = format!("owner-kind-{}", uuid::Uuid::new_v4().simple());
        wire.address = format!("ipc://{}", wire.server_id);
        let endpoint = context.endpoint(&wire.address).unwrap();
        let mut listener =
            runtime().block_on(async { c2_local::LocalListener::bind(&endpoint).unwrap() });
        wire.context_binding = EndpointContextBinding::capture(&endpoint).unwrap().digest();

        // Inject only the OS connect result. Both public methods still execute
        // candidate(), its real error projection, and the submission boundary.
        for (fail, expected) in [
            (
                (|| c2_local::peer_identity_mismatch("peer identity differs")) as fn() -> io::Error,
                OwnerControlErrorKind::IdentityMismatch,
            ),
            (
                || io::Error::new(io::ErrorKind::PermissionDenied, "peer identity differs"),
                OwnerControlErrorKind::Unavailable,
            ),
            (
                || io::Error::from(io::ErrorKind::NotFound),
                OwnerControlErrorKind::Unavailable,
            ),
            (
                || io::Error::from(io::ErrorKind::TimedOut),
                OwnerControlErrorKind::Unavailable,
            ),
        ] {
            let mut prepared = PreparedOwnerTakeover::prepare(
                OwnerRecoveryTicket::from_wire(wire.clone()).unwrap(),
                &context,
            )
            .unwrap();
            prepared.connect_failure_for_test = Some(fail);
            let error = prepared.inspect(Some(Duration::from_secs(2))).unwrap_err();
            assert_eq!(error.kind(), expected);
            assert_eq!(error.cause_kind(), None);
            let commit = prepared.commit(Some(Duration::from_secs(2))).unwrap_err();
            assert_eq!(commit.kind(), OwnerControlErrorKind::NotSent);
            assert_eq!(commit.cause_kind(), Some(expected));
            assert_eq!(prepared.commit(None).unwrap_err().cause_kind(), None);
        }
        assert!(matches!(
            listener.close(),
            c2_local::EndpointReapResult::Reaped | c2_local::EndpointReapResult::NotApplicable
        ));
        #[cfg(unix)]
        assert!(
            std::fs::read_dir(root.path())
                .unwrap()
                .all(|entry| matches!(
                    entry.unwrap().file_name().to_str(),
                    Some(".gate" | ".gate.marker")
                ))
        );
    }

    #[test]
    fn ticket_debug_is_redacted_and_only_explicit_export_contains_secret() {
        let ticket = OwnerRecoveryTicket::from_wire(ticket_wire()).unwrap();
        assert_eq!(format!("{ticket:?}"), "OwnerRecoveryTicket([REDACTED])");
        let exported = ticket.export();
        assert!(exported.windows(32).any(|bytes| bytes == [b'Q'; 32]));
        assert_eq!(
            OwnerRecoveryTicket::import(&exported).unwrap().export(),
            exported
        );
        for error in [malformed(), rejection(OwnerRejection::InvalidGrant)] {
            assert!(!format!("{error:?} {error}").contains(&"Q".repeat(32)));
        }
    }

    #[test]
    fn ticket_import_rejects_truncation_size_version_and_wrong_operation() {
        let ticket = OwnerRecoveryTicket::from_wire(ticket_wire()).unwrap();
        let exported = ticket.export();
        for len in 0..exported.len() {
            assert!(OwnerRecoveryTicket::import(&exported[..len]).is_err());
        }
        let mut trailing = exported.clone();
        trailing.push(0);
        assert_eq!(
            OwnerRecoveryTicket::import(&trailing).unwrap_err().kind(),
            OwnerControlErrorKind::Malformed
        );
        assert_eq!(
            OwnerRecoveryTicket::import(&vec![0; MAX_OWNER_PAYLOAD_SIZE + 1])
                .unwrap_err()
                .kind(),
            OwnerControlErrorKind::Malformed
        );
        let mut version = exported;
        version[1] = OWNER_CONTROL_VERSION + 1;
        assert_eq!(
            OwnerRecoveryTicket::import(&version).unwrap_err().kind(),
            OwnerControlErrorKind::UnsupportedVersion
        );
        let wrong_operation = encode_owner_response(&OwnerResponse::Revoked).unwrap();
        assert_eq!(
            OwnerRecoveryTicket::import(&wrong_operation)
                .unwrap_err()
                .kind(),
            OwnerControlErrorKind::Malformed
        );
    }

    #[test]
    fn ticket_import_validates_logical_and_host_identity_and_epoch() {
        for field in 0..6 {
            let mut ticket = ticket_wire();
            match field {
                0 => ticket.address = "http://owner-test".into(),
                1 => ticket.address = "ipc://../other".into(),
                2 => ticket.server_id = "other-host".into(),
                3 => ticket.server_instance_id.clear(),
                4 => ticket.epoch = 0,
                5 => ticket.server_instance_id = "bad\ninstance".into(),
                _ => unreachable!(),
            }
            let encoded = encode_owner_response(&OwnerResponse::RecoveryTicket(ticket)).unwrap();
            let error = OwnerRecoveryTicket::import(&encoded).unwrap_err();
            assert_eq!(
                error.kind(),
                if field == 2 {
                    OwnerControlErrorKind::IdentityMismatch
                } else {
                    OwnerControlErrorKind::Malformed
                }
            );
        }
    }

    #[test]
    fn status_observation_cannot_replace_commit_identity_checks() {
        let ticket = ticket_wire();
        let mut status = status_wire();
        status.epoch += 1;
        check_status_identity(&status, &ticket).unwrap();
        assert_eq!(
            check_status(&status, &ticket).unwrap_err().kind(),
            OwnerControlErrorKind::IdentityMismatch
        );
        status.server_instance_id = "restarted-host".into();
        assert!(check_status_identity(&status, &ticket).is_err());
        status = status_wire();
        status.context_binding[0] ^= 1;
        assert!(check_status_identity(&status, &ticket).is_err());
    }

    #[test]
    fn response_header_rejects_length_flags_and_request_id_before_body_read() {
        let runtime = runtime();
        for (total_len, request_id, flags) in [
            (14, 1, FLAG_CTRL | FLAG_RESPONSE),
            (
                13 + MAX_OWNER_PAYLOAD_SIZE as u32,
                1,
                FLAG_CTRL | FLAG_RESPONSE,
            ),
            (15, 2, FLAG_CTRL | FLAG_RESPONSE),
            (15, 1, FLAG_CTRL),
            (15, 1, FLAG_CTRL | FLAG_RESPONSE | 0x8000_0000),
        ] {
            let mut header = encode_frame(request_id, flags, &[]);
            header[..4].copy_from_slice(&total_len.to_le_bytes());
            let error = runtime
                .block_on(read_response(&mut header.as_slice(), 1))
                .unwrap_err();
            assert_eq!(error.kind(), OwnerControlErrorKind::Malformed);
        }
    }

    #[test]
    fn response_uses_existing_codec_and_rejects_unknown_versions_and_operations() {
        let runtime = runtime();
        let payload = encode_owner_response(&OwnerResponse::Revoked).unwrap();
        let frame = encode_frame(9, FLAG_CTRL | FLAG_RESPONSE, &payload);
        assert_eq!(
            runtime
                .block_on(read_response(&mut frame.as_slice(), 9))
                .unwrap(),
            OwnerResponse::Revoked
        );
        for (index, byte, kind) in [
            (0, 0, OwnerControlErrorKind::Malformed),
            (
                1,
                OWNER_CONTROL_VERSION + 1,
                OwnerControlErrorKind::UnsupportedVersion,
            ),
            (2, 99, OwnerControlErrorKind::Malformed),
        ] {
            let mut payload = payload.clone();
            payload[index] = byte;
            let frame = encode_frame(9, FLAG_CTRL | FLAG_RESPONSE, &payload);
            assert_eq!(
                runtime
                    .block_on(read_response(&mut frame.as_slice(), 9))
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
    }

    struct TestSession {
        owner: OwnerControl,
        peer: LocalStream,
        // The root outlives both connected endpoints and belongs to this test.
        #[cfg(unix)]
        _root: tempfile::TempDir,
    }

    fn session_pair(runtime: &Handle) -> TestSession {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("c2-owner-api-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        let context = LocalEndpointContext::with_unix_root(root.path()).unwrap();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let address = format!("ipc://owner-api-{}", uuid::Uuid::new_v4().simple());
        let endpoint = context.endpoint(&address).unwrap();
        let (stream, peer) = runtime.block_on(async {
            let mut listener = c2_local::LocalListener::bind(&endpoint).unwrap();
            let pair = tokio::try_join!(
                LocalStream::connect(&endpoint, Duration::from_secs(2)),
                listener.accept(),
            )
            .unwrap();
            assert!(matches!(
                listener.close(),
                c2_local::EndpointReapResult::Reaped | c2_local::EndpointReapResult::NotApplicable
            ));
            pair
        });
        TestSession {
            owner: OwnerControl::from_stream(stream, 1, status_wire()),
            peer,
            #[cfg(unix)]
            _root: root,
        }
    }

    fn receive_revoke(runtime: &Handle, peer: &mut LocalStream) -> u64 {
        // Revoke has no body beyond the existing three-byte owner prefix.
        let mut frame = [0; HEADER_SIZE + 3];
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut frame))
                .await
                .unwrap()
                .unwrap();
        });
        let (header, payload) = decode_frame(&frame).unwrap();
        assert_eq!(header.flags, FLAG_CTRL);
        assert_eq!(
            decode_owner_request(payload).unwrap(),
            OwnerRequest::RevokeRecovery
        );
        header.request_id
    }

    fn send_response(
        runtime: &Handle,
        peer: &mut LocalStream,
        request_id: u64,
        response: OwnerResponse,
    ) {
        let payload = encode_owner_response(&response).unwrap();
        runtime
            .block_on(peer.write_all(&encode_frame(
                request_id,
                FLAG_CTRL | FLAG_RESPONSE,
                &payload,
            )))
            .unwrap();
    }

    fn assert_peer_eof(runtime: &Handle, peer: &mut LocalStream) {
        let mut byte = [0];
        assert_eq!(
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap()
            }),
            0
        );
    }

    #[test]
    fn deadline_after_slot_but_before_first_write_preserves_owner_session() {
        let runtime = runtime();
        let mut session = session_pair(&runtime);
        let mut transaction = SessionUse {
            owner: &session.owner,
            io: Some(session.owner.slot(ConnectDeadline::default()).unwrap()),
            pending: false,
        };
        // Deterministically model expiry after slot acquisition, before the
        // async exchange is polled; no scheduler timing or fake stream is needed.
        let deadline = deadline(Some(Duration::ZERO)).unwrap();
        let error = runtime
            .block_on(session.owner.lifetime.run(deadline, async {
                transaction
                    .io
                    .as_mut()
                    .unwrap()
                    .activate(&session.owner.lifetime)
                    .await
            }))
            .unwrap_err()
            .submission(false);
        assert_eq!(error.kind(), OwnerControlErrorKind::NotSent);
        drop(transaction);
        assert!(!session.owner.lifetime.is_closed());
        std::thread::scope(|scope| {
            let owner = &session.owner;
            let next = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(2))));
            let id = receive_revoke(&runtime, &mut session.peer);
            send_response(&runtime, &mut session.peer, id, OwnerResponse::Revoked);
            next.join().unwrap().unwrap();
        });
    }

    #[test]
    fn close_handoff_releases_an_unactivated_endpoint_without_dropping_owner() {
        let runtime = runtime();
        let (owner, mut bootstrap) = owner_control_pair().unwrap();
        let mut peer = runtime.block_on(bootstrap.activate()).unwrap();
        {
            let cleanup = SessionUse {
                owner: &owner,
                io: Some(owner.slot(ConnectDeadline::default()).unwrap()),
                pending: false,
            };
            let io = cleanup.io.as_ref().unwrap();
            assert!(io.raw.is_some());
            assert!(io.stream.is_none());
            // The closer cannot acquire the occupied slot and there is no
            // installed stream abort handle. Scope teardown must complete it.
            std::thread::scope(|scope| scope.spawn(|| owner.close()).join().unwrap());
            assert!(io.raw.is_some());
        }
        assert!(owner.io.lock().raw.is_none());
        assert_peer_eof(&runtime, &mut peer);
        // The capability value itself is deliberately retained throughout.
        assert!(owner.lifetime.is_closed());
    }

    #[test]
    fn timed_out_slot_waiter_does_not_abort_the_in_flight_session() {
        let runtime = runtime();
        let mut session = session_pair(&runtime);
        std::thread::scope(|scope| {
            let owner = &session.owner;
            let active = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(3))));
            let request_id = receive_revoke(&runtime, &mut session.peer);
            let waiter = scope.spawn(|| owner.request_shutdown(Some(Duration::from_millis(20))));
            assert_eq!(
                waiter.join().unwrap().unwrap_err().kind(),
                OwnerControlErrorKind::NotSent
            );
            assert!(!owner.lifetime.is_closed());
            assert!(
                owner.io.try_lock().is_none(),
                "active request still owns the slot"
            );
            send_response(
                &runtime,
                &mut session.peer,
                request_id,
                OwnerResponse::Revoked,
            );
            active.join().unwrap().unwrap();
            assert!(!owner.lifetime.is_closed());
        });
    }

    #[test]
    fn close_aborts_a_request_without_waiting_for_a_reply_and_drop_closes_idle_stream() {
        let runtime = runtime();
        let mut session = session_pair(&runtime);
        std::thread::scope(|scope| {
            let owner = &session.owner;
            let active = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(3))));
            receive_revoke(&runtime, &mut session.peer);
            owner.close();
            assert_peer_eof(&runtime, &mut session.peer);
            assert_eq!(
                active.join().unwrap().unwrap_err().kind(),
                OwnerControlErrorKind::Indeterminate
            );
            owner.close();
        });
        let mut idle = session_pair(&runtime);
        drop(idle.owner);
        assert_peer_eof(&runtime, &mut idle.peer);
    }

    #[test]
    fn reply_timeout_closes_the_session_before_another_operation_can_use_it() {
        let runtime = runtime();
        let mut session = session_pair(&runtime);
        std::thread::scope(|scope| {
            let owner = &session.owner;
            let active = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_millis(100))));
            receive_revoke(&runtime, &mut session.peer);
            let error = active.join().unwrap().unwrap_err();
            assert_eq!(error.kind(), OwnerControlErrorKind::Indeterminate);
            assert_peer_eof(&runtime, &mut session.peer);
            assert_eq!(
                owner.revoke_recovery(None).unwrap_err().kind(),
                OwnerControlErrorKind::NotSent
            );
        });
    }

    #[test]
    fn invalid_or_unconfirmed_responses_poison_only_the_dedicated_session() {
        let runtime = runtime();
        for wrong_id in [false, true] {
            let mut session = session_pair(&runtime);
            std::thread::scope(|scope| {
                let owner = &session.owner;
                let active = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(3))));
                let request_id = receive_revoke(&runtime, &mut session.peer);
                // Both a wrong operation and a wrong request ID are fatal.
                let payload = encode_owner_response(&if wrong_id {
                    OwnerResponse::Revoked
                } else {
                    OwnerResponse::ShutdownInitiated
                })
                .unwrap();
                let mut frame = encode_frame(
                    request_id + u64::from(wrong_id),
                    FLAG_CTRL | FLAG_RESPONSE,
                    &payload,
                );
                if wrong_id {
                    // An invalid ID must be rejected before reading the body.
                    // Leave it unsent: closing with unread body bytes produces
                    // ECONNRESET on Linux, which is unrelated to this assertion.
                    frame.truncate(HEADER_SIZE);
                }
                // Fault injection stops at write acceptance. The peer is meant
                // to reject and close; a subsequent Windows flush may then fail.
                runtime
                    .block_on(AsyncWriteExt::write_all(&mut session.peer, &frame))
                    .unwrap();
                let error = active.join().unwrap().unwrap_err();
                assert_eq!(error.kind(), OwnerControlErrorKind::Indeterminate);
                assert_eq!(error.cause_kind(), Some(OwnerControlErrorKind::Malformed));
                assert_peer_eof(&runtime, &mut session.peer);
                assert_eq!(
                    owner.revoke_recovery(None).unwrap_err().kind(),
                    OwnerControlErrorKind::NotSent
                );
            });
        }
    }

    #[test]
    fn explicit_rejection_is_conclusive_and_keeps_the_completed_session_usable() {
        let runtime = runtime();
        let mut session = session_pair(&runtime);
        std::thread::scope(|scope| {
            let owner = &session.owner;
            let active = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(3))));
            let request_id = receive_revoke(&runtime, &mut session.peer);
            send_response(
                &runtime,
                &mut session.peer,
                request_id,
                OwnerResponse::Rejected(OwnerRejection::InvalidGrant),
            );
            assert_eq!(
                active.join().unwrap().unwrap_err().kind(),
                OwnerControlErrorKind::InvalidGrant
            );
            let again = scope.spawn(|| owner.revoke_recovery(Some(Duration::from_secs(3))));
            let request_id = receive_revoke(&runtime, &mut session.peer);
            send_response(
                &runtime,
                &mut session.peer,
                request_id,
                OwnerResponse::Revoked,
            );
            again.join().unwrap().unwrap();
        });
    }

    #[test]
    fn owner_capabilities_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OwnerControl>();
        assert_send_sync::<PreparedOwnerTakeover>();
        assert_send_sync::<OwnerRecoveryTicket>();
    }
}
