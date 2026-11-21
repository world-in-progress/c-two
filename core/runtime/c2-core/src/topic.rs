//! Registration-scoped publish/subscribe topic engine (ADR 0005).
//!
//! Each route registration owns isolated topic channels with monotonic `u64`
//! sequence numbers starting at `1`, a bounded history ring (`capacity >= 1`),
//! budgeted immutable frame storage, and a drain barrier for active
//! subscribers. Publishers never block on slow subscribers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use c2_mem::{
    BudgetedBuffer, DomainBudget, OperationPermit, PayloadBufferError, RetentionError,
    RetentionRejectReason,
};
use parking_lot::{Condvar, Mutex};
use tokio::sync::Notify;

use crate::Error;
use crate::call_execution::shared_executor;

/// Hard upper bound on a single topic's history ring depth.
pub const MAX_TOPIC_RING_CAPACITY: usize = 65_536;

/// Starting cursor selected when attaching a topic subscription.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TopicStart {
    /// Start from the currently retained latest frame if one exists, otherwise
    /// wait for sequence `1`.
    #[default]
    Latest,
    /// Skip all frames published before attach and wait for `head_seq + 1`.
    Next,
}

/// Subscriber policy when its next expected sequence number has been evicted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LagPolicy {
    /// Jump to the newest available frame and report the skipped count.
    #[default]
    Skip,
    /// Terminate this subscription with `SubscriptionLagged` (720).
    Error,
}

/// Per-topic registration configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicConfig {
    pub capacity: usize,
}

impl Default for TopicConfig {
    fn default() -> Self {
        Self { capacity: 1 }
    }
}

impl TopicConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if self.capacity == 0 || self.capacity > MAX_TOPIC_RING_CAPACITY {
            return Err(Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!(
                    "topic capacity must be between 1 and {MAX_TOPIC_RING_CAPACITY}, got {}",
                    self.capacity
                ),
            )));
        }
        Ok(())
    }
}

/// Subscription attach options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscribeOptions {
    pub start: TopicStart,
    pub on_lag: LagPolicy,
}

/// Truthful lifecycle reason why a topic or subscription closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TopicCloseCause {
    RouteUnregistered,
    HostShutdown,
    RelayShutdown,
    ConsumerCancelled,
}

impl TopicCloseCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RouteUnregistered => "route_unregistered",
            Self::HostShutdown => "host_shutdown",
            Self::RelayShutdown => "relay_shutdown",
            Self::ConsumerCancelled => "consumer_cancelled",
        }
    }

    pub fn into_error(self) -> C2Error {
        let message = match self {
            Self::RouteUnregistered => "subscription cancelled: route unregistered",
            Self::HostShutdown => "subscription cancelled: host shutdown",
            Self::RelayShutdown => "subscription cancelled: relay shutdown",
            Self::ConsumerCancelled => "subscription cancelled by consumer",
        };
        C2Error::new(ErrorCode::CallCancelled, message)
            .with_details(BTreeMap::from([("cause".into(), self.as_str().into())]))
    }
}

/// Immutable, budgeted frame stored inside a topic history ring and shared
/// across local deliveries (and later multi-loan SHM / relay fan-out).
#[derive(Debug)]
pub struct TopicFrame {
    seq: u64,
    storage: BudgetedBuffer,
}

impl TopicFrame {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn bytes(&self) -> &[u8] {
        &self.storage
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}

/// One immutable topic delivery yielded to a subscriber.
#[derive(Debug, Clone)]
pub struct TopicDelivery {
    frame: Arc<TopicFrame>,
    dropped: u64,
}

impl TopicDelivery {
    pub fn seq(&self) -> u64 {
        self.frame.seq()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn bytes(&self) -> &[u8] {
        self.frame.bytes()
    }

    pub fn clone_bytes(&self) -> Vec<u8> {
        self.frame.bytes().to_vec()
    }

    #[allow(dead_code)]
    pub(crate) fn frame(&self) -> &Arc<TopicFrame> {
        &self.frame
    }
}

/// Terminal observation returned by explicit subscription close.
#[derive(Debug)]
pub struct TopicCloseReceipt {
    pub completed: bool,
    pub terminal: Option<Result<(), Error>>,
    pub complete_items: Option<u64>,
}

/// Registration-wide drain barrier for active subscribers across all topics on
/// one route registration.
struct ActiveSubscriberBarrier {
    active: AtomicUsize,
    lock: Mutex<()>,
    drained: Condvar,
}

impl ActiveSubscriberBarrier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            lock: Mutex::new(()),
            drained: Condvar::new(),
        })
    }

    fn enter(self: &Arc<Self>) -> ActiveSubscriberGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        ActiveSubscriberGuard {
            barrier: Arc::clone(self),
        }
    }

    fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn wait_drained(&self, timeout: Duration) -> bool {
        if self.active.load(Ordering::Acquire) == 0 {
            return true;
        }
        let deadline = Instant::now() + timeout;
        let mut guard = self.lock.lock();
        while self.active.load(Ordering::Acquire) > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let _ = self.drained.wait_for(&mut guard, deadline - now);
        }
        true
    }
}

