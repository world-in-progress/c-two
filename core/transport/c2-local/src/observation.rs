//! Metadata-only observations of one physical local connection.
//!
//! Publishers belong to the transport's existing connection guards. Observers
//! and event receivers never retain a stream, a publisher, or its event source.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;

const EVENT_CAPACITY: usize = 1024;
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Which local endpoint established this physical connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionSide {
    Client,
    Server,
}

impl ConnectionSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEndReason {
    LocalClose,
    PeerCloseMessage,
    PeerEof,
    ConnectionReset,
    ProtocolError,
    IoError,
}

impl ConnectionEndReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalClose => "local_close",
            Self::PeerCloseMessage => "peer_close_message",
            Self::PeerEof => "peer_eof",
            Self::ConnectionReset => "connection_reset",
            Self::ProtocolError => "protocol_error",
            Self::IoError => "io_error",
        }
    }
}

/// A terminal transport fact, not proof of callback drain or peer process death.
/// No peer text or payload is retained in diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionClosed {
    connection_id: u64,
    side: ConnectionSide,
    end: ConnectionEnd,
}

/// Native end facts shared by stream ownership and metadata publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionEnd {
    reason: ConnectionEndReason,
    operation: &'static str,
    os_error: Option<i32>,
    peer_end: bool,
}

impl ConnectionEnd {
    pub(crate) fn new(
        reason: ConnectionEndReason,
        operation: &'static str,
        os_error: Option<i32>,
    ) -> Self {
        Self {
            reason,
            operation,
            os_error,
            peer_end: reason == ConnectionEndReason::PeerEof,
        }
    }

    pub(crate) fn io(operation: &'static str, error: &io::Error) -> Self {
        Self::new(
            match error.kind() {
                io::ErrorKind::ConnectionReset => ConnectionEndReason::ConnectionReset,
                _ => ConnectionEndReason::IoError,
            },
            operation,
            error.raw_os_error(),
        )
    }

    pub(crate) fn read_error(operation: &'static str, error: &io::Error) -> Self {
        let mut end = Self::io(operation, error);
        if error.kind() == io::ErrorKind::UnexpectedEof {
            end.reason = ConnectionEndReason::PeerEof;
            end.peer_end = true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ECONNRESET) {
            end.peer_end = true;
        }
        // Mio normalizes 109 to EOF; byte-mode pipes can also return 232.
        // ERROR_PIPE_NOT_CONNECTED (233) is not proof of peer end.
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(109 | 232)) {
            end.reason = ConnectionEndReason::PeerEof;
            end.peer_end = true;
        }
        end
    }

    pub(crate) const fn is_peer_end(self) -> bool {
        self.peer_end
    }
}

impl ConnectionClosed {
    pub const fn side(&self) -> ConnectionSide {
        self.side
    }

    pub const fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub const fn reason(&self) -> ConnectionEndReason {
        self.end.reason
    }

    pub const fn operation(&self) -> &'static str {
        self.end.operation
    }

    pub const fn os_error(&self) -> Option<i32> {
        self.end.os_error
    }

    /// Whether this record came from EOF or a native read-side peer end.
    /// A write error or a DISCONNECT message provides no such proof.
    pub const fn is_peer_end(&self) -> bool {
        self.end.peer_end
    }
}

/// Read-only metadata and notification for exactly one connection incarnation.
/// IDs are process-local, never reused, and do not identify a peer process/CRM.
#[derive(Clone, Debug)]
pub struct ConnectionObservation {
    connection_id: u64,
    side: ConnectionSide,
    closed: watch::Receiver<Option<ConnectionClosed>>,
}

impl ConnectionObservation {
    pub const fn side(&self) -> ConnectionSide {
        self.side
    }

    pub const fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn snapshot(&self) -> Option<ConnectionClosed> {
        *self.closed.borrow()
    }

