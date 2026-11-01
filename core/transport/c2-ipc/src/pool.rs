//! Cache of business references. Each native connection independently owns
//! its writer, loans and deactivation; cache eviction only stops admission.
use crate::client::{ClientIpcConfig, IpcError};
use crate::sync_client::SyncClient;
use c2_config::{ConnectDeadline, LocalEndpointContext, MemoryBudgetLimits};
use c2_local::ConnectionObservation;
use c2_mem::{MemPool, PoolConfig};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);
pub(crate) fn pool_config_from_client_config(cfg: &ClientIpcConfig) -> PoolConfig {
    // Centralized projection: the pooled-client acquire path must follow the
    // same buddy policy and client idle window as `with_config`. A disabled
    // buddy pool is still a real `MemPool` — it keeps serving dedicated SHM
    // requests and the wire prefix, only the buddy tiers are policy-disabled.
    cfg.base.primary_pool_config(&cfg.pool_tuning())
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
        ($cached:expr, $requested:expr; $($field:ident),+ $(,)?) => {
            $(if $cached.$field != $requested.$field {
                differing.push(stringify!($field));
            })+
        };
    }
    compare!(cached.base, requested.base;
        pool_enabled,
        pool_segment_size,
        max_pool_segments,
        pool_prewarm_segments,
        pool_min_retained_segments,
        reassembly_segment_size,
        reassembly_max_segments,
        max_total_chunks,
        chunk_gc_interval_secs,
        chunk_threshold_ratio,
        chunk_assembler_timeout_secs,
        max_reassembly_bytes,
        chunk_size,
        shm_backing_budget_bytes,
        file_backing_budget_bytes,
        live_reassembly_budget_bytes,
    );
    compare!(cached, requested; shm_threshold, pool_decay_seconds);
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
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

struct PoolEntry {
    client: Arc<SyncClient>,
    references: Arc<PoolReferences>,
}