struct ActiveSubscriberGuard {
    barrier: Arc<ActiveSubscriberBarrier>,
}

impl Drop for ActiveSubscriberGuard {
    fn drop(&mut self) {
        if self.barrier.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _guard = self.barrier.lock.lock();
            self.barrier.drained.notify_all();
        }
    }
}

struct TopicChannelState {
    capacity: usize,
    head_seq: u64,
    ring: VecDeque<Arc<TopicFrame>>,
    closed: Option<TopicCloseCause>,
}

pub(crate) struct TopicChannel {
    topic_name: String,
    budget: DomainBudget,
    barrier: Arc<ActiveSubscriberBarrier>,
    state: Mutex<TopicChannelState>,
    notify: Notify,
}

impl fmt::Debug for TopicChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("TopicChannel")
            .field("topic_name", &self.topic_name)
            .field("capacity", &state.capacity)
            .field("head_seq", &state.head_seq)
            .field("ring_len", &state.ring.len())
            .field("closed", &state.closed)
            .finish()
    }
}

impl TopicChannel {
    fn new(
        topic_name: String,
        capacity: usize,
        budget: DomainBudget,
        barrier: Arc<ActiveSubscriberBarrier>,
    ) -> Arc<Self> {
        Arc::new(Self {
            topic_name,
            budget,
            barrier,
            state: Mutex::new(TopicChannelState {
                capacity,
                head_seq: 0,
                ring: VecDeque::with_capacity(capacity.min(64)),
                closed: None,
            }),
            notify: Notify::new(),
        })
    }

    pub(crate) fn topic_name(&self) -> &str {
        &self.topic_name
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().closed.is_some()
    }

    pub(crate) fn head_seq(&self) -> u64 {
        self.state.lock().head_seq
    }

    pub(crate) fn publish(&self, mut payload: Vec<u8>) -> Result<u64, Error> {
        // Fast pre-check before allocating/charging memory.
        if self.is_closed() {
            return Err(resource_closed_error(&self.topic_name));
        }

        // Trim spare capacity so accounting matches actual payload size.
        payload.shrink_to_fit();
        let charge_bytes = u64::try_from(payload.capacity()).map_err(|_| {
            Error::Semantic(C2Error::new(
                ErrorCode::CallCapacityExceeded,
                "topic payload size exceeds u64",
            ))
        })?;
        let reservation = self
            .budget
            .try_reserve_memory(charge_bytes)
            .map_err(|error| topic_memory_capacity_error(&self.topic_name, error))?;
        let storage = BudgetedBuffer::new(payload, reservation);

        let mut evicted: Option<Arc<TopicFrame>> = None;
        let seq = {
            let mut state = self.state.lock();
            if state.closed.is_some() {
                drop(state);
                drop(storage);
                return Err(resource_closed_error(&self.topic_name));
            }
            let next_seq = state.head_seq.checked_add(1).ok_or_else(|| {
                Error::Semantic(C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    format!("topic {:?} sequence number space exhausted", self.topic_name),
                ))
            })?;
            let frame = Arc::new(TopicFrame {
                seq: next_seq,
                storage,
            });
            state.ring.push_back(frame);
            if state.ring.len() > state.capacity {
                evicted = state.ring.pop_front();
            }
            state.head_seq = next_seq;
            next_seq
        };

