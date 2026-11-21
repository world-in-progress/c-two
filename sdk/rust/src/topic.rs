//! Typed publish/subscribe owners over Core's topic engine (ADR 0005).
//!
//! [`Publisher`] and [`Subscription`] wrap the Core handles generated code
//! binds to one contract topic. Every delivery owns a local value; the
//! explicit retention projection `Delivery<Held<T>>` follows the same
//! copy-backed contract as unary holds, and closing a subscription never
//! invalidates an already-delivered `Held<T>`.

use std::fmt;
use std::time::Duration;

use c2_core::{
    CoreTopicSubscription, Error, HeldResponse, LocalTopicSubscription, TopicCloseReceipt,
    TopicPublisher as CoreTopicPublisher,
};

use crate::Held;

/// Registration-scoped typed publisher handle.
///
/// The handle is bound permanently to one registration's sequence space.
/// After that registration ends, `publish` fails with `ResourceClosed` (707)
/// and never writes to a later registration of the same route name.
#[derive(Clone)]
pub struct Publisher<T> {
    core: CoreTopicPublisher,
    encode: fn(&T) -> Result<Vec<u8>, Error>,
}

impl<T> Publisher<T> {
    #[doc(hidden)]
    pub fn from_core(core: CoreTopicPublisher, encode: fn(&T) -> Result<Vec<u8>, Error>) -> Self {
        Self { core, encode }
    }

    /// Publish one immutable value into this registration's topic history.
    ///
    /// The value is encoded once; the encoded bytes move into the sealed
    /// topic frame without a second copy. Returns the frame's sequence number.
    pub fn publish(&self, value: &T) -> Result<u64, Error> {
        let payload = (self.encode)(value)?;
        self.core.publish(payload)
    }

    pub fn topic_name(&self) -> &str {
        self.core.topic_name()
    }

    pub fn is_closed(&self) -> bool {
        self.core.is_closed()
    }

    pub fn head_seq(&self) -> u64 {
        self.core.head_seq()
    }
}

impl<T> fmt::Debug for Publisher<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Publisher")
            .field("topic_name", &self.topic_name())
            .field("closed", &self.is_closed())
            .field("head_seq", &self.head_seq())
            .finish_non_exhaustive()
    }
}

/// One immutable topic delivery.
///
/// `value` owns its local copy; `dropped` counts the frames skipped between
/// the subscriber's expected sequence number and this frame under the `skip`
/// lag policy.
pub struct Delivery<T> {
    value: T,
    seq: u64,
    dropped: u64,
}

impl<T> Delivery<T> {
    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Consume the delivery into its value, sequence number and drop count.
    pub fn into_parts(self) -> (T, u64, u64) {
        (self.value, self.seq, self.dropped)
    }
}

impl<T: fmt::Debug> fmt::Debug for Delivery<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Delivery")
            .field("seq", &self.seq)
            .field("dropped", &self.dropped)
            .field("value", &self.value)
            .finish()
    }
}

enum SubscriptionTransport {
    Remote(CoreTopicSubscription),
    Local(LocalTopicSubscription),
}

/// A typed live subscription to one contract topic.
///
/// The underlying transport record keeps its operation slot until the real
/// terminal outcome, independently of this facade. Dropping the subscription
/// submits consumer cancellation without blocking; call [`Self::close`] to
/// observe the actual terminal state.
#[must_use = "subscriptions must be consumed or explicitly closed"]
pub struct Subscription<T> {
    transport: SubscriptionTransport,
    decode: fn(&[u8]) -> Result<T, Error>,
    decode_held: fn(HeldResponse) -> Result<Held<T>, Error>,
}

impl<T> Subscription<T> {
    #[doc(hidden)]
    pub fn from_core(
        core: CoreTopicSubscription,
        decode: fn(&[u8]) -> Result<T, Error>,
        decode_held: fn(HeldResponse) -> Result<Held<T>, Error>,
    ) -> Self {
        Self {
            transport: SubscriptionTransport::Remote(core),
            decode,
            decode_held,
        }
    }

