//! Metadata-only observations of one physical local connection.
//!
//! Publishers belong to the transport's existing connection guards. Observers
//! and event receivers never retain a stream, a publisher, or its event source.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::{broadcast, watch};

const EVENT_CAPACITY: usize = 1024;
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEndReason {
    LocalClose,
    PeerCloseMessage,
    PeerEof,
    ConnectionReset,
    HeartbeatTimeout,
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
            Self::HeartbeatTimeout => "heartbeat_timeout",
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
    reason: ConnectionEndReason,
    operation: &'static str,
    os_error: Option<i32>,
}

impl ConnectionClosed {
    pub const fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub const fn reason(&self) -> ConnectionEndReason {
        self.reason
    }

    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    pub const fn os_error(&self) -> Option<i32> {
        self.os_error
    }
}

/// Read-only metadata and notification for exactly one connection incarnation.
/// IDs are process-local, never reused, and do not identify a peer process/CRM.
#[derive(Clone, Debug)]
pub struct ConnectionObservation {
    connection_id: u64,
    closed: watch::Receiver<Option<ConnectionClosed>>,
}

impl ConnectionObservation {
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
}

impl fmt::Display for ConnectionEventsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lagged(count) => write!(formatter, "connection events lagged by {count}"),
        }
    }
}

impl std::error::Error for ConnectionEventsError {}

/// A bounded subscription from its creation point, without historical replay.
/// A lag error permanently terminates this subscription.
pub struct ConnectionEvents {
    receiver: Option<broadcast::Receiver<ConnectionEvent>>,
}

impl ConnectionEvents {
    pub async fn recv(&mut self) -> Result<Option<ConnectionEvent>, ConnectionEventsError> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Ok(None);
        };
        match receiver.recv().await {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::RecvError::Closed) => {
                self.receiver = None;
                Ok(None)
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                self.receiver = None;
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
            receiver: Some(
                self.sender
                    .get_or_init(|| broadcast::channel(EVENT_CAPACITY).0)
                    .subscribe(),
            ),
        }
    }

    /// Called at physical connection admission, before spawning its handler.
    pub fn open_connection(&self) -> ConnectionPublisher {
        let publisher = ConnectionPublisher::with_source(Arc::downgrade(&self.sender));
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
    closed: watch::Sender<Option<ConnectionClosed>>,
    events: Weak<OnceLock<broadcast::Sender<ConnectionEvent>>>,
}

impl Default for ConnectionPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionPublisher {
    pub fn new() -> Self {
        Self::with_source(Weak::new())
    }

    fn with_source(events: Weak<OnceLock<broadcast::Sender<ConnectionEvent>>>) -> Self {
        let connection_id = NEXT_CONNECTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("physical connection identifiers exhausted");
        let (closed, _) = watch::channel(None);
        Self {
            connection_id,
            closed,
            events,
        }
    }

    pub fn observation(&self) -> ConnectionObservation {
        ConnectionObservation {
            connection_id: self.connection_id,
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
        self.closed.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            let closed = ConnectionClosed {
                connection_id: self.connection_id,
                reason,
                operation,
                os_error,
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
        let reason = match error.kind() {
            io::ErrorKind::UnexpectedEof => ConnectionEndReason::PeerEof,
            io::ErrorKind::ConnectionReset => ConnectionEndReason::ConnectionReset,
            _ => ConnectionEndReason::IoError,
        };
        self.close(reason, operation, error.raw_os_error())
    }

    pub fn close_read_error(&self, operation: &'static str, error: &io::Error) -> bool {
        // Mio normalizes ERROR_BROKEN_PIPE reads to EOF. ERROR_NO_DATA may
        // still arrive directly from a byte-mode Named Pipe. Both describe
        // this connection only; ERROR_PIPE_NOT_CONNECTED is not peer EOF.
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(109 | 232)) {
            return self.close(
                ConnectionEndReason::PeerEof,
                operation,
                error.raw_os_error(),
            );
        }
        self.close_io(operation, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    #[tokio::test]
    async fn terminal_once_and_late_waiters_share_the_record() {
        let publisher = ConnectionPublisher::new();
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
            ConnectionPublisher::new().observation().connection_id(),
            observation.connection_id()
        );
    }

    #[tokio::test]
    async fn cancelled_and_timed_out_waiters_leave_other_waiters_live() {
        let publisher = ConnectionPublisher::new();
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
        let first = source.open_connection();
        let second = first.clone();
        let winners = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                first.close(
                    ConnectionEndReason::HeartbeatTimeout,
                    "heartbeat_timeout",
                    None,
                )
            });
            let second =
                scope.spawn(|| second.close(ConnectionEndReason::LocalClose, "drop", None));
            u8::from(first.join().unwrap()) + u8::from(second.join().unwrap())
        });
        assert_eq!(winners, 1);
        let receiver = events.receiver.as_mut().unwrap();
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
        let mut events = source.subscribe();
        let publisher = source.open_connection();
        let observation = match events.recv().await.unwrap().unwrap() {
            ConnectionEvent::Opened(observation) => observation,
            _ => panic!("first event must be Opened"),
        };
        for _ in 0..EVENT_CAPACITY + 1 {
            source.open_connection();
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
        let publisher = source.open_connection();
        let mut events = source.subscribe();
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
        let publisher = source.open_connection();
        let observation = publisher.observation();
        let mut events = source.subscribe();
        drop(source);
        assert!(weak.upgrade().is_none());
        assert!(events.recv().await.unwrap().is_none());
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        assert!(observation.wait_closed(None).await.is_some());
    }

    #[test]
    fn io_diagnostics_retain_only_operation_and_os_code() {
        let publisher = ConnectionPublisher::new();
        publisher.close_io("read_header", &io::Error::from_raw_os_error(12345));
        let closed = publisher.observation().snapshot().unwrap();
        assert_eq!(closed.reason(), ConnectionEndReason::IoError);
        assert_eq!(closed.operation(), "read_header");
        assert_eq!(closed.os_error(), Some(12345));
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_read_closures_do_not_conflate_unconnected_pipe() {
        for code in [109, 232, 233] {
            let publisher = ConnectionPublisher::new();
            publisher.close_read_error("read_header", &io::Error::from_raw_os_error(code));
            let closed = publisher.observation().snapshot().unwrap();
            assert_eq!(closed.os_error(), Some(code));
            assert_eq!(
                closed.reason(),
                if code == 233 {
                    ConnectionEndReason::IoError
                } else {
                    ConnectionEndReason::PeerEof
                }
            );
        }
    }
}