        // Wake subscribers and drop any evicted frame outside the lock.
        self.notify.notify_waiters();
        drop(evicted);
        Ok(seq)
    }

    fn close(&self, cause: TopicCloseCause) {
        let evicted = {
            let mut state = self.state.lock();
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(cause);
            std::mem::take(&mut state.ring)
        };
        self.notify.notify_waiters();
        drop(evicted);
    }

    pub(crate) fn subscribe_local(
        self: &Arc<Self>,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, Error> {
        let permit = self
            .budget
            .reserve_subscription_operation()
            .map_err(|error| subscription_slot_error(&self.topic_name, error))?;

        let (start_seq, guard) = {
            let state = self.state.lock();
            if state.closed.is_some() {
                drop(state);
                drop(permit);
                return Err(resource_closed_error(&self.topic_name));
            }
            let head = state.head_seq;
            let start_seq = match options.start {
                TopicStart::Latest => {
                    if head > 0 {
                        head
                    } else {
                        1
                    }
                }
                TopicStart::Next => head.checked_add(1).ok_or_else(|| {
                    Error::Semantic(C2Error::new(
                        ErrorCode::CallCapacityExceeded,
                        format!("topic {:?} sequence number space exhausted", self.topic_name),
                    ))
                })?,
            };
            (start_seq, self.barrier.enter())
        };

        Ok(LocalTopicSubscription {
            channel: Arc::clone(self),
            start_seq,
            options,
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
            cursor: Mutex::new(LocalCursorState {
                next_expected: start_seq,
                complete_items: 0,
                terminal: None,
                permit: Some(permit),
                guard: Some(guard),
            }),
        })
    }
}

/// Registration-owned collection of topic channels and their shared drain
/// barrier.
pub(crate) struct RouteTopics {
    route_name: String,
    expected: ExpectedRouteContract,
    channels: HashMap<String, Arc<TopicChannel>>,
    barrier: Arc<ActiveSubscriberBarrier>,
    closed: AtomicBool,
}

impl fmt::Debug for RouteTopics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteTopics")
            .field("route_name", &self.route_name)
            .field("topic_count", &self.channels.len())
            .field("active_subscribers", &self.barrier.active_count())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl RouteTopics {
    pub(crate) fn new(
        expected: ExpectedRouteContract,
        topic_configs: &BTreeMap<String, TopicConfig>,
        budget: DomainBudget,
    ) -> Arc<Self> {
        let barrier = ActiveSubscriberBarrier::new();
        let mut channels = HashMap::with_capacity(topic_configs.len());
        for (name, config) in topic_configs {
            channels.insert(
                name.clone(),
                TopicChannel::new(
                    name.clone(),
                    config.capacity,
                    budget.clone(),
                    Arc::clone(&barrier),
                ),
            );
        }
        Arc::new(Self {
            route_name: expected.route_name.clone(),
            expected,
            channels,
            barrier,
            closed: AtomicBool::new(false),
        })
    }

    pub(crate) fn expected(&self) -> &ExpectedRouteContract {
        &self.expected
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn active_subscribers(&self) -> usize {
        self.barrier.active_count()
    }

    pub(crate) fn publisher(&self, topic_name: &str) -> Result<TopicPublisher, Error> {
        if self.is_closed() {
            return Err(resource_closed_error(topic_name));
        }
        let channel = self.channels.get(topic_name).ok_or_else(|| {
            Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!(
                    "unknown topic {:?} on route {:?}",
                    topic_name, self.route_name
                ),
            ))
        })?;
        Ok(TopicPublisher {
            channel: Arc::clone(channel),
        })
    }

    pub(crate) fn subscribe_local(
        &self,
        topic_name: &str,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, Error> {
        if self.is_closed() {
            return Err(resource_closed_error(topic_name));
        }
        let channel = self.channels.get(topic_name).ok_or_else(|| {
            Error::Semantic(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!(
                    "unknown topic {:?} on route {:?}",
                    topic_name, self.route_name
                ),
            ))
        })?;
        channel.subscribe_local(options)
    }

    pub(crate) fn close(&self, cause: TopicCloseCause) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        for channel in self.channels.values() {
            channel.close(cause);
        }
    }

    pub(crate) fn wait_drained(&self, timeout: Duration) -> bool {
        self.barrier.wait_drained(timeout)
    }
}

/// Registration-scoped publisher handle (`registration.publisher(topic)`).
///
/// Bound permanently to one registration's sequence space and history ring.
/// Once that registration ends, `publish()` fails with `ResourceClosed` (707)
/// and never writes to a later registration of the same route name.
#[derive(Clone, Debug)]
pub struct TopicPublisher {
    channel: Arc<TopicChannel>,
}

impl TopicPublisher {
    pub fn topic_name(&self) -> &str {
        self.channel.topic_name()
    }

    pub fn is_closed(&self) -> bool {
        self.channel.is_closed()
    }

    pub fn head_seq(&self) -> u64 {
        self.channel.head_seq()
    }

    /// Publish an immutable payload into this registration's topic history.
    ///
    /// Never blocks on slow subscribers. Fails with `CallCapacityExceeded`
    /// (717) without advancing `seq` or overwriting the previous latest frame
    /// if the Host's `server` memory budget cannot admit the payload.
    pub fn publish(&self, payload: Vec<u8>) -> Result<u64, Error> {
        self.channel.publish(payload)
    }
}