    /// Cancelling or timing out this future affects only this waiter. A late
    /// waiter returns the same terminal record, including with a zero timeout.
    pub async fn wait_closed(&self, timeout: Option<Duration>) -> Option<ConnectionClosed> {
        if let Some(closed) = self.snapshot() {
            return Some(closed);
        }
        let wait = async {
            let mut receiver = self.closed.clone();
            loop {
                if let Some(closed) = *receiver.borrow_and_update() {
                    return Some(closed);
                }
                if receiver.changed().await.is_err() {
                    return *receiver.borrow();
                }
            }
        };
        match timeout {
            None => wait.await,
            Some(timeout) => tokio::time::timeout(timeout, wait).await.ok().flatten(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ConnectionEvent {
    Opened(ConnectionObservation),
    Closed(ConnectionClosed),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEventsError {
    Lagged(u64),
    ConcurrentReceive,
}

impl fmt::Display for ConnectionEventsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lagged(count) => write!(formatter, "connection events lagged by {count}"),
            Self::ConcurrentReceive => {
                write!(formatter, "connection events already has a receiver")
            }
        }
    }
}

impl std::error::Error for ConnectionEventsError {}

/// A bounded subscription from its creation point, without historical replay.
/// A lag error permanently terminates this subscription.
pub struct ConnectionEvents {
    receiver: Mutex<Option<broadcast::Receiver<ConnectionEvent>>>,
    closed: CancellationToken,
}

impl ConnectionEvents {
    /// End only this subscription and wake its receiver; no transport is closed.
    /// Explicit close discards queued events. Concurrent receive/close may
    /// return an event already received before cancellation was observed.
    pub fn close(&self) {
        self.closed.cancel();
        if let Ok(mut receiver) = self.receiver.try_lock() {
            *receiver = None;
        }
    }

    pub async fn recv(&self) -> Result<Option<ConnectionEvent>, ConnectionEventsError> {
        if self.closed.is_cancelled() {
            return Ok(None);
        }
        let mut slot = self
            .receiver
            .try_lock()
            .map_err(|_| ConnectionEventsError::ConcurrentReceive)?;
        let Some(receiver) = slot.as_mut() else {
            return Ok(None);
        };
        let received = tokio::select! {
            biased;
            _ = self.closed.cancelled() => {
                *slot = None;
                return Ok(None);
            }
            received = receiver.recv() => received,
        };
        match received {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::RecvError::Closed) => {
                *slot = None;
                Ok(None)
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                *slot = None;
                Err(ConnectionEventsError::Lagged(count))
            }
        }
    }
}

/// Low-level construction seam: preparing/subscribing performs no I/O and
/// resolves no endpoint or runtime configuration. Retain this with the host,
/// never with an observer; dropping its last clone ends the feed. The bounded
/// queue is allocated on first subscription; earlier publishers still see that
/// source when publishing a later close.
#[derive(Clone, Debug)]
pub struct ConnectionEventSource {
    sender: Arc<OnceLock<broadcast::Sender<ConnectionEvent>>>,
}

impl Default for ConnectionEventSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionEventSource {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(OnceLock::new()),
        }
    }

    pub fn subscribe(&self) -> ConnectionEvents {
        ConnectionEvents {
            receiver: Mutex::new(Some(
                self.sender
                    .get_or_init(|| broadcast::channel(EVENT_CAPACITY).0)
                    .subscribe(),
            )),
            closed: CancellationToken::new(),
        }
    }

    /// Called at physical connection admission, before spawning its handler.
    pub fn open_connection(&self, side: ConnectionSide) -> ConnectionPublisher {
        let publisher = ConnectionPublisher::with_source(Arc::downgrade(&self.sender), side);
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(ConnectionEvent::Opened(publisher.observation()));
        }
        publisher
    }
}

/// Transport construction/termination authority. It cannot be recovered from
/// an observation. Existing receive/registration guards own its Drop fence.
#[derive(Clone, Debug)]
pub struct ConnectionPublisher {
    connection_id: u64,
    side: ConnectionSide,
    closed: watch::Sender<Option<ConnectionClosed>>,
    events: Weak<OnceLock<broadcast::Sender<ConnectionEvent>>>,
}

impl ConnectionPublisher {
    pub fn new(side: ConnectionSide) -> Self {
        Self::with_source(Weak::new(), side)
    }

    fn with_source(
        events: Weak<OnceLock<broadcast::Sender<ConnectionEvent>>>,
        side: ConnectionSide,
    ) -> Self {
        let connection_id = NEXT_CONNECTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("physical connection identifiers exhausted");
        let (closed, _) = watch::channel(None);
        Self {
            connection_id,
            side,
            closed,
            events,
        }
    }

    pub fn observation(&self) -> ConnectionObservation {
        ConnectionObservation {
            connection_id: self.connection_id,
            side: self.side,
            closed: self.closed.subscribe(),
        }
    }

    /// Read the same terminal authority without constructing another observer.
    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// The first terminal fact wins, including against fallback Drop guards.
    pub fn close(
        &self,
        reason: ConnectionEndReason,
        operation: &'static str,
        os_error: Option<i32>,
    ) -> bool {
        self.close_end(ConnectionEnd::new(reason, operation, os_error))
    }

