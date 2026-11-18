//! Cache of business references. Each native connection independently owns
//! its writer, loans and deactivation; cache eviction only stops admission.
use crate::client::IpcError;
use crate::sync_client::SyncClient;
use c2_config::{ConnectDeadline, DomainLimits, IDLE_CONNECTION_TIMEOUT, LocalEndpointContext};
use c2_local::ConnectionObservation;
use c2_mem::{DomainBudget, DomainSnapshot, MemPool};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);

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
    fn expired(&self, grace: Option<Duration>) -> bool {
        let Some(grace) = grace else {
            return false;
        };
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
    reference: PoolReference,
}

// An acquire reserves its reference before waiting for a shared connection.
// This also prevents idle eviction between publication and waiter delivery.
struct PoolReference {
    references: Arc<PoolReferences>,
    counted: bool,
}

impl PoolReference {
    fn acquire(references: &Arc<PoolReferences>) -> Self {
        references.acquire();
        Self {
            references: references.clone(),
            counted: true,
        }
    }

    fn into_lease(self, client: Arc<SyncClient>) -> ClientLease {
        ClientLease {
            client,
            reference: self,
        }
    }
}

impl ClientLease {
    pub fn client(&self) -> &Arc<SyncClient> {
        &self.client
    }

    /// Transfer the counted reference to an explicit-release caller.
    pub fn into_client(mut self) -> Arc<SyncClient> {
        self.reference.counted = false;
        self.client.clone()
    }
}

