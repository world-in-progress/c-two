//! Registration-scoped shared upstream subscriptions for relay topics.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use axum::{
    body::{Body, Bytes},
    http::{StatusCode, header},
    response::Response,
};
use c2_error::{C2Error, ErrorCode};
use c2_ipc::{IpcForwardTopic, IpcTopicSynchronizer, RouteBinding};
use c2_mem::{DomainBudget, OperationPermit};
use c2_wire::rpc::{CancelCause, WireLagPolicy, WireSubscribeOptions, WireTopicStart};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::task::task_tracker::TaskTrackerToken;

use super::conn_pool::UpstreamLease;
use super::connection::ConnectionControl;
use super::forwarding::StreamCancellation;
use super::state::RelayState;
use super::topic_remote::{
    HttpTopicUpstream, HubUpstreamRecord, RemoteConfirm, isolates_hub, open_remote_upstream,
};
use super::types::{RouteEntry, local_route_matches};
use crate::topic_wire::{self, RecordKind, Status, Terminal};

/// One hub serves exactly one registration of one topic. A local scope names
/// the source registration this relay itself hosts; a remote scope names the
/// origin relay's registration as this relay's route table observed it.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
enum HubScope {
    Local {
        instance: String,
        route_uid: String,
    },
    Remote {
        relay_id: String,
        route: String,
        registered_at: u64,
    },
}

impl HubScope {
    fn local(route: &RouteEntry) -> Result<Self, C2Error> {
        Ok(Self::Local {
            instance: route.server_instance_id.clone().ok_or_else(|| {
                C2Error::new(
                    ErrorCode::ResourceUnavailable,
                    "local route has no server identity",
                )
            })?,
            route_uid: route.route_uid.clone().ok_or_else(|| {
                C2Error::new(
                    ErrorCode::ResourceUnavailable,
                    "local route has no registration identity",
                )
            })?,
        })
    }

