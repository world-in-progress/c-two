//! Reference-counted pool of [`SyncClient`] instances.
//!
//! Clients connecting to the same server address share a single
//! `SyncClient`. When all references are released the client is
//! kept alive for a grace period before being destroyed.

use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use c2_config::{ConnectDeadline, LocalEndpoint, LocalEndpointContext, MemoryBudgetLimits};
use c2_mem::{MemPool, PoolConfig};

/// Label counter for client pools. MemPool adds its incarnation and owns
/// platform segment-name derivation.
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);
const CONNECT_TRANSIENT_RETRY_ATTEMPTS: usize = 3;

use crate::client::{ClientIpcConfig, IpcError};
use crate::sync_client::SyncClient;

pub(crate) fn pool_config_from_client_config(cfg: &ClientIpcConfig) -> PoolConfig {
    // Centralized projection: the pooled-client acquire path must follow the
    // same buddy policy and client idle window as `with_config`. A disabled
    // buddy pool is still a real `MemPool` — it keeps serving dedicated SHM
    // requests and the wire prefix, only the buddy tiers are policy-disabled.
    cfg.base.primary_pool_config(&cfg.pool_tuning())
}

/// The cache's shared transport memory context.
///
/// Installed from the resolved config of the first connection attempt —
/// including one that later fails — and then frozen for the cache's lifetime.
/// Every cached connection's request pool and reassembly pool charges this one
/// budget, so one owning domain cannot double its cap by opening more
/// connections. The context owns only accounting counters; pools, clients, and
/// callbacks never cycle through it.
struct DomainMemory {
    limits: MemoryBudgetLimits,
    budget: c2_mem::MemoryBudget,
}

impl DomainMemory {
    fn from_config(cfg: &ClientIpcConfig) -> Self {
        let limits = cfg.memory_budget_limits();
        Self {
            limits,
            budget: c2_mem::MemoryBudget::from_limits(&limits),
        }
    }
}

/// Structured rejection for a configuration whose memory-budget limits
/// diverge from an already-frozen domain context.
fn budget_limits_mismatch(
    frozen: &MemoryBudgetLimits,
    requested: &MemoryBudgetLimits,
    scope: &str,
) -> IpcError {
    IpcError::Pool(format!(
        "client pool memory budget frozen at shm={}, file={}, reassembly={}; refusing {scope} \
         configuration with divergent limits shm={}, file={}, reassembly={}",
        frozen.shm_backing_budget_bytes,
        frozen.file_backing_budget_bytes,
        frozen.live_reassembly_budget_bytes,
        requested.shm_backing_budget_bytes,
        requested.file_backing_budget_bytes,
        requested.live_reassembly_budget_bytes,
    ))
}

/// Structured rejection for a same-address cache hit whose requested resolved
/// client policy differs from the cached connection's policy.
///
/// Budget limits can be equal while transfer behavior still differs (buddy
/// policy, prewarm, SHM threshold, chunking, reassembly geometry), so the
/// cache must not silently reuse a connection built from a different resolved
/// `ClientIpcConfig`. The cached entry is left untouched: existing callers
/// keep their connection and the caller with the divergent policy gets a
/// deterministic error instead of a wrong-policy client.
fn client_policy_mismatch(
    address: &str,
    cached: &ClientIpcConfig,
    requested: &ClientIpcConfig,
) -> IpcError {
    let differing = differing_client_policy_fields(cached, requested);
    IpcError::Pool(format!(
        "client cache at {address} already holds a connection created from a different resolved \
         client policy; refusing to reuse it for a divergent configuration (differing fields: \
         {}). Close the cached connection and reacquire, or request the identical resolved policy.",
        differing.join(", ")
    ))
}

/// Names of the resolved client-policy fields that differ between two configs.
///
/// Used in the rejection message so the caller can see exactly which policy
/// knob changed instead of guessing from a truncated dump.
fn differing_client_policy_fields(
    cached: &ClientIpcConfig,
    requested: &ClientIpcConfig,
) -> Vec<&'static str> {
    let mut differing = Vec::new();
    macro_rules! compare {
        ($name:literal, $cached:expr, $requested:expr) => {
            if $cached != $requested {
                differing.push($name);
            }
        };
    }
    compare!(
        "pool_enabled",
        cached.base.pool_enabled,
        requested.base.pool_enabled
    );
    compare!(
        "pool_segment_size",
        cached.base.pool_segment_size,
        requested.base.pool_segment_size
    );
    compare!(
        "max_pool_segments",
        cached.base.max_pool_segments,
        requested.base.max_pool_segments
    );
    compare!(
        "max_pool_memory",
        cached.base.max_pool_memory,
        requested.base.max_pool_memory
    );
    compare!(
        "pool_prewarm_segments",
        cached.base.pool_prewarm_segments,
        requested.base.pool_prewarm_segments
    );
    compare!(
        "pool_min_retained_segments",
        cached.base.pool_min_retained_segments,
        requested.base.pool_min_retained_segments
    );
    compare!(
        "reassembly_segment_size",
        cached.base.reassembly_segment_size,
        requested.base.reassembly_segment_size
    );
    compare!(
        "reassembly_max_segments",
        cached.base.reassembly_max_segments,
        requested.base.reassembly_max_segments
    );
    compare!(
        "max_total_chunks",
        cached.base.max_total_chunks,
        requested.base.max_total_chunks
    );
    compare!(
        "chunk_gc_interval_secs",
        cached.base.chunk_gc_interval_secs,
        requested.base.chunk_gc_interval_secs
    );
    compare!(
        "chunk_threshold_ratio",
        cached.base.chunk_threshold_ratio,
        requested.base.chunk_threshold_ratio
    );
    compare!(
        "chunk_assembler_timeout_secs",
        cached.base.chunk_assembler_timeout_secs,
        requested.base.chunk_assembler_timeout_secs
    );
    compare!(
        "max_reassembly_bytes",
        cached.base.max_reassembly_bytes,
        requested.base.max_reassembly_bytes
    );
    compare!(
        "chunk_size",
        cached.base.chunk_size,
        requested.base.chunk_size
    );
    compare!(
        "shm_backing_budget_bytes",
        cached.base.shm_backing_budget_bytes,
        requested.base.shm_backing_budget_bytes
    );
    compare!(
        "file_backing_budget_bytes",
        cached.base.file_backing_budget_bytes,
        requested.base.file_backing_budget_bytes
    );
    compare!(
        "live_reassembly_budget_bytes",
        cached.base.live_reassembly_budget_bytes,
        requested.base.live_reassembly_budget_bytes
    );
    compare!(
        "shm_threshold",
        cached.shm_threshold,
        requested.shm_threshold
    );
    compare!(
        "pool_decay_seconds",
        cached.pool_decay_seconds,
        requested.pool_decay_seconds
    );
    if differing.is_empty() {
        differing.push("none");
    }
    differing
}

/// Read-only snapshot of a [`ClientPool`]'s shared client memory context.
///
/// The pool reports `None` until the first connection attempt freezes the
/// domain; observing the snapshot never creates the context or connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientCacheMemorySnapshot {
    /// Limits the domain budget enforces; frozen at the first attempt.
    pub limits: MemoryBudgetLimits,
    /// Consistent view of the three budget cells.
    pub budget: c2_mem::BudgetSnapshot,
}

fn connect_lock<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: ConnectDeadline,
    stage: &'static str,
) -> Result<parking_lot::MutexGuard<'a, T>, IpcError> {
    crate::client::connect_check(deadline, stage)?;
    let guard = match deadline.instant() {
        None => mutex.lock(),
        Some(instant) => mutex
            .try_lock_until(instant)
            .ok_or_else(|| crate::client::connect_expired(stage))?,
    };
    crate::client::connect_check(deadline, stage)?;
    Ok(guard)
}

fn cleanup_deadline(deadline: ConnectDeadline) -> Instant {
    let guard = Instant::now() + DETACHED_CLOSE_TIMEOUT;
    deadline.instant().map_or(guard, |caller| caller.min(guard))
}

fn is_transient_connect_error(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::Io(io_error)
            if matches!(
                io_error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
            )
    )
}

fn connect_with_transient_retry(
    endpoint: &LocalEndpoint,
    cfg: &ClientIpcConfig,
    budget: &c2_mem::MemoryBudget,
    deadline: ConnectDeadline,
    mut fresh: FreshClientGuard,
) -> Result<(Arc<SyncClient>, FreshClientGuard), IpcError> {
    crate::sync_client::ensure_runtime_with_deadline(deadline)?;
    let pool_config = pool_config_from_client_config(cfg);
    let prewarm_segments = cfg.pool_prewarm_segments as usize;

    for attempt in 0..CONNECT_TRANSIENT_RETRY_ATTEMPTS {
        crate::client::connect_check(deadline, "pool_connect")?;
        let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3c{:08x}{:08x}", std::process::id(), counter);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            pool_config.clone(),
            prefix,
            budget.clone(),
        )));
        // Explicit prewarm only: without it the pool stays unmapped until the
        // first allocation (config validation rejects prewarm with buddy
        // disabled).
        if prewarm_segments > 0 {
            pool.lock()
                .ensure_buddy_segments(prewarm_segments)
                .map_err(|e| IpcError::Io(std::io::Error::other(e)))?;
        }

        let (client, result) = SyncClient::connect_transport_pool_attempt_with_deadline(
            endpoint.clone(),
            pool,
            cfg.clone(),
            budget.clone(),
            deadline,
        );
        let client = Arc::new(client);
        fresh.attach(client.clone());
        match result {
            Ok(()) => return Ok((client, fresh)),
            Err(error) => {
                // Even a handshake attempt that spawned native receive work
                // must keep its exact owner until the native close confirms.
                fresh.retire();
                if matches!(&error, IpcError::LocalCallRejected(error) if error.code == c2_error::ErrorCode::CallDeadlineExceeded)
                {
                    return Err(error);
                }
                crate::client::connect_check(deadline, "pool_cleanup")?;
                let confirmed = fresh.work.record.state() == RetiredState::Confirmed;
                if attempt + 1 >= CONNECT_TRANSIENT_RETRY_ATTEMPTS
                    || !confirmed
                    || !is_transient_connect_error(&error)
                {
                    return Err(error);
                }
                let delay = Duration::from_millis(10 * (attempt as u64 + 1));
                let remaining = deadline
                    .remaining("pool_retry")
                    .map_err(|_| crate::client::connect_expired("pool_retry"))?;
                std::thread::sleep(remaining.map_or(delay, |left| delay.min(left)));
                crate::client::connect_check(deadline, "pool_retry")?;
                let work = {
                    let mut coordinator =
                        connect_lock(&fresh.coordinator.0, deadline, "pool_cleanup_wait")?;
                    let ticket = coordinator.next_ticket;
                    coordinator.next_ticket += 1;
                    let record = Arc::new(RetiredClose::new(
                        fresh.work.record.address.clone(),
                        None,
                        RetiredState::Preparing,
                    ));
                    coordinator.retired.insert(ticket, record.clone());
                    RetiredWork { ticket, record }
                };
                fresh = FreshClientGuard {
                    work,
                    coordinator: fresh.coordinator.clone(),
                    deadline,
                    done: false,
                };
            }
        }
    }

    unreachable!("connect retry loop always returns before exhausting attempts")
}

/// Exact-entry reference state: a cancelled connect never needs to wait for
/// either RuntimeState or the global cache lock to release its reference.
struct PoolReferences {
    count: AtomicUsize,
    // Monotonic nanoseconds relative to a private origin, plus one; zero means
    // active. Release never needs a mutex, including while the cache is locked.
    origin: Instant,
    released_tick: AtomicU64,
}

impl PoolReferences {
    fn new(count: usize, last_release: Option<Instant>) -> Self {
        Self {
            count: AtomicUsize::new(count),
            origin: last_release.unwrap_or_else(Instant::now),
            released_tick: AtomicU64::new(u64::from(last_release.is_some())),
        }
    }
    fn tick(&self) -> u64 {
        self.origin.elapsed().as_nanos().min((u64::MAX - 1) as u128) as u64 + 1
    }
    fn acquire(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.released_tick.store(0, Ordering::Release);
    }
    fn release(&self) -> bool {
        if self.count.load(Ordering::Acquire) == 0 {
            return false;
        }
        // Publish before count can become zero; an idle observer then sees a
        // current release stamp, including racing acquire/release cycles.
        self.released_tick.fetch_max(self.tick(), Ordering::AcqRel);
        match self
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            }) {
            Ok(_) => true,
            Err(_) => false,
        }
    }
    fn expired(&self, grace: Duration) -> bool {
        if self.count.load(Ordering::Acquire) != 0 {
            return false;
        }
        let released = self.released_tick.load(Ordering::Acquire);
        released != 0
            && self.count.load(Ordering::Acquire) == 0
            && u128::from(self.tick().saturating_sub(released)) >= grace.as_nanos()
    }
}

/// One counted lease on an exact pooled connection. Drop releases only this
/// entry, even if the cache has replaced it or is currently being drained.
#[doc(hidden)]
pub struct ClientLease {
    client: Arc<SyncClient>,
    references: Arc<PoolReferences>,
    counted: bool,
}

impl ClientLease {
    pub fn client(&self) -> &Arc<SyncClient> {
        &self.client
    }

    /// Transfer the counted reference to an explicit-release caller.
    pub fn into_client(mut self) -> Arc<SyncClient> {
        self.counted = false;
        self.client.clone()
    }
}