#[derive(Clone)]
struct LocalTopicTerminal {
    error: C2Error,
    complete_items: u64,
}

struct LocalCursorState {
    next_expected: u64,
    complete_items: u64,
    terminal: Option<LocalTopicTerminal>,
    permit: Option<OperationPermit>,
    guard: Option<ActiveSubscriberGuard>,
}

impl LocalCursorState {
    fn settle(&mut self, error: C2Error) -> LocalTopicTerminal {
        if let Some(existing) = &self.terminal {
            return existing.clone();
        }
        let terminal = LocalTopicTerminal {
            error,
            complete_items: self.complete_items,
        };
        self.terminal = Some(terminal.clone());
        // Release business slot and drain-barrier guard as soon as terminal is settled.
        self.permit.take();
        self.guard.take();
        terminal
    }
}

/// Same-process subscription attached to a registration's topic channel.
pub struct LocalTopicSubscription {
    channel: Arc<TopicChannel>,
    start_seq: u64,
    options: SubscribeOptions,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    cursor: Mutex<LocalCursorState>,
}

impl fmt::Debug for LocalTopicSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cursor = self.cursor.lock();
        formatter
            .debug_struct("LocalTopicSubscription")
            .field("topic_name", &self.channel.topic_name())
            .field("start_seq", &self.start_seq)
            .field("options", &self.options)
            .field("next_expected", &cursor.next_expected)
            .field("complete_items", &cursor.complete_items)
            .field("terminated", &cursor.terminal.is_some())
            .finish()
    }
}

enum SelectOutcome {
    Delivery(TopicDelivery),
    Wait,
    Terminal(C2Error),
}

impl LocalTopicSubscription {
    /// Confirmed initial sequence number captured atomically at attach time.
    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn topic_name(&self) -> &str {
        self.channel.topic_name()
    }

    pub fn options(&self) -> SubscribeOptions {
        self.options
    }

    /// Block until the next delivery is available or the subscription reaches
    /// its terminal error (`CallCancelled` 718 or `SubscriptionLagged` 720).
    pub fn next_delivery(&self) -> Result<TopicDelivery, Error> {
        let runtime = shared_executor()?;
        runtime.block_on(self.next_delivery_async())
    }

    /// Async counterpart of [`Self::next_delivery`].
    pub async fn next_delivery_async(&self) -> Result<TopicDelivery, Error> {
        loop {
            let chan_notified = self.channel.notify.notified();
            let cancel_notified = self.cancel_notify.notified();
            tokio::pin!(chan_notified);
            tokio::pin!(cancel_notified);

            let step = {
                let mut cursor = self.cursor.lock();
                if let Some(terminal) = &cursor.terminal {
                    return Err(Error::Semantic(terminal.error.clone()));
                }
                if self.cancelled.load(Ordering::Acquire) {
                    let terminal = cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
                    return Err(Error::Semantic(terminal.error));
                }

                // Register cancel waiter before checking/releasing locks so a
                // concurrent `cancel()` can never be missed.
                cancel_notified.as_mut().enable();
                if self.cancelled.load(Ordering::Acquire) {
                    let terminal = cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
                    return Err(Error::Semantic(terminal.error));
                }

                let state = self.channel.state.lock();
                let next_expected = cursor.next_expected;
                if let Some(cause) = state.closed {
                    let terminal = cursor.settle(cause.into_error());
                    SelectOutcome::Terminal(terminal.error)
                } else if state.head_seq < next_expected {
                    // Desired frame not published yet. Register channel waiter
                    // while holding channel state lock.
                    chan_notified.as_mut().enable();
                    SelectOutcome::Wait
                } else if let Some(first) = state.ring.front() {
                    let first_seq = first.seq();
                    if next_expected >= first_seq {
                        let offset = usize::try_from(next_expected - first_seq)
                            .expect("ring offset fits usize");
                        let frame = Arc::clone(
                            state
                                .ring
                                .get(offset)
                                .expect("contiguous sequence numbers inside history ring"),
                        );
                        cursor.next_expected = frame.seq().saturating_add(1);
                        cursor.complete_items += 1;
                        SelectOutcome::Delivery(TopicDelivery { frame, dropped: 0 })
                    } else {
                        // `next_expected` was evicted from the bounded ring.
                        let last = state.ring.back().expect("non-empty ring has back");
                        let last_seq = last.seq();
                        match self.options.on_lag {
                            LagPolicy::Skip => {
                                let frame = Arc::clone(last);
                                let dropped = last_seq - next_expected;
                                cursor.next_expected = last_seq.saturating_add(1);
                                cursor.complete_items += 1;
                                SelectOutcome::Delivery(TopicDelivery { frame, dropped })
                            }
                            LagPolicy::Error => {
                                let dropped = first_seq - next_expected;
                                let lag_err = subscription_lagged_error(
                                    &self.channel.topic_name,
                                    next_expected,
                                    first_seq,
                                    last_seq,
                                    dropped,
                                );
                                let terminal = cursor.settle(lag_err);
                                SelectOutcome::Terminal(terminal.error)
                            }
                        }
                    }
                } else {
                    // Empty ring while head_seq >= next_expected only happens
                    // if ring was closed/cleared; handled above, wait defensively.
                    chan_notified.as_mut().enable();
                    SelectOutcome::Wait
                }
            };

            match step {
                SelectOutcome::Delivery(delivery) => return Ok(delivery),
                SelectOutcome::Terminal(err) => return Err(Error::Semantic(err)),
                SelectOutcome::Wait => {
                    tokio::select! {
                        _ = chan_notified => {}
                        _ = cancel_notified => {}
                    }
                }
            }
        }
    }