    fn remote(entry: &RouteEntry) -> Self {
        Self::Remote {
            relay_id: entry.relay_id.clone(),
            route: entry.name.clone(),
            registered_at: entry.registered_at.to_bits(),
        }
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct HubKey {
    scope: HubScope,
    topic: String,
}

#[derive(Default)]
pub(crate) struct TopicHubTable {
    hubs: Mutex<HashMap<HubKey, Weak<Hub>>>,
    budget_changed: Arc<Notify>,
    file_segments: Arc<AtomicUsize>,
    latest_budgets: Arc<LatestBudgets>,
}

#[derive(Default)]
struct LatestBudgets {
    totals: Mutex<(u64, u64)>,
}
impl TopicHubTable {
    pub(crate) fn cancel_sources_for_shutdown(&self) {
        let hubs: Vec<_> = self
            .hubs
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for hub in hubs {
            let (cancel, wait) = {
                let state = hub.inner.lock();
                (state.source_cancel.clone(), state.upstream_wait.clone())
            };
            // Hubs whose upstream is still opening have no source handle yet;
            // the wait token is what unblocks their driver.
            wait.cancel();
            if let Some(cancel) = cancel {
                cancel.cancel(CancelCause::Lifecycle);
            }
        }
    }

    /// Evict one stalled TCP reader that holds old topic memory, searching
    /// the requesting hub first and then every other hub sharing this relay's
    /// budget domain. Without the cross-hub pass, a reader that stopped
    /// reading one topic could starve every other topic's frame reception
    /// forever.
    fn evict_stalled_across(
        &self,
        current: &Arc<Hub>,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> bool {
        if current.evict_stalled(shutdown) {
            return true;
        }
        let hubs: Vec<_> = self
            .hubs
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for hub in hubs {
            if Arc::ptr_eq(&hub, current) {
                continue;
            }
            if hub.evict_stalled(shutdown) {
                return true;
            }
        }
        false
    }

    /// The live local hub's source for one registration, if its upstream is
    /// still serving subscribers. Used by the lightweight confirmation.
    pub(crate) fn active_local(
        &self,
        instance: &str,
        route_uid: &str,
        topic: &str,
    ) -> Option<HubSynchronizer> {
        let hub = self
            .hubs
            .lock()
            .get(&HubKey {
                scope: HubScope::Local {
                    instance: instance.to_string(),
                    route_uid: route_uid.to_string(),
                },
                topic: topic.to_string(),
            })?
            .upgrade()?;
        let state = hub.inner.lock();
        (state.terminal.is_none() && !state.closing)
            .then(|| state.source.clone())
            .flatten()
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn blocked_subscribers(&self) -> usize {
        self.hubs
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .map(|hub| {
                hub.inner
                    .lock()
                    .subscribers
                    .values()
                    .filter_map(Weak::upgrade)
                    .filter(|subscriber| {
                        subscriber
                            .connection
                            .as_ref()
                            .is_some_and(|conn| conn.is_blocked())
                    })
                    .count()
            })
            .sum()
    }

    async fn join(&self, key: HubKey, subscriber: Arc<Subscriber>) -> (Arc<Hub>, bool, JoinGuard) {
        loop {
            let waiting = {
                let mut hubs = self.hubs.lock();
                let waiting = if let Some(hub) = hubs.get(&key).and_then(Weak::upgrade) {
                    let mut state = hub.inner.lock();
                    if !state.closing && state.terminal.is_none() {
                        state.joiners += 1;
                        drop(state);
                        return (
                            hub.clone(),
                            false,
                            JoinGuard {
                                hub: Some(hub),
                                subscriber,
                            },
                        );
                    }
                    if state.terminal.is_none() {
                        let notified = hub.changed.clone().notified_owned();
                        Some(Box::pin(notified))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(mut notified) = waiting {
                    notified.as_mut().enable();
                    Some(notified)
                } else {
                    let hub = Arc::new(Hub {
                        key: key.clone(),
                        inner: Mutex::new(HubState {
                            joiners: 1,
                            ..Default::default()
                        }),
                        changed: Arc::new(Notify::new()),
                        budget_changed: self.budget_changed.clone(),
                        file_segments: self.file_segments.clone(),
                        latest_budgets: self.latest_budgets.clone(),
                    });
                    hubs.insert(key, Arc::downgrade(&hub));
                    return (
                        hub.clone(),
                        true,
                        JoinGuard {
                            hub: Some(hub),
                            subscriber,
                        },
                    );
                }
            };
            if let Some(waiting) = waiting {
                waiting.await;
            }
        }
    }

    fn remove(&self, hub: &Arc<Hub>) {
        let mut hubs = self.hubs.lock();
        if hubs
            .get(&hub.key)
            .is_some_and(|current| Weak::ptr_eq(current, &Arc::downgrade(hub)))
        {
            hubs.remove(&hub.key);
        }
    }
}

struct HubFrame {
    seq: u64,
    total: u64,
    chunks: Vec<Bytes>,
    memory_bytes: u64,
    file_bytes: u64,
    _metadata: Arc<c2_mem::MemoryReservation>,
}

struct LatestFrame {
    frame: Arc<HubFrame>,
    budgets: Arc<LatestBudgets>,
}
impl LatestFrame {
    fn new(frame: HubFrame, budgets: Arc<LatestBudgets>) -> Self {
        {
            let mut totals = budgets.totals.lock();
            totals.0 += frame.memory_bytes;
            totals.1 += frame.file_bytes;
        }
        Self {
            frame: Arc::new(frame),
            budgets,
        }
    }
}
impl std::ops::Deref for LatestFrame {
    type Target = HubFrame;
    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}
impl Drop for LatestFrame {
    fn drop(&mut self) {
        let mut totals = self.budgets.totals.lock();
        totals.0 -= self.frame.memory_bytes;
        totals.1 -= self.frame.file_bytes;
    }
}
#[derive(Default)]
struct HubState {
    joiners: usize,
    next_subscriber: u64,
    source: Option<HubSynchronizer>,
    source_cancel: Option<HubCancel>,
    /// Cancelled as soon as this hub stops needing an upstream that may still
    /// be opening: relay shutdown, the last downstream leaving, or a terminal
    /// outcome. The driver's opening waits select on it so a stalled source
    /// cannot hold the shutdown or the slot hostage.
    upstream_wait: tokio_util::sync::CancellationToken,
    history: VecDeque<LatestFrame>,
    latest: Option<LatestFrame>,
    latest_eligible: HashSet<u64>,
    incoming: Option<(u64, HashSet<u64>)>,
    subscribers: HashMap<u64, Weak<Subscriber>>,
    retired: Vec<Weak<Subscriber>>,
    terminal: Option<C2Error>,
    closing: bool,
    /// Set once `run_source` finished the real upstream cleanup. Distinct
    /// from `terminal`: a confirm refusal may make the hub terminal for
    /// downstream readers long before the old upstream has drained, and the
    /// last admission slot must stay held until that drain completes.
    upstream_done: bool,
    last_hold: Option<Arc<Subscriber>>,
}

/// The head-sequence authority of one hub: the IPC subscription's SYNC
/// acknowledgement when this relay is the source, or the merged cross-relay
/// confirmation when the source registration lives on an origin relay.
#[derive(Clone)]
pub(crate) enum HubSynchronizer {
    Ipc(IpcTopicSynchronizer),
    Remote(Arc<RemoteConfirm>),
}

impl HubSynchronizer {
    pub(crate) async fn head_seq(&self) -> Result<u64, C2Error> {
        match self {
            Self::Ipc(synchronizer) => synchronizer.sync_head_seq().await.map_err(upstream_error),
            Self::Remote(confirm) => confirm.head_seq().await,
        }
    }
}

/// Cancellation of one hub's upstream, uniform over the IPC and cross-relay
/// HTTP transports. Both exercise the source's cancel channel exactly once.
#[derive(Clone)]
enum HubCancel {
    Ipc(c2_ipc::IpcForwardCancellation),
    Http(Arc<super::topic_remote::HttpUpstreamCancel>),
}

impl HubCancel {
    fn cancel(&self, cause: CancelCause) {
        match self {
            Self::Ipc(cancel) => {
                let _ = cancel.cancel(cause);
            }
            Self::Http(cancel) => cancel.cancel(cause),
        }
    }
}

/// One hub upstream, normalized over the IPC subscription this relay hosts
/// and the single cross-relay HTTP subscription toward the origin relay.
enum TopicSource {
    Ipc(IpcForwardTopic),
    Http(HttpTopicUpstream),
}

impl TopicSource {
    async fn next_record(&mut self) -> Result<HubUpstreamRecord, C2Error> {
        match self {
            Self::Ipc(source) => match source.next_record().await {
                Ok(c2_ipc::ForwardTopicRecord::ItemBegin { seq, total_bytes }) => {
                    Ok(HubUpstreamRecord::ItemBegin { seq, total_bytes })
                }
                Ok(c2_ipc::ForwardTopicRecord::CallEnd(terminal)) => Ok(HubUpstreamRecord::End {
                    error: terminal.error,
                }),
                Err(error) => Err(upstream_error(error)),
            },
            Self::Http(source) => source.next_record().await,
        }
    }

    async fn read_chunk(
        &mut self,
        slice: c2_mem::WindowSlice,
        max_bytes: usize,
    ) -> Result<Option<c2_mem::WindowChunk>, C2Error> {
        match self {
            Self::Ipc(source) => source
                .read_chunk(slice, max_bytes)
                .await
                .map_err(upstream_error),
            Self::Http(source) => source.read_chunk(slice, max_bytes).await.map(Some),
        }
    }

    fn cancellation(&self) -> HubCancel {
        match self {
            Self::Ipc(source) => HubCancel::Ipc(source.cancellation()),
            Self::Http(source) => HubCancel::Http(source.cancel_handle()),
        }
    }

    async fn completion(&mut self) -> Result<Option<C2Error>, C2Error> {
        match self {
            Self::Ipc(source) => {
                source
                    .completion()
                    .await
                    .map_err(upstream_error)
                    .map(|terminal| {
                        Some(
                            terminal
                                .error
                                .unwrap_or_else(|| {
                                    C2Error::new(
                                        ErrorCode::ProtocolViolation,
                                        "subscription ended without an error",
                                    )
                                })
                                .clone(),
                        )
                    })
            }
            Self::Http(source) => source.completion().await,
        }
    }
}
impl HubState {
    fn prune_history(&mut self) {
        // A joining subscriber may already have a confirmed source sequence
        // while its SYNC acknowledgement is still travelling back to it.
        if self.joiners != 0 {
            return;
        }
        let first_needed = self
            .subscribers
            .values()
            .filter_map(Weak::upgrade)
            .filter(|subscriber| {
                !subscriber.detached.load(Ordering::Acquire)
                    && !subscriber.sequence_exhausted.load(Ordering::Acquire)
            })
            .map(|subscriber| subscriber.next_seq.load(Ordering::Acquire))
            .min();
        while self
            .history
            .front()
            .is_some_and(|frame| first_needed.is_none_or(|seq| frame.seq < seq))
        {
            self.history.pop_front();
        }
    }
}
struct Hub {
    key: HubKey,
    inner: Mutex<HubState>,
    changed: Arc<Notify>,
    budget_changed: Arc<Notify>,
    file_segments: Arc<AtomicUsize>,
    latest_budgets: Arc<LatestBudgets>,
}
struct JoinGuard {
    hub: Option<Arc<Hub>>,
    subscriber: Arc<Subscriber>,
}
impl Drop for JoinGuard {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.take() {
            let mut state = hub.inner.lock();
            state.joiners -= 1;
            state.prune_history();
            if state.joiners == 0 && state.subscribers.is_empty() && !state.upstream_done {
                // Even a hub already made terminal by a confirm refusal holds
                // its last slot until the upstream drain completes.
                state.closing = true;
                state.last_hold = Some(self.subscriber.clone());
                state.upstream_wait.cancel();
            }
            drop(state);
            hub.changed.notify_waiters();
        }
    }
}
struct Subscriber {
    _permit: OperationPermit,
    _tracked: TaskTrackerToken,
    connection: Option<Arc<ConnectionControl>>,
    detached: AtomicBool,
    selected: AtomicBool,
    frame_started: AtomicBool,
    evicted: AtomicBool,
    unsent: Mutex<VecDeque<Bytes>>,
    next_seq: AtomicU64,
    sequence_exhausted: AtomicBool,
    inflight_chunks: AtomicUsize,
    changed: Notify,
}
impl Hub {
    fn fail(&self, error: C2Error) {
        let mut state = self.inner.lock();
        if state.terminal.is_none() {
            state.terminal = Some(error);
        }
        state.closing = true;
        state.history.clear();
        if state.latest_eligible.is_empty() {
            state.latest.take();
        }
        state.upstream_wait.cancel();
        drop(state);
        self.changed.notify_waiters();
    }
    /// Complete the upstream cleanup: release the last held admission slot
    /// and mark the upstream drained so no later leave can re-hold it.
    fn finish_upstream(&self) {
        let mut state = self.inner.lock();
        state.last_hold.take();
        state.upstream_done = true;
        drop(state);
        self.changed.notify_waiters();
    }
    fn detach(&self, id: u64, subscriber: &Arc<Subscriber>) {
        if subscriber.detached.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.inner.lock();
        state.subscribers.remove(&id);
        state.retired.retain(|old| old.strong_count() != 0);
        state.retired.push(Arc::downgrade(subscriber));
        state.latest_eligible.remove(&id);
        if let Some((_, eligible)) = &mut state.incoming {
            eligible.remove(&id);
        }
        state.prune_history();
        if state.terminal.is_some() && state.latest_eligible.is_empty() {
            state.latest.take();
        }
        if state.subscribers.is_empty() && state.joiners == 0 && !state.upstream_done {
            // Hold the last subscriber (and its admission slot) until the
            // upstream drain finishes, even when a confirm refusal already
            // made the hub terminal for downstream readers.
            state.closing = true;
            state.last_hold = Some(subscriber.clone());
            state.history.clear();
            state.latest.take();
            state.upstream_wait.cancel();
        }
        drop(state);
        self.changed.notify_waiters();
    }
    async fn source(&self) -> Result<HubSynchronizer, C2Error> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.inner.lock();
                if let Some(error) = &state.terminal {
                    return Err(error.clone());
                }
                if let Some(source) = &state.source {
                    return Ok(source.clone());
                }
            }
            notified.await;
        }
    }
    fn set_source(
        &self,
        source: HubSynchronizer,
        cancel: HubCancel,
        shutdown: &tokio_util::sync::CancellationToken,
    ) {
        let mut state = self.inner.lock();
        state.source = Some(source);
        state.source_cancel = Some(cancel.clone());
        drop(state);
        if shutdown.is_cancelled() {
            cancel.cancel(CancelCause::Lifecycle);
        }
        self.changed.notify_waiters();
    }
    fn begin_frame(&self, seq: u64) {
        let mut state = self.inner.lock();
        let eligible = state
            .subscribers
            .iter()
            .filter_map(|(&id, weak)| {
                let sub = weak.upgrade()?;
                (!sub.detached.load(Ordering::Acquire)
                    && !sub.selected.load(Ordering::Acquire)
                    && !sub.sequence_exhausted.load(Ordering::Acquire)
                    && sub.next_seq.load(Ordering::Acquire) <= seq)
                    .then_some(id)
            })
            .collect();
        state.incoming = Some((seq, eligible));
    }
    fn stopped(&self) -> bool {
        self.inner.lock().closing
    }
    fn evict_history(&self) -> bool {
        let old = {
            let mut state = self.inner.lock();
            state.history.pop_front().or_else(|| {
                state.latest_eligible.clear();
                state.latest.take()
            })
        };
        let had_old = old.is_some();
        if had_old {
            drop(old);
            self.changed.notify_waiters();
        }
        had_old
    }
    fn evict_stalled(&self, shutdown: &tokio_util::sync::CancellationToken) -> bool {
        let state = self.inner.lock();
        let blocked = state.subscribers.iter().find_map(|(&id, weak)| {
            let sub = weak.upgrade()?;
            let blocked = sub
                .connection
                .as_ref()
                .is_some_and(|conn| conn.is_blocked());
            (blocked
                && !sub.evicted.load(Ordering::Acquire)
                && (sub.frame_started.load(Ordering::Acquire)
                    || sub.inflight_chunks.load(Ordering::Acquire) != 0))
                .then_some((id, sub))
        });
        let retired = state.retired.iter().filter_map(Weak::upgrade).find(|sub| {
            !sub.evicted.load(Ordering::Acquire)
                && (sub.frame_started.load(Ordering::Acquire)
                    || sub.inflight_chunks.load(Ordering::Acquire) != 0)
                && sub
                    .connection
                    .as_ref()
                    .is_some_and(|conn| conn.is_blocked())
        });
        drop(state);
        if let Some((id, sub)) = blocked {
            let Some(connection) = &sub.connection else {
                return false;
            };
            if shutdown.is_cancelled()
                || !connection.is_blocked()
                || (!sub.frame_started.load(Ordering::Acquire)
                    && sub.inflight_chunks.load(Ordering::Acquire) == 0)
                || sub.evicted.swap(true, Ordering::AcqRel)
            {
                return false;
            }
            connection.abort();
            sub.unsent.lock().clear();
            self.detach(id, &sub);
            sub.changed.notify_waiters();
            self.changed.notify_waiters();
            true
        } else if let Some(sub) = retired {
            let Some(connection) = &sub.connection else {
                return false;
            };
            if shutdown.is_cancelled()
                || !connection.is_blocked()
                || (!sub.frame_started.load(Ordering::Acquire)
                    && sub.inflight_chunks.load(Ordering::Acquire) == 0)
                || sub.evicted.swap(true, Ordering::AcqRel)
            {
                return false;
            }
            connection.abort();
            sub.unsent.lock().clear();
            sub.changed.notify_waiters();
            self.changed.notify_waiters();
            true
        } else {
            false
        }
    }
}

fn cancellation(cause: &str) -> C2Error {
    C2Error::new(
        ErrorCode::CallCancelled,
        format!("subscription cancelled: {cause}"),
    )
    .with_details(BTreeMap::from([("cause".into(), cause.into())]))
}
fn capacity(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::CallCapacityExceeded, message)
}
fn upstream_error(error: c2_ipc::IpcCallError) -> C2Error {
    match error.into_source() {
        c2_ipc::IpcError::CallRejected(error) | c2_ipc::IpcError::LocalCallRejected(error) => error,
        c2_ipc::IpcError::CrmError(bytes) => C2Error::from_wire_bytes(&bytes)
            .ok()
            .flatten()
            .unwrap_or_else(|| C2Error::new(ErrorCode::ProtocolViolation, "invalid upstream C2E1")),
        error => C2Error::new(ErrorCode::ResourceUnavailable, error.to_string()),
    }
}

/// Attach after caller has resolved the route and acquired its source endpoint.
///
/// `origin_only` marks the single-hop relay-to-relay upstream: only that
/// requester learns the registration fingerprint to confirm against later.
/// Ordinary clients — on the source relay or on an anchor — never see it.
pub(crate) async fn subscribe(
    state: Arc<RelayState>,
    route: RouteEntry,
    topic: String,
    options: WireSubscribeOptions,
    lease: UpstreamLease,
    binding: RouteBinding,
    permit: OperationPermit,
    tracked: TaskTrackerToken,
    connection: Option<Arc<ConnectionControl>>,
    origin_only: bool,
) -> Result<Response, C2Error> {
    let key = HubKey {
        scope: HubScope::local(&route)?,
        topic: topic.clone(),
    };
    let registration_header = origin_only.then(|| {
        super::topic_remote::registration_fingerprint(
            route.server_instance_id.as_deref().unwrap_or_default(),
            route.route_uid.as_deref().unwrap_or_default(),
        )
    });
    let driver_state = state.clone();
    let driver_route = route;
    let driver_topic = topic.clone();
    attach(
        state,
        key,
        options,
        permit,
        tracked,
        connection,
        registration_header,
        move |hub| {
            drive(
                driver_state,
                hub,
                driver_route,
                driver_topic,
                lease,
                binding,
            )
        },
    )
    .await
}

/// Attach for a route this relay only knows through its mesh view: join the
/// remote hub for the origin relay's registration and let its single
/// cross-relay upstream feed every downstream subscriber.
pub(crate) async fn subscribe_remote(
    state: Arc<RelayState>,
    entry: RouteEntry,
    expected: c2_contract::ExpectedRouteContract,
    topic: String,
    options: WireSubscribeOptions,
    permit: OperationPermit,
    tracked: TaskTrackerToken,
    connection: Option<Arc<ConnectionControl>>,
) -> Result<Response, C2Error> {
    let key = HubKey {
        scope: HubScope::remote(&entry),
        topic: topic.clone(),
    };
    let driver_state = state.clone();
    let driver_entry = entry;
    let driver_topic = topic.clone();
    attach(
        state,
        key,
        options,
        permit,
        tracked,
        connection,
        None,
        move |hub| drive_remote(driver_state, hub, driver_entry, expected, driver_topic),
    )
    .await
}

/// The subscriber half shared by the local and remote hubs: join (or found)
/// the hub, confirm the source head for this subscriber's start point, then
/// stream READY, items and one terminal record to one downstream reader.
async fn attach<F, Fut>(
    state: Arc<RelayState>,
    key: HubKey,
    options: WireSubscribeOptions,
    permit: OperationPermit,
    tracked: TaskTrackerToken,
    connection: Option<Arc<ConnectionControl>>,
    registration_header: Option<String>,
    launch: F,
) -> Result<Response, C2Error>
where
    F: FnOnce(Arc<Hub>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let subscriber = Arc::new(Subscriber {
        _permit: permit,
        _tracked: tracked,
        connection,
        detached: AtomicBool::new(false),
        selected: AtomicBool::new(false),
        frame_started: AtomicBool::new(false),
        evicted: AtomicBool::new(false),
        unsent: Mutex::new(VecDeque::new()),
        next_seq: AtomicU64::new(0),
        sequence_exhausted: AtomicBool::new(false),
        inflight_chunks: AtomicUsize::new(0),
        changed: Notify::new(),
    });
    let (hub, leader, guard) = state.topics.join(key, subscriber.clone()).await;
    if leader {
        state.forwarding.spawn_driver(launch(hub.clone()));
    }
    let source = hub.source().await?;
    let head = match source.head_seq().await {
        Ok(head) => head,
        Err(error) => {
            // A refusal that proves the registration no longer serves the
            // route isolates the shared hub now; the upstream cleanup that
            // follows only releases its slot.
            if isolates_hub(&error) {
                hub.fail(error.clone());
            }
            return Err(error);
        }
    };
    let start_seq = match options.start {
        WireTopicStart::Latest => head.max(1),
        WireTopicStart::Next => head
            .checked_add(1)
            .ok_or_else(|| capacity("topic sequence space exhausted"))?,
    };
    let (token, control) = state
        .forwarding
        .register_stream()
        .map_err(|e| C2Error::new(ErrorCode::ResourceUnavailable, e.to_string()))?;
    let id = {
        let mut inner = hub.inner.lock();
        if let Some(error) = &inner.terminal {
            state.forwarding.remove_stream(&token);
            return Err(error.clone());
        }
        if inner.closing {
            state.forwarding.remove_stream(&token);
            return Err(cancellation("relay_shutdown"));
        }
        let Some(next_id) = inner.next_subscriber.checked_add(1) else {
            state.forwarding.remove_stream(&token);
            return Err(capacity("subscriber identifiers exhausted"));
        };
        inner.next_subscriber = next_id;
        let id = inner.next_subscriber;
        subscriber.next_seq.store(start_seq, Ordering::Release);
        inner.subscribers.insert(id, Arc::downgrade(&subscriber));
        if let Some((seq, eligible)) = &mut inner.incoming {
            if start_seq <= *seq {
                eligible.insert(id);
            }
        }
        id
    };
    drop(guard);
    let monitor_hub = hub.clone();
    let monitor_sub = subscriber.clone();
    let monitor_control = control.clone();
    let monitor_state = state.clone();
    let monitor_token = token.clone();
    let done = tokio_util::sync::CancellationToken::new();
    let monitor_done = done.clone();
    state.forwarding.spawn_driver(async move {
        let mut body_done = false;
        let mut cancellation_seen = false;
        let mut consumer_seen = false;
        loop {
            if body_done && monitor_sub.inflight_chunks.load(Ordering::Acquire) == 0 { break; }
            if let Some(connection) = &monitor_sub.connection {
                tokio::select! {
                    cause = monitor_control.requested(), if !cancellation_seen => {
                        cancellation_seen = true;
                        consumer_seen = cause == CancelCause::Consumer;
                        if consumer_seen { monitor_hub.detach(id, &monitor_sub); }
                        else { monitor_hub.changed.notify_waiters(); }
                    },
                    _ = monitor_control.wait_consumer_requested(), if cancellation_seen && !consumer_seen => {
                        consumer_seen = true;
                        monitor_hub.detach(id, &monitor_sub);
                    },
                    _ = monitor_done.cancelled(), if !body_done => body_done = true,
                    _ = connection.changed.notified() => monitor_hub.changed.notify_waiters(),
                    _ = monitor_sub.changed.notified() => {},
                }
            } else {
                tokio::select! {
                    cause = monitor_control.requested(), if !cancellation_seen => {
                        cancellation_seen = true;
                        consumer_seen = cause == CancelCause::Consumer;
                        if consumer_seen { monitor_hub.detach(id, &monitor_sub); }
                        else { monitor_hub.changed.notify_waiters(); }
                    },
                    _ = monitor_control.wait_consumer_requested(), if cancellation_seen && !consumer_seen => {
                        consumer_seen = true;
                        monitor_hub.detach(id, &monitor_sub);
                    },
                    _ = monitor_done.cancelled(), if !body_done => body_done = true,
                    _ = monitor_sub.changed.notified() => {},
                }
            }
        }
        monitor_state.forwarding.remove_stream(&monitor_token);
    });
    let body = Body::from_stream(futures::stream::unfold(
        BodyState {
            hub,
            subscriber,
            control,
            done,
            id,
            next_seq: start_seq,
            lag: options.on_lag,
            phase: BodyPhase::Ready,
            pending: None,
            front_offset: 0,
            complete: 0,
            pending_terminal: None,
            budget: state.forwarding.budget().clone(),
            selection_charge: None,
            staging: None,
        },
        |mut body| async move {
            let result = body.next().await;
            result.map(|result| (result, body))
        },
    ));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(topic_wire::MEDIA_TYPE),
    );
    if let Ok(value) = header::HeaderValue::from_str(&token) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static("x-c2-call-token"),
            value,
        );
    }
    if let Some(registration) = registration_header
        && let Ok(value) = header::HeaderValue::from_str(&registration)
    {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(super::topic_remote::REGISTRATION_HEADER),
            value,
        );
    }
    Ok(response)
}