impl std::ops::Deref for ClientLease {
    type Target = SyncClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Drop for ClientLease {
    fn drop(&mut self) {
        if self.counted {
            self.references.release();
        }
    }
}

// ── Pool entry ───────────────────────────────────────────────────────────

struct PoolEntry {
    client: Arc<SyncClient>,
    references: Arc<PoolReferences>,
}

/// Close-barrier deadline for clients detached by pool bookkeeping
/// (evictions, stale replacements, race losers, discards).
const DETACHED_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

// ── ClientPool ───────────────────────────────────────────────────────────

/// Mutable cache state guarded by the pool lock.
struct CacheState {
    entries: HashMap<String, PoolEntry>,
    /// Monotonic cache generation. `close_all` bumps the epoch after a drain
    /// so a connect that started before the shutdown fence cannot insert
    /// itself afterwards, while later acquires (restart/reacquire) proceed
    /// under the new epoch.
    epoch: u64,
    /// `Some(generation)` while a `close_all` drain transaction is between
    /// its fence and its epoch bump. Acquisitions started or observed in
    /// this window reject.
    closing_generation: Option<u64>,
    /// Shared transport memory context, frozen by the first connection
    /// attempt. Draining the cache closes connections but never uninstalls
    /// or resets this context: charges retained by completed or held data
    /// stay observable after shutdown, and reacquires under a new epoch keep
    /// charging the same domain limits.
    domain_memory: Option<DomainMemory>,
}

/// Lifecycle of one retired (detached) client record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetiredState {
    /// A bounded close barrier for this record is currently running.
    Closing = 1,
    /// A fresh acquire is still connecting. It is fenced by the cache epoch;
    /// no connected client exists yet and a drain need not wait for its I/O.
    Preparing = 2,
    /// Native work has confirmed close, or a fresh acquire published its client.
    Confirmed = 3,
    /// The last close barrier returned unconfirmed. The record keeps
    /// ownership of the client reachable for bounded settlement, ordinary
    /// acquire/sweep, or a later drain to retry. Never treated as stopped.
    UnconfirmedIdle = 4,
}

