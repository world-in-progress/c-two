//! Publish/subscribe topic primitives in `c2-core` (ADR 0005).
//!
//! Server-side registration-scoped topic channels and single-write sealed SHM
//! history rings live in `c2_server::topic`. This module re-exports the
//! language-neutral configuration/data types, adapts server errors into
//! [`crate::Error`], and provides [`CoreTopicSubscription`] / [`CoreTopicDelivery`]
//! for remote IPC (and later relay HTTP) subscriptions.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_ipc::{IpcError, IpcSubscribeCancellation, IpcTopicDelivery, IpcTopicSubscription, StreamTerminal};
use c2_wire::rpc::CallStatus;
use tokio::sync::Mutex;

pub use c2_server::topic::{
    LagPolicy, MAX_TOPIC_RING_CAPACITY, RouteTopics, SubscribeOptions, TopicCloseCause,
    TopicConfig, TopicDelivery, TopicFrame, TopicStart,
};

use crate::call_execution::{scope_error, shared_executor};
use crate::client::{ClientTarget, normalize_ipc_call_error};
use crate::{CallScope, CallState, Error, HeldResponse, TransportPhase, normalize_ipc_error};

/// Terminal observation returned by explicit subscription close.
#[derive(Debug)]
pub struct TopicCloseReceipt {
    pub completed: bool,
    pub terminal: Option<Result<(), Error>>,
    pub complete_items: Option<u64>,
}

impl From<c2_server::topic::TopicCloseReceipt> for TopicCloseReceipt {
    fn from(receipt: c2_server::topic::TopicCloseReceipt) -> Self {
        Self {
            completed: receipt.completed,
            terminal: receipt
                .terminal
                .map(|result| result.map_err(Error::Semantic)),
            complete_items: receipt.complete_items,
        }
    }
}

/// Registration-scoped publisher handle (`registration.publisher(topic)`).
///
/// Bound permanently to one registration's sequence space and history ring.
/// Once that registration ends, `publish()` fails with `ResourceClosed` (707)
/// and never writes to a later registration of the same route name.
#[derive(Clone, Debug)]
pub struct TopicPublisher {
    inner: c2_server::topic::TopicPublisher,
}

impl TopicPublisher {
    pub(crate) fn from_server(inner: c2_server::topic::TopicPublisher) -> Self {
        Self { inner }
    }

    pub fn topic_name(&self) -> &str {
        self.inner.topic_name()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub fn head_seq(&self) -> u64 {
        self.inner.head_seq()
    }

    /// Publish an immutable payload into this registration's topic history.
    pub fn publish(&self, payload: Vec<u8>) -> Result<u64, Error> {
        self.inner.publish(payload).map_err(Error::Semantic)
    }

    /// Publish a borrowed byte slice directly into SHM or budgeted storage.
    pub fn publish_bytes(&self, payload: &[u8]) -> Result<u64, Error> {
        self.inner.publish_bytes(payload).map_err(Error::Semantic)
    }

    /// Allocate one SHM or budgeted backing of exact length `len` and write
    /// directly into it outside pool locks before publishing atomically.
    pub fn publish_with<F>(&self, len: usize, write_into: F) -> Result<u64, Error>
    where
        F: FnOnce(&mut [u8]) -> Result<(), c2_error::C2Error>,
    {
        self.inner
            .publish_with(len, write_into)
            .map_err(Error::Semantic)
    }
}

/// Same-process subscription attached to a registration's topic channel.
#[derive(Debug)]
pub struct LocalTopicSubscription {
    inner: c2_server::topic::LocalTopicSubscription,
}

impl LocalTopicSubscription {
    pub(crate) fn from_server(inner: c2_server::topic::LocalTopicSubscription) -> Self {
        Self { inner }
    }

    /// Confirmed initial sequence number captured atomically at attach time.
    pub fn start_seq(&self) -> u64 {
        self.inner.start_seq()
    }

    pub fn topic_name(&self) -> &str {
        self.inner.topic_name()
    }

    pub fn options(&self) -> SubscribeOptions {
        self.inner.options()
    }