struct BodyState {
    hub: Arc<Hub>,
    subscriber: Arc<Subscriber>,
    control: Arc<StreamCancellation>,
    done: tokio_util::sync::CancellationToken,
    id: u64,
    next_seq: u64,
    lag: WireLagPolicy,
    phase: BodyPhase,
    pending: Option<Bytes>,
    front_offset: usize,
    complete: u64,
    pending_terminal: Option<C2Error>,
    budget: DomainBudget,
    selection_charge: Option<Arc<c2_mem::MemoryReservation>>,
    staging: Option<Arc<c2_mem::ReusableWindow>>,
}
enum BodyPhase {
    Ready,
    Selecting,
    Sending,
    Ended,
}
struct OwnedBytes {
    bytes: Bytes,
    subscriber: Arc<Subscriber>,
    data: bool,
    _metadata: Option<Arc<c2_mem::MemoryReservation>>,
}
impl AsRef<[u8]> for OwnedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for OwnedBytes {
    fn drop(&mut self) {
        if self.data
            && self
                .subscriber
                .inflight_chunks
                .fetch_sub(1, Ordering::AcqRel)
                == 1
        {
            self.subscriber.changed.notify_waiters();
            self.subscriber.changed.notify_one();
        }
    }
}
impl Drop for BodyState {
    fn drop(&mut self) {
        self.done.cancel();
        self.subscriber.selected.store(false, Ordering::Release);
        self.subscriber
            .frame_started
            .store(false, Ordering::Release);
        self.subscriber.unsent.lock().clear();
        self.hub.detach(self.id, &self.subscriber);
    }
}
impl BodyState {
    fn held(&self, bytes: Bytes, data: bool) -> Bytes {
        if data {
            self.subscriber
                .inflight_chunks
                .fetch_add(1, Ordering::AcqRel);
        }
        Bytes::from_owner(OwnedBytes {
            bytes,
            subscriber: self.subscriber.clone(),
            data,
            _metadata: self.selection_charge.clone(),
        })
    }
    async fn next(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if let Some(bytes) = self.pending.take() {
            return Some(Ok(self.held(bytes, true)));
        }
        loop {
            match self.phase {
                BodyPhase::Ready => {
                    self.phase = BodyPhase::Selecting;
                    return Some(Ok(self.held(
                        Bytes::from(topic_wire::ready(self.next_seq).unwrap()),
                        false,
                    )));
                }
                BodyPhase::Sending => {
                    if self.subscriber.evicted.load(Ordering::Acquire) {
                        self.selection_charge.take();
                        self.staging.take();
                        self.subscriber.selected.store(false, Ordering::Release);
                        self.subscriber
                            .frame_started
                            .store(false, Ordering::Release);
                        self.phase = BodyPhase::Ended;
                        return Some(Err(std::io::ErrorKind::ConnectionAborted.into()));
                    }
                    // Retain at most one independently charged transport
                    // fragment per subscriber. If the peer stops reading,
                    // the shared source segment can then be released after
                    // its unstarted fragments have been discarded.
                    {
                        let changed = self.subscriber.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        if self.subscriber.inflight_chunks.load(Ordering::Acquire) != 0 {
                            changed.await;
                            continue;
                        }
                    }
                    let next_chunk = self.subscriber.unsent.lock().front().cloned();
                    if let Some(front) = next_chunk {
                        let window = self
                            .staging
                            .as_ref()
                            .expect("nonempty frame window")
                            .clone();
                        let length = (front.len() - self.front_offset)
                            .min(usize::try_from(window.capacity()).unwrap_or(usize::MAX));
                        let slice = match window.reserve(length as u64).await {
                            Ok(slice) => slice,
                            Err(error) => return self.abort_incomplete(error.to_string()),
                        };
                        let mut bytes = Vec::new();
                        if bytes.try_reserve_exact(length).is_err()
                            || bytes.capacity() as u64 > slice.bytes()
                        {
                            return self.abort_incomplete("topic transport allocation failed");
                        }
                        bytes.extend_from_slice(
                            &front[self.front_offset..self.front_offset + length],
                        );
                        {
                            let mut unsent = self.subscriber.unsent.lock();
                            if self.subscriber.evicted.load(Ordering::Acquire) {
                                drop(unsent);
                                return self.abort_incomplete("topic connection was evicted");
                            }
                            self.front_offset += length;
                            if self.front_offset == front.len() {
                                unsent.pop_front();
                                self.front_offset = 0;
                            }
                        }
                        let bytes = Bytes::from_owner(slice.into_buffer(bytes));
                        let length = bytes.len() as u64;
                        self.pending = Some(bytes);
                        return Some(Ok(self.held(
                            Bytes::copy_from_slice(&topic_wire::header(
                                RecordKind::ItemData,
                                length,
                            )),
                            false,
                        )));
                    }
                    if self.subscriber.evicted.load(Ordering::Acquire) {
                        continue;
                    }
                    self.complete += 1;
                    self.selection_charge.take();
                    self.staging.take();
                    self.subscriber.selected.store(false, Ordering::Release);
                    self.subscriber
                        .frame_started
                        .store(false, Ordering::Release);
                    self.phase = BodyPhase::Selecting;
                }
                BodyPhase::Ended => return None,
                BodyPhase::Selecting => {
                    if self.control.is_consumer_requested() {
                        self.hub.detach(self.id, &self.subscriber);
                    }
                    let notified = self.hub.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    enum Selection {
                        Frame(Arc<HubFrame>),
                        End(C2Error),
                        Wait,
                    }
                    let selection = {
                        let mut state = self.hub.inner.lock();
                        if let Some(error) = &self.pending_terminal {
                            let last_is_pending = state
                                .last_hold
                                .as_ref()
                                .is_some_and(|last| Arc::ptr_eq(last, &self.subscriber));
                            if last_is_pending && state.terminal.is_none() {
                                Selection::Wait
                            } else {
                                Selection::End(error.clone())
                            }
                        } else if self.control.is_consumer_requested() {
                            let last_is_pending = state
                                .last_hold
                                .as_ref()
                                .is_some_and(|last| Arc::ptr_eq(last, &self.subscriber));
                            if last_is_pending && state.terminal.is_none() {
                                Selection::Wait
                            } else {
                                Selection::End(cancellation("consumer_cancelled"))
                            }
                        } else if self.control.cause() == Some(CancelCause::Lifecycle)
                            && !state.latest_eligible.contains(&self.id)
                        {
                            if state.terminal.is_none() {
                                Selection::Wait
                            } else {
                                Selection::End(cancellation("relay_shutdown"))
                            }
                        } else if state.terminal.is_some()
                            && !state.latest_eligible.contains(&self.id)
                        {
                            Selection::End(state.terminal.as_ref().unwrap().clone())
                        } else if self.subscriber.sequence_exhausted.load(Ordering::Acquire) {
                            Selection::Wait
                        } else if let Some(latest) = &state.latest {
                            // Keep the next expected frame while it remains in
                            // the relay's budgeted history. Once it has been
                            // evicted, Skip resumes at the newest frame.
                            let frame = if state.terminal.is_none()
                                && self.control.cause() != Some(CancelCause::Lifecycle)
                            {
                                state
                                    .history
                                    .iter()
                                    .find(|frame| frame.seq == self.next_seq)
                                    .unwrap_or(latest)
                            } else {
                                latest
                            };
                            if frame.seq < self.next_seq {
                                Selection::Wait
                            } else if frame.seq > self.next_seq && self.lag == WireLagPolicy::Error
                            {
                                let first_available = state
                                    .history
                                    .iter()
                                    .find(|frame| frame.seq > self.next_seq)
                                    .map_or(latest.seq, |frame| frame.seq);
                                Selection::End(
                                    C2Error::new(
                                        ErrorCode::SubscriptionLagged,
                                        "topic subscription lagged",
                                    )
                                    .with_details(
                                        BTreeMap::from([
                                            ("topic".into(), self.hub.key.topic.clone()),
                                            ("expected_seq".into(), self.next_seq.to_string()),
                                            (
                                                "available_first_seq".into(),
                                                first_available.to_string(),
                                            ),
                                            ("available_last_seq".into(), latest.seq.to_string()),
                                            (
                                                "dropped".into(),
                                                (first_available - self.next_seq).to_string(),
                                            ),
                                        ]),
                                    ),
                                )
                            } else {
                                let frame = Arc::clone(&frame.frame);
                                state.latest_eligible.remove(&self.id);
                                self.subscriber.selected.store(true, Ordering::Release);
                                self.subscriber
                                    .next_seq
                                    .store(frame.seq.saturating_add(1), Ordering::Release);
                                self.subscriber
                                    .sequence_exhausted
                                    .store(frame.seq == u64::MAX, Ordering::Release);
                                state.prune_history();
                                Selection::Frame(frame)
                            }
                        } else {
                            Selection::Wait
                        }
                    };
                    match selection {
                        Selection::Frame(frame) => {
                            let bytes_per_chunk = 2 * std::mem::size_of::<Bytes>()
                                + 2 * (std::mem::size_of::<OwnedBytes>()
                                    + 2 * std::mem::size_of::<usize>());
                            let Some(metadata_bytes) = frame
                                .chunks
                                .len()
                                .checked_mul(bytes_per_chunk)
                                .and_then(|n| u64::try_from(n).ok())
                            else {
                                self.subscriber.selected.store(false, Ordering::Release);
                                self.pending_terminal =
                                    Some(capacity("topic subscriber metadata size overflow"));
                                self.hub.detach(self.id, &self.subscriber);
                                continue;
                            };
                            let reservation = match self.budget.try_reserve_memory(metadata_bytes) {
                                Ok(reservation) => reservation,
                                Err(error) => {
                                    self.subscriber.selected.store(false, Ordering::Release);
                                    self.pending_terminal = Some(capacity(error.to_string()));
                                    self.hub.detach(self.id, &self.subscriber);
                                    continue;
                                }
                            };
                            self.selection_charge = Some(Arc::new(reservation));
                            if frame.total != 0 {
                                let memory = self.budget.snapshot().memory;
                                let available = memory.limit_bytes - memory.used_bytes;
                                let max_width = c2_config::REMOTE_PAYLOAD_CHUNK_SIZE
                                    .min(c2_config::CHUNK_SIZE)
                                    .max(1);
                                let fair_share = if available <= 4096 {
                                    available
                                } else {
                                    (available / 2).max(4096)
                                };
                                let width = frame.total.min(max_width).min(fair_share);
                                let reservation = if width == 0 {
                                    None
                                } else {
                                    self.budget.try_reserve_memory(width).ok()
                                };
                                let Some(reservation) = reservation else {
                                    self.selection_charge.take();
                                    self.subscriber.selected.store(false, Ordering::Release);
                                    self.pending_terminal =
                                        Some(capacity("no memory for a topic transport fragment"));
                                    self.hub.detach(self.id, &self.subscriber);
                                    continue;
                                };
                                self.staging = Some(Arc::new(reservation.into_reusable_window()));
                            }
                            self.next_seq = frame.seq.saturating_add(1);
                            {
                                let mut unsent = self.subscriber.unsent.lock();
                                if !self.subscriber.evicted.load(Ordering::Acquire) {
                                    *unsent = frame.chunks.iter().cloned().collect();
                                }
                            }
                            if self.subscriber.evicted.load(Ordering::Acquire) {
                                self.selection_charge.take();
                                self.staging.take();
                                self.subscriber.selected.store(false, Ordering::Release);
                                self.pending_terminal = Some(capacity(
                                    "stalled topic subscriber exceeded relay capacity",
                                ));
                                continue;
                            }
                            self.subscriber.selected.store(true, Ordering::Release);
                            self.phase = BodyPhase::Sending;
                            self.subscriber.frame_started.store(true, Ordering::Release);
                            return Some(Ok(self.held(
                                Bytes::from(
                                    topic_wire::item_begin(frame.seq, frame.total).unwrap(),
                                ),
                                false,
                            )));
                        }
                        Selection::End(error) => {
                            self.hub.detach(self.id, &self.subscriber);
                            let wait_for_source = {
                                let state = self.hub.inner.lock();
                                state.terminal.is_none()
                                    && state
                                        .last_hold
                                        .as_ref()
                                        .is_some_and(|last| Arc::ptr_eq(last, &self.subscriber))
                            };
                            if wait_for_source {
                                self.pending_terminal = Some(error);
                                continue;
                            }
                            self.phase = BodyPhase::Ended;
                            let terminal = Terminal {
                                status: Status::Failed,
                                complete_items: self.complete,
                                error,
                            };
                            return Some(Ok(
                                self.held(Bytes::from(topic_wire::terminal(&terminal)), false)
                            ));
                        }
                        Selection::Wait => {
                            if self.control.cause().is_some() {
                                notified.await;
                            } else {
                                tokio::select! { _ = notified => {}, _ = self.control.requested() => {} }
                            }
                        }
                    }
                }
            }
        }
    }