    /// Typed projection of a same-process subscription attached directly to a
    /// registration. A held delivery from a local subscription owns an
    /// explicit local copy of the frame, matching the remote hold contract.
    #[doc(hidden)]
    pub fn from_local(
        local: LocalTopicSubscription,
        decode: fn(&[u8]) -> Result<T, Error>,
        decode_held: fn(HeldResponse) -> Result<Held<T>, Error>,
    ) -> Self {
        Self {
            transport: SubscriptionTransport::Local(local),
            decode,
            decode_held,
        }
    }

    /// Confirmed initial sequence number captured atomically at attach time.
    pub fn start_seq(&self) -> u64 {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => sub.start_seq(),
            SubscriptionTransport::Local(sub) => sub.start_seq(),
        }
    }

    pub fn topic_name(&self) -> &str {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => sub.topic_name(),
            SubscriptionTransport::Local(sub) => sub.topic_name(),
        }
    }

    /// Block until the next delivery is available or the subscription reaches
    /// its terminal error (`CallCancelled` 718, `SubscriptionLagged` 720,
    /// deadline or transport error).
    pub fn next_delivery(&self) -> Result<Delivery<T>, Error> {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => {
                let delivery = sub.next_delivery()?;
                let value = (self.decode)(delivery.bytes())?;
                Ok(Delivery {
                    value,
                    seq: delivery.seq(),
                    dropped: delivery.dropped(),
                })
            }
            SubscriptionTransport::Local(sub) => {
                let delivery = sub.next_delivery()?;
                let value = (self.decode)(delivery.bytes())?;
                Ok(Delivery {
                    value,
                    seq: delivery.seq(),
                    dropped: delivery.dropped(),
                })
            }
        }
    }

    /// Explicitly retained variant of [`Self::next_delivery`].
    ///
    /// The returned `Held<T>` owns its local copy; releasing it is the
    /// caller's responsibility and survives closing this subscription.
    pub fn next_delivery_held(&self) -> Result<Delivery<Held<T>>, Error> {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => {
                let delivery = sub.next_delivery()?;
                let seq = delivery.seq();
                let dropped = delivery.dropped();
                let held = (self.decode_held)(delivery.into_held())?;
                Ok(Delivery {
                    value: held,
                    seq,
                    dropped,
                })
            }
            SubscriptionTransport::Local(sub) => {
                let delivery = sub.next_delivery()?;
                let response = HeldResponse::from_owned_bytes(delivery.clone_bytes());
                let held = (self.decode_held)(response)?;
                Ok(Delivery {
                    value: held,
                    seq: delivery.seq(),
                    dropped: delivery.dropped(),
                })
            }
        }
    }

    /// Submit consumer cancellation intent without blocking.
    pub fn cancel(&self) -> Result<(), Error> {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => sub.cancel(),
            SubscriptionTransport::Local(sub) => {
                sub.cancel();
                Ok(())
            }
        }
    }

    /// Cancel and observe up to the default 30 second budget.
    pub fn close(&self) -> TopicCloseReceipt {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => sub.close(),
            SubscriptionTransport::Local(sub) => sub.close(),
        }
    }

    /// Cancel and observe the real terminal outcome within `timeout`.
    /// `None` waits indefinitely; zero duration performs a single observation.
    /// Local subscriptions close without a network wait, so the timeout only
    /// bounds the remote transport.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> TopicCloseReceipt {
        match &self.transport {
            SubscriptionTransport::Remote(sub) => sub.close_with_timeout(timeout),
            SubscriptionTransport::Local(sub) => sub.close(),
        }
    }
}

impl<T> fmt::Debug for Subscription<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Subscription")
            .field("topic_name", &self.topic_name())
            .field("start_seq", &self.start_seq())
            .finish_non_exhaustive()
    }
}
