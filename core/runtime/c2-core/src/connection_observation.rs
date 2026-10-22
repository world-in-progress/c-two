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
use c2_local::{
    ConnectionClosed, ConnectionEvent, ConnectionEvents, ConnectionEventsError,
    ConnectionObservation,
};

/// Outcome of one connection-event receive that did not time out.
#[derive(Clone, Debug)]
pub enum ConnectionEventWait {
    /// The next event from this subscription.
    Event(ConnectionEvent),
    /// The event source ended; every later receive reports the same outcome.
    Ended,
    /// This subscription lagged and is permanently terminated.
    Lagged(u64),
}

/// Block the calling thread until one connection's terminal record is
/// available or `timeout` elapses. Timing out cancels only this waiter; a
/// late waiter still observes the same terminal record.
pub fn wait_connection_closed(
    observation: &ConnectionObservation,
    timeout: Option<Duration>,
) -> Result<Option<ConnectionClosed>, LifecycleError> {
    let runtime = Runtime::server_runtime()?;
    Ok(runtime.block_on(observation.wait_closed(timeout)))
}

/// Block the calling thread until the next connection event is available or
/// `timeout` elapses. `Ok(None)` reports only the timeout: no event is
/// consumed and the subscription stays usable for the next receive.
pub fn recv_connection_event(
    events: &mut ConnectionEvents,
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
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_local::{ConnectionEndReason, ConnectionEventSource, ConnectionPublisher};

    #[test]
    fn blocking_wait_serves_the_one_terminal_record_to_late_waiters() {
        let publisher = ConnectionPublisher::new();
        let observation = publisher.observation();
        assert_eq!(
            wait_connection_closed(&observation, Some(Duration::from_millis(10))).unwrap(),
            None,
            "an open connection times out without changing the observation"
        );
        let closed = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| wait_connection_closed(&observation, None).unwrap());
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !waiter.is_finished(),
                "the waiter must block until the close"
            );
            assert!(publisher.close(ConnectionEndReason::PeerCloseMessage, "disconnect", None));
            waiter.join().unwrap()
        })
        .expect("terminal record once published");
        assert_eq!(closed.connection_id(), observation.connection_id());
        assert_eq!(closed.reason(), ConnectionEndReason::PeerCloseMessage);
        assert_eq!(
            wait_connection_closed(&observation, Some(Duration::ZERO)).unwrap(),
            Some(closed),
            "a late waiter observes the same terminal record"
        );
    }

    #[test]
    fn timed_out_receive_consumes_nothing_and_metadata_keeps_no_feed_alive() {
        let source = ConnectionEventSource::new();
        let mut events = source.subscribe();
        assert!(matches!(
            recv_connection_event(&mut events, Some(Duration::from_millis(10))).unwrap(),
            None
        ));
        let publisher = source.open_connection();
        publisher.close(
            ConnectionEndReason::HeartbeatTimeout,
            "heartbeat_timeout",
            None,
        );
        let observation = match recv_connection_event(&mut events, Some(Duration::from_secs(2)))
            .unwrap()
            .expect("queued events survive an earlier timed-out receive")
        {
            ConnectionEventWait::Event(ConnectionEvent::Opened(observation)) => observation,
            other => panic!("expected Opened, got {other:?}"),
        };
        match recv_connection_event(&mut events, Some(Duration::ZERO))
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
            recv_connection_event(&mut events, Some(Duration::ZERO)).unwrap(),
            None
        ));
        drop(source);
        drop(publisher);
        assert!(matches!(
            recv_connection_event(&mut events, Some(Duration::from_secs(2)))
                .unwrap()
                .expect("the feed ends once its owners are gone"),
            ConnectionEventWait::Ended
        ));
        assert_eq!(
            wait_connection_closed(&observation, Some(Duration::ZERO)).unwrap(),
            observation.snapshot(),
            "an obtained observation stays usable after its feed ended"
        );
    }

    #[test]
    fn lagged_receive_reports_missed_events_and_keeps_observations_usable() {
        let source = ConnectionEventSource::new();
        let mut events = source.subscribe();
        let publisher = source.open_connection();
        // The server-side feed keeps a fixed capacity of 1024 events, so
        // publishing 1026 while this subscription consumes none must report
        // both lost events instead of blocking or silently skipping.
        for _ in 0..1025 {
            source.open_connection();
        }
        match recv_connection_event(&mut events, Some(Duration::from_secs(2)))
            .unwrap()
            .expect("lag is reported instead of waiting for new events")
        {
            ConnectionEventWait::Lagged(missed) => assert_eq!(missed, 2),
            other => panic!("expected Lagged, got {other:?}"),
        }
        assert!(matches!(
            recv_connection_event(&mut events, Some(Duration::from_secs(2)))
                .unwrap()
                .expect("a lagged subscription is ended"),
            ConnectionEventWait::Ended
        ));
        publisher.close(ConnectionEndReason::LocalClose, "close", None);
        let observation = publisher.observation();
        assert_eq!(
            wait_connection_closed(&observation, Some(Duration::ZERO)).unwrap(),
            observation.snapshot(),
            "existing observations stay usable after their feed lagged"
        );
    }

    #[test]
    fn blocking_receive_waits_for_events_published_on_another_thread() {
        let source = ConnectionEventSource::new();
        let mut events = source.subscribe();
        let received = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| recv_connection_event(&mut events, None).unwrap());
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !waiter.is_finished(),
                "the receive must block while no event exists"
            );
            source.open_connection();
            waiter.join().unwrap()
        });
        match received.expect("an event published while blocking") {
            ConnectionEventWait::Event(ConnectionEvent::Opened(_)) => {}
            other => panic!("expected Opened, got {other:?}"),
        }
    }
}