    fn abort_incomplete(
        &mut self,
        message: impl Into<String>,
    ) -> Option<Result<Bytes, std::io::Error>> {
        if let Some(connection) = &self.subscriber.connection {
            connection.abort();
        }
        self.subscriber.unsent.lock().clear();
        self.selection_charge.take();
        self.staging.take();
        self.subscriber.selected.store(false, Ordering::Release);
        self.subscriber
            .frame_started
            .store(false, Ordering::Release);
        self.hub.detach(self.id, &self.subscriber);
        self.phase = BodyPhase::Ended;
        Some(Err(std::io::Error::other(message.into())))
    }
}

struct Segment {
    storage: Option<SegmentStorage>,
    changed: Arc<Notify>,
    file_segments: Arc<AtomicUsize>,
    file: bool,
    _metadata: Arc<c2_mem::MemoryReservation>,
}
enum SegmentStorage {
    Memory(c2_mem::WindowChunk),
    File {
        mapping: c2_mem::spill::SpillMapping,
        _reservation: c2_mem::FileSpillReservation,
    },
}
impl AsRef<[u8]> for Segment {
    fn as_ref(&self) -> &[u8] {
        match self.storage.as_ref().expect("live topic segment") {
            SegmentStorage::Memory(chunk) => chunk.as_ref(),
            SegmentStorage::File { mapping, .. } => &mapping[..],
        }
    }
}
impl Drop for Segment {
    fn drop(&mut self) {
        drop(self.storage.take());
        if self.file {
            self.file_segments.fetch_sub(1, Ordering::AcqRel);
        }
        self.changed.notify_waiters();
    }
}