    /// Submit consumer cancellation intent without blocking.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }

    /// Cancel and observe the terminal receipt.
    pub fn close(&self) -> TopicCloseReceipt {
        self.cancel();
        let mut cursor = self.cursor.lock();
        let terminal = cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
        TopicCloseReceipt {
            completed: true,
            terminal: Some(Err(Error::Semantic(terminal.error))),
            complete_items: Some(terminal.complete_items),
        }
    }
}

impl Drop for LocalTopicSubscription {
    fn drop(&mut self) {
        self.cancel();
        let mut cursor = self.cursor.lock();
        cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
    }
}

fn resource_closed_error(topic_name: &str) -> Error {
    Error::Semantic(C2Error::new(
        ErrorCode::ResourceClosed,
        format!("topic {topic_name:?} registration is closed"),
    ))
}

fn subscription_lagged_error(
    topic_name: &str,
    expected_seq: u64,
    available_first_seq: u64,
    available_last_seq: u64,
    dropped: u64,
) -> C2Error {
    C2Error::new(
        ErrorCode::SubscriptionLagged,
        format!(
            "subscription on topic {topic_name:?} lagged: expected seq {expected_seq}, available [{available_first_seq}, {available_last_seq}]"
        ),
    )
    .with_details(BTreeMap::from([
        ("topic".into(), topic_name.into()),
        ("expected_seq".into(), expected_seq.to_string()),
        (
            "available_first_seq".into(),
            available_first_seq.to_string(),
        ),
        ("available_last_seq".into(), available_last_seq.to_string()),
        ("dropped".into(), dropped.to_string()),
    ]))
}

fn topic_memory_capacity_error(topic_name: &str, error: PayloadBufferError) -> Error {
    let mut details = BTreeMap::from([
        ("role".into(), "server".into()),
        ("stage".into(), "topic_publish".into()),
        ("topic".into(), topic_name.into()),
    ]);
    match error {
        PayloadBufferError::TooLarge { requested, limit } => {
            details.insert("requested".into(), requested.to_string());
            details.insert("limit".into(), limit.to_string());
        }
        PayloadBufferError::InsufficientCapacity {
            requested,
            available,
        } => {
            details.insert("requested".into(), requested.to_string());
            details.insert("available".into(), available.to_string());
        }
        PayloadBufferError::DeadlineExceeded => {
            details.insert("reason".into(), "deadline_exceeded".into());
        }
    }
    Error::Semantic(
        C2Error::new(ErrorCode::CallCapacityExceeded, error.to_string()).with_details(details),
    )
}

fn subscription_slot_error(topic_name: &str, error: RetentionError) -> Error {
    let code = match error.reason {
        RetentionRejectReason::Closed => ErrorCode::ResourceClosed,
        RetentionRejectReason::LimitReached => ErrorCode::CallCapacityExceeded,
    };
    Error::Semantic(C2Error::new(code, error.to_string()).with_details(BTreeMap::from([
        ("role".into(), "server".into()),
        ("stage".into(), "topic_subscribe".into()),
        ("topic".into(), topic_name.into()),
        ("used_operations".into(), error.used_operations.to_string()),
        ("max_operations".into(), error.max_operations.to_string()),
    ])))
}