    pub(crate) fn close_end(&self, end: ConnectionEnd) -> bool {
        self.closed.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            let closed = ConnectionClosed {
                connection_id: self.connection_id,
                side: self.side,
                end,
            };
            *state = Some(closed);
            if let Some(source) = self.events.upgrade()
                && let Some(events) = source.get()
            {
                let _ = events.send(ConnectionEvent::Closed(closed));
            }
            true
        })
    }

    pub fn close_io(&self, operation: &'static str, error: &io::Error) -> bool {
        self.close_end(ConnectionEnd::io(operation, error))
    }

    pub fn close_read_error(&self, operation: &'static str, error: &io::Error) -> bool {
        self.close_end(ConnectionEnd::read_error(operation, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    #[tokio::test]
    async fn terminal_once_and_late_waiters_share_the_record() {
        let publisher = ConnectionPublisher::new(ConnectionSide::Client);
        let observation = publisher.observation();
        assert!(observation.snapshot().is_none());
        assert!(publisher.close(ConnectionEndReason::PeerEof, "read_header", None));
        assert!(!publisher.close(ConnectionEndReason::LocalClose, "drop", None));
        let closed = observation.snapshot().unwrap();
        assert_eq!(closed.connection_id(), observation.connection_id());
        assert_eq!(closed.reason(), ConnectionEndReason::PeerEof);
        assert_eq!(observation.wait_closed(None).await, Some(closed));
        assert_eq!(
            observation.wait_closed(Some(Duration::ZERO)).await,
            Some(closed)
        );
        assert_ne!(
            ConnectionPublisher::new(ConnectionSide::Client)
                .observation()
                .connection_id(),
            observation.connection_id()
        );
    }

    #[tokio::test]
    async fn cancelled_and_timed_out_waiters_leave_other_waiters_live() {
        let publisher = ConnectionPublisher::new(ConnectionSide::Client);
        let observation = publisher.observation();
        let mut cancelled = Box::pin(observation.wait_closed(None));
        assert!(
            poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        drop(cancelled);
        assert_eq!(observation.wait_closed(Some(Duration::ZERO)).await, None);
        let mut remaining = Box::pin(observation.wait_closed(None));
        assert!(
            poll_fn(|cx| Poll::Ready(remaining.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        assert_eq!(remaining.await, observation.snapshot());
    }

    #[test]
    fn racing_terminal_publishers_emit_one_closed_event() {
        let source = ConnectionEventSource::new();
        let mut events = source.subscribe();
        let first = source.open_connection(ConnectionSide::Server);
        let second = first.clone();
        let winners = std::thread::scope(|scope| {
            let first = scope
                .spawn(|| first.close(ConnectionEndReason::ProtocolError, "protocol_error", None));
            let second =
                scope.spawn(|| second.close(ConnectionEndReason::LocalClose, "drop", None));
            u8::from(first.join().unwrap()) + u8::from(second.join().unwrap())
        });
        assert_eq!(winners, 1);
        let receiver = events.receiver.get_mut().as_mut().unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(ConnectionEvent::Opened(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(ConnectionEvent::Closed(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn event_overflow_terminates_only_that_subscription() {
        let source = ConnectionEventSource::new();
        let events = source.subscribe();
        let publisher = source.open_connection(ConnectionSide::Server);
        let observation = match events.recv().await.unwrap().unwrap() {
            ConnectionEvent::Opened(observation) => observation,
            _ => panic!("first event must be Opened"),
        };
        for _ in 0..EVENT_CAPACITY + 1 {
            source.open_connection(ConnectionSide::Server);
        }
        assert_eq!(
            events.recv().await.unwrap_err(),
            ConnectionEventsError::Lagged(1)
        );
        assert!(events.recv().await.unwrap().is_none());
        publisher.close(ConnectionEndReason::PeerEof, "read_header", None);
        assert_eq!(observation.wait_closed(None).await, observation.snapshot());
    }

    #[tokio::test]
    async fn late_subscription_sees_future_close_without_synthetic_open() {
        let source = ConnectionEventSource::new();
        let publisher = source.open_connection(ConnectionSide::Server);
        let events = source.subscribe();
        publisher.close(ConnectionEndReason::PeerCloseMessage, "disconnect", None);
        assert!(matches!(
            events.recv().await.unwrap(),
            Some(ConnectionEvent::Closed(_))
        ));
        drop(source);
        assert!(events.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn observations_and_publishers_do_not_keep_event_source_alive() {
        let source = ConnectionEventSource::new();
        let weak = Arc::downgrade(&source.sender);
        let publisher = source.open_connection(ConnectionSide::Server);
        let observation = publisher.observation();
        let events = source.subscribe();
        drop(source);
        assert!(weak.upgrade().is_none());
        assert!(events.recv().await.unwrap().is_none());
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        assert!(observation.wait_closed(None).await.is_some());
    }

    #[tokio::test]
    async fn closing_subscription_wakes_receiver_without_closing_connection() {
        let source = ConnectionEventSource::new();
        let events = source.subscribe();
        let other = source.subscribe();
        let mut receive = Box::pin(events.recv());
        poll_fn(|cx| {
            assert!(receive.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            events.recv().await.unwrap_err(),
            ConnectionEventsError::ConcurrentReceive
        );
        events.close();
        assert!(receive.await.unwrap().is_none());
        events.close();
        let publisher = source.open_connection(ConnectionSide::Client);
        assert!(publisher.observation().snapshot().is_none());
        assert!(events.recv().await.unwrap().is_none());
        let Some(ConnectionEvent::Opened(observation)) = other.recv().await.unwrap() else {
            panic!("the other subscription remains usable");
        };
        assert_eq!(observation.side(), ConnectionSide::Client);
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        let Some(ConnectionEvent::Closed(closed)) = other.recv().await.unwrap() else {
            panic!("same connection terminal fact");
        };
        assert_eq!(closed.side(), ConnectionSide::Client);
        assert_eq!(closed.connection_id(), observation.connection_id());
        assert_eq!(observation.snapshot(), Some(closed));
    }

    #[test]
    fn io_diagnostics_retain_only_operation_and_os_code() {
        let publisher = ConnectionPublisher::new(ConnectionSide::Client);
        publisher.close_io("read_header", &io::Error::from_raw_os_error(12345));
        let closed = publisher.observation().snapshot().unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::IoError);
        assert_eq!(closed.operation(), "read_header");
        assert_eq!(closed.os_error(), Some(12345));
        assert!(!closed.is_peer_end());
    }

    #[cfg(unix)]
    #[test]
    fn only_native_read_reset_proves_peer_end() {
        let error = io::Error::from_raw_os_error(libc::ECONNRESET);
        let reader = ConnectionPublisher::new(ConnectionSide::Client);
        reader.close_read_error("read_header", &error);
        let closed = reader.observation().snapshot().unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::ConnectionReset);
        assert_eq!(closed.os_error(), Some(libc::ECONNRESET));
        assert!(closed.is_peer_end());

        let writer = ConnectionPublisher::new(ConnectionSide::Client);
        writer.close_io("write", &error);
        let closed = writer.observation().snapshot().unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::ConnectionReset);
        assert_eq!(closed.os_error(), Some(libc::ECONNRESET));
        assert!(!closed.is_peer_end());
    }

    #[test]
    fn read_eof_is_distinct_from_write_failure_and_peer_close_message() {
        let error = io::Error::from(io::ErrorKind::UnexpectedEof);
        let reader = ConnectionPublisher::new(ConnectionSide::Server);
        reader.close_read_error("read_header", &error);
        assert!(reader.observation().snapshot().unwrap().is_peer_end());

        let writer = ConnectionPublisher::new(ConnectionSide::Server);
        writer.close_io("write", &error);
        let closed = writer.observation().snapshot().unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::IoError);
        assert!(!closed.is_peer_end());

        let control = ConnectionPublisher::new(ConnectionSide::Server);
        control.close(ConnectionEndReason::PeerCloseMessage, "disconnect", None);
        assert!(!control.observation().snapshot().unwrap().is_peer_end());
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_read_closures_do_not_conflate_unconnected_pipe() {
        for code in [109, 232, 233] {
            let publisher = ConnectionPublisher::new(ConnectionSide::Client);
            publisher.close_read_error("read_header", &io::Error::from_raw_os_error(code));
            let closed = publisher.observation().snapshot().unwrap();
            assert_eq!(closed.os_error(), Some(code));
            assert_eq!(closed.is_peer_end(), code != 233);
            assert_eq!(
                closed.reason(),
                if code == 233 {
                    ConnectionEndReason::IoError
                } else {
                    ConnectionEndReason::PeerEof
                }
            );
            let writer = ConnectionPublisher::new(ConnectionSide::Client);
            writer.close_io("write", &io::Error::from_raw_os_error(code));
            assert!(!writer.observation().snapshot().unwrap().is_peer_end());
        }
    }
}