async fn wait_stop(hub: &Hub, shutdown: &tokio_util::sync::CancellationToken) -> CancelCause {
    loop {
        let notified = hub.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shutdown.is_cancelled() {
            return CancelCause::Lifecycle;
        }
        if hub.stopped() {
            return CancelCause::Consumer;
        }
        tokio::select! { _ = notified => {}, _ = shutdown.cancelled() => return CancelCause::Lifecycle }
    }
}

async fn wait_consumer_stop(hub: &Hub) -> CancelCause {
    loop {
        let notified = hub.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if hub.stopped() {
            return CancelCause::Consumer;
        }
        notified.await;
    }
}

async fn wait_frame_stop(
    hub: &Hub,
    shutdown: &tokio_util::sync::CancellationToken,
    lifecycle: bool,
) -> CancelCause {
    if lifecycle {
        wait_consumer_stop(hub).await
    } else {
        wait_stop(hub, shutdown).await
    }
}

async fn drive(
    relay: Arc<RelayState>,
    hub: Arc<Hub>,
    route: RouteEntry,
    topic_name: String,
    lease: UpstreamLease,
    binding: RouteBinding,
) {
    let shutdown = relay.forwarding.shutdown_token();
    let upstream_wait = hub.inner.lock().upstream_wait.clone();
    let client = lease.client();
    let options = WireSubscribeOptions {
        start: WireTopicStart::Latest,
        on_lag: WireLagPolicy::Skip,
    };
    // A hub still opening has no source handle to cancel, so its OPEN and
    // attestation waits select on the hub's wait token: shutdown, the last
    // downstream leaving, or a terminal outcome unblocks a stalled source.
    let source = match tokio::select! {
        source = client.forward_subscribe_bound(&binding, &topic_name, options) => source,
        _ = upstream_wait.cancelled() => {
            abandon_opening(&relay, &hub, &shutdown);
            return;
        }
    } {
        Ok(source) => source,
        Err(error) => {
            hub.fail(upstream_error(error));
            hub.finish_upstream();
            relay.topics.remove(&hub);
            return;
        }
    };
    // OPEN follows a name rather than carrying a UID. Confirm after OPEN, then
    // use SYNC on that subscription so a replaced registration cannot be
    // mistaken for the route whose UID keys this hub.
    let attestation_expected = c2_contract::ExpectedRouteContract {
        route_name: route.name.clone(),
        crm_ns: route.crm_ns.clone(),
        crm_name: route.crm_name.clone(),
        crm_ver: route.crm_ver.clone(),
        abi_hash: route.abi_hash.clone(),
        signature_hash: route.signature_hash.clone(),
    };
    let attestation = tokio::select! {
        attestation = client.attest_route_for_registration(&attestation_expected) => attestation,
        _ = upstream_wait.cancelled() => {
            let _ = source.cancellation().cancel(CancelCause::Consumer);
            let _ = source.completion().await;
            abandon_opening(&relay, &hub, &shutdown);
            return;
        }
    };
    let valid = attestation
        .as_ref()
        .is_ok_and(|attested| Some(attested.route_uid.as_str()) == route.route_uid.as_deref())
        && relay
            .local_route(&route.name)
            .is_some_and(|current| local_route_matches(&current, &route));
    if !valid {
        let _ = source.cancellation().cancel(CancelCause::Consumer);
        let _ = source.completion().await;
        hub.fail(C2Error::new(
            ErrorCode::ResourceUnavailable,
            "route registration changed while opening subscription",
        ));
        hub.finish_upstream();
        relay.topics.remove(&hub);
        return;
    }
    hub.set_source(
        HubSynchronizer::Ipc(source.synchronizer()),
        HubCancel::Ipc(source.cancellation()),
        &shutdown,
    );
    run_source(relay, hub, TopicSource::Ipc(source)).await;
    drop(lease);
}

/// Fail a hub whose opening wait was cancelled by the hub's wait token, after
/// any caller-owned source cleanup, and finish its upstream lifecycle.
fn abandon_opening(
    relay: &Arc<RelayState>,
    hub: &Arc<Hub>,
    shutdown: &tokio_util::sync::CancellationToken,
) {
    let cause = if shutdown.is_cancelled() {
        cancellation("relay_shutdown")
    } else {
        cancellation("consumer_cancelled")
    };
    hub.fail(cause);
    hub.finish_upstream();
    relay.topics.remove(hub);
}

/// Open the single cross-relay upstream for a hub whose route this relay knows
/// only through its mesh view. The origin relay attests the registration it
/// actually serves; this relay confirms its table still describes the same
/// peer registration before publishing the source.
async fn drive_remote(
    relay: Arc<RelayState>,
    hub: Arc<Hub>,
    entry: RouteEntry,
    expected: c2_contract::ExpectedRouteContract,
    topic_name: String,
) {
    let shutdown = relay.forwarding.shutdown_token();
    let upstream_wait = hub.inner.lock().upstream_wait.clone();
    let client = crate::relay_client_builder_with_proxy(relay.config().use_proxy)
        .build()
        .expect("c-two: failed to build reqwest Client for relay traffic");
    // A hub still opening has no source handle to cancel, so the cross-relay
    // subscription request selects on the hub's wait token.
    let opened = match tokio::select! {
        opened = open_remote_upstream(client.clone(), &entry.relay_url, &expected, &topic_name) => opened,
        _ = upstream_wait.cancelled() => {
            abandon_opening(&relay, &hub, &shutdown);
            return;
        }
    } {
        Ok(opened) => opened,
        Err(error) => {
            hub.fail(error);
            hub.finish_upstream();
            relay.topics.remove(&hub);
            return;
        }
    };
    let mut source = opened.source;
    if !relay.peer_entry_matches(&entry) {
        source.cancel_handle().cancel(CancelCause::Consumer);
        let _ = source.completion().await;
        hub.fail(C2Error::new(
            ErrorCode::ResourceUnavailable,
            "route registration changed while opening subscription",
        ));
        hub.finish_upstream();
        relay.topics.remove(&hub);
        return;
    }
    let confirm = Arc::new(RemoteConfirm::new(
        client,
        &entry.relay_url,
        &expected,
        &topic_name,
        &opened.registration,
    ));
    hub.set_source(
        HubSynchronizer::Remote(confirm),
        HubCancel::Http(source.cancel_handle()),
        &shutdown,
    );
    run_source(relay, hub, TopicSource::Http(source)).await;
}

/// The shared upstream loop of one hub: copy each frame into the relay's
/// single budgeted copy, publish it to every downstream cursor, and end the
/// hub with the real terminal outcome once the upstream finishes.
async fn run_source(relay: Arc<RelayState>, hub: Arc<Hub>, mut source: TopicSource) {
    let shutdown = relay.forwarding.shutdown_token();
    let mut cancelled = None;
    let mut pending_error = None;
    loop {
        let record = if cancelled.is_some() {
            source.next_record().await
        } else {
            tokio::select! {
                result = source.next_record() => result,
                cause = wait_stop(&hub, &shutdown) => {
                    source.cancellation().cancel(cause);
                    cancelled = Some(cause);
                    if cause == CancelCause::Consumer { break; }
                    continue;
                }
            }
        };
        match record {
            Ok(HubUpstreamRecord::ItemBegin { seq, total_bytes }) => {
                hub.begin_frame(seq);
                #[cfg(feature = "test-support")]
                {
                    let pause = relay.topic_pause.lock().take();
                    if let Some((entered, release)) = pause {
                        let _ = entered.send(());
                        let _ = release.await;
                    }
                }
                match receive_frame(
                    relay.forwarding.budget(),
                    &relay.topics,
                    &hub,
                    &shutdown,
                    &mut source,
                    seq,
                    total_bytes,
                )
                .await
                {
                    Ok(Some(frame)) => {
                        let mut inner = hub.inner.lock();
                        if !inner.closing {
                            if let Some(previous) = inner.latest.take() {
                                inner.history.push_back(previous);
                            }
                            inner.latest =
                                Some(LatestFrame::new(frame, hub.latest_budgets.clone()));
                            inner.latest_eligible = inner
                                .incoming
                                .take()
                                .map_or_else(HashSet::new, |(_, ids)| ids);
                            inner.prune_history();
                        }
                        drop(inner);
                        hub.changed.notify_waiters();
                    }
                    Ok(None) => {
                        source.cancellation().cancel(CancelCause::Consumer);
                        break;
                    }
                    Err(error) => {
                        pending_error = Some(error);
                        source.cancellation().cancel(CancelCause::Consumer);
                        break;
                    }
                }
            }
            Ok(HubUpstreamRecord::End { error }) => {
                pending_error = Some(error.unwrap_or_else(|| {
                    C2Error::new(
                        ErrorCode::ProtocolViolation,
                        "subscription ended without an error",
                    )
                }));
                cancelled = Some(CancelCause::Consumer);
                break;
            }
            Err(error) => {
                pending_error = Some(error);
                break;
            }
        }
    }
    if cancelled.is_none() {
        source.cancellation().cancel(CancelCause::Consumer);
    }
    let terminal = source.completion().await;
    let outcome = if shutdown.is_cancelled() {
        cancellation("relay_shutdown")
    } else if let Some(error) = pending_error {
        error
    } else {
        match terminal {
            Ok(Some(error)) => error,
            Ok(None) => C2Error::new(
                ErrorCode::ProtocolViolation,
                "subscription ended without an error",
            ),
            Err(error) => error,
        }
    };
    hub.fail(outcome);
    hub.finish_upstream();
    relay.topics.remove(&hub);
}