    /// Block until the next delivery is available or the subscription reaches
    /// its terminal error (`CallCancelled` 718 or `SubscriptionLagged` 720).
    pub fn next_delivery(&self) -> Result<TopicDelivery, Error> {
        let runtime = shared_executor()?;
        runtime.block_on(self.next_delivery_async())
    }

    /// Async counterpart of [`Self::next_delivery`].
    pub async fn next_delivery_async(&self) -> Result<TopicDelivery, Error> {
        self.inner
            .next_delivery_async()
            .await
            .map_err(Error::Semantic)
    }

    /// Submit consumer cancellation intent without blocking.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Cancel and observe the terminal receipt.
    pub fn close(&self) -> TopicCloseReceipt {
        self.inner.close().into()
    }
}

/// One immutable topic delivery received by a remote [`crate::Client`].
///
/// Owns a local copy charged to the client domain (`HeldResponse`), completely
/// decoupled from the server's SHM allocation and subscription lifecycle.
pub struct CoreTopicDelivery {
    payload: HeldResponse,
    seq: u64,
    dropped: u64,
}

impl fmt::Debug for CoreTopicDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoreTopicDelivery")
            .field("seq", &self.seq)
            .field("dropped", &self.dropped)
            .field("len", &self.payload.bytes().len())
            .finish()
    }
}

impl CoreTopicDelivery {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn bytes(&self) -> &[u8] {
        self.payload.bytes()
    }

    pub fn clone_bytes(&self) -> Vec<u8> {
        self.payload.bytes().to_vec()
    }

    pub fn into_held(self) -> HeldResponse {
        self.payload
    }
}

enum SubscriptionTransport {
    Ipc(IpcTopicSubscription),
}

impl SubscriptionTransport {
    async fn recv(&mut self) -> Result<IpcTopicDelivery, Error> {
        match self {
            Self::Ipc(sub) => sub.recv().await.map_err(normalize_ipc_call_error),
        }
    }

    async fn completion(&mut self) -> Result<StreamTerminal, Error> {
        match self {
            Self::Ipc(sub) => sub.completion().await.map_err(normalize_ipc_call_error),
        }
    }

    fn try_completion(&self) -> Option<Result<StreamTerminal, Error>> {
        match self {
            Self::Ipc(sub) => sub
                .try_completion()
                .map(|result| result.map_err(normalize_ipc_call_error)),
        }
    }
}

/// Client-owned view of one remote topic subscription.
///
/// The underlying transport record retains its client-domain operation slot
/// until the real terminal `CALL_END` or transport end, independently of this
/// facade. Dropping this handle submits consumer cancellation without blocking.
pub struct CoreTopicSubscription {
    transport: Mutex<SubscriptionTransport>,
    target: Arc<ClientTarget>,
    cancellation: IpcSubscribeCancellation,
    scope: CallScope,
    start_seq: u64,
    topic_name: String,
    runtime: tokio::runtime::Handle,
}

impl fmt::Debug for CoreTopicSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoreTopicSubscription")
            .field("topic_name", &self.topic_name)
            .field("start_seq", &self.start_seq)
            .finish_non_exhaustive()
    }
}

impl CoreTopicSubscription {
    pub(crate) fn from_ipc(
        sub: IpcTopicSubscription,
        target: Arc<ClientTarget>,
        scope: CallScope,
        cancellation: IpcSubscribeCancellation,
    ) -> Self {
        let start_seq = sub.start_seq();
        let topic_name = sub.topic_name().to_string();
        Self {
            transport: Mutex::new(SubscriptionTransport::Ipc(sub)),
            target,
            cancellation,
            scope,
            start_seq,
            topic_name,
            runtime: shared_executor()
                .expect("subscription execution already started on shared runtime"),
        }
    }

    /// Confirmed initial sequence number reported by server `READY(start_seq)`.
    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn topic_name(&self) -> &str {
        &self.topic_name
    }

    /// Block until the next delivery arrives or the subscription reaches a
    /// terminal error (`CallCancelled` 718, `SubscriptionLagged` 720, deadline
    /// or transport error).
    pub fn next_delivery(&self) -> Result<CoreTopicDelivery, Error> {
        self.runtime.block_on(self.next_delivery_async())
    }

