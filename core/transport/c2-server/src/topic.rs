//! Registration-scoped publish/subscribe topic engine (ADR 0005).
//!
//! Each route registration owns isolated topic channels with monotonic `u64`
//! sequence numbers starting at `1`, a bounded history ring (`capacity >= 1`),
//! single-write sealed SHM / budgeted immutable frame storage, and a sync+async
//! drain barrier for active local and remote subscribers. Publishers never
//! block on slow subscribers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use c2_mem::{
    BudgetedBuffer, DomainBudget, MemPool, OperationPermit, PayloadBufferError, RetentionError,
    RetentionRejectReason, SealedShmBlock,
};
use c2_wire::rpc::{WireLagPolicy, WireSubscribeOptions, WireTopicStart};
use parking_lot::{Condvar, Mutex, RwLock};
use tokio::sync::Notify;

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

impl From<WireTopicStart> for TopicStart {
    fn from(value: WireTopicStart) -> Self {
        match value {
            WireTopicStart::Latest => Self::Latest,
            WireTopicStart::Next => Self::Next,
        }
    }
}

impl From<TopicStart> for WireTopicStart {
    fn from(value: TopicStart) -> Self {
        match value {
            TopicStart::Latest => Self::Latest,
            TopicStart::Next => Self::Next,
        }
    }
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

impl From<WireLagPolicy> for LagPolicy {
    fn from(value: WireLagPolicy) -> Self {
        match value {
            WireLagPolicy::Skip => Self::Skip,
            WireLagPolicy::Error => Self::Error,
        }
    }
}

impl From<LagPolicy> for WireLagPolicy {
    fn from(value: LagPolicy) -> Self {
        match value {
            LagPolicy::Skip => Self::Skip,
            LagPolicy::Error => Self::Error,
        }
    }
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
    pub fn validate(&self) -> Result<(), C2Error> {
        if self.capacity == 0 || self.capacity > MAX_TOPIC_RING_CAPACITY {
            return Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!(
                    "topic capacity must be between 1 and {MAX_TOPIC_RING_CAPACITY}, got {}",
                    self.capacity
                ),
            ));
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

impl From<WireSubscribeOptions> for SubscribeOptions {
    fn from(value: WireSubscribeOptions) -> Self {
        Self {
            start: value.start.into(),
            on_lag: value.on_lag.into(),
        }
    }
}

impl From<SubscribeOptions> for WireSubscribeOptions {
    fn from(value: SubscribeOptions) -> Self {
        Self {
            start: value.start.into(),
            on_lag: value.on_lag.into(),
        }
    }
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

#[derive(Debug)]
enum TopicFrameStorage {
    Shm(SealedShmBlock),
    Inline(BudgetedBuffer),
}

/// Immutable, budgeted frame stored inside a topic history ring and shared
/// across local deliveries and 1-to-N multi-loan SHM / inline remote delivery.
#[derive(Debug)]
pub struct TopicFrame {
    seq: u64,
    storage: TopicFrameStorage,
}

impl TopicFrame {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.storage {
            TopicFrameStorage::Shm(block) => block.as_slice(),
            TopicFrameStorage::Inline(buffer) => buffer,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    pub fn is_shm(&self) -> bool {
        matches!(self.storage, TopicFrameStorage::Shm(_))
    }

    pub(crate) fn shm_block(&self) -> Option<&SealedShmBlock> {
        match &self.storage {
            TopicFrameStorage::Shm(block) => Some(block),
            TopicFrameStorage::Inline(_) => None,
        }
    }

    pub fn copy_range(&self, offset: usize, dst: &mut [u8]) -> Result<(), String> {
        match &self.storage {
            TopicFrameStorage::Shm(block) => block.copy_range(offset, dst),
            TopicFrameStorage::Inline(buffer) => {
                let end = offset
                    .checked_add(dst.len())
                    .ok_or_else(|| "topic frame range overflow".to_string())?;
                if end > buffer.len() {
                    return Err(format!(
                        "topic frame range {offset}..{end} exceeds length {}",
                        buffer.len()
                    ));
                }
                dst.copy_from_slice(&buffer[offset..end]);
                Ok(())
            }
        }
    }
}

/// One immutable topic delivery yielded to a local subscriber.
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