async fn receive_frame(
    budget: &DomainBudget,
    topics: &TopicHubTable,
    hub: &Arc<Hub>,
    shutdown: &tokio_util::sync::CancellationToken,
    source: &mut TopicSource,
    seq: u64,
    total: u64,
) -> Result<Option<HubFrame>, C2Error> {
    let limits = budget.snapshot();
    let max_memory = limits.memory.limit_bytes;
    let max_file = limits.file_spill.limit_bytes;
    if max_memory == 0 {
        return Err(capacity("relay has no memory for a topic copy window"));
    }
    if total > max_memory && total > max_memory.saturating_sub(1).saturating_add(max_file) {
        return Err(capacity(
            "topic frame exceeds relay memory and file spill budgets",
        ));
    }
    let mut lifecycle = false;
    let (memory_data, width, metadata_bytes, file_backed, mut memory, mut file_reservations) = loop {
        let snapshot = budget.snapshot();
        let available_memory = snapshot.memory.limit_bytes - snapshot.memory.used_bytes;
        let available_file = snapshot.file_spill.limit_bytes - snapshot.file_spill.used_bytes;
        if total > available_memory && hub.evict_history() {
            continue;
        }
        // Empty frames still retain a frame and history entry, so their
        // metadata must consume the same bounded relay memory domain.
        let direct_width = total.min(c2_config::CHUNK_SIZE).max(1);
        let direct_metadata = frame_metadata_bytes(total, direct_width)?;
        let plan = if total
            .checked_add(direct_metadata)
            .is_some_and(|charge| charge <= available_memory)
        {
            Some((total, direct_width, direct_metadata, false))
        } else if available_memory > 0 {
            let width = c2_config::CHUNK_SIZE.min(total).min(available_memory / 2);
            if width == 0 {
                None
            } else {
                let metadata = frame_metadata_bytes(total, width)?;
                width.checked_add(metadata).and_then(|overhead| {
                    let direct = available_memory.checked_sub(overhead)?;
                    let file_bytes = total.saturating_sub(direct);
                    (file_bytes <= available_file).then_some((direct, width, metadata, true))
                })
            }
        } else {
            None
        };
        if let Some((memory_data, width, metadata_bytes, file_backed)) = plan {
            let memory_charge = memory_data
                .checked_add(metadata_bytes)
                .and_then(|charge| charge.checked_add(if file_backed { width } else { 0 }))
                .ok_or_else(|| capacity("topic frame memory size overflow"))?;
            if let Ok(memory) = budget.try_reserve_memory(memory_charge) {
                let mut files = VecDeque::new();
                let mut file_remaining = total - memory_data;
                let mut reserved = true;
                while file_remaining != 0 {
                    let n = file_remaining.min(width);
                    match budget.try_reserve_file_spill(n) {
                        Ok(value) => files.push_back(value),
                        Err(_) => {
                            reserved = false;
                            break;
                        }
                    }
                    file_remaining -= n;
                }
                if reserved {
                    break (
                        memory_data,
                        width,
                        metadata_bytes,
                        file_backed,
                        memory,
                        files,
                    );
                }
                drop(files);
                drop(memory);
            }
        }
        let notified = hub.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let budget_notified = hub.budget_changed.notified();
        tokio::pin!(budget_notified);
        budget_notified.as_mut().enable();
        if hub.evict_history() {
            continue;
        }
        if shutdown.is_cancelled() {
            // No downstream has begun this source frame. Do not wait for an
            // un-evicted stopped reader to release its earlier frame before
            // cancelling the shared upstream and letting the Host drain.
            source.cancellation().cancel(CancelCause::Consumer);
            return Ok(None);
        }
        if topics.evict_stalled_across(hub, shutdown) {
            tokio::task::yield_now().await;
            continue;
        }
        // Other live topics retain their latest frame until they advance. If
        // those retained backings leave too little total capacity even after
        // every transient allocation is gone, waiting cannot make progress.
        let minimum_metadata =
            frame_metadata_bytes(total, total.min(c2_config::CHUNK_SIZE).max(1))?;
        let (retained_memory, retained_file) = *hub.latest_budgets.totals.lock();
        let usable_memory = max_memory.saturating_sub(retained_memory);
        let usable_file = max_file.saturating_sub(retained_file);
        if usable_memory == 0
            || (total.saturating_add(minimum_metadata) > usable_memory
                && total
                    > usable_memory
                        .saturating_sub(minimum_metadata.saturating_add(1))
                        .saturating_add(usable_file))
        {
            return Err(capacity("other active topics retain the relay budget"));
        }
        let minimum_memory = if available_file == 0 {
            total.saturating_add(minimum_metadata)
        } else {
            total
                .saturating_sub(available_file)
                .saturating_add(1)
                .saturating_add(minimum_metadata)
                .min(total.saturating_add(minimum_metadata))
        };
        // The lower bound above does not include the actual chunk geometry.
        // Once that geometry has failed, reserving an already available lower
        // bound would wake immediately and spin without changing the plan.
        let desired_memory = minimum_memory.max(available_memory.saturating_add(1));
        if desired_memory > max_memory && hub.file_segments.load(Ordering::Acquire) == 0 {
            return Err(capacity("relay file spill budget is unavailable"));
        }
        tokio::select! {
            result = budget.reserve_memory(desired_memory), if desired_memory <= max_memory => {
                drop(result.map_err(|e| capacity(e.to_string()))?);
            },
            _ = notified => {},
            _ = budget_notified => {},
            cause = wait_frame_stop(hub, shutdown, lifecycle) => {
                if cause == CancelCause::Lifecycle {
                    source.cancellation().cancel(CancelCause::Consumer);
                    return Ok(None);
                } else { return Ok(None); }
            },
        }
    };
    let metadata = Arc::new(
        memory
            .split_off(metadata_bytes)
            .map_err(|e| capacity(e.to_string()))?,
    );
    let staging = if file_backed {
        Some(
            memory
                .split_off(width)
                .map_err(|e| capacity(e.to_string()))?
                .into_reusable_window(),
        )
    } else {
        None
    };
    let mut remaining = total;
    let mut memory_remaining = memory_data;
    let mut chunks = Vec::new();
    while remaining != 0 {
        let n = if memory_remaining != 0 {
            remaining.min(memory_remaining).min(width)
        } else {
            remaining.min(width)
        };
        let file = memory_remaining == 0;
        let window = if file {
            staging
                .as_ref()
                .expect("file staging window")
                .reserve(n)
                .await
                .map_err(|e| capacity(e.to_string()))?
        } else {
            let reserve = memory.split_off(n).map_err(|e| capacity(e.to_string()))?;
            reserve
                .into_reusable_window()
                .reserve(n)
                .await
                .map_err(|e| capacity(e.to_string()))?
        };
        let cancellation = source.cancellation();
        let read = source.read_chunk(window, n as usize);
        tokio::pin!(read);
        let chunk = loop {
            tokio::select! {
                result = &mut read => break match result {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => return Err(C2Error::new(ErrorCode::ResourceUnavailable, "upstream ended during topic frame")),
                    Err(error) => return Err(error),
                },
                cause = wait_frame_stop(hub, shutdown, lifecycle) => {
                    if cause == CancelCause::Lifecycle {
                        cancellation.cancel(cause);
                        lifecycle = true;
                    } else { return Ok(None); }
                },
            }
        };
        let got = chunk.as_ref().len() as u64;
        if got != n {
            return Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                "short topic copy window",
            ));
        }
        let storage = if file {
            let (mut mapping, _) =
                c2_mem::create_file_spill(n as usize, &c2_config::default_spill_dir())
                    .map_err(|e| capacity(e.to_string()))?;
            mapping[..].copy_from_slice(chunk.as_ref());
            drop(chunk);
            SegmentStorage::File {
                mapping,
                _reservation: file_reservations
                    .pop_front()
                    .expect("reserved file segment"),
            }
        } else {
            memory_remaining -= n;
            SegmentStorage::Memory(chunk)
        };
        if file {
            hub.file_segments.fetch_add(1, Ordering::AcqRel);
        }
        chunks.push(Bytes::from_owner(Segment {
            storage: Some(storage),
            changed: hub.budget_changed.clone(),
            file_segments: hub.file_segments.clone(),
            file,
            _metadata: metadata.clone(),
        }));
        remaining -= n;
    }
    Ok(Some(HubFrame {
        seq,
        total,
        chunks,
        memory_bytes: memory_data + metadata_bytes,
        file_bytes: total - memory_data,
        _metadata: metadata,
    }))
}

fn frame_metadata_bytes(total: u64, width: u64) -> Result<u64, C2Error> {
    let count = total
        .div_ceil(width)
        .checked_add(1)
        .ok_or_else(|| capacity("topic chunk count overflow"))?;
    let per_chunk = std::mem::size_of::<Segment>()
        + std::mem::size_of::<Bytes>()
        + std::mem::size_of::<c2_mem::FileSpillReservation>()
        + 4 * std::mem::size_of::<usize>();
    count
        .checked_mul(per_chunk as u64)
        .and_then(|n| n.checked_add(std::mem::size_of::<HubFrame>() as u64))
        .ok_or_else(|| capacity("topic frame metadata size overflow"))
}