    /// Async counterpart of [`Self::next_delivery`].
    pub async fn next_delivery_async(&self) -> Result<CoreTopicDelivery, Error> {
        self.check_deadline()?;
        let mut transport = self.transport.lock().await;
        let received = match self.scope.deadline() {
            Some(deadline) => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), transport.recv())
                    .await
                    .map_err(|_| {
                        scope_error(crate::CallScopeError::DeadlineExceeded {
                            phase: TransportPhase::DispatchUncertain,
                        })
                    })?
            }
            None => transport.recv().await,
        };
        if let Err(error) = &received {
            self.target.observe_error(error);
        }
        self.check_deadline()?;
        match received {
            Ok(delivery) => Ok(CoreTopicDelivery {
                payload: HeldResponse::from_budgeted_bytes(delivery.bytes),
                seq: delivery.seq,
                dropped: delivery.dropped,
            }),
            Err(error) => {
                let _ = self.scope.complete_failure();
                Err(error)
            }
        }
    }

    #[doc(hidden)]
    pub fn check_deadline(&self) -> Result<(), Error> {
        match self.scope.observe_deadline() {
            CallState::ExpiredPreDispatch => Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                phase: TransportPhase::PreDispatch,
            })),
            CallState::ExpiredDispatchUncertain => {
                Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                    phase: TransportPhase::DispatchUncertain,
                }))
            }
            _ => Ok(()),
        }
    }

    /// Submit consumer cancellation intent without blocking.
    pub fn cancel(&self) -> Result<(), Error> {
        self.cancellation.cancel();
        Ok(())
    }

    /// Cancel and wait up to 30 seconds for the server's terminal `CALL_END`.
    pub fn close(&self) -> TopicCloseReceipt {
        self.close_with_timeout(Some(Duration::from_secs(30)))
    }

    /// Cancel and observe the real terminal outcome within `timeout`.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> TopicCloseReceipt {
        self.runtime
            .block_on(self.close_with_timeout_async(timeout))
    }

    #[doc(hidden)]
    pub async fn close_with_timeout_async(&self, timeout: Option<Duration>) -> TopicCloseReceipt {
        let started = Instant::now();
        let _ = self.cancel();
        let transport = match timeout {
            Some(Duration::ZERO) => self.transport.try_lock().ok(),
            Some(timeout) => tokio::time::timeout(timeout, self.transport.lock())
                .await
                .ok(),
            None => Some(self.transport.lock().await),
        };
        let Some(mut transport) = transport else {
            return TopicCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            };
        };
        let remaining = timeout.map(|t| t.saturating_sub(started.elapsed()));
        let terminal = match remaining {
            Some(Duration::ZERO) => transport.try_completion(),
            Some(timeout) => tokio::time::timeout(timeout, transport.completion())
                .await
                .ok(),
            None => Some(transport.completion().await),
        };
        match terminal {
            Some(Ok(terminal)) => {
                let complete_items = Some(terminal.complete_items);
                let outcome = subscription_terminal_outcome(terminal);
                if let Err(error) = &outcome {
                    self.target.observe_error(error);
                }
                let _ = self.scope.complete_failure();
                TopicCloseReceipt {
                    completed: true,
                    terminal: Some(outcome),
                    complete_items,
                }
            }
            Some(Err(error)) => {
                self.target.observe_error(&error);
                let _ = self.scope.complete_failure();
                TopicCloseReceipt {
                    completed: true,
                    terminal: Some(Err(error)),
                    complete_items: None,
                }
            }
            None => TopicCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            },
        }
    }
}

impl Drop for CoreTopicSubscription {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn subscription_terminal_outcome(terminal: StreamTerminal) -> Result<(), Error> {
    match terminal.status {
        CallStatus::Success => Ok(()),
        CallStatus::NotExecuted => Err(normalize_ipc_error(
            IpcError::CallRejected(terminal.error.expect("error CALL_END")),
            TransportPhase::PreDispatch,
        )),
        CallStatus::Failed => Err(normalize_ipc_error(
            IpcError::CrmError(terminal.error.expect("error CALL_END").to_wire_bytes()),
            TransportPhase::DispatchUncertain,
        )),
    }
}
