//! Blocking connection-observation waits owned by Core.
//!
//! SDK sync entry points validate and project their parameters, release
//! their interpreter lock, and delegate the wait here; they never build a
//! Tokio runtime or receive a generic block-on escape hatch. Each wait
//! drives the existing async observation surface on the same short-lived
//! current-thread runtime as the Host/session sync seams, so observers
//! still retain no runtime, transport, or lease by waiting.

use std::time::Duration;

use crate::{LifecycleError, Runtime};
use c2_local::{ConnectionEvent, ConnectionEvents, ConnectionEventsError};

/// Outcome of one connection-event receive that did not time out.
#[derive(Clone, Debug)]
pub enum ConnectionEventWait {
    /// The next event from this subscription.
    Event(ConnectionEvent),
    /// The event source ended; every later receive reports the same outcome.
    Ended,
    /// This subscription lagged and is permanently terminated.
    Lagged(u64),
    /// Another receiver already owns this subscription.
    ConcurrentReceive,
}

/// Block the calling thread until the next connection event is available or
/// `timeout` elapses. `Ok(None)` reports only the timeout: no event is
/// consumed and the subscription stays usable for the next receive.
pub fn recv_connection_event(
    events: &ConnectionEvents,
    timeout: Option<Duration>,
) -> Result<Option<ConnectionEventWait>, LifecycleError> {
    let runtime = Runtime::server_runtime()?;
    let recv = async {
        match timeout {
            Some(duration) => tokio::time::timeout(duration, events.recv()).await.ok(),
            None => Some(events.recv().await),
        }
    };
    Ok(runtime.block_on(recv).map(|received| match received {
        Ok(Some(event)) => ConnectionEventWait::Event(event),
        Ok(None) => ConnectionEventWait::Ended,
        Err(ConnectionEventsError::Lagged(missed)) => ConnectionEventWait::Lagged(missed),
        Err(ConnectionEventsError::ConcurrentReceive) => ConnectionEventWait::ConcurrentReceive,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_local::{ConnectionEndReason, ConnectionEventSource};

    #[test]
    fn timed_out_receive_consumes_nothing_and_metadata_keeps_no_feed_alive() {
        let source = ConnectionEventSource::new();
        let events = source.subscribe();
        assert!(matches!(
            recv_connection_event(&events, Some(Duration::from_millis(10))).unwrap(),
            None
        ));
        let publisher = source.open_connection(c2_local::ConnectionSide::Server);
        publisher.close(
            ConnectionEndReason::HeartbeatTimeout,
            "heartbeat_timeout",
            None,
        );
        let observation = match recv_connection_event(&events, Some(Duration::from_secs(2)))
            .unwrap()
            .expect("queued events survive an earlier timed-out receive")
        {
            ConnectionEventWait::Event(ConnectionEvent::Opened(observation)) => observation,
            other => panic!("expected Opened, got {other:?}"),
        };
        match recv_connection_event(&events, Some(Duration::ZERO))
            .unwrap()
            .expect("the close event is still queued")
        {
            ConnectionEventWait::Event(ConnectionEvent::Closed(record)) => {
                assert_eq!(record.connection_id(), observation.connection_id());
                assert_eq!(record.reason(), ConnectionEndReason::HeartbeatTimeout);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
        assert!(matches!(
            recv_connection_event(&events, Some(Duration::ZERO)).unwrap(),
            None
        ));
        let terminal = publisher.observation().snapshot();
        drop(source);
        drop(publisher);
        assert!(matches!(
            recv_connection_event(&events, Some(Duration::from_secs(2)))
                .unwrap()
                .expect("the feed ends once its owners are gone"),
            ConnectionEventWait::Ended
        ));
        assert_eq!(
            observation.snapshot(),
            terminal,
            "an obtained observation stays usable after its feed ended"
        );
    }

    #[test]
    fn lagged_receive_reports_missed_events_and_keeps_observations_usable() {
        let source = ConnectionEventSource::new();
        let events = source.subscribe();
        let publisher = source.open_connection(c2_local::ConnectionSide::Server);
        // The feed keeps a fixed capacity of 1024 events, so
        // publishing 1026 while this subscription consumes none must report
        // both lost events instead of blocking or silently skipping.
        for _ in 0..1025 {
            source.open_connection(c2_local::ConnectionSide::Server);
        }
        match recv_connection_event(&events, Some(Duration::from_secs(2)))
            .unwrap()
            .expect("lag is reported instead of waiting for new events")
        {
            ConnectionEventWait::Lagged(missed) => assert_eq!(missed, 2),
            other => panic!("expected Lagged, got {other:?}"),
        }
        assert!(matches!(
            recv_connection_event(&events, Some(Duration::from_secs(2)))
                .unwrap()
                .expect("a lagged subscription is ended"),
            ConnectionEventWait::Ended
        ));
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        let observation = publisher.observation();
        let closed = observation
            .snapshot()
            .expect("terminal record survives feed lag");
        assert_eq!(closed.connection_id(), observation.connection_id());
        assert_eq!(closed.reason(), ConnectionEndReason::LocalClose);
    }

    #[test]
    fn blocking_receive_waits_for_events_published_on_another_thread() {
        let source = ConnectionEventSource::new();
        let events = source.subscribe();
        let received = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| recv_connection_event(&events, None).unwrap());
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !waiter.is_finished(),
                "the receive must block while no event exists"
            );
            source.open_connection(c2_local::ConnectionSide::Server);
            waiter.join().unwrap()
        });
        match received.expect("an event published while blocking") {
            ConnectionEventWait::Event(ConnectionEvent::Opened(_)) => {}
            other => panic!("expected Opened, got {other:?}"),
        }
    }
}