#[cfg(test)]
mod tests {
    use super::super::forwarding::ForwardingDomain;
    use super::*;

    fn body(hub: Arc<Hub>, domain: &ForwardingDomain, id: u64, lag: WireLagPolicy) -> BodyState {
        let (permit, tracked) = domain.admit_subscription().unwrap();
        let (_, control) = domain.register_stream().unwrap();
        let subscriber = Arc::new(Subscriber {
            _permit: permit,
            _tracked: tracked,
            connection: None,
            detached: AtomicBool::new(false),
            selected: AtomicBool::new(false),
            frame_started: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            unsent: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            sequence_exhausted: AtomicBool::new(false),
            inflight_chunks: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        hub.inner
            .lock()
            .subscribers
            .insert(id, Arc::downgrade(&subscriber));
        BodyState {
            hub,
            subscriber,
            control,
            done: tokio_util::sync::CancellationToken::new(),
            id,
            next_seq: 1,
            lag,
            phase: BodyPhase::Ready,
            pending: None,
            front_offset: 0,
            complete: 0,
            pending_terminal: None,
            budget: domain.budget().clone(),
            selection_charge: None,
            staging: None,
        }
    }

    #[tokio::test]
    async fn skip_and_error_subscribers_have_independent_cursors() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(
            &c2_config::DomainLimits::default(),
        ));
        let hub = Arc::new(Hub {
            key: HubKey {
                scope: HubScope::Local {
                    instance: "i".into(),
                    route_uid: "r".into(),
                },
                topic: "state".into(),
            },
            inner: Mutex::new(HubState::default()),
            changed: Arc::new(Notify::new()),
            budget_changed: Arc::new(Notify::new()),
            file_segments: Arc::new(AtomicUsize::new(0)),
            latest_budgets: Arc::new(LatestBudgets::default()),
        });
        let mut fast = body(hub.clone(), &domain, 1, WireLagPolicy::Skip);
        let mut slow = body(hub.clone(), &domain, 2, WireLagPolicy::Error);
        {
            let frame = |seq| {
                LatestFrame::new(
                    HubFrame {
                        seq,
                        total: 3,
                        chunks: vec![Bytes::from_static(b"abc")],
                        memory_bytes: 0,
                        file_bytes: 0,
                        _metadata: Arc::new(domain.budget().try_reserve_memory(0).unwrap()),
                    },
                    hub.latest_budgets.clone(),
                )
            };
            let mut state = hub.inner.lock();
            state.history.push_back(frame(2));
            state.latest = Some(frame(3));
        }
        let _ = fast.next().await.unwrap().unwrap();
        let _ = slow.next().await.unwrap().unwrap();
        let begin = fast.next().await.unwrap().unwrap();
        let (kind, _) = topic_wire::decode_header(begin[..9].try_into().unwrap()).unwrap();
        assert_eq!(kind, RecordKind::ItemBegin);
        assert_eq!(u64::from_le_bytes(begin[9..17].try_into().unwrap()), 3);
        let end = slow.next().await.unwrap().unwrap();
        let terminal = topic_wire::decode_terminal(&end[9..]).unwrap();
        assert_eq!(terminal.error.code, ErrorCode::SubscriptionLagged);
        assert_eq!(
            terminal.error.details.get("dropped").map(String::as_str),
            Some("1")
        );
        assert_eq!(terminal.error.details["available_first_seq"], "2");
        assert_eq!(terminal.error.details["available_last_seq"], "3");
        let data_header = fast.next().await.unwrap().unwrap();
        assert_eq!(
            topic_wire::decode_header(data_header[..9].try_into().unwrap()).unwrap(),
            (RecordKind::ItemData, 3)
        );
        assert_eq!(
            fast.next().await.unwrap().unwrap(),
            Bytes::from_static(b"abc")
        );
        assert_eq!(hub.inner.lock().subscribers.len(), 1);
    }