    pub fn is_shm(&self) -> bool {
        self.frame.is_shm()
    }
}

/// Terminal observation returned by explicit subscription close.
#[derive(Debug)]
pub struct TopicCloseReceipt {
    pub completed: bool,
    pub terminal: Option<Result<(), C2Error>>,
    pub complete_items: Option<u64>,
}

/// Registration-wide drain barrier for active local and remote subscribers
/// across all topics on one route registration.
struct ActiveSubscriberBarrier {
    active: AtomicUsize,
    lock: Mutex<()>,
    drained_cv: Condvar,
    drained_notify: Notify,
}

impl ActiveSubscriberBarrier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            lock: Mutex::new(()),
            drained_cv: Condvar::new(),
            drained_notify: Notify::new(),
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
            let _ = self.drained_cv.wait_for(&mut guard, deadline - now);
        }
        true
    }

    async fn wait_drained_async(&self) {
        loop {
            let notified = self.drained_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct ActiveSubscriberGuard {
    barrier: Arc<ActiveSubscriberBarrier>,
}

impl Drop for ActiveSubscriberGuard {
    fn drop(&mut self) {
        if self.barrier.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _guard = self.barrier.lock.lock();
            self.barrier.drained_cv.notify_all();
            self.barrier.drained_notify.notify_waiters();
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
    response_pool: Option<Arc<RwLock<MemPool>>>,
    max_payload_size: u64,
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

pub(crate) struct RemoteTopicAttach {
    pub(crate) channel: Arc<TopicChannel>,
    pub(crate) start_seq: u64,
    pub(crate) options: SubscribeOptions,
    pub(crate) _guard: ActiveSubscriberGuard,
}

pub(crate) enum RemoteSelectOutcome {
    Frame(Arc<TopicFrame>),
    Wait,
    Terminal(C2Error),
}

impl TopicChannel {
    fn new(
        topic_name: String,
        capacity: usize,
        budget: DomainBudget,
        response_pool: Option<Arc<RwLock<MemPool>>>,
        max_payload_size: u64,
        barrier: Arc<ActiveSubscriberBarrier>,
    ) -> Arc<Self> {
        Arc::new(Self {
            topic_name,
            budget,
            response_pool,
            max_payload_size,
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

    pub(crate) fn close_cause(&self) -> Option<TopicCloseCause> {
        self.state.lock().closed
    }

    pub(crate) fn head_seq(&self) -> u64 {
        self.state.lock().head_seq
    }

    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    pub(crate) fn publish(&self, mut payload: Vec<u8>) -> Result<u64, C2Error> {
        if self.is_closed() {
            return Err(resource_closed_error(&self.topic_name));
        }
        let len = payload.len();
        if (len as u64) > self.max_payload_size {
            return Err(payload_too_large_error(
                &self.topic_name,
                len,
                self.max_payload_size,
            ));
        }

        let storage = self.store_vec_payload(&mut payload)?;
        drop(payload);
        self.commit_storage(storage)
    }

    pub(crate) fn publish_bytes(&self, payload: &[u8]) -> Result<u64, C2Error> {
        self.publish_with(payload.len(), |dst| {
            dst.copy_from_slice(payload);
            Ok(())
        })
    }

    pub(crate) fn publish_with<F>(&self, len: usize, write_into: F) -> Result<u64, C2Error>
    where
        F: FnOnce(&mut [u8]) -> Result<(), C2Error>,
    {
        if self.is_closed() {
            return Err(resource_closed_error(&self.topic_name));
        }
        if (len as u64) > self.max_payload_size {
            return Err(payload_too_large_error(
                &self.topic_name,
                len,
                self.max_payload_size,
            ));
        }

        let storage = self.store_generated_payload(len, write_into)?;
        self.commit_storage(storage)
    }

    fn store_vec_payload(&self, payload: &mut Vec<u8>) -> Result<TopicFrameStorage, C2Error> {
        let len = payload.len();
        if len > c2_config::SHM_THRESHOLD as usize
            && u32::try_from(len).is_ok()
            && let Some(pool) = &self.response_pool
            && let Ok(block) = SealedShmBlock::from_bytes(Arc::clone(pool), payload)
        {
            return Ok(TopicFrameStorage::Shm(block));
        }
        let mut owned = std::mem::take(payload);
        owned.shrink_to_fit();
        let charge_bytes = u64::try_from(owned.capacity()).map_err(|_| {
            C2Error::new(
                ErrorCode::CallCapacityExceeded,
                "topic payload size exceeds u64",
            )
        })?;
        let reservation = self
            .budget
            .try_reserve_memory(charge_bytes)
            .map_err(|error| topic_memory_capacity_error(&self.topic_name, error))?;
        Ok(TopicFrameStorage::Inline(BudgetedBuffer::new(
            owned,
            reservation,
        )))
    }

    fn store_generated_payload<F>(
        &self,
        len: usize,
        write_into: F,
    ) -> Result<TopicFrameStorage, C2Error>
    where
        F: FnOnce(&mut [u8]) -> Result<(), C2Error>,
    {
        if len > c2_config::SHM_THRESHOLD as usize
            && u32::try_from(len).is_ok()
            && let Some(pool) = &self.response_pool
        {
            let handle = {
                let mut guard = pool.write();
                guard.try_alloc_shm(len).ok()
            };
            if let Some(mut handle) = handle {
                let fill_result = (|| -> Result<(), C2Error> {
                    let mut pin = {
                        let guard = pool.read();
                        guard.pin_shm_mut(&mut handle).map_err(|error| {
                            C2Error::new(ErrorCode::ResourceOutputSerializing, error)
                        })?
                    };
                    write_into(pin.as_mut_slice())
                })();
                if let Err(error) = fill_result {
                    pool.write().release_handle(handle);
                    return Err(error);
                }
                let block =
                    SealedShmBlock::seal(Arc::clone(pool), handle).map_err(|(handle, error)| {
                        pool.write().release_handle(handle);
                        C2Error::new(ErrorCode::ResourceOutputSerializing, error)
                    })?;
                return Ok(TopicFrameStorage::Shm(block));
            }
        }
        let charge_bytes = u64::try_from(len).map_err(|_| {
            C2Error::new(
                ErrorCode::CallCapacityExceeded,
                "topic payload size exceeds u64",
            )
        })?;
        let reservation = self
            .budget
            .try_reserve_memory(charge_bytes)
            .map_err(|error| topic_memory_capacity_error(&self.topic_name, error))?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(len).map_err(|error| {
            C2Error::new(
                ErrorCode::CallCapacityExceeded,
                format!("topic {:?} allocation failed: {error}", self.topic_name),
            )
        })?;
        owned.resize(len, 0);
        write_into(&mut owned)?;
        Ok(TopicFrameStorage::Inline(BudgetedBuffer::new(
            owned,
            reservation,
        )))
    }

    fn commit_storage(&self, storage: TopicFrameStorage) -> Result<u64, C2Error> {
        let mut evicted: Option<Arc<TopicFrame>> = None;
        let seq = {
            let mut state = self.state.lock();
            if state.closed.is_some() {
                drop(state);
                drop(storage);
                return Err(resource_closed_error(&self.topic_name));
            }
            let next_seq = state.head_seq.checked_add(1).ok_or_else(|| {
                C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    format!("topic {:?} sequence number space exhausted", self.topic_name),
                )
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

    fn compute_attach_locked(
        &self,
        state: &TopicChannelState,
        options: SubscribeOptions,
    ) -> Result<(u64, ActiveSubscriberGuard), C2Error> {
        if state.closed.is_some() {
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
                C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    format!("topic {:?} sequence number space exhausted", self.topic_name),
                )
            })?,
        };
        Ok((start_seq, self.barrier.enter()))
    }

    pub(crate) fn subscribe_local(
        self: &Arc<Self>,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, C2Error> {
        let permit = self
            .budget
            .reserve_subscription_operation()
            .map_err(|error| subscription_slot_error(&self.topic_name, error))?;

        let (start_seq, guard) = {
            let state = self.state.lock();
            match self.compute_attach_locked(&state, options) {
                Ok(attached) => attached,
                Err(error) => {
                    drop(state);
                    drop(permit);
                    return Err(error);
                }
            }
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

    pub(crate) fn attach_remote(
        self: &Arc<Self>,
        options: SubscribeOptions,
    ) -> Result<RemoteTopicAttach, C2Error> {
        let state = self.state.lock();
        let (start_seq, guard) = self.compute_attach_locked(&state, options)?;
        Ok(RemoteTopicAttach {
            channel: Arc::clone(self),
            start_seq,
            options,
            _guard: guard,
        })
    }

    /// Select the next frame for a remote subscriber under the ring lock, or
    /// arm `chan_notified` before releasing the lock so no publish/close wakes
    /// can be missed.
    pub(crate) fn select_next_for_remote(
        &self,
        next_expected: &mut u64,
        on_lag: LagPolicy,
        mut chan_notified: Pin<&mut tokio::sync::futures::Notified<'_>>,
    ) -> RemoteSelectOutcome {
        let state = self.state.lock();
        if let Some(cause) = state.closed {
            return RemoteSelectOutcome::Terminal(cause.into_error());
        }
        if state.head_seq < *next_expected {
            chan_notified.as_mut().enable();
            return RemoteSelectOutcome::Wait;
        }
        let Some(first) = state.ring.front() else {
            chan_notified.as_mut().enable();
            return RemoteSelectOutcome::Wait;
        };
        let first_seq = first.seq();
        if *next_expected >= first_seq {
            let offset =
                usize::try_from(*next_expected - first_seq).expect("ring offset fits usize");
            let frame = Arc::clone(
                state
                    .ring
                    .get(offset)
                    .expect("contiguous sequence numbers inside history ring"),
            );
            *next_expected = frame.seq().saturating_add(1);
            RemoteSelectOutcome::Frame(frame)
        } else {
            let last = state.ring.back().expect("non-empty ring has back");
            let last_seq = last.seq();
            match on_lag {
                LagPolicy::Skip => {
                    let frame = Arc::clone(last);
                    *next_expected = last_seq.saturating_add(1);
                    RemoteSelectOutcome::Frame(frame)
                }
                LagPolicy::Error => {
                    let dropped = first_seq - *next_expected;
                    RemoteSelectOutcome::Terminal(subscription_lagged_error(
                        &self.topic_name,
                        *next_expected,
                        first_seq,
                        last_seq,
                        dropped,
                    ))
                }
            }
        }
    }
}

/// Registration-owned collection of topic channels and their shared drain
/// barrier.
pub struct RouteTopics {
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
    pub fn new(
        expected: ExpectedRouteContract,
        topic_configs: &BTreeMap<String, TopicConfig>,
        budget: DomainBudget,
        response_pool: Option<Arc<RwLock<MemPool>>>,
        max_payload_size: u64,
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
                    response_pool.clone(),
                    max_payload_size,
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

    pub fn expected(&self) -> &ExpectedRouteContract {
        &self.expected
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn active_subscribers(&self) -> usize {
        self.barrier.active_count()
    }

    pub fn publisher(&self, topic_name: &str) -> Result<TopicPublisher, C2Error> {
        if self.is_closed() {
            return Err(resource_closed_error(topic_name));
        }
        let channel = self
            .channels
            .get(topic_name)
            .ok_or_else(|| unknown_topic_error(&self.route_name, topic_name))?;
        Ok(TopicPublisher {
            channel: Arc::clone(channel),
        })
    }

    pub fn subscribe_local(
        &self,
        topic_name: &str,
        options: SubscribeOptions,
    ) -> Result<LocalTopicSubscription, C2Error> {
        if self.is_closed() {
            return Err(resource_closed_error(topic_name));
        }
        let channel = self
            .channels
            .get(topic_name)
            .ok_or_else(|| unknown_topic_error(&self.route_name, topic_name))?;
        channel.subscribe_local(options)
    }

    pub(crate) fn attach_remote(
        &self,
        topic_name: &str,
        options: SubscribeOptions,
    ) -> Result<RemoteTopicAttach, C2Error> {
        if self.is_closed() {
            return Err(resource_closed_error(topic_name));
        }
        let channel = self
            .channels
            .get(topic_name)
            .ok_or_else(|| unknown_topic_error(&self.route_name, topic_name))?;
        channel.attach_remote(options)
    }

    pub fn close(&self, cause: TopicCloseCause) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        for channel in self.channels.values() {
            channel.close(cause);
        }
    }

    pub fn wait_drained(&self, timeout: Duration) -> bool {
        self.barrier.wait_drained(timeout)
    }

    pub async fn wait_drained_async(&self) {
        self.barrier.wait_drained_async().await;
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
    pub fn publish(&self, payload: Vec<u8>) -> Result<u64, C2Error> {
        self.channel.publish(payload)
    }

    /// Publish a borrowed byte slice directly into this topic's SHM or budgeted
    /// backing without first copying into an intermediate `Vec<u8>`.
    pub fn publish_bytes(&self, payload: &[u8]) -> Result<u64, C2Error> {
        self.channel.publish_bytes(payload)
    }

    /// Allocate one SHM or budgeted backing of exact length `len`, invoke
    /// `write_into` directly into it outside pool locks, and publish atomically.
    pub fn publish_with<F>(&self, len: usize, write_into: F) -> Result<u64, C2Error>
    where
        F: FnOnce(&mut [u8]) -> Result<(), C2Error>,
    {
        self.channel.publish_with(len, write_into)
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

    /// Async delivery wait yielding either the next immutable frame or the
    /// terminal error (`CallCancelled` 718 or `SubscriptionLagged` 720).
    pub async fn next_delivery_async(&self) -> Result<TopicDelivery, C2Error> {
        loop {
            let chan_notified = self.channel.notify.notified();
            let cancel_notified = self.cancel_notify.notified();
            tokio::pin!(chan_notified);
            tokio::pin!(cancel_notified);

            let step = {
                let mut cursor = self.cursor.lock();
                if let Some(terminal) = &cursor.terminal {
                    return Err(terminal.error.clone());
                }
                if self.cancelled.load(Ordering::Acquire) {
                    let terminal = cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
                    return Err(terminal.error);
                }

                // Register cancel waiter before checking/releasing locks so a
                // concurrent `cancel()` can never be missed.
                cancel_notified.as_mut().enable();
                if self.cancelled.load(Ordering::Acquire) {
                    let terminal = cursor.settle(TopicCloseCause::ConsumerCancelled.into_error());
                    return Err(terminal.error);
                }

                let state = self.channel.state.lock();
                let next_expected = cursor.next_expected;
                if let Some(cause) = state.closed {
                    let terminal = cursor.settle(cause.into_error());
                    SelectOutcome::Terminal(terminal.error)
                } else if state.head_seq < next_expected {
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
                    chan_notified.as_mut().enable();
                    SelectOutcome::Wait
                }
            };

            match step {
                SelectOutcome::Delivery(delivery) => return Ok(delivery),
                SelectOutcome::Terminal(err) => return Err(err),
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
            terminal: Some(Err(terminal.error)),
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

pub(crate) fn unknown_topic_error(route_name: &str, topic_name: &str) -> C2Error {
    C2Error::new(
        ErrorCode::ProtocolViolation,
        format!("unknown topic {topic_name:?} on route {route_name:?}"),
    )
    .with_details(BTreeMap::from([
        ("reason".into(), "unknown_topic".into()),
        ("route".into(), route_name.into()),
        ("topic".into(), topic_name.into()),
    ]))
}

fn resource_closed_error(topic_name: &str) -> C2Error {
    C2Error::new(
        ErrorCode::ResourceClosed,
        format!("topic {topic_name:?} registration is closed"),
    )
}

fn payload_too_large_error(topic_name: &str, actual: usize, max_payload_size: u64) -> C2Error {
    C2Error::new(
        ErrorCode::ResourceOutputSerializing,
        format!(
            "topic {topic_name:?} payload size {actual} exceeds max_payload_size {max_payload_size}"
        ),
    )
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

fn topic_memory_capacity_error(topic_name: &str, error: PayloadBufferError) -> C2Error {
    let mut details = BTreeMap::from([
        ("stage".into(), "topic_publish".into()),
        ("topic".into(), topic_name.into()),
    ]);
    match &error {
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
        PayloadBufferError::DeadlineExceeded => {}
    }
    C2Error::new(
        ErrorCode::CallCapacityExceeded,
        format!("topic {topic_name:?} publish rejected by server memory budget: {error}"),
    )
    .with_details(details)
}

fn subscription_slot_error(topic_name: &str, error: RetentionError) -> C2Error {
    let reason = match error.reason {
        RetentionRejectReason::Closed => "closed",
        RetentionRejectReason::LimitReached => "limit_reached",
    };
    C2Error::new(
        ErrorCode::CallCapacityExceeded,
        format!("subscription on topic {topic_name:?} rejected by call slot budget: {error}"),
    )
    .with_details(BTreeMap::from([
        ("stage".into(), "call_execution_admission".into()),
        ("reason".into(), reason.into()),
        ("topic".into(), topic_name.into()),
        ("used".into(), error.used_operations.to_string()),
        ("max".into(), error.max_operations.to_string()),
    ]))
}