impl std::ops::Deref for ClientLease {
    type Target = SyncClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Drop for PoolReference {
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

// Keep detached client metadata until its original physical close is observed.
// Preparing records fence connects across cache drains/seal.
enum CloseState {
    Preparing,
    Closing {
        observation: Option<ConnectionObservation>,
        _client: Option<Arc<SyncClient>>,
    },
    Finished,
}
struct CloseRecord {
    address: String,
    state: Mutex<CloseState>,
    changed: Condvar,
    async_changed: tokio::sync::Notify,
}
impl CloseRecord {
    fn preparing(address: &str) -> Self {
        Self {
            address: address.into(),
            state: Mutex::new(CloseState::Preparing),
            changed: Condvar::new(),
            async_changed: tokio::sync::Notify::new(),
        }
    }
    fn finish(&self) {
        *self.state.lock() = CloseState::Finished;
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }
    fn deactivate(&self, client: Arc<SyncClient>) {
        // Install the observation and retain the business Arc before changing
        // admission. Native tasks still own the stream and loan book.
        *self.state.lock() = CloseState::Closing {
            observation: client.connection_observation(),
            _client: Some(client.clone()),
        };
        client.deactivate();
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }
    async fn wait_async(&self) {
        loop {
            let changed = self.async_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let observation = {
                let state = self.state.lock();
                match &*state {
                    CloseState::Preparing => None,
                    CloseState::Closing {
                        observation: Some(observation),
                        ..
                    } => Some(Some(observation.clone())),
                    _ => Some(None),
                }
            };
            match observation {
                Some(Some(observation)) => {
                    observation.wait_closed(None).await;
                    return;
                }
                Some(None) => return,
                None => changed.await,
            }
        }
    }

    fn complete(&self) -> bool {
        match &*self.state.lock() {
            CloseState::Preparing => false,
            CloseState::Closing {
                observation: Some(observation),
                ..
            } => observation.snapshot().is_some(),
            CloseState::Closing {
                observation: None, ..
            }
            | CloseState::Finished => true,
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
            CloseState::Closing {
                observation: Some(observation),
                ..
            } => observation.clone(),
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
        let CloseState::Closing {
            observation: Some(observation),
            ..
        } = &*state
        else {
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
            self.record.deactivate(client);
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
    connecting: HashMap<String, Weak<PendingConnection>>,
    epoch: Option<u64>,
    draining: bool,
    records: HashMap<u64, Arc<CloseRecord>>,
    next_record: u64,
}

struct PendingConnection {
    epoch: u64,
    references: Arc<PoolReferences>,
    result: Mutex<Option<Result<Arc<SyncClient>, IpcError>>>,
    changed: Condvar,
}

impl PendingConnection {
    fn wait(&self, deadline: ConnectDeadline) -> Result<Arc<SyncClient>, IpcError> {
        let mut result = connect_lock(&self.result, deadline, "pool_connect_wait")?;
        loop {
            crate::client::connect_check(deadline, "pool_connect_wait")?;
            if let Some(result) = &*result {
                return result
                    .as_ref()
                    .map(Arc::clone)
                    .map_err(crate::client::stream::clone_observation_error);
            }
            match deadline.instant() {
                Some(instant) => {
                    self.changed.wait_until(&mut result, instant);
                }
                None => self.changed.wait(&mut result),
            }
        }
    }
}

// Publish to the acquires already attached, including if the owner unwinds.
// The cache holds only a weak reference, so completing an attempt needs no
// cache lock and cannot extend the owner's deadline waiting for that lock.
struct PendingConnectionOwner {
    pending: Arc<PendingConnection>,
    finished: bool,
}

impl PendingConnectionOwner {
    fn finish(&mut self, result: &Result<Arc<SyncClient>, IpcError>) {
        *self.pending.result.lock() = Some(
            result
                .as_ref()
                .map(Arc::clone)
                .map_err(crate::client::stream::clone_observation_error),
        );
        self.finished = true;
        self.pending.changed.notify_all();
    }
}

impl Drop for PendingConnectionOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(&Err(IpcError::Pool(
                "client pool connection attempt interrupted".into(),
            )));
        }
    }
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
    grace_period: Option<Duration>,
    budget: DomainBudget,
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    /// A cache over this platform's default endpoint context with the fixed
    /// idle reaping window and a default role-domain budget.
    pub fn new() -> Self {
        Self::from_parts(
            Some(IDLE_CONNECTION_TIMEOUT),
            LocalEndpointContext::default_for_platform().map_err(|error| error.to_string()),
            DomainBudget::from_limits(&DomainLimits::default()),
        )
    }
    /// A cache over an explicit endpoint context with the fixed idle reaping
    /// window and a default role-domain budget.
    pub fn with_endpoint_context(context: LocalEndpointContext) -> Self {
        Self::from_parts(
            Some(IDLE_CONNECTION_TIMEOUT),
            Ok(context),
            DomainBudget::from_limits(&DomainLimits::default()),
        )
    }
    /// Create a cache with its own role-domain budget from explicit limits.
    pub fn with_endpoint_context_and_limits(
        context: LocalEndpointContext,
        limits: DomainLimits,
    ) -> Result<Self, IpcError> {
        limits
            .validate()
            .map_err(|error| IpcError::Config(error.to_string()))?;
        Ok(Self::with_endpoint_context_and_budget(
            context,
            DomainBudget::from_limits(&limits),
        ))
    }

    /// A cache over an explicit endpoint context whose every connection
    /// charges the caller's shared domain budget. The budget is frozen here
    /// and never re-created per attempt.
    pub fn with_endpoint_context_and_budget(
        context: LocalEndpointContext,
        budget: DomainBudget,
    ) -> Self {
        Self::from_parts(Some(IDLE_CONNECTION_TIMEOUT), Ok(context), budget)
    }
    /// Test-only cache with an explicit idle window (for millisecond-level
    /// reaping checks). Production callers use the 60-second default.
    #[cfg(test)]
    pub(crate) fn with_grace_period_and_context(
        grace_period: Option<Duration>,
        context: LocalEndpointContext,
    ) -> Self {
        Self::from_parts(
            grace_period,
            Ok(context),
            DomainBudget::from_limits(&DomainLimits::default()),
        )
    }
    pub fn with_connection_event_source(mut self, source: c2_local::ConnectionEventSource) -> Self {
        self.connection_event_source = Some(source);
        self
    }
    fn from_parts(
        grace_period: Option<Duration>,
        endpoint_context: Result<LocalEndpointContext, String>,
        budget: DomainBudget,
    ) -> Self {
        Self {
            endpoint_context,
            connection_event_source: None,
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                connecting: HashMap::new(),
                epoch: Some(0),
                draining: false,
                records: HashMap::new(),
                next_record: 0,
            })),
            grace_period,
            budget,
            sweeper: Mutex::new(None),
            drain_gate: Mutex::new(()),
        }
    }
    /// The frozen role-domain budget every pooled connection charges.
    pub fn domain_budget(&self) -> &DomainBudget {
        &self.budget
    }

    /// Consistent read-only accounting snapshot of the pool's domain.
    pub fn domain_snapshot(&self) -> DomainSnapshot {
        self.budget.snapshot()
    }

    /// A foreign endpoint owns its own physical connections while sharing this
    /// factory's frozen backing budget. Its retirement cannot seal another endpoint.
    #[doc(hidden)]
    pub fn fork_endpoint_scope(&self, deadline: ConnectDeadline) -> Result<Self, IpcError> {
        {
            let state = connect_lock(&self.state, deadline, "pool_scope_wait")?;
            if state.epoch.is_none() {
                return Err(IpcError::PoolRetired);
            }
        }
        let mut scoped = Self::from_parts(
            self.grace_period,
            Ok(self.endpoint_context()?.clone()),
            self.budget.clone(),
        );
        scoped.connection_event_source = self.connection_event_source.clone();
        Ok(scoped)
    }

    /// Seal a native endpoint's whole cache, including unsuccessful route preparations.
    #[doc(hidden)]
    pub fn deactivate_endpoint_scope(&self) {
        let detached = {
            let mut state = self.state.lock();
            state.epoch = None;
            std::mem::take(&mut state.entries)
                .into_iter()
                .map(|(address, entry)| (state.record(&address), entry.client))
                .collect::<Vec<_>>()
        };
        for (record, client) in detached {
            record.deactivate(client);
        }
    }

    #[doc(hidden)]
    pub async fn close_endpoint_scope(&self) {
        self.deactivate_endpoint_scope();
        let records = self
            .state
            .lock()
            .records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for record in records {
            record.wait_async().await;
        }
        self.state
            .lock()
            .records
            .retain(|_, record| !record.complete());
    }

    pub fn endpoint_context(&self) -> Result<&LocalEndpointContext, IpcError> {
        self.endpoint_context
            .as_ref()
            .map_err(|error| IpcError::Config(error.clone()))
    }
    pub fn acquire(&self, address: &str) -> Result<Arc<SyncClient>, IpcError> {
        self.acquire_with_deadline(address, ConnectDeadline::default())
    }
    pub fn acquire_with_deadline(
        &self,
        address: &str,
        deadline: ConnectDeadline,
    ) -> Result<Arc<SyncClient>, IpcError> {
        self.acquire_lease_with_deadline(address, deadline)
            .map(ClientLease::into_client)
    }
    pub fn acquire_lease_with_deadline(
        &self,
        address: &str,
        deadline: ConnectDeadline,
    ) -> Result<ClientLease, IpcError> {
        let epoch;
        let budget;
        let fresh;
        let pending;
        let reference;
        let mut detached = Vec::new();
        {
            let mut state = connect_lock(&self.state, deadline, "pool_wait")?;
            epoch = state.epoch.ok_or(IpcError::PoolRetired)?;
            state.check_epoch(epoch)?;
            state.records.retain(|_, record| !record.complete());
            state
                .connecting
                .retain(|_, pending| pending.strong_count() != 0);
            // Transport parameters are fixed constants now, so a same-endpoint
            // cache hit is accepted without any per-config policy comparison.
            // The frozen budget below is the only shared capacity authority.
            budget = self.budget.clone();
            if let Some(entry) = state.entries.get(address) {
                if entry.client.is_connected() && !entry.references.expired(self.grace_period) {
                    return Ok(
                        PoolReference::acquire(&entry.references).into_lease(entry.client.clone())
                    );
                }
            }
            if let Some(connecting) = state
                .connecting
                .get(address)
                .and_then(Weak::upgrade)
                .filter(|connecting| {
                    connecting.epoch == epoch && connecting.result.lock().is_none()
                })
            {
                let reference = PoolReference::acquire(&connecting.references);
                drop(state);
                let client = connecting.wait(deadline)?;
                connect_lock(&self.state, deadline, "pool_publish_wait")?.check_epoch(epoch)?;
                return Ok(reference.into_lease(client));
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
            pending = Arc::new(PendingConnection {
                epoch,
                references: Arc::new(PoolReferences::new(0, None)),
                result: Mutex::new(None),
                changed: Condvar::new(),
            });
            reference = PoolReference::acquire(&pending.references);
            state
                .connecting
                .insert(address.into(), Arc::downgrade(&pending));
        }
        let mut owner = PendingConnectionOwner {
            pending: pending.clone(),
            finished: false,
        };
        let result = (|| {
            let mut fresh = fresh;
            for (record, client) in detached {
                record.deactivate(client);
            }
            let endpoint = self
                .endpoint_context()?
                .endpoint(address)
                .map_err(crate::control::endpoint_error)?;
            let io = crate::sync_client::shared_io_handle(deadline)?;
            self.start_sweeper(&io, deadline)?;
            let mut attempt = 0;
            let client = loop {
                crate::client::connect_check(deadline, "pool_connect")?;
                connect_lock(&self.state, deadline, "pool_wait")?.check_epoch(epoch)?;
                let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed);
                let prefix = format!("/cc3c{:08x}{counter:016x}", std::process::id());
                let pool = Arc::new(RwLock::new(
                    MemPool::new_with_prefix_and_budget(
                        c2_config::PoolConfig::primary_for_budget(
                            budget.snapshot().memory.limit_bytes,
                        ),
                        prefix,
                        budget.clone(),
                    )
                    .map_err(IpcError::Config)?,
                ));
                let (candidate, result) = SyncClient::connect_transport_pool_attempt_with_deadline(
                    endpoint.clone(),
                    pool,
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
            let replaced = state.entries.insert(
                address.into(),
                PoolEntry {
                    client: client.clone(),
                    references: pending.references.clone(),
                },
            );
            debug_assert!(
                replaced.is_none(),
                "only the pending connection can publish"
            );
            fresh.published = true;
            fresh.record.finish();
            drop(state);
            Ok(client)
        })();
        owner.finish(&result);
        result.map(|client| reference.into_lease(client))
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
        detached.0.deactivate(detached.1);
        Ok(true)
    }
    pub fn sweep_expired(&self) {
        Self::sweep_expired_state(&self.state, self.grace_period);
    }

    fn start_sweeper(
        &self,
        io: &tokio::runtime::Handle,
        deadline: ConnectDeadline,
    ) -> Result<(), IpcError> {
        let Some(grace) = self.grace_period else {
            return Ok(());
        };
        let mut sweeper = connect_lock(&self.sweeper, deadline, "pool_maintenance_wait")?;
        if sweeper.is_none() {
            let state = Arc::downgrade(&self.state);
            let interval = grace.clamp(Duration::from_millis(1), Duration::from_secs(1));
            *sweeper = Some(io.spawn(async move {
                let mut ticks = tokio::time::interval(interval);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticks.tick().await;
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    Self::sweep_expired_state(&state, Some(grace));
                }
            }));
        }
        Ok(())
    }

    fn sweep_expired_state(state: &Mutex<CacheState>, grace: Option<Duration>) {
        let detached = {
            let mut state = state.lock();
            state.records.retain(|_, record| !record.complete());
            let expired: Vec<_> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.references.expired(grace))
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
            record.deactivate(client);
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
            record.deactivate(client);
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

impl Drop for ClientPool {
    fn drop(&mut self) {
        if let Some(sweeper) = self.sweeper.get_mut().take() {
            sweeper.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::LocalEndpointContext;

    fn context() -> LocalEndpointContext {
        LocalEndpointContext::default_for_platform().unwrap()
    }
    #[test]
    fn test_pool_new() {
        let pool = ClientPool::new();
        assert_eq!(pool.active_count(), 0);
    }
    #[test]
    fn test_pool_grace_period_sweep() {
        let pool =
            ClientPool::with_grace_period_and_context(Some(Duration::from_millis(50)), context());

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
        let pool =
            ClientPool::with_grace_period_and_context(Some(Duration::from_secs(60)), context());

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
        let pool = ClientPool::with_endpoint_context(context());
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
        let pool = ClientPool::with_endpoint_context(context());
        assert!(!pool.release_if_same("ipc://nonexistent", &Arc::new(make_disconnected_client())));
    }
    #[test]
    fn test_pool_release_already_zero() {
        let pool = ClientPool::with_endpoint_context(context());
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
        let pool = ClientPool::with_endpoint_context(context());
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
        let pool = ClientPool::with_endpoint_context(context());
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
        let pool = ClientPool::with_endpoint_context(context());

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
    fn frozen_domain_budget_is_shared_across_forks() {
        let pool = ClientPool::with_endpoint_context_and_budget(
            context(),
            DomainBudget::from_limits(&DomainLimits {
                memory_budget_bytes: 4096,
                file_spill_budget_bytes: 0,
                max_outstanding_calls: 0,
            }),
        );
        let scoped = pool
            .fork_endpoint_scope(ConnectDeadline::default())
            .unwrap();
        assert_eq!(
            pool.domain_snapshot().memory.limit_bytes,
            scoped.domain_snapshot().memory.limit_bytes
        );
        assert_eq!(scoped.domain_snapshot().memory.limit_bytes, 4096);
        let reservation = scoped
            .domain_budget()
            .try_reserve_memory(4096)
            .expect("whole frozen budget is available");
        assert!(pool.domain_budget().try_reserve_memory(1).is_err());
        drop(reservation);
    }
    #[test]
    fn test_client_pool_unique_prefixes() {
        // Verify that successive client MemPool creations get different
        // SHM prefixes via CLIENT_POOL_GEN counter.
        let pc = c2_config::PoolConfig::default();
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