    #[tokio::test]
    async fn subscriber_receives_retained_history_before_latest() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(
            &c2_config::DomainLimits::default(),
        ));
        let hub = Arc::new(Hub {
            key: HubKey {
                scope: HubScope::Local {
                    instance: "i".into(),
                    route_uid: "r".into(),
                },
                topic: "state".into(),
            },
            inner: Mutex::new(HubState::default()),
            changed: Arc::new(Notify::new()),
            budget_changed: Arc::new(Notify::new()),
            file_segments: Arc::new(AtomicUsize::new(0)),
            latest_budgets: Arc::new(LatestBudgets::default()),
        });
        let frame = |seq, bytes| {
            LatestFrame::new(
                HubFrame {
                    seq,
                    total: 1,
                    chunks: vec![Bytes::from_static(bytes)],
                    memory_bytes: 0,
                    file_bytes: 0,
                    _metadata: Arc::new(domain.budget().try_reserve_memory(0).unwrap()),
                },
                hub.latest_budgets.clone(),
            )
        };
        {
            let mut state = hub.inner.lock();
            state.joiners = 1;
            state.history.push_back(frame(1, b"a"));
            state.latest = Some(frame(2, b"b"));
            state.prune_history();
            assert_eq!(state.history.len(), 1);
        }
        let mut subscriber = body(hub.clone(), &domain, 1, WireLagPolicy::Error);
        {
            let mut state = hub.inner.lock();
            state.joiners = 0;
            state.prune_history();
        }
        let _ready = subscriber.next().await.unwrap().unwrap();
        let first = subscriber.next().await.unwrap().unwrap();
        assert_eq!(u64::from_le_bytes(first[9..17].try_into().unwrap()), 1);
        let _data_header = subscriber.next().await.unwrap().unwrap();
        assert_eq!(
            subscriber.next().await.unwrap().unwrap(),
            Bytes::from_static(b"a")
        );
        let second = subscriber.next().await.unwrap().unwrap();
        assert_eq!(u64::from_le_bytes(second[9..17].try_into().unwrap()), 2);
        assert!(hub.inner.lock().history.is_empty());
    }

    #[tokio::test]
    async fn selected_frame_keeps_its_send_window_when_other_work_exhausts_memory() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 4096,
            ..Default::default()
        }));
        let hub = Arc::new(Hub {
            key: HubKey {
                scope: HubScope::Local {
                    instance: "i".into(),
                    route_uid: "r".into(),
                },
                topic: "state".into(),
            },
            inner: Mutex::new(HubState::default()),
            changed: Arc::new(Notify::new()),
            budget_changed: Arc::new(Notify::new()),
            file_segments: Arc::new(AtomicUsize::new(0)),
            latest_budgets: Arc::new(LatestBudgets::default()),
        });
        let mut subscriber = body(hub.clone(), &domain, 1, WireLagPolicy::Skip);
        hub.inner.lock().latest = Some(LatestFrame::new(
            HubFrame {
                seq: 1,
                total: 3,
                chunks: vec![Bytes::from_static(b"abc")],
                memory_bytes: 0,
                file_bytes: 0,
                _metadata: Arc::new(domain.budget().try_reserve_memory(0).unwrap()),
            },
            hub.latest_budgets.clone(),
        ));
        let _ready = subscriber.next().await.unwrap().unwrap();
        let begin = subscriber.next().await.unwrap().unwrap();
        assert_eq!(
            topic_wire::decode_header(begin[..9].try_into().unwrap())
                .unwrap()
                .0,
            RecordKind::ItemBegin
        );
        let snapshot = domain.budget().snapshot();
        let _other = domain
            .budget()
            .try_reserve_memory(snapshot.memory.limit_bytes - snapshot.memory.used_bytes)
            .unwrap();
        let header = subscriber.next().await.unwrap().unwrap();
        assert_eq!(
            topic_wire::decode_header(header[..9].try_into().unwrap()).unwrap(),
            (RecordKind::ItemData, 3)
        );
        assert_eq!(
            subscriber.next().await.unwrap().unwrap(),
            Bytes::from_static(b"abc")
        );
    }

    #[tokio::test]
    async fn large_frame_uses_chunk_sized_transport_window_when_budget_allows() {
        let chunk_size = usize::try_from(
            c2_config::REMOTE_PAYLOAD_CHUNK_SIZE
                .min(c2_config::CHUNK_SIZE)
                .max(1),
        )
        .unwrap();
        let domain = ForwardingDomain::new(DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: (chunk_size as u64) * 16,
            ..Default::default()
        }));
        let hub = Arc::new(Hub {
            key: HubKey {
                scope: HubScope::Local {
                    instance: "i".into(),
                    route_uid: "r".into(),
                },
                topic: "state".into(),
            },
            inner: Mutex::new(HubState::default()),
            changed: Arc::new(Notify::new()),
            budget_changed: Arc::new(Notify::new()),
            file_segments: Arc::new(AtomicUsize::new(0)),
            latest_budgets: Arc::new(LatestBudgets::default()),
        });
        let mut subscriber = body(hub.clone(), &domain, 1, WireLagPolicy::Skip);
        let chunk = Bytes::from(vec![0x5a; chunk_size]);
        hub.inner.lock().latest = Some(LatestFrame::new(
            HubFrame {
                seq: 1,
                total: (chunk_size * 2) as u64,
                chunks: vec![chunk.clone(), chunk],
                memory_bytes: 0,
                file_bytes: 0,
                _metadata: Arc::new(domain.budget().try_reserve_memory(0).unwrap()),
            },
            hub.latest_budgets.clone(),
        ));
        let _ready = subscriber.next().await.unwrap().unwrap();
        let _begin = subscriber.next().await.unwrap().unwrap();
        for _ in 0..2 {
            let header = subscriber.next().await.unwrap().unwrap();
            assert_eq!(
                topic_wire::decode_header(header[..9].try_into().unwrap()).unwrap(),
                (RecordKind::ItemData, chunk_size as u64)
            );
            let data = subscriber.next().await.unwrap().unwrap();
            assert_eq!(data.len(), chunk_size);
        }
    }

    #[tokio::test]
    async fn capacity_refusal_before_item_begin_is_a_canonical_terminal() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 4096,
            ..Default::default()
        }));
        let hub = Arc::new(Hub {
            key: HubKey {
                scope: HubScope::Local {
                    instance: "i".into(),
                    route_uid: "r".into(),
                },
                topic: "state".into(),
            },
            inner: Mutex::new(HubState::default()),
            changed: Arc::new(Notify::new()),
            budget_changed: Arc::new(Notify::new()),
            file_segments: Arc::new(AtomicUsize::new(0)),
            latest_budgets: Arc::new(LatestBudgets::default()),
        });
        let mut subscriber = body(hub.clone(), &domain, 1, WireLagPolicy::Skip);
        let _other_subscriber = body(hub.clone(), &domain, 2, WireLagPolicy::Skip);
        hub.inner.lock().latest = Some(LatestFrame::new(
            HubFrame {
                seq: 1,
                total: 3,
                chunks: vec![Bytes::from_static(b"abc")],
                memory_bytes: 0,
                file_bytes: 0,
                _metadata: Arc::new(domain.budget().try_reserve_memory(0).unwrap()),
            },
            hub.latest_budgets.clone(),
        ));
        let _ready = subscriber.next().await.unwrap().unwrap();
        let memory = domain.budget().snapshot().memory;
        let _other_work = domain
            .budget()
            .try_reserve_memory(memory.limit_bytes - memory.used_bytes)
            .unwrap();
        let bytes = subscriber.next().await.unwrap().unwrap();
        assert_eq!(
            topic_wire::decode_header(bytes[..9].try_into().unwrap())
                .unwrap()
                .0,
            RecordKind::Terminal
        );
        let terminal = topic_wire::decode_terminal(&bytes[9..]).unwrap();
        assert_eq!(terminal.complete_items, 0);
        assert_eq!(terminal.error.code, ErrorCode::CallCapacityExceeded);
    }

    #[tokio::test]
    async fn last_slot_stays_reserved_until_old_upstream_finishes() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(&c2_config::DomainLimits {
            max_outstanding_calls: 1,
            ..Default::default()
        }));
        let table = TopicHubTable::default();
        let (permit, tracked) = domain.admit_subscription().unwrap();
        let subscriber = Arc::new(Subscriber {
            _permit: permit,
            _tracked: tracked,
            connection: None,
            detached: AtomicBool::new(false),
            selected: AtomicBool::new(false),
            frame_started: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            unsent: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            sequence_exhausted: AtomicBool::new(false),
            inflight_chunks: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        let key = HubKey {
            scope: HubScope::Local {
                instance: "i".into(),
                route_uid: "r".into(),
            },
            topic: "state".into(),
        };
        let (old, leader, guard) = table.join(key, subscriber.clone()).await;
        assert!(leader);
        drop(guard);
        drop(subscriber);
        assert!(
            domain.admit_subscription().is_err(),
            "the original slot belongs to upstream cleanup"
        );
        old.fail(cancellation("consumer_cancelled"));
        old.finish_upstream();
        let (_permit, _tracked) = domain.admit_subscription().unwrap();
    }

    #[tokio::test]
    async fn an_isolated_hub_holds_its_last_slot_until_the_upstream_drains() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(&c2_config::DomainLimits {
            max_outstanding_calls: 1,
            ..Default::default()
        }));
        let table = TopicHubTable::default();
        let (permit, tracked) = domain.admit_subscription().unwrap();
        let subscriber = Arc::new(Subscriber {
            _permit: permit,
            _tracked: tracked,
            connection: None,
            detached: AtomicBool::new(false),
            selected: AtomicBool::new(false),
            frame_started: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            unsent: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            sequence_exhausted: AtomicBool::new(false),
            inflight_chunks: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        let key = HubKey {
            scope: HubScope::Local {
                instance: "i".into(),
                route_uid: "r".into(),
            },
            topic: "state".into(),
        };
        let (hub, _, guard) = table.join(key, subscriber.clone()).await;
        // A confirm refusal isolates the hub for downstream readers while its
        // old HTTP upstream is still draining: terminal, but not drained.
        hub.fail(C2Error::new(ErrorCode::ResourceNotFound, "route gone"));
        drop(guard);
        drop(subscriber);
        assert!(
            domain.admit_subscription().is_err(),
            "the last slot belongs to the draining upstream"
        );
        // `run_source` completes the upstream cleanup and releases the slot.
        hub.finish_upstream();
        let (_permit, _tracked) = domain.admit_subscription().unwrap();
    }

    #[tokio::test]
    async fn shutdown_and_last_leave_cancel_the_upstream_wait_token() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(
            &c2_config::DomainLimits::default(),
        ));
        let table = TopicHubTable::default();
        let make_subscriber = || {
            let (permit, tracked) = domain.admit_subscription().unwrap();
            Arc::new(Subscriber {
                _permit: permit,
                _tracked: tracked,
                connection: None,
                detached: AtomicBool::new(false),
                selected: AtomicBool::new(false),
                frame_started: AtomicBool::new(false),
                evicted: AtomicBool::new(false),
                unsent: Mutex::new(VecDeque::new()),
                next_seq: AtomicU64::new(1),
                sequence_exhausted: AtomicBool::new(false),
                inflight_chunks: AtomicUsize::new(0),
                changed: Notify::new(),
            })
        };
        // The last downstream leaves while the driver is still opening.
        let (first, _, guard) = table
            .join(
                HubKey {
                    scope: HubScope::Local {
                        instance: "i".into(),
                        route_uid: "a".into(),
                    },
                    topic: "state".into(),
                },
                make_subscriber(),
            )
            .await;
        let wait = first.inner.lock().upstream_wait.clone();
        assert!(!wait.is_cancelled());
        drop(guard);
        assert!(
            wait.is_cancelled(),
            "the last downstream leaving must unblock the opening wait"
        );
        // Relay shutdown reaches even hubs with no source handle yet.
        let (second, _, guard) = table
            .join(
                HubKey {
                    scope: HubScope::Local {
                        instance: "i".into(),
                        route_uid: "b".into(),
                    },
                    topic: "state".into(),
                },
                make_subscriber(),
            )
            .await;
        let wait = second.inner.lock().upstream_wait.clone();
        assert!(!wait.is_cancelled());
        table.cancel_sources_for_shutdown();
        assert!(
            wait.is_cancelled(),
            "shutdown must cancel an opening upstream's wait"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn a_stalled_reader_on_one_topic_frees_memory_for_another() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(
            &c2_config::DomainLimits::default(),
        ));
        let table = TopicHubTable::default();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (permit, tracked) = domain.admit_subscription().unwrap();
        let stalled_control = ConnectionControl::for_tests();
        let stalled = Arc::new(Subscriber {
            _permit: permit,
            _tracked: tracked,
            connection: Some(stalled_control.clone()),
            detached: AtomicBool::new(false),
            selected: AtomicBool::new(false),
            frame_started: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            unsent: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            sequence_exhausted: AtomicBool::new(false),
            inflight_chunks: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        let (topic_x, _, guard_x) = table
            .join(
                HubKey {
                    scope: HubScope::Local {
                        instance: "i".into(),
                        route_uid: "x".into(),
                    },
                    topic: "state".into(),
                },
                stalled.clone(),
            )
            .await;
        drop(guard_x);
        let (permit, tracked) = domain.admit_subscription().unwrap();
        let healthy = Arc::new(Subscriber {
            _permit: permit,
            _tracked: tracked,
            connection: None,
            detached: AtomicBool::new(false),
            selected: AtomicBool::new(false),
            frame_started: AtomicBool::new(false),
            evicted: AtomicBool::new(false),
            unsent: Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(1),
            sequence_exhausted: AtomicBool::new(false),
            inflight_chunks: AtomicUsize::new(0),
            changed: Notify::new(),
        });
        let (topic_y, _, guard_y) = table
            .join(
                HubKey {
                    scope: HubScope::Local {
                        instance: "i".into(),
                        route_uid: "y".into(),
                    },
                    topic: "state".into(),
                },
                healthy.clone(),
            )
            .await;
        drop(guard_y);
        // Topic X's reader stopped reading mid-frame and holds old memory;
        // topic Y's own hub has nothing evictable.
        stalled.frame_started.store(true, Ordering::Release);
        stalled_control.set_blocked(true);
        {
            let id = 1;
            topic_x
                .inner
                .lock()
                .subscribers
                .insert(id, Arc::downgrade(&stalled));
        }
        assert!(
            table.evict_stalled_across(&topic_y, &shutdown),
            "a stalled reader on another topic must be evictable"
        );
        assert!(stalled.evicted.load(Ordering::Acquire));
        assert!(stalled.detached.load(Ordering::Acquire));
        assert!(topic_x.inner.lock().subscribers.is_empty());
        // The healthy topic's reader stays untouched.
        assert!(!healthy.evicted.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn closing_hub_does_not_overlap_a_new_upstream() {
        let domain = ForwardingDomain::new(DomainBudget::from_limits(
            &c2_config::DomainLimits::default(),
        ));
        let table = Arc::new(TopicHubTable::default());
        let make_subscriber = || {
            let (permit, tracked) = domain.admit_subscription().unwrap();
            Arc::new(Subscriber {
                _permit: permit,
                _tracked: tracked,
                connection: None,
                detached: AtomicBool::new(false),
                selected: AtomicBool::new(false),
                frame_started: AtomicBool::new(false),
                evicted: AtomicBool::new(false),
                unsent: Mutex::new(VecDeque::new()),
                next_seq: AtomicU64::new(1),
                sequence_exhausted: AtomicBool::new(false),
                inflight_chunks: AtomicUsize::new(0),
                changed: Notify::new(),
            })
        };
        let key = HubKey {
            scope: HubScope::Local {
                instance: "i".into(),
                route_uid: "r".into(),
            },
            topic: "state".into(),
        };
        let (old, _, guard) = table.join(key.clone(), make_subscriber()).await;
        drop(guard);
        let subscriber = make_subscriber();
        let next = {
            let table = table.clone();
            let key = key.clone();
            tokio::spawn(async move { table.join(key, subscriber).await })
        };
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        old.fail(cancellation("consumer_cancelled"));
        old.finish_upstream();
        let (new, leader, guard) = next.await.unwrap();
        assert!(leader);
        assert!(!Arc::ptr_eq(&old, &new));
        // A delayed terminal from the old instance must not change the new one.
        old.fail(cancellation("host_shutdown"));
        table.remove(&old);
        assert!(new.inner.lock().terminal.is_none());
        assert!(Weak::ptr_eq(
            table.hubs.lock().get(&new.key).unwrap(),
            &Arc::downgrade(&new),
        ));
        table.remove(&new);
        assert!(table.hubs.lock().is_empty());
        drop(guard);
    }
}