/// One client detached from the cache whose bounded close is owned by the
/// coordinator. Records are inserted in the same critical section that
/// removes the entry from `CacheState.entries` and removed only after a
/// close confirms, so a `close_all` can never observe an empty cache with
/// unaccounted detached work.
struct RetiredClose {
    address: String,
    client: Mutex<Option<Arc<SyncClient>>>,
    state: AtomicU8,
    // Per-record notification avoids Condvar's unbounded re-acquisition of
    // the global coordinator after a caller's timed wait has expired.
    waiters: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl RetiredClose {
    fn new(address: String, client: Option<Arc<SyncClient>>, state: RetiredState) -> Self {
        Self {
            address,
            client: Mutex::new(client),
            state: AtomicU8::new(state as u8),
            waiters: Mutex::new(Vec::new()),
        }
    }
    fn state(&self) -> RetiredState {
        match self.state.load(Ordering::Acquire) {
            1 => RetiredState::Closing,
            2 => RetiredState::Preparing,
            3 => RetiredState::Confirmed,
            4 => RetiredState::UnconfirmedIdle,
            _ => unreachable!("invalid native retired close state"),
        }
    }
    fn set_state(&self, state: RetiredState) {
        self.state.store(state as u8, Ordering::Release);
        // Waiter registration checks the state again after releasing this
        // short, record-local slot; a concurrent completion cannot be lost.
        let waiters = std::mem::take(&mut *self.waiters.lock());
        for waiter in waiters {
            let _ = waiter.send(());
        }
    }
    fn wait_until_settled(&self, deadline: ConnectDeadline) -> Result<(), IpcError> {
        while self.state() == RetiredState::Closing {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut waiters = connect_lock(&self.waiters, deadline, "pool_cleanup_wait")?;
            waiters.push(tx);
            drop(waiters);
            if self.state() != RetiredState::Closing {
                break;
            }
            match deadline.instant() {
                Some(end) => {
                    rx.recv_timeout(end.saturating_duration_since(Instant::now()))
                        .map_err(|_| crate::client::connect_expired("pool_cleanup_wait"))?;
                }
                None => {
                    let _ = rx.recv();
                }
            }
            crate::client::connect_check(deadline, "pool_cleanup_wait")?;
        }
        crate::client::connect_check(deadline, "pool_cleanup_wait")
    }
    /// Exactly one settlement/acquire/sweep/drain owns each retry barrier.
    fn claim_retry(&self) -> bool {
        self.state
            .compare_exchange(
                RetiredState::UnconfirmedIdle as u8,
                RetiredState::Closing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

struct RetiredWork {
    ticket: u64,
    record: Arc<RetiredClose>,
}

/// Reserve cleanup ownership before connection I/O. Even expiry while waiting
/// for the map/coordinator to publish can retain the unconfirmed native client
/// without acquiring either global lock after the caller deadline.
struct FreshClientGuard {
    work: RetiredWork,
    coordinator: Arc<(Mutex<CloseTxnState>, Condvar)>,
    deadline: ConnectDeadline,
    done: bool,
}

impl FreshClientGuard {
    fn attach(&self, client: Arc<SyncClient>) {
        *self.work.record.client.lock() = Some(client);
        self.work.record.set_state(RetiredState::Closing);
    }
    fn complete(&mut self) {
        self.work.record.client.lock().take();
        self.work.record.set_state(RetiredState::Confirmed);
        self.done = true;
        prune_confirmed(&self.coordinator, &self.work);
    }
    fn retire(&mut self) {
        if !self.done {
            if close_retired_work(
                &self.coordinator,
                &self.work,
                cleanup_deadline(self.deadline),
            )
            .is_err()
            {
                schedule_retired_settlement(&self.coordinator, &self.work);
            }
            self.done = true;
        }
    }
}
impl Drop for FreshClientGuard {
    fn drop(&mut self) {
        self.retire();
    }
}

struct RetiredBatch {
    work: Vec<RetiredWork>,
    coordinator: Arc<(Mutex<CloseTxnState>, Condvar)>,
    deadline: ConnectDeadline,
}
impl RetiredBatch {
    fn push(&mut self, work: RetiredWork) {
        self.work.push(work);
    }
    fn close(&mut self) {
        run_retired_work(
            &self.coordinator,
            &self.work,
            cleanup_deadline(self.deadline),
        );
        for work in &self.work {
            schedule_retired_settlement(&self.coordinator, work);
        }
        self.work.clear();
    }
}
impl Drop for RetiredBatch {
    fn drop(&mut self) {
        self.close();
    }
}

/// Drain-transaction coordinator, guarded separately from [`CacheState`] so
/// entry bookkeeping never happens while a drain holds I/O and concurrent
/// `close_all` callers serialize instead of interleaving their fences.
struct CloseTxnState {
    next_generation: u64,
    /// Drain transaction currently owning the closing window, if any. A
    /// second `close_all` waits for this to clear (bounded by its own
    /// deadline) instead of clearing the fence under the active drain.
    active_generation: Option<u64>,
    /// Detached clients with a registered (running or unconfirmed-idle)
    /// close barrier. Keyed by ticket; see [`RetiredClose`].
    retired: HashMap<u64, Arc<RetiredClose>>,
    next_ticket: u64,
}

/// Honest outcome of one cache drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCacheCloseReport {
    /// Number of cache entries detached for close.
    pub detached: usize,
    /// Addresses whose close barrier did not confirm within the deadline.
    pub unconfirmed: Vec<String>,
    /// Why this caller could not claim a completed drain: it timed out
    /// waiting for a concurrent drain transaction or for detached-close
    /// barriers started by discard/sweep/loser cleanup. Nothing is claimed
    /// stopped in this case.
    pub error: Option<String>,
}

/// Reference-counted pool of `SyncClient` instances.
///
/// Clients connecting to the same server address share a single
/// `SyncClient`. When all references are released the client is
/// kept alive for a grace period before being destroyed.
///
/// Every connect and close happens outside the pool lock; the lock only
/// guards entry bookkeeping, the closing fence, and the epoch. Detached
/// clients (expiry sweeps, stale replacements, concurrent same-address race
/// losers, discards, drains) are always closed explicitly through the bounded
/// shared-ownership close instead of relying on `Drop`.
pub struct ClientPool {
    // One immutable namespace per cache. Address keys are scoped by this owner;
    // draining changes only the epoch, never the endpoint or memory domain.
    endpoint_context: Result<LocalEndpointContext, String>,
    state: Mutex<CacheState>,
    grace_period: Duration,
    default_config: Mutex<Option<ClientIpcConfig>>,
    /// See [`CloseTxnState`]. Shared shape so test hooks can wait on it.
    close_txn: Arc<(Mutex<CloseTxnState>, Condvar)>,
}

// Compile-time assertion: ClientPool must be Send + Sync.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<ClientPool>();
        _assert_sync::<ClientPool>();
    }
};

/// Run one registered retired record's bounded close barrier.
///
/// The barrier runs with the time remaining to `deadline` (an absolute
/// aggregate budget owned by the caller, e.g. one cache shutdown), never a
/// fresh timeout. On confirmation the exact ticket's record is removed; on
/// an unconfirmed timeout the record **stays** in the registry as
/// `RetiredState::UnconfirmedIdle`, keeping the client's ownership reachable
/// so ordinary maintenance or a later drain can retry it honestly.
/// Returns `Err(address)` when the close did not confirm.
fn prune_confirmed(txn: &(Mutex<CloseTxnState>, Condvar), work: &RetiredWork) {
    if let Some(mut state) = txn.0.try_lock() {
        if work.record.state() == RetiredState::Confirmed
            && state
                .retired
                .get(&work.ticket)
                .is_some_and(|record| Arc::ptr_eq(record, &work.record))
        {
            state.retired.remove(&work.ticket);
        }
    }
    txn.1.notify_all();
}

fn close_retired_work(
    txn: &(Mutex<CloseTxnState>, Condvar),
    work: &RetiredWork,
    deadline: Instant,
) -> Result<(), String> {
    let client = work.record.client.lock().clone();
    let confirmed = client.as_ref().is_none_or(|client| {
        client.close_shared(deadline.saturating_duration_since(Instant::now()))
    });
    if confirmed {
        // Confirmation, rather than registry-lock availability, releases this
        // exact owner. Retained payloads keep their own pool Arcs and charges.
        work.record.client.lock().take();
    }
    drop(client);
    work.record.set_state(if confirmed {
        RetiredState::Confirmed
    } else {
        RetiredState::UnconfirmedIdle
    });
    prune_confirmed(txn, work);
    if confirmed {
        Ok(())
    } else {
        Err(work.record.address.clone())
    }
}

/// A caller whose total budget is exhausted cannot wait for native cleanup.
/// Give its registered owner one independent, bounded settlement attempt.
/// This finite worker closes only this record; it never loops, acquires pool
/// metadata locks, resets the connect deadline, or publishes a client. A
/// failed spawn/barrier leaves UnconfirmedIdle for ordinary maintenance.
fn schedule_retired_settlement(txn: &Arc<(Mutex<CloseTxnState>, Condvar)>, work: &RetiredWork) {
    if !work.record.claim_retry() {
        return;
    }
    let settlement = RetiredWork {
        ticket: work.ticket,
        record: work.record.clone(),
    };
    let coordinator = txn.clone();
    let deadline = Instant::now() + DETACHED_CLOSE_TIMEOUT;
    if std::thread::Builder::new()
        .name("c2-client-retire".into())
        .spawn(move || {
            let _ = close_retired_work(&coordinator, &settlement, deadline);
        })
        .is_err()
    {
        work.record.set_state(RetiredState::UnconfirmedIdle);
        txn.1.notify_all();
    }
}

/// Claim idle failures under the coordinator, without touching healthy cache
/// entries or changing the domain/epoch. Close outside every metadata lock.
fn claim_retired_retries(coordinator: &CloseTxnState) -> Vec<RetiredWork> {
    if coordinator.active_generation.is_some() {
        return Vec::new();
    }
    coordinator
        .retired
        .iter()
        .filter_map(|(ticket, record)| {
            record.claim_retry().then(|| RetiredWork {
                ticket: *ticket,
                record: record.clone(),
            })
        })
        .collect()
}

fn close_retired_ticket(
    txn: &(Mutex<CloseTxnState>, Condvar),
    ticket: u64,
    deadline: Instant,
) -> Result<(), String> {
    let record = {
        let state = txn.0.lock();
        match state.retired.get(&ticket) {
            Some(record) if record.state() == RetiredState::Closing => record.clone(),
            _ => return Ok(()),
        }
    };
    close_retired_work(txn, &RetiredWork { ticket, record }, deadline)
}

fn run_retired_work(
    txn: &(Mutex<CloseTxnState>, Condvar),
    work: &[RetiredWork],
    deadline: Instant,
) {
    for record in work {
        let _ = close_retired_work(txn, record, deadline);
    }
}

fn run_retired_closes_until(
    txn: &(Mutex<CloseTxnState>, Condvar),
    tickets: &[u64],
    deadline: Instant,
) {
    for ticket in tickets {
        let _ = close_retired_ticket(txn, *ticket, deadline);
    }
}

impl ClientPool {
    /// Create a new pool with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self::from_endpoint_context(
            grace_period,
            LocalEndpointContext::default_for_platform().map_err(|error| error.to_string()),
        )
    }

    /// Create one cache domain from a resolved, immutable endpoint context.
    /// Unix custom-root containers must exist before acquire. Windows callers
    /// supply the platform's Named Pipe context, with no Unix path projection.
    pub fn with_endpoint_context(grace_period: Duration, context: LocalEndpointContext) -> Self {
        Self::from_endpoint_context(grace_period, Ok(context))
    }

    fn from_endpoint_context(
        grace_period: Duration,
        context: Result<LocalEndpointContext, String>,
    ) -> Self {
        Self {
            endpoint_context: context,
            state: Mutex::new(CacheState {
                entries: HashMap::new(),
                epoch: 0,
                closing_generation: None,
                domain_memory: None,
            }),
            grace_period,
            default_config: Mutex::new(None),
            close_txn: Arc::new((
                Mutex::new(CloseTxnState {
                    next_generation: 0,
                    active_generation: None,
                    retired: HashMap::new(),
                    next_ticket: 0,
                }),
                Condvar::new(),
            )),
        }
    }

    /// The namespace shared by every entry and every epoch of this cache.
    pub fn endpoint_context(&self) -> Result<&LocalEndpointContext, IpcError> {
        self.endpoint_context
            .as_ref()
            .map_err(|error| IpcError::Config(error.clone()))
    }

    /// Set the default IPC config for newly created clients.
    ///
    /// After the domain memory context freezes (first connection attempt), a
    /// default whose budget limits diverge from the frozen domain is rejected:
    /// the cache must not be able to acquire a second budget policy through
    /// its default configuration path.
    pub fn set_default_config(&self, config: ClientIpcConfig) -> Result<(), IpcError> {
        let state = self.state.lock();
        if let Some(domain) = state.domain_memory.as_ref() {
            let limits = config.memory_budget_limits();
            if domain.limits != limits {
                return Err(budget_limits_mismatch(
                    &domain.limits,
                    &limits,
                    "cache default",
                ));
            }
        }
        *self.default_config.lock() = Some(config);
        Ok(())
    }

    /// Read-only snapshot of this cache's shared client memory context.
    ///
    /// `None` until the first connection attempt freezes the domain. This
    /// never connects, never creates the context, and never resets usage:
    /// charges retained by completed or held data stay observable across
    /// shutdowns for as long as the pool (or any budget guard) exists.
    pub fn memory_budget_snapshot(&self) -> Option<ClientCacheMemorySnapshot> {
        self.state
            .lock()
            .domain_memory
            .as_ref()
            .map(|domain| ClientCacheMemorySnapshot {
                limits: domain.limits,
                budget: domain.budget.snapshot(),
            })
    }

    /// Read-only observer of this cache's frozen domain budget, if frozen.
    ///
    /// The returned handle shares only the accounting counters and the
    /// resolved limits, so a retired cache can keep reporting charges held by
    /// outstanding owners without retaining any client, pool, or callback
    /// authority. Observing never connects and never installs a context.
    pub fn memory_budget_observer(&self) -> Option<c2_mem::BudgetObserver> {
        self.state
            .lock()
            .domain_memory
            .as_ref()
            .map(|domain| c2_mem::BudgetObserver::new(domain.limits, domain.budget.clone()))
    }

    /// Register one detached client as a pending bounded close.
    ///
    /// MUST be called in the same critical section that removed the client
    /// from `CacheState.entries` (the caller holds the state lock); this
    /// takes the coordinator lock internally (state → txn order), so
    /// detachment and close-accounting become one atomic step and a
    /// concurrent `close_all` can never observe an empty cache with the
    /// detached client unaccounted. Performs no I/O.
    fn retire_locked(&self, address: &str, client: Arc<SyncClient>) -> u64 {
        let mut txn = self.close_txn.0.lock();
        self.retire_under_coordinator(&mut txn, address, client)
            .ticket
    }

    fn register_close_work(
        &self,
        txn: &mut CloseTxnState,
        address: &str,
        client: Option<Arc<SyncClient>>,
        state: RetiredState,
    ) -> RetiredWork {
        let ticket = txn.next_ticket;
        txn.next_ticket += 1;
        let record = Arc::new(RetiredClose::new(address.to_string(), client, state));
        txn.retired.insert(ticket, record.clone());
        self.close_txn.1.notify_all();
        RetiredWork { ticket, record }
    }

    fn retire_under_coordinator(
        &self,
        txn: &mut CloseTxnState,
        address: &str,
        client: Arc<SyncClient>,
    ) -> RetiredWork {
        self.register_close_work(txn, address, Some(client), RetiredState::Closing)
    }

    /// Acquire a client for `address`. Creates and connects if needed.
    /// Increments reference count.
    ///
    /// The connect runs outside the pool lock under an epoch fence: if a
    /// concurrent `close_all` drains the cache while this acquire is
    /// connecting, the fresh connection is closed explicitly and the acquire
    /// fails instead of inserting an entry after the drain.
    ///
    /// The shared memory context freezes atomically under the pool lock with
    /// the resolved config, before any connection I/O: the first attempt —
    /// including one that later fails — installs the domain budget from that
    /// config, and every later acquire must resolve the same budget limits or
    /// reject, so per-address configuration can never bypass the domain's
    /// limits or race a second budget policy into existence.
    pub fn acquire(
        &self,
        address: &str,
        config: Option<&ClientIpcConfig>,
    ) -> Result<Arc<SyncClient>, IpcError> {
        self.acquire_with_deadline(address, config, ConnectDeadline::default())
    }

    pub fn acquire_with_deadline(
        &self,
        address: &str,
        config: Option<&ClientIpcConfig>,
        deadline: ConnectDeadline,
    ) -> Result<Arc<SyncClient>, IpcError> {
        self.acquire_lease_with_deadline(address, config, deadline)
            .map(ClientLease::into_client)
    }

    pub fn acquire_lease_with_deadline(
        &self,
        address: &str,
        config: Option<&ClientIpcConfig>,
        deadline: ConnectDeadline,
    ) -> Result<ClientLease, IpcError> {
        let mut retired = RetiredBatch {
            work: Vec::new(),
            coordinator: self.close_txn.clone(),
            deadline,
        };
        let mut fresh = None;
        let epoch;
        let cfg;
        let budget;
        let mut rejection: Option<IpcError> = None;
        {
            let mut state = connect_lock(&self.state, deadline, "pool_wait")?;
            let mut coordinator = connect_lock(&self.close_txn.0, deadline, "pool_cleanup_wait")?;
            coordinator
                .retired
                .retain(|_, record| record.state() != RetiredState::Confirmed);
            if state.closing_generation.is_some() {
                return Err(IpcError::Pool(
                    "client pool acquire rejected: cache is closing".to_string(),
                ));
            }
            epoch = state.epoch;
            // Resolve config before any fast-path return: the domain budget
            // gate below must see the same resolved config on a cache hit as
            // on a fresh connect, so a per-address configuration can never
            // bypass the frozen domain limits on the same-address fast path.
            cfg = match config {
                Some(c) => c.clone(),
                None => connect_lock(&self.default_config, deadline, "pool_config_wait")?
                    .clone()
                    .unwrap_or_default(),
            };
            // Detach expired entries before potentially creating a new one;
            // each detachment registers its pending close in this same
            // critical section.
            for (expired_address, client) in
                Self::sweep_expired_locked(&mut state, self.grace_period)
            {
                retired.push(self.retire_under_coordinator(
                    &mut coordinator,
                    &expired_address,
                    client,
                ));
            }
            // First-connect-wins memory context, installed or validated in
            // the same critical section that resolved the config so a racing
            // acquire can never connect under a second budget policy.
            let limits = cfg.memory_budget_limits();
            match state.domain_memory.as_ref() {
                Some(domain) if domain.limits != limits => {
                    budget = domain.budget.clone();
                    rejection = Some(budget_limits_mismatch(
                        &domain.limits,
                        &limits,
                        "cache domain",
                    ));
                }
                Some(domain) => {
                    budget = domain.budget.clone();
                }
                None => {
                    let domain = DomainMemory::from_config(&cfg);
                    budget = domain.budget.clone();
                    state.domain_memory = Some(domain);
                }
            }
            // A same-address hit is only valid when the cached connection was
            // created from the same complete resolved policy. The domain
            // budget identity is shared, but buddy policy, prewarm, SHM
            // threshold, chunking, and reassembly geometry must not be
            // silently bypassed by returning an old client.
            if rejection.is_none()
                && let Some(entry) = state.entries.get(address)
                && entry.client.is_connected()
                && entry.client.config() != &cfg
            {
                rejection = Some(client_policy_mismatch(address, entry.client.config(), &cfg));
            }
            // A rejected acquire must not detach a live entry: the frozen
            // domain or the cached policy already rejected the requested
            // configuration, so the cache keeps serving existing callers
            // unchanged.
            if rejection.is_none()
                && let Some(entry) = state.entries.get_mut(address)
            {
                if entry.client.is_connected() {
                    crate::client::connect_check(deadline, "pool_acquire")?;
                    entry.references.acquire();
                    let lease = ClientLease {
                        client: entry.client.clone(),
                        references: entry.references.clone(),
                        counted: true,
                    };
                    drop(coordinator);
                    drop(state);
                    retired.close();
                    if let Err(error) = crate::client::connect_check(deadline, "pool_acquire") {
                        return Err(error);
                    }
                    return Ok(lease);
                }
                // Stale — detach and fall through to create a new one.
                if let Some(entry) = state.entries.remove(address) {
                    retired.push(self.retire_under_coordinator(
                        &mut coordinator,
                        address,
                        entry.client,
                    ));
                }
            }
            if rejection.is_none() {
                for work in claim_retired_retries(&coordinator) {
                    retired.push(work);
                }
                fresh = Some(FreshClientGuard {
                    work: self.register_close_work(
                        &mut coordinator,
                        address,
                        None,
                        RetiredState::Preparing,
                    ),
                    coordinator: self.close_txn.clone(),
                    deadline,
                    done: false,
                });
            }
        }
        // Drop the pool lock before closing retired records or connecting
        // (both may block). Retired tickets registered above are closed even
        // when this acquire is rejected, so no detached client is stranded.
        retired.close();
        if let Some(error) = rejection {
            return Err(error);
        }

        // Another caller's finite settlement can still own backing charges.
        // A fresh allocation waits for it using this operation's remaining
        // budget. Cache hits already returned above and stay independent.
        self.wait_retired_settlement(deadline)?;
        // Derive once before retries; every attempt uses this same snapshot.
        let endpoint = self
            .endpoint_context()?
            .endpoint(address)
            .map_err(crate::control::endpoint_error)?;
        let (client, mut fresh) = connect_with_transient_retry(
            &endpoint,
            &cfg,
            &budget,
            deadline,
            fresh.expect("fresh acquisition reserves cleanup ownership"),
        )?;

        let mut state = match connect_lock(&self.state, deadline, "pool_publish_wait") {
            Ok(state) => state,
            Err(error) => return Err(error),
        };
        let mut coordinator = match connect_lock(&self.close_txn.0, deadline, "pool_cleanup_wait") {
            Ok(coordinator) => coordinator,
            Err(error) => {
                drop(state);
                return Err(error);
            }
        };
        if state.closing_generation.is_some() || state.epoch != epoch {
            // A drain completed while this connect was in flight. The entry
            // cannot join the drained generation; register and close it
            // explicitly while still holding the state lock so the barrier
            // is accounted before we release it.
            drop(coordinator);
            drop(state);
            fresh.retire();
            return Err(IpcError::Pool(
                "client pool acquire rejected: cache closed while connecting".to_string(),
            ));
        }
        if let Some(entry) = state.entries.get_mut(address)
            && entry.client.is_connected()
        {
            if entry.client.config() != &cfg {
                // A racing acquire inserted a connection built from a
                // different resolved policy. Its entry stays intact for its
                // existing callers; this fresh connection is the loser and is
                // closed explicitly, and the caller gets a deterministic
                // mismatch instead of a wrong-policy client.
                let winner_policy = entry.client.config().clone();
                drop(coordinator);
                drop(state);
                fresh.retire();
                return Err(client_policy_mismatch(address, &winner_policy, &cfg));
            }
            // Another thread raced and inserted the same address; its client
            // wins and this fresh connection is the loser. Bump the winner
            // under the lock, register the loser's close, then run it
            // outside the lock.
            crate::client::connect_check(deadline, "pool_acquire")?;
            entry.references.acquire();
            let winner = ClientLease {
                client: entry.client.clone(),
                references: entry.references.clone(),
                counted: true,
            };
            drop(coordinator);
            drop(state);
            fresh.retire();
            if let Err(error) = crate::client::connect_check(deadline, "pool_acquire") {
                return Err(error);
            }
            return Ok(winner);
        }
        crate::client::connect_check(deadline, "pool_publish")?;
        let references = Arc::new(PoolReferences::new(1, None));
        let lease = ClientLease {
            client: client.clone(),
            references: references.clone(),
            counted: true,
        };
        let replaced = state.entries.insert(
            address.to_owned(),
            PoolEntry {
                client: Arc::clone(&client),
                references,
            },
        );
        // A stale entry raced back in and lost; register its close in the
        // same critical section that replaced it.
        let replaced_ticket = replaced
            .map(|old| self.retire_under_coordinator(&mut coordinator, address, old.client));
        fresh.complete();
        coordinator.retired.remove(&fresh.work.ticket);
        drop(coordinator);
        drop(state);
        if let Some(work) = replaced_ticket {
            let _ = close_retired_work(&self.close_txn, &work, cleanup_deadline(deadline));
        }
        if let Err(error) = crate::client::connect_check(deadline, "pool_acquire") {
            return Err(error);
        }
        Ok(lease)
    }

    /// Decrement reference count. When it reaches 0, mark for grace-period
    /// cleanup.
    pub fn release(&self, address: &str) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(address) {
            if !entry.references.release() {
                eprintln!("ClientPool::release: ref_count already 0 for {address}");
            }
        } else {
            eprintln!("ClientPool::release: no entry for {address}");
        }
    }

    /// Release one reference only when the pool still contains the acquired client.
    ///
    /// This identity check prevents a late drop from an evicted connection
    /// decrementing the reference count of a replacement at the same address.
    pub fn release_if_same(&self, address: &str, observed: &Arc<SyncClient>) -> bool {
        let mut state = self.state.lock();
        let Some(entry) = state.entries.get_mut(address) else {
            return false;
        };
        if !Arc::ptr_eq(&entry.client, observed) {
            return false;
        }
        entry.references.release()
    }

    /// Remove one observed unusable client without evicting a racing replacement.
    ///
    /// Callers must use this only after a pre-dispatch operation proves that
    /// the exact acquired connection can no longer serve requests. The
    /// detached client's bounded close is registered in the same critical
    /// section that removes the entry, then runs outside the pool lock.
    pub fn discard_if_same(&self, address: &str, observed: &Arc<SyncClient>) -> bool {
        self.discard_if_same_with_deadline(address, observed, ConnectDeadline::default())
            .unwrap_or(false)
    }

    pub fn discard_if_same_with_deadline(
        &self,
        address: &str,
        observed: &Arc<SyncClient>,
        deadline: ConnectDeadline,
    ) -> Result<bool, IpcError> {
        let ticket = {
            let mut state = connect_lock(&self.state, deadline, "pool_discard_wait")?;
            let mut coordinator = connect_lock(&self.close_txn.0, deadline, "pool_cleanup_wait")?;
            let is_same = state
                .entries
                .get(address)
                .is_some_and(|entry| Arc::ptr_eq(&entry.client, observed));
            if is_same {
                let entry = state
                    .entries
                    .remove(address)
                    .expect("entry presence checked above");
                Some(self.retire_under_coordinator(&mut coordinator, address, entry.client))
            } else {
                None
            }
        };
        match ticket {
            Some(ticket) => {
                let _ = close_retired_work(&self.close_txn, &ticket, cleanup_deadline(deadline));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Detach entries that have been unreferenced longer than `grace_period`.
    fn sweep_expired_locked(
        state: &mut CacheState,
        grace: Duration,
    ) -> Vec<(String, Arc<SyncClient>)> {
        let mut detached = Vec::new();
        state.entries.retain(|address, entry| {
            if entry.references.expired(grace) {
                detached.push((address.clone(), Arc::clone(&entry.client)));
                return false;
            }
            true
        });
        detached
    }

    fn wait_retired_settlement(&self, deadline: ConnectDeadline) -> Result<(), IpcError> {
        let records = {
            let coordinator = connect_lock(&self.close_txn.0, deadline, "pool_cleanup_wait")?;
            coordinator
                .retired
                .values()
                .filter(|record| record.state() == RetiredState::Closing)
                .cloned()
                .collect::<Vec<_>>()
        };
        for record in records {
            record.wait_until_settled(deadline)?;
        }
        Ok(())
    }

    /// Sweep expired entries and retry unconfirmed retired owners outside the
    /// pool lock. All barriers share one bounded maintenance budget.
    /// Call this periodically from SDK bindings or before acquire.
    pub fn sweep_expired(&self) {
        let maintenance = ConnectDeadline::start(
            c2_config::ConnectOptions::new().with_timeout(DETACHED_CLOSE_TIMEOUT),
        )
        .expect("fixed maintenance budget is representable");
        let tickets = {
            let mut state = self.state.lock();
            Self::sweep_expired_locked(&mut state, self.grace_period)
                .into_iter()
                .map(|(address, client)| self.retire_locked(&address, client))
                .collect::<Vec<u64>>()
        };
        let deadline = maintenance.instant().unwrap();
        let retries = claim_retired_retries(&self.close_txn.0.lock());
        run_retired_closes_until(&self.close_txn, &tickets, deadline);
        run_retired_work(&self.close_txn, &retries, deadline);
        let _ = self.wait_retired_settlement(maintenance);
    }

    /// Drain and explicitly close every cache entry, then reopen the cache
    /// under a new epoch.
    ///
    /// Drain transactions are serialized through a closing generation: a
    /// concurrent `close_all` waits for the active drain to finish (bounded
    /// by this caller's absolute deadline) and then performs its own drain,
    /// so no caller can clear the fence or bump the epoch while another
    /// drain is still closing slow clients, and an entry raced in between
    /// two drains is covered by the second. Before starting, the drain waits
    /// (within the same deadline) for registered pending-close records from
    /// discard/sweep/loser cleanup — registration happens atomically with
    /// detachment, so a shutdown can never observe an empty cache with
    /// unaccounted detached work — and reports an `error` instead of
    /// claiming completion if the wait expires.
    ///
    /// All child closes share **one aggregate absolute deadline** derived
    /// from `timeout`: each child receives only the time remaining, so a
    /// cache of N blocked clients costs at most one deadline, not N. Drain
    /// children are registered as retired records in the same critical
    /// section that empties the map; an unconfirmed child keeps its record
    /// (`UnconfirmedIdle`) so its ownership stays reachable. The drain also
    /// retries records left unconfirmed by earlier barriers and reports
    /// every record still registered at finalize time as unconfirmed — a
    /// client is only claimed stopped once its record is removed after a
    /// confirmed close. No backing memory is force-released. Acquisitions
    /// during the closing window reject; after this returns, new acquires
    /// (restart/reacquire) proceed under the new epoch.
    pub fn close_all(&self, timeout: Duration) -> ClientCacheCloseReport {
        let mut report = ClientCacheCloseReport {
            detached: 0,
            unconfirmed: Vec::new(),
            error: None,
        };
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400 * 365));

        let generation = {
            let (txn_lock, txn_condvar) = &*self.close_txn;
            let mut txn = txn_lock.lock();
            loop {
                txn.retired
                    .retain(|_, record| record.state() != RetiredState::Confirmed);
                if txn.active_generation.is_none()
                    && !txn
                        .retired
                        .values()
                        .any(|record| record.state() == RetiredState::Closing)
                {
                    break;
                }
                let now = Instant::now();
                let blocked_by_drain = txn.active_generation.is_some();
                if now >= deadline {
                    report.error = Some(Self::drain_wait_error(blocked_by_drain));
                    return report;
                }
                let result =
                    txn_condvar.wait_for(&mut txn, (deadline - now).min(Duration::from_millis(5)));
                if result.timed_out()
                    && Instant::now() >= deadline
                    && (txn.active_generation.is_some()
                        || txn
                            .retired
                            .values()
                            .any(|record| record.state() == RetiredState::Closing))
                {
                    report.error = Some(Self::drain_wait_error(txn.active_generation.is_some()));
                    return report;
                }
            }
            let generation = txn.next_generation;
            txn.next_generation += 1;
            txn.active_generation = Some(generation);
            generation
        };
        self.close_txn.1.notify_all();

        // Empty the map and register every drained child as a pending close
        // in the same critical section; also claim (for retry) records that
        // earlier barriers left unconfirmed.
        let (drained_tickets, retry_tickets) = {
            let mut state = self.state.lock();
            state.closing_generation = Some(generation);
            let drained: Vec<(String, Arc<SyncClient>)> = std::mem::take(&mut state.entries)
                .into_iter()
                .map(|(address, entry)| (address, entry.client))
                .collect();
            let mut txn = self.close_txn.0.lock();
            let drained_tickets: Vec<u64> = drained
                .into_iter()
                .map(|(address, client)| {
                    let ticket = txn.next_ticket;
                    txn.next_ticket += 1;
                    txn.retired.insert(
                        ticket,
                        Arc::new(RetiredClose::new(
                            address,
                            Some(client),
                            RetiredState::Closing,
                        )),
                    );
                    ticket
                })
                .collect();
            let retry_tickets: Vec<u64> = txn
                .retired
                .iter_mut()
                .filter(|(_, record)| record.claim_retry())
                .map(|(ticket, _)| *ticket)
                .collect();
            (drained_tickets, retry_tickets)
        };
        self.close_txn.1.notify_all();
        report.detached = drained_tickets.len();

        // Children first, then retries, all bounded by the one aggregate
        // deadline (each receives only the time remaining).
        for ticket in drained_tickets.into_iter().chain(retry_tickets) {
            if let Err(address) = close_retired_ticket(&self.close_txn, ticket, deadline) {
                report.unconfirmed.push(address);
            }
        }

        // Barriers may have started while this drain was closing entries (a
        // concurrent discard of a drained-but-live client). Do not claim
        // completion while any are active; the wait stays inside the same
        // aggregate deadline.
        {
            let (txn_lock, txn_condvar) = &*self.close_txn;
            let mut txn = txn_lock.lock();
            while txn
                .retired
                .values()
                .any(|record| record.state() == RetiredState::Closing)
            {
                let now = Instant::now();
                if now >= deadline {
                    report.error = Some(Self::drain_wait_error(false));
                    break;
                }
                let result =
                    txn_condvar.wait_for(&mut txn, (deadline - now).min(Duration::from_millis(5)));
                if result.timed_out()
                    && Instant::now() >= deadline
                    && txn
                        .retired
                        .values()
                        .any(|record| record.state() == RetiredState::Closing)
                {
                    report.error = Some(Self::drain_wait_error(false));
                    break;
                }
            }
        }

        {
            let mut state = self.state.lock();
            debug_assert_eq!(state.closing_generation, Some(generation));
            state.closing_generation = None;
            state.epoch = state.epoch.saturating_add(1);
        }
        // Finalize the generation and report every record still registered:
        // those clients are owned by this cache, not confirmed stopped, and
        // must stay visible to callers.
        {
            let mut txn = self.close_txn.0.lock();
            debug_assert_eq!(txn.active_generation, Some(generation));
            txn.active_generation = None;
            txn.retired
                .retain(|_, record| record.state() != RetiredState::Confirmed);
            for record in txn
                .retired
                .values()
                .filter(|record| record.state() != RetiredState::Preparing)
            {
                let already_reported = report
                    .unconfirmed
                    .iter()
                    .any(|address| *address == record.address);
                if !already_reported {
                    report.unconfirmed.push(record.address.clone());
                }
            }
        }
        self.close_txn.1.notify_all();
        report
    }

    fn drain_wait_error(blocked_by_drain: bool) -> String {
        if blocked_by_drain {
            "timed out waiting for a concurrent cache drain".to_string()
        } else {
            "timed out waiting for in-flight detached client closes".to_string()
        }
    }

    /// Number of active entries (for testing).
    pub fn active_count(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// Reference count for an address (for testing).
    pub fn refcount(&self, address: &str) -> usize {
        self.state
            .lock()
            .entries
            .get(address)
            .map_or(0, |e| e.references.count.load(Ordering::Acquire))
    }

    /// Check if a client exists for the address (for testing).
    pub fn has_client(&self, address: &str) -> bool {
        self.state.lock().entries.contains_key(address)
    }
}

#[cfg(test)]
impl ClientPool {
    /// Whether a drain transaction currently owns the closing window.
    pub(crate) fn is_draining_for_test(&self) -> bool {
        self.close_txn.0.lock().active_generation.is_some()
    }

    /// Deterministically wait until a drain transaction is active. The close
    /// coordinator's condvar is notified when a generation starts, so this
    /// wakes on the real transition instead of polling.
    pub(crate) fn wait_until_draining_for_test(&self, timeout: Duration) -> bool {
        let (lock, condvar) = &*self.close_txn;
        let mut txn = lock.lock();
        let deadline = Instant::now() + timeout;
        while txn.active_generation.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let result = condvar.wait_for(&mut txn, deadline - now);
            if result.timed_out() {
                return txn.active_generation.is_some();
            }
        }
        true
    }

    /// Deterministically wait until at least one retired record with a
    /// pending (Closing) close barrier is registered. Registration happens
    /// atomically with detachment and notifies this condvar, so the wake
    /// fires exactly in the window between detach and close I/O.
    pub(crate) fn wait_until_detached_close_in_flight_for_test(&self, timeout: Duration) -> bool {
        let (lock, condvar) = &*self.close_txn;
        let mut txn = lock.lock();
        let has_closing = |txn: &CloseTxnState| {
            txn.retired
                .values()
                .any(|record| record.state() == RetiredState::Closing)
        };
        let deadline = Instant::now() + timeout;
        while !has_closing(&txn) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let result = condvar.wait_for(&mut txn, deadline - now);
            if result.timed_out() {
                return has_closing(&txn);
            }
        }
        true
    }

    /// Number of registered close records; completed metadata awaiting prune
    /// is excluded. Preparing records retain only acquisition bookkeeping.
    pub(crate) fn retired_records_for_test(&self) -> usize {
        self.close_txn
            .0
            .lock()
            .retired
            .values()
            .filter(|record| record.state() != RetiredState::Confirmed)
            .count()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::IpcClient;
    use c2_local::{LocalEndpoint, LocalListener};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;
    use tokio::io::AsyncReadExt;

    use c2_server::{
        ConcurrencyMode, CrmCallback, CrmError, RequestData, ResponseMeta, RouteBuildSpec,
        SchedulerLimits, Server, ServerIpcConfig,
    };

    struct Echo;

    impl CrmCallback for Echo {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<parking_lot::RwLock<c2_mem::MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"ok".to_vec()))
        }
    }

    fn connect_budget(milliseconds: u64) -> ConnectDeadline {
        ConnectDeadline::start(
            c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(milliseconds)),
        )
        .unwrap()
    }

    #[test]
    fn connect_deadline_pool_state_wait_uses_caller_budget() {
        let pool = Arc::new(ClientPool::new(Duration::from_secs(30)));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder_pool = pool.clone();
        let holder = thread::spawn(move || {
            let _guard = holder_pool.state.lock();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_millis(250));
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let result = pool.acquire_with_deadline("malformed-no-io", None, connect_budget(25));
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        holder.join().unwrap();
        let IpcError::LocalCallRejected(error) =
            result.err().expect("deadline before endpoint lookup")
        else {
            panic!("canonical deadline required")
        };
        assert_eq!(error.details["stage"], "pool_wait");
        assert!(
            elapsed < Duration::from_millis(150),
            "unbounded pool lock wait: {elapsed:?}"
        );
        assert_eq!(pool.active_count(), 0);
        assert!(pool.memory_budget_snapshot().is_none());
    }

    #[test]
    fn connect_deadline_coordinator_wait_cannot_freeze_or_publish() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let _coordinator = pool.close_txn.0.lock();
        let started = Instant::now();
        let result = pool.acquire_with_deadline("malformed-no-io", None, connect_budget(25));
        let IpcError::LocalCallRejected(error) = result.err().unwrap() else {
            panic!("canonical deadline required")
        };
        assert_eq!(error.details["stage"], "pool_cleanup_wait");
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(pool.active_count(), 0);
        assert!(pool.memory_budget_snapshot().is_none());
    }

    #[test]
    fn connect_deadline_lease_drop_avoids_map_and_coordinator_locks() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let references = Arc::new(PoolReferences::new(1, None));
        let client = Arc::new(make_disconnected_client());
        let lease = ClientLease {
            client,
            references: references.clone(),
            counted: true,
        };
        let _state = pool.state.lock();
        let _coordinator = pool.close_txn.0.lock();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let releaser = thread::spawn(move || {
            drop(lease);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("exact-entry release must not wait on global locks");
        releaser.join().unwrap();
        assert_eq!(references.count.load(Ordering::Acquire), 0);
        assert_ne!(references.released_tick.load(Ordering::Acquire), 0);
    }

    #[test]
    fn connect_deadline_expired_fresh_client_retains_cleanup_and_recovers() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let shared = Arc::new(make_disconnected_client());
        let refs = Arc::new(PoolReferences::new(2, None));
        pool.state.lock().entries.insert(
            "ipc://unrelated-shared".into(),
            PoolEntry {
                client: shared.clone(),
                references: refs.clone(),
            },
        );
        let epoch = pool.state.lock().epoch;
        let client = Arc::new(make_disconnected_client());
        let weak = Arc::downgrade(&client);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        client.hold_writer_slot_for_test(ready_tx, release_rx);
        ready_rx.blocking_recv().unwrap();
        let work = {
            let mut coordinator = pool.close_txn.0.lock();
            pool.register_close_work(
                &mut coordinator,
                "ipc://fresh-expired",
                None,
                RetiredState::Preparing,
            )
        };
        let fresh = FreshClientGuard {
            work,
            coordinator: pool.close_txn.clone(),
            deadline: connect_budget(0),
            done: false,
        };
        fresh.attach(client.clone());
        drop(client);
        let state = pool.state.lock();
        let coordinator = pool.close_txn.0.lock();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let closer = thread::spawn(move || {
            drop(fresh);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_millis(150))
            .expect("expiry cleanup must not wait on pool metadata locks");
        closer.join().unwrap();
        assert!(
            weak.upgrade().is_some(),
            "unconfirmed close retains its client"
        );
        assert_eq!(
            coordinator.retired.values().next().unwrap().state(),
            RetiredState::Closing
        );
        drop(coordinator);
        drop(state);
        release_tx.send(()).unwrap();
        wait_for_retired_settlement(&pool);
        assert_eq!(pool.retired_records_for_test(), 0);
        assert!(
            weak.upgrade().is_none(),
            "confirmed close retires exact ownership without a pool drain"
        );
        let state = pool.state.lock();
        assert_eq!(state.epoch, epoch);
        assert_eq!(refs.count.load(Ordering::Acquire), 2);
        assert!(Arc::ptr_eq(
            &state.entries["ipc://unrelated-shared"].client,
            &shared
        ));
    }

    fn wait_for_retired_settlement(pool: &ClientPool) {
        let end = Instant::now() + Duration::from_secs(1);
        let mut coordinator = pool.close_txn.0.lock();
        loop {
            coordinator
                .retired
                .retain(|_, record| record.state() != RetiredState::Confirmed);
            if coordinator.retired.is_empty() {
                return;
            }
            assert!(
                Instant::now() < end,
                "ordinary settlement left retired native owners behind"
            );
            pool.close_txn.1.wait_until(&mut coordinator, end);
        }
    }

    #[test]
    fn connect_deadline_idle_cleanup_retried_by_acquire_and_sweep() {
        for use_acquire in [false, true] {
            let pool = ClientPool::new(Duration::from_secs(30));
            let cfg = ClientIpcConfig::default();
            pool.state.lock().domain_memory = Some(DomainMemory::from_config(&cfg));
            let epoch = pool.state.lock().epoch;
            let client = Arc::new(make_disconnected_client());
            let weak = Arc::downgrade(&client);
            pool.register_close_work(
                &mut pool.close_txn.0.lock(),
                "ipc://idle",
                Some(client),
                RetiredState::UnconfirmedIdle,
            );
            if use_acquire {
                // Invalid endpoint rejects after ordinary retired settlement,
                // without listening, changing epoch, or draining the cache.
                assert!(
                    pool.acquire_with_deadline("invalid-address", Some(&cfg), connect_budget(100))
                        .is_err()
                );
            } else {
                pool.sweep_expired();
            }
            assert!(weak.upgrade().is_none());
            assert_eq!(pool.retired_records_for_test(), 0);
            assert_eq!(pool.state.lock().epoch, epoch);
            assert_eq!(
                pool.memory_budget_snapshot().unwrap().limits,
                cfg.memory_budget_limits()
            );
        }
    }

    #[test]
    fn connect_deadline_settlement_wait_does_not_relock_global_coordinator() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let client = Arc::new(make_disconnected_client());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        client.hold_writer_slot_for_test(ready_tx, release_rx);
        ready_rx.blocking_recv().unwrap();
        let work = pool.register_close_work(
            &mut pool.close_txn.0.lock(),
            "ipc://settlement-wait",
            Some(client),
            RetiredState::UnconfirmedIdle,
        );
        schedule_retired_settlement(&pool.close_txn, &work);
        let coordinator = pool.close_txn.0.lock();
        let record = work.record.clone();
        let started = Instant::now();
        let error = record.wait_until_settled(connect_budget(100)).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(matches!(error, IpcError::LocalCallRejected(ref error)
            if error.code == c2_error::ErrorCode::CallDeadlineExceeded
                && error.details.get("stage").map(String::as_str) == Some("pool_cleanup_wait")));
        assert_eq!(work.record.state(), RetiredState::Closing);
        drop(coordinator);
        release_tx.send(()).unwrap();
        wait_for_retired_settlement(&pool);
    }

    #[test]
    fn connect_deadline_finite_settlement_preserves_unconfirmed_owner_for_maintenance() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let client = Arc::new(make_disconnected_client());
        let weak = Arc::downgrade(&client);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        client.hold_writer_slot_for_test(ready_tx, release_rx);
        ready_rx.blocking_recv().unwrap();
        let work = pool.register_close_work(
            &mut pool.close_txn.0.lock(),
            "ipc://still-blocked",
            Some(client),
            RetiredState::UnconfirmedIdle,
        );
        let started = Instant::now();
        schedule_retired_settlement(&pool.close_txn, &work);
        // A second scheduler cannot start another close for the same owner.
        schedule_retired_settlement(&pool.close_txn, &work);
        work.record
            .wait_until_settled(connect_budget(7000))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(7));
        assert_eq!(work.record.state(), RetiredState::UnconfirmedIdle);
        assert!(weak.upgrade().is_some());
        assert_eq!(pool.retired_records_for_test(), 1);
        // No automatic loop: retry is driven by ordinary maintenance after
        // the blocked native writer has actually become available.
        release_tx.send(()).unwrap();
        pool.sweep_expired();
        assert_eq!(pool.retired_records_for_test(), 0);
        assert_eq!(work.record.state(), RetiredState::Confirmed);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn connect_deadline_prewarm_accounting_settles_without_drain() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let mut cfg = ClientIpcConfig::default();
        cfg.base.pool_segment_size = 1024 * 1024;
        cfg.base.max_pool_segments = 1;
        cfg.base.max_pool_memory = 1024 * 1024;
        cfg.base.pool_prewarm_segments = 1;
        cfg.base.shm_backing_budget_bytes = 1_536_000;
        let domain = DomainMemory::from_config(&cfg);
        let budget = domain.budget.clone();
        pool.state.lock().domain_memory = Some(domain);
        pool.set_default_config(cfg.clone()).unwrap();
        let initial_epoch = pool.state.lock().epoch;
        let mem = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            pool_config_from_client_config(&cfg),
            format!("/timeout_accounting_{}", std::process::id()),
            budget.clone(),
        )));
        mem.lock().ensure_buddy_segments(1).unwrap();
        let charged = budget.snapshot().shm.used_bytes;
        assert!(charged > 1024 * 1024);
        assert!(charged * 2 > cfg.base.shm_backing_budget_bytes);
        // Zero expires before OS connect. The real prewarmed transport pool
        // is still attached to the failed attempt, exactly as after handshake.
        let (client, result) = SyncClient::connect_transport_pool_attempt_with_deadline(
            LocalEndpoint::from_address("ipc://prewarm-accounting").unwrap(),
            mem,
            cfg.clone(),
            budget.clone(),
            connect_budget(0),
        );
        assert!(matches!(result, Err(IpcError::LocalCallRejected(_))));
        let client = Arc::new(client);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        client.hold_writer_slot_for_test(ready_tx, release_rx);
        ready_rx.blocking_recv().unwrap();
        let work = pool.register_close_work(
            &mut pool.close_txn.0.lock(),
            "ipc://prewarm-accounting",
            None,
            RetiredState::Preparing,
        );
        let fresh = FreshClientGuard {
            work,
            coordinator: pool.close_txn.clone(),
            deadline: connect_budget(0),
            done: false,
        };
        fresh.attach(client.clone());
        drop(client);
        let started = Instant::now();
        drop(fresh);
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            charged,
            "busy native owner must retain its actual charge"
        );
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.refcount("ipc://prewarm-accounting"), 0);
        release_tx.send(()).unwrap();
        wait_for_retired_settlement(&pool);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
        assert_eq!(pool.state.lock().epoch, initial_epoch);
        assert_eq!(
            pool.memory_budget_snapshot().unwrap().limits,
            cfg.memory_budget_limits()
        );
        assert_eq!(pool.default_config.lock().as_ref(), Some(&cfg));
        // The same domain can fund the same prewarm again without close_all,
        // increasing its limit, or disabling prewarm.
        let mut recovered = MemPool::new_with_prefix_and_budget(
            pool_config_from_client_config(&cfg),
            format!("/timeout_accounting_recovered_{}", std::process::id()),
            budget.clone(),
        );
        recovered.ensure_buddy_segments(1).unwrap();
        assert_eq!(budget.snapshot().shm.used_bytes, charged);
        assert_eq!(budget.snapshot().shm.rejected_allocations, 0);
        drop(recovered);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
    }

    #[test]
    fn connect_deadline_old_lease_cannot_release_replacement() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let old = Arc::new(PoolReferences::new(1, None));
        let replacement = Arc::new(PoolReferences::new(1, None));
        let lease = ClientLease {
            client: Arc::new(make_disconnected_client()),
            references: old.clone(),
            counted: true,
        };
        pool.state.lock().entries.insert(
            "ipc://replacement".into(),
            PoolEntry {
                client: Arc::new(make_disconnected_client()),
                references: replacement.clone(),
            },
        );
        drop(lease);
        assert_eq!(old.count.load(Ordering::Acquire), 0);
        assert_eq!(pool.refcount("ipc://replacement"), 1);
    }

    fn unique_ipc_address(prefix: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "ipc://{}_{}_{}",
            prefix,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn expected_contract(name: &str) -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: name.to_string(),
            crm_ns: "test.pool".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
        }
    }

    async fn register_test_route(server: &Server, name: &str) {
        let expected = expected_contract(name);
        let built = server
            .build_route(
                RouteBuildSpec {
                    name: name.to_string(),
                    crm_ns: expected.crm_ns,
                    crm_name: expected.crm_name,
                    crm_ver: expected.crm_ver,
                    abi_hash: expected.abi_hash,
                    signature_hash: expected.signature_hash,
                    method_names: vec!["ping".to_string()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Echo),
            )
            .expect("test route should build");
        let reservation = server
            .reserve_route(built)
            .await
            .expect("test route should reserve");
        server
            .commit_reserved_route(reservation)
            .await
            .expect("test route should commit");
    }

    #[test]
    fn test_pool_new() {
        let pool = ClientPool::new(Duration::from_secs(30));
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_grace_period_sweep() {
        let pool = ClientPool::new(Duration::from_millis(50));

        // Manually insert a fake entry with ref_count=0 and old release time.
        {
            let client = make_disconnected_client();
            let mut state = pool.state.lock();
            state.entries.insert(
                "ipc://fake".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    references: Arc::new(PoolReferences::new(
                        0,
                        Some(Instant::now() - Duration::from_millis(200)),
                    )),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0, "expired entry should be swept");
    }

    #[test]
    fn test_pool_grace_period_not_expired() {
        let pool = ClientPool::new(Duration::from_secs(60));

        {
            let client = make_disconnected_client();
            let mut state = pool.state.lock();
            state.entries.insert(
                "ipc://recent".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    references: Arc::new(PoolReferences::new(0, Some(Instant::now()))),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(
            pool.active_count(),
            1,
            "recently released entry should survive"
        );
    }

    #[test]
    fn test_pool_refcount_tracking() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://reftest";

        // Insert a fake connected-looking entry for refcount math.
        {
            let client = make_disconnected_client();
            let mut state = pool.state.lock();
            state.entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    references: Arc::new(PoolReferences::new(2, None)),
                },
            );
        }

        assert_eq!(pool.refcount(addr), 2);

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 1);
        assert!(pool.has_client(addr));

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
        // last_release should now be set; entry still present.
        assert!(pool.has_client(addr));
    }

    #[test]
    fn test_pool_release_unknown_address() {
        // Should not panic — just print a warning.
        let pool = ClientPool::new(Duration::from_secs(30));
        pool.release("ipc://nonexistent");
    }

    #[test]
    fn test_pool_release_already_zero() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://zero";

        {
            let client = make_disconnected_client();
            let mut state = pool.state.lock();
            state.entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    references: Arc::new(PoolReferences::new(0, Some(Instant::now()))),
                },
            );
        }

        // Should not panic or underflow.
        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
    }

    #[test]
    fn discard_if_same_removes_only_the_observed_stale_client() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let address = "ipc://stale";
        let observed = Arc::new(make_disconnected_client());
        let raced_replacement = Arc::new(make_disconnected_client());

        pool.state.lock().entries.insert(
            address.to_string(),
            PoolEntry {
                client: Arc::clone(&observed),
                references: Arc::new(PoolReferences::new(1, None)),
            },
        );

        assert!(!pool.discard_if_same(address, &raced_replacement));
        assert!(pool.has_client(address));
        assert!(pool.discard_if_same(address, &observed));
        assert!(!pool.has_client(address));
    }

    #[test]
    fn release_if_same_cannot_decrement_a_racing_replacement() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let address = "ipc://replacement";
        let observed = Arc::new(make_disconnected_client());
        let replacement = Arc::new(make_disconnected_client());

        pool.state.lock().entries.insert(
            address.to_string(),
            PoolEntry {
                client: Arc::clone(&replacement),
                references: Arc::new(PoolReferences::new(1, None)),
            },
        );

        assert!(!pool.release_if_same(address, &observed));
        assert_eq!(pool.refcount(address), 1);
        assert!(pool.release_if_same(address, &replacement));
        assert_eq!(pool.refcount(address), 0);
    }

    #[test]
    fn test_pool_close_all_drains_and_reopens_under_new_epoch() {
        let pool = ClientPool::new(Duration::from_secs(30));

        {
            let c1 = make_disconnected_client();
            let c2 = make_disconnected_client();
            let mut state = pool.state.lock();
            state.entries.insert(
                "ipc://a".to_owned(),
                PoolEntry {
                    client: Arc::new(c1),
                    references: Arc::new(PoolReferences::new(1, None)),
                },
            );
            state.entries.insert(
                "ipc://b".to_owned(),
                PoolEntry {
                    client: Arc::new(c2),
                    references: Arc::new(PoolReferences::new(0, Some(Instant::now()))),
                },
            );
        }
        assert_eq!(pool.active_count(), 2);

        let report = pool.close_all(Duration::from_secs(1));
        assert_eq!(pool.active_count(), 0);
        assert_eq!(report.detached, 2);
        assert!(report.unconfirmed.is_empty(), "{report:?}");
        assert!(report.error.is_none(), "{report:?}");
        // Cache reopened: no active closing generation and epoch bumped.
        let state = pool.state.lock();
        assert_eq!(state.closing_generation, None);
        assert_eq!(state.epoch, 1);
        drop(state);
        assert!(!pool.is_draining_for_test());
    }

    #[test]
    fn acquire_rejects_while_cache_is_closing() {
        let pool = ClientPool::new(Duration::from_secs(30));
        // Enter the closing window without draining, exactly like an
        // in-progress close_all does between its fence and epoch bump.
        pool.state.lock().closing_generation = Some(0);

        let result = pool.acquire("ipc:///nonexistent_socket_path", None);
        match result {
            Err(IpcError::Pool(message)) => assert!(
                message.contains("closing"),
                "acquire must reject during the closing window, got {message}"
            ),
            Err(other) => panic!("unexpected error kind: {other}"),
            Ok(_) => panic!("acquire must reject during the closing window"),
        }

        pool.state.lock().closing_generation = None;
    }

    #[test]
    fn test_pool_acquire_no_server() {
        // acquire() should fail gracefully when no server is listening.
        let pool = ClientPool::new(Duration::from_secs(30));
        let result = pool.acquire("ipc:///nonexistent_socket_path", None);
        assert!(result.is_err(), "acquire without server should fail");
    }

    #[test]
    fn acquire_retries_transient_handshake_eof() {
        let address = format!("ipc://pool_retry_{}", std::process::id());
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    for attempt in 0..2 {
                        let mut stream = listener.accept().await.unwrap();

                        let mut len_buf = [0_u8; 4];
                        stream.read_exact(&mut len_buf).await.unwrap();
                        let body_len = u32::from_le_bytes(len_buf) as usize;
                        let mut body = vec![0_u8; body_len];
                        stream.read_exact(&mut body).await.unwrap();

                        if attempt == 0 {
                            continue;
                        }

                        let route = c2_wire::handshake::RouteInfo {
                            name: "grid".to_string(),
                            route_uid: "grid-route-uid-0001".to_string(),
                            route_revision: 1,
                            crm_ns: "test.pool".to_string(),
                            crm_name: "Grid".to_string(),
                            crm_ver: "0.1.0".to_string(),
                            abi_hash:
                                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                                    .to_string(),
                            signature_hash:
                                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                                    .to_string(),
                            max_payload_size: 1024,
                            methods: vec![c2_wire::handshake::MethodEntry {
                                name: "ping".to_string(),
                                index: 0,
                            }],
                        };
                        let identity = c2_wire::handshake::ServerIdentity {
                            server_id: "pool-retry-server".to_string(),
                            server_instance_id: "pool-retry-instance".to_string(),
                        };
                        let payload = c2_wire::handshake::encode_server_handshake(
                            &[],
                            c2_wire::handshake::CAP_CALL_V2
                                | c2_wire::handshake::CAP_METHOD_IDX
                                | c2_wire::handshake::CAP_CHUNKED,
                            &[route],
                            "",
                            &identity,
                        )
                        .unwrap();
                        let frame = c2_wire::frame::encode_frame(
                            0,
                            c2_wire::flags::FLAG_HANDSHAKE | c2_wire::flags::FLAG_RESPONSE,
                            &payload,
                        );
                        stream.write_all(&frame).await.unwrap();
                        let mut extra_len = [0_u8; 4];
                        match tokio::time::timeout(
                            Duration::from_millis(150),
                            stream.read_exact(&mut extra_len),
                        )
                        .await
                        {
                            Err(_) => {}
                            Ok(Ok(_)) => {
                                panic!("direct IPC connect must not open a route-watch stream")
                            }
                            Ok(Err(err)) => panic!("unexpected post-handshake read error: {err}"),
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("retry handshake fixture must finish");
            });
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retry listener readiness");

        let pool = ClientPool::new(Duration::from_secs(60));
        let client = pool.acquire(&address, None).unwrap();
        assert_eq!(client.route_names(), vec!["grid".to_string()]);
        pool.release(&address);

        server_thread.join().unwrap();
    }

    /// One valid server handshake reply frame for the "grid" route.
    fn grid_handshake_reply_frame() -> Vec<u8> {
        let route = c2_wire::handshake::RouteInfo {
            name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
            crm_ns: "test.pool".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            max_payload_size: 1024 * 1024 * 1024,
            methods: vec![c2_wire::handshake::MethodEntry {
                name: "ping".to_string(),
                index: 0,
            }],
        };
        let identity = c2_wire::handshake::ServerIdentity {
            server_id: "pool-race-server".to_string(),
            server_instance_id: "pool-race-instance".to_string(),
        };
        let payload = c2_wire::handshake::encode_server_handshake(
            &[],
            c2_wire::handshake::CAP_CALL_V2
                | c2_wire::handshake::CAP_METHOD_IDX
                | c2_wire::handshake::CAP_CHUNKED,
            &[route],
            "",
            &identity,
        )
        .unwrap();
        c2_wire::frame::encode_frame(
            0,
            c2_wire::flags::FLAG_HANDSHAKE | c2_wire::flags::FLAG_RESPONSE,
            &payload,
        )
    }

    #[test]
    fn concurrent_same_address_loser_is_explicitly_closed() {
        let address = format!("ipc://pool_loser_{}", std::process::id());
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
        let (observation_tx, observation_rx) = std::sync::mpsc::channel();

        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                listener_ready_tx.send(()).unwrap();

                // Accept both connections and read each handshake request
                // before answering either, so both clients finish connect()
                // only after both are already dialing: the insert race is
                // deterministic no matter which thread wins.
                let mut streams = Vec::new();
                for _ in 0..2 {
                    let mut stream = listener.accept().await.unwrap();
                    let mut len_buf = [0_u8; 4];
                    stream.read_exact(&mut len_buf).await.unwrap();
                    let body_len = u32::from_le_bytes(len_buf) as usize;
                    let mut body = vec![0_u8; body_len];
                    stream.read_exact(&mut body).await.unwrap();
                    streams.push(stream);
                }
                let reply = grid_handshake_reply_frame();
                for stream in &mut streams {
                    stream.write_all(&reply).await.unwrap();
                }

                // Watch both server-side streams. The losing client must be
                // closed explicitly: its disconnect signal or EOF arrives
                // while the pool still holds both references and the winner
                // stays silently connected.
                let mut watchers = Vec::new();
                for stream in streams {
                    let observation_tx = observation_tx.clone();
                    watchers.push(tokio::spawn(async move {
                        let mut stream = stream;
                        let mut len_buf = [0_u8; 4];
                        let observed = match tokio::time::timeout(
                            Duration::from_secs(3),
                            stream.read_exact(&mut len_buf),
                        )
                        .await
                        {
                            // Bytes arrived (disconnect signal) or the peer
                            // closed the stream: either proves an explicit
                            // close of this connection.
                            Ok(Ok(_)) | Ok(Err(_)) => true,
                            // Silence for the whole window: still connected.
                            Err(_) => false,
                        };
                        let _ = observation_tx.send(observed);
                    }));
                }
                for watcher in watchers {
                    watcher.await.unwrap();
                }
            });
        });
        listener_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("loser-race listener readiness");

        let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
        let barrier = Arc::new(Barrier::new(2));
        let mut acquire_threads = Vec::new();
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            let address = address.clone();
            acquire_threads.push(thread::spawn(move || {
                barrier.wait();
                pool.acquire(&address, None)
            }));
        }
        let first = acquire_threads
            .remove(0)
            .join()
            .expect("first acquire thread")
            .expect("first acquire should connect");
        let second = acquire_threads
            .remove(0)
            .join()
            .expect("second acquire thread")
            .expect("second acquire should connect");

        assert!(
            Arc::ptr_eq(&first, &second),
            "same-address acquires must share one client"
        );
        assert!(first.is_connected());
        assert_eq!(pool.active_count(), 1, "no duplicate entry may remain");
        assert_eq!(pool.refcount(&address), 2);

        let observations: Vec<bool> = (0..2)
            .map(|_| {
                observation_rx
                    .recv_timeout(Duration::from_secs(6))
                    .expect("stream observation")
            })
            .collect();
        assert_eq!(
            observations.iter().filter(|closed| **closed).count(),
            1,
            "exactly the losing connection must be closed, saw {observations:?}"
        );

        server_thread.join().unwrap();
    }

    #[test]
    fn close_shared_is_bounded_and_honest_when_writer_slot_is_held() {
        // The blocked-writer scenario is modeled deterministically: a test
        // task holds the writer slot exactly like a bulk write stuck on a
        // non-reading peer would, with a readiness signal instead of pipe
        // buffer pressure and no oversized stack buffers.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_close_writer_held");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            register_test_route(&server, "grid").await;
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            // All SyncClient operations below block on the shared global
            // client runtime and must run on a plain thread, not on this
            // runtime's worker.
            let client = Arc::new(
                tokio::task::spawn_blocking(move || {
                    SyncClient::connect(&address, None, ClientIpcConfig::default())
                })
                .await
                .expect("connect task")
                .expect("connect"),
            );
            assert!(client.is_connected());

            let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            client.hold_writer_slot_for_test(acquired_tx, release_rx);
            acquired_rx.await.expect("writer slot hold readiness");

            let (close_tx, close_rx) = std::sync::mpsc::channel::<(bool, Duration)>();
            let close_client = Arc::clone(&client);
            let close_thread = thread::spawn(move || {
                let started = Instant::now();
                let confirmed = close_client.close_shared(Duration::from_secs(1));
                close_tx.send((confirmed, started.elapsed()))
            });
            let (confirmed, elapsed) =
                close_rx.recv_timeout(Duration::from_secs(20)).expect("close result");
            assert!(
                elapsed < Duration::from_secs(10),
                "close barrier must be bounded even with a held writer slot, took {elapsed:?}"
            );
            assert!(
                !confirmed,
                "a close whose writer slot stays held for the whole deadline must not claim confirmation"
            );
            assert!(!client.is_connected());
            close_thread.join().unwrap().expect("close channel send");

            // Honesty and recovery: once the holder releases, a later close
            // must be able to confirm. An unconfirmed close may not poison
            // the client or block forever.
            drop(release_tx);
            let reconfirmed =
                tokio::task::spawn_blocking(move || client.close_shared(Duration::from_secs(5)))
                    .await
                    .expect("re-close task");
            assert!(
                reconfirmed,
                "close must confirm once the writer slot frees again"
            );

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn close_shared_aborts_blocked_receiver_within_deadline() {
        // Fake peer serves the real handshake, then delivers two bytes of a
        // frame length prefix and goes silent. A receive-side probe confirms
        // that this task consumed those bytes and polled the rest to Pending
        // before close starts. The close barrier must abort
        // the stalled stream, let the receive task finish within its bounded
        // join slice, and confirm — without waiting for the peer.
        let address = unique_ipc_address("pool_close_receiver_blocked");
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
        let (receiver_blocked_tx, receiver_blocked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                listener_ready_tx.send(()).unwrap();
                let mut stream = listener.accept().await.unwrap();

                let mut len_buf = [0_u8; 4];
                stream.read_exact(&mut len_buf).await.unwrap();
                let body_len = u32::from_le_bytes(len_buf) as usize;
                let mut body = vec![0_u8; body_len];
                stream.read_exact(&mut body).await.unwrap();

                stream
                    .write_all(&grid_handshake_reply_frame())
                    .await
                    .unwrap();
                // Partial next frame: two length-prefix bytes, then silence.
                stream.write_all(&[0_u8, 0]).await.unwrap();

                let _ = release_rx.recv();
                drop(stream);
            });
        });
        listener_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked-receiver listener readiness");

        let client = SyncClient::connect_with_partial_header_probe_for_test(
            &address,
            ClientIpcConfig::default(),
            receiver_blocked_tx,
            None,
        )
        .expect("connect against the real handshake");
        receiver_blocked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the receive task must consume two header bytes and poll the rest to Pending");
        let started = Instant::now();
        let confirmed = client.close_shared(Duration::from_secs(2));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "close barrier must be bounded with a blocked receiver, took {elapsed:?}"
        );
        assert!(
            confirmed,
            "aborting the stalled stream must let the receive task finish and the close confirm"
        );
        assert!(!client.is_connected());

        let _ = release_tx.send(());
        server_thread.join().unwrap();
    }

    #[test]
    fn close_all_rejects_in_flight_connect_and_reopens_under_new_epoch() {
        // A stalled handshake peer holds one pool acquire deterministically
        // mid-connect: the handshake request has been read but no reply was
        // sent. Draining the cache during that stall must fence the connect
        // out of the new epoch: when the handshake finally succeeds, the
        // fresh connection is closed explicitly and the acquire rejects. A
        // later acquire under the new epoch must succeed (restart/reacquire).
        let address = unique_ipc_address("pool_epoch_fence");
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
        let (request_read_tx, request_read_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel::<bool>();

        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                listener_ready_tx.send(()).unwrap();
                let mut stream = listener.accept().await.unwrap();

                let mut len_buf = [0_u8; 4];
                stream.read_exact(&mut len_buf).await.unwrap();
                let body_len = u32::from_le_bytes(len_buf) as usize;
                let mut body = vec![0_u8; body_len];
                stream.read_exact(&mut body).await.unwrap();
                request_read_tx.send(()).unwrap();

                // Deterministic stall: complete the handshake only on release.
                let _ = release_rx.recv();
                stream
                    .write_all(&grid_handshake_reply_frame())
                    .await
                    .unwrap();

                let mut probe = [0_u8; 1];
                let closed = match tokio::time::timeout(
                    Duration::from_secs(3),
                    stream.read_exact(&mut probe),
                )
                .await
                {
                    // Disconnect bytes arrived or the peer closed: either
                    // proves the epoch-loser connection was closed.
                    Ok(Ok(_)) | Ok(Err(_)) => true,
                    Err(_) => false,
                };
                let _ = closed_tx.send(closed);
            });
        });
        listener_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("epoch-fence listener readiness");

        let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
        let acquire_pool = Arc::clone(&pool);
        let acquire_address = address.clone();
        let acquire_thread = thread::spawn(move || acquire_pool.acquire(&acquire_address, None));
        request_read_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("handshake request observed while the connect is stalled");

        // Drain while the connect is in flight. The cache is empty; the drain
        // exists to fence the in-flight connect out of the new epoch.
        let report = pool.close_all(Duration::from_secs(1));
        assert_eq!(report.detached, 0);
        assert!(report.unconfirmed.is_empty(), "{report:?}");

        // Resume the handshake. The transport connect now succeeds, but the
        // epoch fence must reject the insert and close the connection.
        release_tx.send(()).unwrap();
        let rejected = match acquire_thread.join().unwrap() {
            Err(IpcError::Pool(message)) => message,
            Err(other) => panic!("unexpected error kind: {other:?}"),
            Ok(_) => panic!("an acquire that straddled a drain must reject"),
        };
        assert!(
            rejected.contains("closed while connecting"),
            "epoch fence must reject the straddled acquire, got {rejected}"
        );
        assert!(
            closed_rx
                .recv_timeout(Duration::from_secs(6))
                .expect("post-drain connection observation"),
            "the post-drain connection must be closed explicitly"
        );
        server_thread.join().unwrap();

        // Reopen: a fresh acquire under the new epoch succeeds against a
        // serving peer, and a second drain closes it.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let reopen_address = unique_ipc_address("pool_epoch_reopen");
            let server =
                Arc::new(Server::new(&reopen_address, ServerIpcConfig::default()).unwrap());
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("reopen server should be responsive");

            // The pool's blocking acquire/drain must run off this runtime's
            // workers; hop to a blocking thread like production SDK glue.
            let reopen_pool = Arc::clone(&pool);
            let (client, report) = tokio::task::spawn_blocking(move || {
                let client = reopen_pool
                    .acquire(&reopen_address, None)
                    .expect("acquire must work under the new epoch");
                let report = reopen_pool.close_all(Duration::from_secs(2));
                (client, report)
            })
            .await
            .expect("reopen acquire task");
            assert_eq!(report.detached, 1);
            assert!(report.unconfirmed.is_empty(), "{report:?}");
            assert!(!client.is_connected());

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("reopen server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn concurrent_close_all_drains_serialize_and_acquires_reject_during_blocked_drain() {
        // Two close_all callers race while the first drain's only entry is a
        // client whose writer slot is held. The second caller must neither
        // clear the fence nor bump the epoch under the active drain: it times
        // out against its own deadline with an honest error, acquires keep
        // rejecting, and after the first drain completes (guard released) the
        // cache reopens under the new epoch.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_serial_drains");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let client = {
                let pool = Arc::clone(&pool);
                let address = address.clone();
                tokio::task::spawn_blocking(move || pool.acquire(&address, None))
                    .await
                    .expect("acquire task")
                    .expect("acquire")
            };
            let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            client.hold_writer_slot_for_test(acquired_tx, release_rx);
            acquired_rx.await.expect("writer slot hold readiness");

            // First drain: blocks on the held writer slot.
            let drain_pool = Arc::clone(&pool);
            let drain_thread = thread::spawn(move || {
                let started = Instant::now();
                let report = drain_pool.close_all(Duration::from_secs(3));
                (report, started.elapsed())
            });
            assert!(
                pool.wait_until_draining_for_test(Duration::from_secs(5)),
                "first drain must enter its closing generation"
            );

            // Acquisitions during the active drain reject. (Blocking pool
            // operations run off this runtime's workers.)
            let probe_pool = Arc::clone(&pool);
            let probe_address = address.clone();
            let probe_result =
                tokio::task::spawn_blocking(move || probe_pool.acquire(&probe_address, None))
                    .await
                    .expect("acquire probe task");
            match probe_result {
                Err(IpcError::Pool(message)) => assert!(
                    message.contains("closing"),
                    "acquire must reject during the active drain, got {message}"
                ),
                Err(other) => panic!("acquire during active drain must reject: {other:?}"),
                Ok(_) => panic!("acquire during active drain must reject"),
            }

            // Second caller: times out waiting for the serialized drain and
            // reports an error instead of claiming a drain or clearing the
            // fence.
            let second_pool = Arc::clone(&pool);
            let second = tokio::task::spawn_blocking(move || {
                second_pool.close_all(Duration::from_millis(300))
            })
            .await
            .expect("second drain task");
            assert_eq!(second.detached, 0, "{second:?}");
            assert!(
                second
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("concurrent cache drain")),
                "second caller must report the serialized-drain timeout: {second:?}"
            );
            assert!(
                pool.is_draining_for_test(),
                "the timed-out caller must not clear the active drain fence"
            );

            // Release the writer: the first drain confirms and reopens the
            // cache under a new epoch.
            drop(release_tx);
            let (first_report, first_elapsed) = drain_thread.join().expect("first drain thread");
            assert_eq!(first_report.detached, 1, "{first_report:?}");
            assert!(first_report.unconfirmed.is_empty(), "{first_report:?}");
            assert!(first_report.error.is_none(), "{first_report:?}");
            assert!(
                first_elapsed < Duration::from_secs(5),
                "first drain must stay bounded, took {first_elapsed:?}"
            );
            assert!(!pool.is_draining_for_test());
            assert_eq!(pool.state.lock().epoch, 1);

            // Restart semantics: a fresh acquire under the new epoch works.
            let reopen_pool = Arc::clone(&pool);
            let reopen_address = address.clone();
            let reopened =
                tokio::task::spawn_blocking(move || reopen_pool.acquire(&reopen_address, None))
                    .await
                    .expect("reopen acquire task")
                    .expect("acquire after drain");
            assert!(reopened.is_connected());
            let final_pool = Arc::clone(&pool);
            let final_report =
                tokio::task::spawn_blocking(move || final_pool.close_all(Duration::from_secs(2)))
                    .await
                    .expect("final drain task");
            assert!(final_report.error.is_none(), "{final_report:?}");
            assert!(final_report.unconfirmed.is_empty(), "{final_report:?}");

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn two_concurrent_closes_of_one_blocked_receiver_serialize_and_stay_retryable() {
        // The receive task is proven blocked mid-frame. Its cancellation Drop
        // then parks until this test releases it. Both concurrent close
        // barriers must stay bounded and unconfirmed while that exact task
        // remains nonterminal; a later close joins its restored handle.
        let address = unique_ipc_address("pool_double_close_receiver");
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
        let (receiver_blocked_tx, receiver_blocked_rx) = std::sync::mpsc::channel();
        let (receiver_drop_entered_tx, receiver_drop_entered_rx) = std::sync::mpsc::channel();
        let (release_receiver_drop_tx, release_receiver_drop_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                listener_ready_tx.send(()).unwrap();
                let mut stream = listener.accept().await.unwrap();
                let mut len_buf = [0_u8; 4];
                stream.read_exact(&mut len_buf).await.unwrap();
                let body_len = u32::from_le_bytes(len_buf) as usize;
                let mut body = vec![0_u8; body_len];
                stream.read_exact(&mut body).await.unwrap();
                stream
                    .write_all(&grid_handshake_reply_frame())
                    .await
                    .unwrap();
                stream.write_all(&[0_u8, 0]).await.unwrap();
                let _ = release_rx.recv();
                drop(stream);
            });
        });
        listener_ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("double-close listener readiness");

        let client = Arc::new(
            SyncClient::connect_with_partial_header_probe_for_test(
                &address,
                ClientIpcConfig::default(),
                receiver_blocked_tx,
                Some(crate::client::ReceiverDropGateForTest {
                    entered: receiver_drop_entered_tx,
                    release: release_receiver_drop_rx,
                }),
            )
            .expect("connect against the real handshake"),
        );
        receiver_blocked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("this receive task must poll the rest of the partial header to Pending");

        let barrier = Arc::new(Barrier::new(2));
        let (result_tx, result_rx) = std::sync::mpsc::channel::<(bool, Duration)>();
        let mut close_threads = Vec::new();
        for _ in 0..2 {
            let client = Arc::clone(&client);
            let barrier = Arc::clone(&barrier);
            let result_tx = result_tx.clone();
            close_threads.push(thread::spawn(move || {
                barrier.wait();
                let started = Instant::now();
                let confirmed = client.close_shared(Duration::from_secs(2));
                let _ = result_tx.send((confirmed, started.elapsed()));
            }));
        }
        drop(result_tx);
        receiver_drop_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the blocked receiver must enter its cancellation Drop gate");
        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(
                result_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("both concurrent closes must return"),
            );
        }
        for (_, elapsed) in &results {
            assert!(
                elapsed < &Duration::from_secs(8),
                "each concurrent close must stay bounded, took {elapsed:?}"
            );
        }
        assert!(
            results.iter().all(|(confirmed, _)| !confirmed),
            "neither close may confirm while the receiver's cancellation Drop is gated: {results:?}"
        );
        for thread in close_threads {
            thread.join().expect("close thread");
        }

        // Retryability: the aborted receive task is still observable through
        // its restored handle; a subsequent close joins it and confirms.
        release_receiver_drop_tx
            .send(())
            .expect("release the receiver cancellation gate");
        let retry = client.close_shared(Duration::from_secs(2));
        assert!(
            retry,
            "a later close must confirm after the serialized barriers"
        );
        assert!(!client.is_connected());

        let _ = release_tx.send(());
        server_thread.join().unwrap();
    }

    #[test]
    fn close_all_does_not_claim_drained_while_discard_barrier_is_in_flight() {
        // A discard closes a detached client outside the pool lock; while
        // that bounded barrier is still active (writer slot held), close_all
        // must withhold any drained claim with an error instead of silently
        // reporting success.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_discard_in_flight");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let client = {
                let pool = Arc::clone(&pool);
                let address = address.clone();
                tokio::task::spawn_blocking(move || pool.acquire(&address, None))
                    .await
                    .expect("acquire task")
                    .expect("acquire")
            };
            let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            client.hold_writer_slot_for_test(acquired_tx, release_rx);
            acquired_rx.await.expect("writer slot hold readiness");

            // Discard on another thread: detaches the entry, then blocks in
            // its accounted close barrier on the held writer slot.
            let discard_pool = Arc::clone(&pool);
            let discard_address = address.clone();
            let discard_client = Arc::clone(&client);
            let discard_thread = thread::spawn(move || {
                discard_pool.discard_if_same(&discard_address, &discard_client)
            });
            assert!(
                pool.wait_until_detached_close_in_flight_for_test(Duration::from_secs(5)),
                "discard must register its pending close before any close I/O"
            );

            // This moment is exactly the reviewed detach→I/O gap: the entry
            // is detached and the record registered, while the barrier has
            // not made progress (its writer slot is held). close_all must
            // not be able to claim a false success here.
            let report = pool.close_all(Duration::from_millis(300));
            assert_eq!(report.detached, 0, "{report:?}");
            assert!(
                report
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("in-flight detached client closes")),
                "close_all must not claim drained while a discard barrier is active: {report:?}"
            );
            assert!(
                !pool.is_draining_for_test(),
                "the withheld close_all must not start a drain generation"
            );

            // Let the discard barrier finish; a later close_all is clean.
            drop(release_tx);
            assert!(
                discard_thread.join().expect("discard thread"),
                "discard must complete once the writer slot frees"
            );
            let clean_pool = Arc::clone(&pool);
            let clean =
                tokio::task::spawn_blocking(move || clean_pool.close_all(Duration::from_secs(2)))
                    .await
                    .expect("clean drain task");
            assert_eq!(clean.detached, 0, "{clean:?}");
            assert!(clean.error.is_none(), "{clean:?}");
            assert!(clean.unconfirmed.is_empty(), "{clean:?}");
            assert_eq!(pool.retired_records_for_test(), 0);

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn prior_unconfirmed_drained_client_is_retried_and_reported_by_later_close_all() {
        // A drain whose child close cannot confirm (held writer slot) must
        // keep the client's retired record registered and report it
        // unconfirmed; a later close_all retries that record and only clears
        // it after a confirmed close. Unconfirmed ownership never disappears
        // silently.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_retry_unconfirmed");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let client = {
                let pool = Arc::clone(&pool);
                let address = address.clone();
                tokio::task::spawn_blocking(move || pool.acquire(&address, None))
                    .await
                    .expect("acquire task")
                    .expect("acquire")
            };
            let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            client.hold_writer_slot_for_test(acquired_tx, release_rx);
            acquired_rx.await.expect("writer slot hold readiness");

            // First drain: the child close cannot confirm within 200 ms.
            let first_pool = Arc::clone(&pool);
            let first = tokio::task::spawn_blocking(move || {
                first_pool.close_all(Duration::from_millis(200))
            })
            .await
            .expect("first drain task");
            assert_eq!(first.detached, 1, "{first:?}");
            assert!(first.error.is_none(), "{first:?}");
            assert_eq!(first.unconfirmed, vec![address.clone()], "{first:?}");
            assert_eq!(
                pool.retired_records_for_test(),
                1,
                "the unconfirmed client must stay registered for retry"
            );
            assert!(!pool.is_draining_for_test());

            // Free the writer, then drain again: the retry pass owns the
            // unconfirmed record and confirms its close.
            drop(release_tx);
            let second_pool = Arc::clone(&pool);
            let second =
                tokio::task::spawn_blocking(move || second_pool.close_all(Duration::from_secs(2)))
                    .await
                    .expect("second drain task");
            assert_eq!(second.detached, 0, "{second:?}");
            assert!(second.error.is_none(), "{second:?}");
            assert!(
                second.unconfirmed.is_empty(),
                "the retried record must confirm and be removed: {second:?}"
            );
            assert_eq!(pool.retired_records_for_test(), 0);

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn close_all_deadline_is_aggregate_across_blocked_entries() {
        // Three cached clients, each with a held writer slot so every child
        // close barrier would block for the full timeout if given one. All
        // children share ONE absolute drain deadline: total wall time stays
        // near a single timeout (independent of N) and every blocked entry
        // is reported honestly as unconfirmed.
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_aggregate_deadline");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let mut release_guards = Vec::new();
            let mut entries: Vec<(String, Arc<SyncClient>)> = Vec::new();
            for index in 0..3 {
                let connect_address = address.clone();
                let client =
                    tokio::task::spawn_blocking(move || {
                        SyncClient::connect(&connect_address, None, ClientIpcConfig::default())
                    })
                    .await
                    .expect("connect task")
                    .expect("connect");
                let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
                client.hold_writer_slot_for_test(acquired_tx, release_rx);
                acquired_rx.await.expect("writer slot hold readiness");
                release_guards.push(release_tx);
                entries.push((format!("ipc://aggregate-{index}"), Arc::new(client)));
            }
            {
                let mut state = pool.state.lock();
                for (key, client) in entries {
                    state.entries.insert(
                        key,
                        PoolEntry {
                            client,
                            references: Arc::new(PoolReferences::new(1, None)),
                        },
                    );
                }
            }

            let drain_pool = Arc::clone(&pool);
            let (report_tx, report_rx) = std::sync::mpsc::channel::<(ClientCacheCloseReport, Duration)>();
            let drain_thread = thread::spawn(move || {
                let started = Instant::now();
                let report = drain_pool.close_all(Duration::from_secs(1));
                let _ = report_tx.send((report, started.elapsed()));
            });
            let (report, elapsed) = report_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("aggregate drain must return");
            drain_thread.join().unwrap();

            assert_eq!(report.detached, 3, "{report:?}");
            assert_eq!(report.unconfirmed.len(), 3, "{report:?}");
            assert!(report.error.is_none(), "{report:?}");
            assert!(
                elapsed < Duration::from_secs(2),
                "one aggregate deadline must bound all children (got {elapsed:?} for 3 blocked entries)"
            );
            assert_eq!(pool.active_count(), 0);
            assert!(!pool.is_draining_for_test());

            // Release the held slots; nothing remains pinned.
            drop(release_guards);
            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn pooled_direct_client_observes_route_registered_after_handshake() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("pool_live_route_refresh");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            register_test_route(&server, "manager").await;
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let pool = Arc::new(ClientPool::new(Duration::from_secs(60)));
            let acquire_pool = Arc::clone(&pool);
            let acquire_address = address.clone();
            let client =
                tokio::task::spawn_blocking(move || acquire_pool.acquire(&acquire_address, None))
                    .await
                    .expect("acquire task should complete")
                    .expect("manager client");
            assert!(client.route_names().contains(&"manager".to_string()));
            assert!(!client.route_names().contains(&"builder".to_string()));

            register_test_route(&server, "builder").await;
            let builder_contract = expected_contract("builder");

            let ensure_client = Arc::clone(&client);
            let ensure_contract = builder_contract.clone();
            let binding =
                tokio::task::spawn_blocking(move || ensure_client.acquire_route(&ensure_contract))
                    .await
                    .expect("ensure task should complete")
                    .expect("direct pooled IPC client should acquire builder through route lookup");
            assert_eq!(binding.route_name(), "builder");

            assert!(
                client.route_names().contains(&"builder".to_string()),
                "route lookup should cache the acquired builder route without watch"
            );
            pool.release(&address);

            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn registration_attestation_accepts_committed_closed_route_without_business_acquire() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let address = unique_ipc_address("registration_closed_route");
            let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
            let expected = expected_contract("grid");
            let built = server
                .build_route(
                    RouteBuildSpec {
                        name: "grid".to_string(),
                        crm_ns: expected.crm_ns.clone(),
                        crm_name: expected.crm_name.clone(),
                        crm_ver: expected.crm_ver.clone(),
                        abi_hash: expected.abi_hash.clone(),
                        signature_hash: expected.signature_hash.clone(),
                        method_names: vec!["ping".to_string()],
                        access_map: HashMap::new(),
                        concurrency_mode: ConcurrencyMode::ReadParallel,
                        limits: SchedulerLimits::default(),
                    },
                    Arc::new(Echo),
                )
                .expect("test route should build");
            let reservation = server.reserve_route(built).await.expect("reserve route");
            let admission = server
                .commit_reserved_route_closed(reservation)
                .await
                .expect("closed registration commit");
            let runner = {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.run().await })
            };
            server
                .wait_until_responsive(Duration::from_secs(2))
                .await
                .expect("server should be responsive");

            let mut client = IpcClient::with_config(&address, ClientIpcConfig::default());
            client.connect().await.expect("client connects");
            assert!(
                matches!(
                    client.acquire_route(&expected).await,
                    Err(IpcError::RouteClosed { .. })
                ),
                "business acquire must reject closed registration routes"
            );
            let binding = client
                .attest_route_for_registration(&expected)
                .await
                .expect("registration attestation accepts committed closed route");
            assert_eq!(binding.route_name(), "grid");

            server
                .open_route_admission(admission)
                .await
                .expect("route admission opens for cleanup");
            client.close().await;
            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn test_pool_set_default_config() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let cfg = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                chunk_size: 65536,
                ..c2_config::BaseIpcConfig::default()
            },
            shm_threshold: 1024,
            ..ClientIpcConfig::default()
        };
        pool.set_default_config(cfg)
            .expect("unfrozen pool accepts a default config");
        // Verify the config is stored (indirectly — acquire would use it).
        let stored = pool.default_config.lock();
        assert!(stored.is_some());
        let c = stored.as_ref().unwrap();
        assert_eq!(c.shm_threshold, 1024);
        assert_eq!(c.chunk_size, 65536);
    }

    #[test]
    fn pool_rejects_default_config_with_divergent_budget_after_freeze() {
        let pool = ClientPool::new(Duration::from_secs(30));
        // Freeze the domain from a config with a distinctive budget cell.
        let frozen = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                shm_backing_budget_bytes: 1,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };
        let snapshot = pool.memory_budget_snapshot();
        assert!(snapshot.is_none(), "snapshot must not create the context");
        let _ = pool.acquire("ipc:///nonexistent_memory_context_freeze", Some(&frozen));
        let snapshot = pool
            .memory_budget_snapshot()
            .expect("first attempt freezes the domain");
        assert_eq!(snapshot.limits.shm_backing_budget_bytes, 1);

        let divergent = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                shm_backing_budget_bytes: 2,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };
        let error = pool
            .set_default_config(divergent)
            .expect_err("divergent default must be rejected after freeze");
        assert!(
            error.to_string().contains("memory budget frozen"),
            "{error}"
        );
        // The frozen domain budget is unchanged and still observable.
        let after = pool.memory_budget_snapshot().unwrap();
        assert_eq!(after.limits.shm_backing_budget_bytes, 1);
    }

    /// The cache's frozen domain observer must survive a full drain: closing
    /// every client never uninstalls or resets the domain, so retained charges
    /// stay observable after shutdown.
    #[test]
    fn client_cache_budget_observer_reports_frozen_domain_across_close_all() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let config = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                shm_backing_budget_bytes: 4096,
                file_backing_budget_bytes: 2048,
                live_reassembly_budget_bytes: 1024,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };
        assert!(
            pool.memory_budget_observer().is_none(),
            "observing must not create or freeze a domain"
        );
        // A failed first attempt still freezes the domain atomically.
        let _ = pool.acquire("ipc:///nonexistent_cache_budget_observer", Some(&config));
        let observer = pool
            .memory_budget_observer()
            .expect("the first attempt freezes the domain");
        assert_eq!(*observer.limits(), config.memory_budget_limits());
        assert_eq!(observer.used_bytes(), 0);

        let report = pool.close_all(Duration::from_secs(1));
        assert!(report.error.is_none(), "{report:?}");

        let after = pool
            .memory_budget_observer()
            .expect("draining clients must not uninstall the frozen domain");
        assert_eq!(*after.limits(), config.memory_budget_limits());
        assert_eq!(after.snapshot(), observer.snapshot());
    }

    /// A same-address cache hit must match the complete resolved client
    /// policy, not only the shared budget limits: buddy policy, prewarm,
    /// chunking, threshold, decay, and reassembly geometry all change how the
    /// cached connection transfers data.
    #[test]
    fn same_address_hit_requires_the_complete_resolved_config() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let address = unique_ipc_address("same_address_full_config");
        let (server, runner) = {
            let address = address.clone();
            rt.block_on(async move {
                let server = Arc::new(Server::new(&address, ServerIpcConfig::default()).unwrap());
                let runner = {
                    let server = Arc::clone(&server);
                    tokio::spawn(async move { server.run().await })
                };
                server
                    .wait_until_responsive(Duration::from_secs(2))
                    .await
                    .expect("server should be responsive");
                (server, runner)
            })
        };

        // Pool acquires connect through the global sync client runtime, so
        // they must run outside this test's `block_on` context.
        let pool = ClientPool::new(Duration::from_secs(60));
        let base = c2_config::BaseIpcConfig {
            pool_segment_size: 65_536,
            max_pool_segments: 1,
            max_pool_memory: 65_536,
            ..c2_config::BaseIpcConfig::default()
        };
        let canonical = ClientIpcConfig {
            base: base.clone(),
            shm_threshold: 1,
            pool_decay_seconds: 30.0,
        };

        let first = pool
            .acquire(&address, Some(&canonical))
            .expect("first acquire connects");
        let reused = pool
            .acquire(&address, Some(&canonical))
            .expect("an identical resolved config reuses the cached connection");
        assert!(
            Arc::ptr_eq(&first, &reused),
            "identical resolved config must reuse the same client"
        );

        // Every variant keeps the budget limits identical to `canonical`
        // and changes exactly one resolved policy field.
        let variants: Vec<(&str, ClientIpcConfig)> = vec![
            (
                "chunk_size",
                ClientIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        chunk_size: 65_536,
                        ..base.clone()
                    },
                    ..canonical.clone()
                },
            ),
            (
                "shm_threshold",
                ClientIpcConfig {
                    shm_threshold: 4_096,
                    ..canonical.clone()
                },
            ),
            (
                "pool_enabled",
                ClientIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        pool_enabled: false,
                        ..base.clone()
                    },
                    ..canonical.clone()
                },
            ),
            (
                "pool_prewarm_segments",
                ClientIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        pool_prewarm_segments: 1,
                        ..base.clone()
                    },
                    ..canonical.clone()
                },
            ),
            (
                "chunk_threshold_ratio",
                ClientIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        chunk_threshold_ratio: 0.5,
                        ..base.clone()
                    },
                    ..canonical.clone()
                },
            ),
            (
                "reassembly_segment_size",
                ClientIpcConfig {
                    base: c2_config::BaseIpcConfig {
                        reassembly_segment_size: 32 * 1024 * 1024,
                        ..base.clone()
                    },
                    ..canonical.clone()
                },
            ),
            (
                "pool_decay_seconds",
                ClientIpcConfig {
                    pool_decay_seconds: 5.0,
                    ..canonical.clone()
                },
            ),
        ];

        for (field, variant) in variants {
            assert_eq!(
                variant.memory_budget_limits(),
                canonical.memory_budget_limits(),
                "fixture for {field} must keep budget identity equal"
            );
            let error = pool
                .acquire(&address, Some(&variant))
                .err()
                .expect("a divergent resolved policy must be rejected on a cache hit");
            let message = error.to_string();
            assert!(
                message.contains("different resolved client policy"),
                "{field}: {message}"
            );
            assert!(message.contains(field), "{field}: {message}");
            // The rejection must not evict or alter the live entry.
            let still_cached = pool
                .acquire(&address, Some(&canonical))
                .expect("the canonical policy still serves the cache");
            assert!(
                Arc::ptr_eq(&first, &still_cached),
                "{field}: a rejected policy must not disturb the cached entry"
            );
        }

        // Rejected acquires leave exactly the original entry in place.
        assert_eq!(pool.active_count(), 1);
        let report = pool.close_all(Duration::from_secs(2));
        assert!(report.error.is_none(), "{report:?}");

        rt.block_on(async move {
            server
                .shutdown_and_wait(Duration::from_secs(2))
                .await
                .expect("server should shut down");
            runner.await.unwrap().unwrap();
        });
    }

    #[test]
    fn pool_config_from_client_config_uses_max_pool_segments() {
        let cfg = ClientIpcConfig {
            base: c2_config::BaseIpcConfig {
                pool_segment_size: 65_536,
                max_pool_segments: 3,
                ..c2_config::BaseIpcConfig::default()
            },
            shm_threshold: 1024,
            ..ClientIpcConfig::default()
        };

        let pc = pool_config_from_client_config(&cfg);

        assert_eq!(pc.segment_size, 65_536);
        assert_eq!(pc.max_segments, 3);
    }

    #[test]
    fn pool_config_from_client_config_projects_client_decay() {
        let cfg = ClientIpcConfig {
            pool_decay_seconds: 3.25,
            ..ClientIpcConfig::default()
        };
        assert_eq!(
            pool_config_from_client_config(&cfg).buddy_idle_decay_secs,
            3.25
        );
    }

    // ── Helper ───────────────────────────────────────────────────────────

    #[test]
    fn test_client_pool_unique_prefixes() {
        // Verify that successive client MemPool creations get different
        // SHM prefixes via CLIENT_POOL_GEN counter.
        let pc = PoolConfig::default();
        let c1 = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let p1 = format!("/cc3c{:08x}{:08x}", std::process::id(), c1);
        let c2 = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let p2 = format!("/cc3c{:08x}{:08x}", std::process::id(), c2);
        assert_ne!(p1, p2, "consecutive prefixes must differ");
        // Verify the pools can be created with these prefixes.
        let pool1 = MemPool::new_with_prefix(pc.clone(), p1.clone());
        let pool2 = MemPool::new_with_prefix(pc.clone(), p2.clone());
        let repeated_label = MemPool::new_with_prefix(pc, p1.clone());
        assert!(pool1.prefix().starts_with(&format!("{p1}_")));
        assert!(pool2.prefix().starts_with(&format!("{p2}_")));
        assert_ne!(pool1.prefix(), repeated_label.prefix());
        assert_ne!(pool1.prefix(), pool2.prefix());
        assert!(pool1.prefix().len() <= c2_contract::MAX_WIRE_TEXT_BYTES);
    }

    /// Create a disconnected SyncClient for testing pool bookkeeping.
    fn make_disconnected_client() -> SyncClient {
        SyncClient::new_unconnected("ipc:///pool_test_fake")
    }
}