// No retained client or settlement worker: this record observes the original
// native driver. Preparing records fence connects across cache drains/seal.
enum CloseState {
    Preparing,
    Closing(Option<ConnectionObservation>),
    Finished,
}
struct CloseRecord {
    address: String,
    state: Mutex<CloseState>,
    changed: Condvar,
}
impl CloseRecord {
    fn preparing(address: &str) -> Self {
        Self {
            address: address.into(),
            state: Mutex::new(CloseState::Preparing),
            changed: Condvar::new(),
        }
    }
    fn finish(&self) {
        *self.state.lock() = CloseState::Finished;
        self.changed.notify_all();
    }
    fn close(&self, client: &SyncClient) {
        // Install the observation before requesting deactivation. Native tasks
        // retain the stream and loan book independently of this business Arc.
        *self.state.lock() = CloseState::Closing(client.connection_observation());
        client.deactivate();
        self.changed.notify_all();
    }
    fn complete(&self) -> bool {
        match &*self.state.lock() {
            CloseState::Preparing => false,
            CloseState::Closing(Some(observation)) => observation.snapshot().is_some(),
            CloseState::Closing(None) | CloseState::Finished => true,
        }
    }
    fn wait(&self, deadline: Instant, preparing: bool) -> bool {
        let mut state = self.state.lock();
        while matches!(*state, CloseState::Preparing) {
            if !preparing {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            self.changed.wait_until(&mut state, deadline);
        }
        let observation = match &*state {
            CloseState::Closing(Some(observation)) => observation.clone(),
            _ => return true,
        };
        drop(state);
        if observation.snapshot().is_some() {
            return true;
        }
        let Ok(handle) = crate::sync_client::shared_io_handle(
            ConnectDeadline::start(
                c2_config::ConnectOptions::new()
                    .with_timeout(deadline.saturating_duration_since(Instant::now())),
            )
            .expect("bounded close deadline"),
        ) else {
            return false;
        };
        handle
            .block_on(
                observation.wait_closed(Some(deadline.saturating_duration_since(Instant::now()))),
            )
            .is_some()
    }
    fn abnormal(&self) -> Option<IpcClientCloseDetail> {
        let state = self.state.lock();
        let CloseState::Closing(Some(observation)) = &*state else {
            return None;
        };
        let closed = observation.snapshot()?;
        (closed.reason() != c2_local::ConnectionEndReason::LocalClose).then(|| {
            IpcClientCloseDetail {
                address: self.address.clone(),
                reason: closed.operation().into(),
            }
        })
    }
}

struct FreshClient {
    record: Arc<CloseRecord>,
    client: Option<Arc<SyncClient>>,
    published: bool,
}
impl Drop for FreshClient {
    fn drop(&mut self) {
        if self.published {
            self.record.finish();
        } else if let Some(client) = self.client.take() {
            self.record.close(&client);
        } else {
            self.record.finish();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcClientCloseDetail {
    pub address: String,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCacheCloseReport {
    pub detached: usize,
    pub unconfirmed: Vec<String>,
    pub error: Option<String>,
    pub abnormal: Vec<IpcClientCloseDetail>,
}

struct CacheState {
    entries: HashMap<String, PoolEntry>,
    epoch: Option<u64>,
    draining: bool,
    budget: Option<c2_mem::MemoryBudget>,
    records: HashMap<u64, Arc<CloseRecord>>,
    next_record: u64,
}
impl CacheState {
    fn record(&mut self, address: &str) -> Arc<CloseRecord> {
        let record = Arc::new(CloseRecord::preparing(address));
        let id = self.next_record;
        self.next_record = id
            .checked_add(1)
            .expect("cache close record sequence exhausted");
        self.records.insert(id, record.clone());
        record
    }
    fn check_epoch(&self, epoch: u64) -> Result<(), IpcError> {
        if self.epoch.is_none() {
            return Err(IpcError::PoolRetired);
        }
        if self.draining || self.epoch != Some(epoch) {
            return Err(IpcError::Pool("client pool closed while connecting".into()));
        }
        Ok(())
    }
}

type ObservedCloses = HashMap<u64, Option<Arc<CloseRecord>>>;

pub struct ClientPool {
    endpoint_context: Result<LocalEndpointContext, String>,
    connection_event_source: Option<c2_local::ConnectionEventSource>,
    state: Arc<Mutex<CacheState>>,
    grace_period: Duration,
    default_config: Mutex<Option<ClientIpcConfig>>,
    drain_gate: Mutex<()>,
}

#[doc(hidden)]
pub struct ClientCloseObservation<'a> {
    pool: &'a ClientPool,
    observed: ObservedCloses,
}
#[doc(hidden)]
pub struct ClientCloseSeed {
    state: std::sync::Weak<Mutex<CacheState>>,
    observed: ObservedCloses,
}
#[doc(hidden)]
#[must_use]
pub struct ClientPoolSeal<'a> {
    state: parking_lot::MutexGuard<'a, CacheState>,
    source: &'a Arc<Mutex<CacheState>>,
}
impl ClientPoolSeal<'_> {
    pub fn commit(mut self) -> ClientCloseSeed {
        let observed = self
            .state
            .records
            .iter()
            .filter(|(_, record)| !record.complete())
            .map(|(id, record)| (*id, Some(record.clone())))
            .collect();
        self.state.epoch = None;
        ClientCloseSeed {
            state: Arc::downgrade(self.source),
            observed,
        }
    }
}
impl ClientCloseObservation<'_> {
    pub fn merge_seed(&mut self, seed: ClientCloseSeed) {
        assert!(
            std::sync::Weak::ptr_eq(&Arc::downgrade(&self.pool.state), &seed.state),
            "client close seed belongs to a different pool"
        );
        for (id, record) in seed.observed {
            self.observed.entry(id).or_insert(record);
        }
    }
    pub fn close_all(&mut self, timeout: Duration) -> ClientCacheCloseReport {
        self.pool.close_all_observed(timeout, &mut self.observed)
    }
}

impl ClientPool {
    pub fn new(grace_period: Duration) -> Self {
        Self::from_endpoint_context(
            grace_period,
            LocalEndpointContext::default_for_platform().map_err(|error| error.to_string()),
        )
    }
    pub fn with_endpoint_context(grace_period: Duration, context: LocalEndpointContext) -> Self {
        Self::from_endpoint_context(grace_period, Ok(context))
    }
    pub fn with_connection_event_source(mut self, source: c2_local::ConnectionEventSource) -> Self {
        self.connection_event_source = Some(source);
        self
    }
    fn from_endpoint_context(
        grace_period: Duration,
        endpoint_context: Result<LocalEndpointContext, String>,
    ) -> Self {
        Self {
            endpoint_context,
            connection_event_source: None,
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                epoch: Some(0),
                draining: false,
                budget: None,
                records: HashMap::new(),
                next_record: 0,
            })),
            grace_period,
            default_config: Mutex::new(None),
            drain_gate: Mutex::new(()),
        }
    }
    pub fn endpoint_context(&self) -> Result<&LocalEndpointContext, IpcError> {
        self.endpoint_context
            .as_ref()
            .map_err(|error| IpcError::Config(error.clone()))
    }
    pub fn set_default_config(&self, config: ClientIpcConfig) -> Result<(), IpcError> {
        let state = self.state.lock();
        if let Some(budget) = state.budget.as_ref() {
            let limits = budget.limits();
            let requested = config.memory_budget_limits();
            if limits != requested {
                return Err(budget_limits_mismatch(&limits, &requested, "cache default"));
            }
        }
        *self.default_config.lock() = Some(config);
        Ok(())
    }
    pub fn memory_budget_snapshot(&self) -> Option<ClientCacheMemorySnapshot> {
        self.state
            .lock()
            .budget
            .as_ref()
            .map(|budget| ClientCacheMemorySnapshot {
                limits: budget.limits(),
                budget: budget.snapshot(),
            })
    }
    pub fn memory_budget_observer(&self) -> Option<c2_mem::BudgetObserver> {
        self.state
            .lock()
            .budget
            .clone()
            .map(c2_mem::BudgetObserver::new)
    }
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
        let epoch;
        let cfg;
        let budget;
        let mut fresh;
        let mut detached = Vec::new();
        {
            let mut state = connect_lock(&self.state, deadline, "pool_wait")?;
            epoch = state.epoch.ok_or(IpcError::PoolRetired)?;
            state.check_epoch(epoch)?;
            state.records.retain(|_, record| !record.complete());
            cfg = match config {
                Some(config) => config.clone(),
                None => connect_lock(&self.default_config, deadline, "pool_config_wait")?
                    .clone()
                    .unwrap_or_default(),
            };
            let requested = cfg.memory_budget_limits();
            budget = state
                .budget
                .get_or_insert_with(|| c2_mem::MemoryBudget::from_limits(&requested))
                .clone();
            let limits = budget.limits();
            if limits != requested {
                return Err(budget_limits_mismatch(&limits, &requested, "cache domain"));
            }
            if let Some(entry) = state.entries.get(address) {
                if entry.client.is_connected() && !entry.references.expired(self.grace_period) {
                    if entry.client.config() != &cfg {
                        return Err(client_policy_mismatch(address, entry.client.config(), &cfg));
                    }
                    entry.references.acquire();
                    return Ok(ClientLease {
                        client: entry.client.clone(),
                        references: entry.references.clone(),
                        counted: true,
                    });
                }
            }
            let expired: Vec<_> = state
                .entries
                .iter()
                .filter(|(key, entry)| {
                    key.as_str() == address || entry.references.expired(self.grace_period)
                })
                .map(|(address, _)| address.clone())
                .collect();
            for address in expired {
                let entry = state.entries.remove(&address).unwrap();
                detached.push((state.record(&address), entry.client));
            }
            fresh = FreshClient {
                record: state.record(address),
                client: None,
                published: false,
            };
        }
        for (record, client) in detached {
            record.close(&client);
        }
        let endpoint = self
            .endpoint_context()?
            .endpoint(address)
            .map_err(crate::control::endpoint_error)?;
        crate::sync_client::shared_io_handle(deadline)?;
        let mut attempt = 0;
        let client = loop {
            crate::client::connect_check(deadline, "pool_connect")?;
            connect_lock(&self.state, deadline, "pool_wait")?.check_epoch(epoch)?;
            let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("/cc3c{:08x}{counter:016x}", std::process::id());
            let pool = Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    pool_config_from_client_config(&cfg),
                    prefix,
                    budget.clone(),
                )
                .map_err(IpcError::Config)?,
            ));
            let (candidate, result) = SyncClient::connect_transport_pool_attempt_with_deadline(
                endpoint.clone(),
                pool,
                cfg.clone(),
                budget.clone(),
                deadline,
                self.connection_event_source.as_ref(),
            )?;
            let candidate = Arc::new(candidate);
            fresh.client = Some(candidate.clone());
            match result {
                Ok(()) => break candidate,
                Err(error) => {
                    // These are initial, unpublished connect attempts. A
                    // business connection never reconnects or replays here.
                    let transient = matches!(&error, IpcError::Io(error) | IpcError::Connect(error) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected));
                    if !transient || attempt == 2 {
                        return Err(error);
                    }
                    let remaining = deadline
                        .remaining("pool_cleanup")
                        .map_err(|_| crate::client::connect_expired("pool_cleanup"))?;
                    if !candidate.close_shared(
                        remaining
                            .unwrap_or(Duration::from_secs(5))
                            .min(Duration::from_secs(5)),
                    ) {
                        return Err(error);
                    }
                    fresh.client.take();
                    attempt += 1;
                    let remaining = deadline
                        .remaining("pool_retry")
                        .map_err(|_| crate::client::connect_expired("pool_retry"))?;
                    let delay = Duration::from_millis(10 * attempt);
                    std::thread::sleep(remaining.map_or(delay, |left| delay.min(left)));
                }
            }
        };
        let mut state = connect_lock(&self.state, deadline, "pool_publish_wait")?;
        state.check_epoch(epoch)?;
        if let Some(entry) = state.entries.get(address) {
            if entry.client.is_connected() && !entry.references.expired(self.grace_period) {
                if entry.client.config() != &cfg {
                    return Err(client_policy_mismatch(address, entry.client.config(), &cfg));
                }
                entry.references.acquire();
                return Ok(ClientLease {
                    client: entry.client.clone(),
                    references: entry.references.clone(),
                    counted: true,
                });
            }
        }
        let references = Arc::new(PoolReferences::new(1, None));
        let replaced = state.entries.insert(
            address.into(),
            PoolEntry {
                client: client.clone(),
                references: references.clone(),
            },
        );
        let replacement = replaced.map(|entry| (state.record(address), entry.client));
        fresh.published = true;
        fresh.record.finish();
        drop(state);
        if let Some((record, client)) = replacement {
            record.close(&client);
        }
        Ok(ClientLease {
            client,
            references,
            counted: true,
        })
    }
    pub fn release_if_same(&self, address: &str, client: &Arc<SyncClient>) -> bool {
        self.state
            .lock()
            .entries
            .get(address)
            .is_some_and(|entry| Arc::ptr_eq(&entry.client, client) && entry.references.release())
    }
    pub fn discard_if_same(&self, address: &str, client: &Arc<SyncClient>) -> bool {
        self.discard_if_same_with_deadline(address, client, ConnectDeadline::default())
            .unwrap_or(false)
    }
    pub fn discard_if_same_with_deadline(
        &self,
        address: &str,
        client: &Arc<SyncClient>,
        deadline: ConnectDeadline,
    ) -> Result<bool, IpcError> {
        let detached = {
            let mut state = connect_lock(&self.state, deadline, "pool_discard_wait")?;
            if !state
                .entries
                .get(address)
                .is_some_and(|entry| Arc::ptr_eq(&entry.client, client))
            {
                return Ok(false);
            }
            let entry = state.entries.remove(address).unwrap();
            (state.record(address), entry.client)
        };
        detached.0.close(&detached.1);
        Ok(true)
    }
    pub fn sweep_expired(&self) {
        let detached = {
            let mut state = self.state.lock();
            state.records.retain(|_, record| !record.complete());
            let expired: Vec<_> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.references.expired(self.grace_period))
                .map(|(address, _)| address.clone())
                .collect();
            expired
                .into_iter()
                .map(|address| {
                    let entry = state.entries.remove(&address).unwrap();
                    (state.record(&address), entry.client)
                })
                .collect::<Vec<_>>()
        };
        for (record, client) in detached {
            record.close(&client);
        }
    }
    pub fn prepare_seal(&self, deadline: Instant) -> Option<ClientPoolSeal<'_>> {
        self.state
            .try_lock_until(deadline)
            .map(|state| ClientPoolSeal {
                state,
                source: &self.state,
            })
    }
    pub fn observe_closes(&self) -> ClientCloseObservation<'_> {
        ClientCloseObservation {
            pool: self,
            observed: HashMap::new(),
        }
    }
    pub fn close_all(&self, timeout: Duration) -> ClientCacheCloseReport {
        self.observe_closes().close_all(timeout)
    }
    fn close_all_observed(
        &self,
        timeout: Duration,
        observed: &mut ObservedCloses,
    ) -> ClientCacheCloseReport {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut report = ClientCacheCloseReport {
            detached: 0,
            unconfirmed: Vec::new(),
            error: None,
            abnormal: Vec::new(),
        };
        let Some(_drain) = self.drain_gate.try_lock_until(deadline) else {
            report.error = Some("timed out waiting for a concurrent cache drain".into());
            return report;
        };
        let (detached, sealed) = {
            let mut state = self.state.lock();
            state.draining = true;
            let entries = std::mem::take(&mut state.entries);
            report.detached = entries.len();
            let detached = entries
                .into_iter()
                .map(|(address, entry)| (state.record(&address), entry.client))
                .collect::<Vec<_>>();
            for (id, record) in &state.records {
                if !record.complete() {
                    observed.entry(*id).or_insert_with(|| Some(record.clone()));
                }
            }
            (detached, state.epoch.is_none())
        };
        for (record, client) in detached {
            record.close(&client);
        }
        for record in observed.values_mut() {
            let Some(original) = record.as_ref() else {
                continue;
            };
            if !original.wait(deadline, sealed) {
                if !report.unconfirmed.contains(&original.address) {
                    report.unconfirmed.push(original.address.clone());
                }
            } else if original.complete() {
                if let Some(detail) = original.abnormal() {
                    report.abnormal.push(detail);
                }
                record.take();
            }
        }
        let mut state = self.state.lock();
        state.draining = false;
        state.epoch = state
            .epoch
            .map(|epoch| epoch.checked_add(1).expect("client cache epoch exhausted"));
        state.records.retain(|_, record| !record.complete());
        report
    }
    pub fn active_count(&self) -> usize {
        self.state.lock().entries.len()
    }
    pub fn refcount(&self, address: &str) -> usize {
        self.state
            .lock()
            .entries
            .get(address)
            .map_or(0, |entry| entry.references.count.load(Ordering::Acquire))
    }
    pub fn has_client(&self, address: &str) -> bool {
        self.state.lock().entries.contains_key(address)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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

        let observed = pool.state.lock().entries[addr].client.clone();
        assert!(pool.release_if_same(addr, &observed));
        assert_eq!(pool.refcount(addr), 1);
        assert!(pool.has_client(addr));

        assert!(pool.release_if_same(addr, &observed));
        assert_eq!(pool.refcount(addr), 0);
        // last_release should now be set; entry still present.
        assert!(pool.has_client(addr));
    }
    #[test]
    fn test_pool_release_unknown_address() {
        let pool = ClientPool::new(Duration::from_secs(30));
        assert!(!pool.release_if_same("ipc://nonexistent", &Arc::new(make_disconnected_client())));
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
        let observed = pool.state.lock().entries[addr].client.clone();
        assert!(!pool.release_if_same(addr, &observed));
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
        assert!(!state.draining);
        assert_eq!(state.epoch, Some(1));
        drop(state);
        assert!(!pool.state.lock().draining);
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
        let pool1 = MemPool::new_with_prefix(pc.clone(), p1.clone()).unwrap();
        let pool2 = MemPool::new_with_prefix(pc.clone(), p2.clone()).unwrap();
        let repeated_label = MemPool::new_with_prefix(pc, p1.clone()).unwrap();
        assert!(pool1.prefix().starts_with(&format!("{p1}_")));
        assert!(pool2.prefix().starts_with(&format!("{p2}_")));
        assert_ne!(pool1.prefix(), repeated_label.prefix());
        assert_ne!(pool1.prefix(), pool2.prefix());
        assert!(pool1.prefix().len() <= c2_contract::MAX_WIRE_TEXT_BYTES);
    }
    fn make_disconnected_client() -> SyncClient {
        SyncClient::new_unconnected("ipc:///pool_test_fake")
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;
