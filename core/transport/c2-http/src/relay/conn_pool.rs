use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use c2_ipc::IpcClient;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::relay::types::UpstreamEndpointKey;

const RECONNECT_MAX_ATTEMPTS: usize = 2;
const RECONNECT_RETRY_DELAY_MS: u64 = 10;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_retryable_connect_error(error: &c2_ipc::IpcError) -> bool {
    let (c2_ipc::IpcError::Io(error) | c2_ipc::IpcError::Connect(error)) = error else {
        return false;
    };
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionRefused
            | ErrorKind::NotFound
            | ErrorKind::TimedOut
    )
}

/// A single upstream IPC endpoint slot.
struct UpstreamSlot {
    key: UpstreamEndpointKey,
    inner: Mutex<SlotInner>,
    notify: Notify,
    // Unit tests can exercise pool transitions without opening an IPC stream.
    #[cfg(test)]
    test_assume_connected: bool,
}

struct SlotInner {
    client: Option<Arc<IpcClient>>,
    last_activity: u64,
    active_requests: usize,
    state: SlotState,
    reconnect_attempt: Option<Arc<()>>,
    owner_generation: u64,
    owner_lease_epoch: u64,
    owner_lease_deadline: Option<Instant>,
    #[cfg(test)]
    test_connected_override: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    OwnerOnly,
    Ready,
    Evicted,
    Disconnected,
    Reconnecting,
    Retired,
}

/// Restores only this connection attempt if its acquire future is dropped.
struct ReconnectGuard<'a> {
    slot: &'a UpstreamSlot,
    attempt: Arc<()>,
}

impl<'a> ReconnectGuard<'a> {
    fn new(slot: &'a UpstreamSlot, inner: &mut SlotInner) -> Self {
        let attempt = Arc::new(());
        inner.state = SlotState::Reconnecting;
        inner.reconnect_attempt = Some(attempt.clone());
        Self { slot, attempt }
    }

    fn is_current(&self, inner: &SlotInner) -> bool {
        inner.state == SlotState::Reconnecting
            && inner
                .reconnect_attempt
                .as_ref()
                .is_some_and(|attempt| Arc::ptr_eq(attempt, &self.attempt))
    }
}

impl Drop for ReconnectGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.slot.inner.lock();
        if !self.is_current(&inner) {
            return;
        }
        inner.reconnect_attempt = None;
        inner.state = SlotState::Disconnected;
        inner.last_activity = now_millis();
        drop(inner);
        self.slot.notify.notify_waiters();
    }
}

#[derive(Debug)]
pub enum AcquireError {
    NotFound,
    Unreachable {
        endpoint: UpstreamEndpointKey,
        error: c2_ipc::IpcError,
    },
}

pub struct UpstreamLease {
    slot: Arc<UpstreamSlot>,
    client: Arc<IpcClient>,
}

pub enum CachedClient {
    Ready { endpoint: UpstreamEndpointKey },
    OwnerOnly { endpoint: UpstreamEndpointKey },
    Evicted { endpoint: UpstreamEndpointKey },
    Disconnected { endpoint: UpstreamEndpointKey },
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerReplacementEvidence {
    ConfirmedDead,
    ConfirmedRouteMissing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerReplaceError {
    StaleToken,
    NotReplaceable,
}

#[derive(Clone)]
pub struct OwnerSlotToken {
    slot: Arc<UpstreamSlot>,
    key: UpstreamEndpointKey,
    owner_generation: u64,
    owner_lease_epoch: u64,
}

pub type OwnerToken = OwnerSlotToken;

struct PoolInner {
    entries: HashMap<UpstreamEndpointKey, Arc<UpstreamSlot>>,
    next_owner_generation: u64,
}

/// Pool of IPC connections keyed by upstream endpoint identity.
///
/// Separated from RouteTable to keep route metadata independent of
/// connection lifecycle. Supports lazy reconnection and idle eviction.
pub struct ConnectionPool {
    inner: Mutex<PoolInner>,
    owner_lease_duration: Option<Duration>,
    #[cfg(test)]
    test_assume_connected: bool,
}

impl ConnectionPool {
    #[cfg(test)]
    pub fn new() -> Self {
        let mut pool = Self::with_owner_lease_duration(None);
        pool.test_assume_connected = true;
        pool
    }

    pub fn with_owner_lease_duration(owner_lease_duration: Option<Duration>) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                entries: HashMap::new(),
                next_owner_generation: 1,
            }),
            owner_lease_duration,
            #[cfg(test)]
            test_assume_connected: false,
        }
    }

    fn slot(&self, key: &UpstreamEndpointKey) -> Option<Arc<UpstreamSlot>> {
        self.inner.lock().entries.get(key).cloned()
    }

    fn slot_matches(&self, key: &UpstreamEndpointKey, expected: &Arc<UpstreamSlot>) -> bool {
        self.slot(key)
            .is_some_and(|current| Arc::ptr_eq(&current, expected))
    }

    fn owner_lease_deadline(&self) -> Option<Instant> {
        self.owner_lease_duration
            .and_then(|duration| Instant::now().checked_add(duration))
    }

    pub async fn acquire_with<C, Fut>(
        &self,
        key: &UpstreamEndpointKey,
        connector: C,
    ) -> Result<UpstreamLease, AcquireError>
    where
        C: Fn(UpstreamEndpointKey) -> Fut,
        Fut: Future<Output = Result<Arc<IpcClient>, c2_ipc::IpcError>>,
    {
        let Some(slot) = self.slot(key) else {
            return Err(AcquireError::NotFound);
        };
        let mut attempts = 0;
        loop {
            attempts += 1;
            match slot.clone().acquire_with(&connector).await {
                Ok(lease) => return Ok(lease),
                Err(AcquireError::Unreachable { error, .. })
                    if attempts < RECONNECT_MAX_ATTEMPTS && is_retryable_connect_error(&error) =>
                {
                    tokio::time::sleep(Duration::from_millis(RECONNECT_RETRY_DELAY_MS)).await;
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Insert a pre-connected client for an endpoint.
    #[cfg(test)]
    pub fn insert(&self, key: UpstreamEndpointKey, client: Arc<IpcClient>) {
        let mut inner = self.inner.lock();
        let owner_generation = next_owner_generation(&mut inner);
        let old_slot = inner.entries.insert(
            key.clone(),
            Arc::new(UpstreamSlot::new(
                key,
                Some(client),
                SlotState::Ready,
                owner_generation,
                self.owner_lease_deadline(),
                #[cfg(test)]
                self.test_assume_connected,
            )),
        );
        drop(inner);
        if let Some(old_slot) = old_slot
            && let Some(client) = old_slot.retire()
        {
            close_replaced_client(client);
        }
    }

    /// Insert route owner metadata without attaching a relay data-plane client.
    pub fn insert_owner(&self, key: UpstreamEndpointKey) {
        let mut inner = self.inner.lock();
        if let Some(slot) = inner.entries.get(&key).cloned() {
            drop(inner);
            slot.renew_current_owner_lease(self.owner_lease_deadline());
            return;
        }
        let owner_generation = next_owner_generation(&mut inner);
        inner.entries.insert(
            key.clone(),
            Arc::new(UpstreamSlot::new(
                key,
                None,
                SlotState::OwnerOnly,
                owner_generation,
                self.owner_lease_deadline(),
                #[cfg(test)]
                self.test_assume_connected,
            )),
        );
    }

    pub fn lookup(&self, key: &UpstreamEndpointKey) -> CachedClient {
        let Some(slot) = self.slot(key) else {
            return CachedClient::Missing;
        };
        slot.lookup()
    }

    /// Get stored address for reconnection.
    #[cfg(test)]
    pub fn get_address(&self, key: &UpstreamEndpointKey) -> Option<String> {
        self.slot(key).map(|slot| slot.address())
    }

    /// Capture the current owner identity before awaiting a probe.
    pub fn owner_token(&self, key: &UpstreamEndpointKey) -> Option<OwnerToken> {
        let slot = self.slot(key)?;
        slot.owner_token()
    }

    /// Endpoint entries to evict (dead or idle beyond timeout_ms).
    pub fn idle_entries(&self, idle_timeout_ms: u64) -> Vec<UpstreamEndpointKey> {
        self.inner
            .lock()
            .entries
            .iter()
            .filter(|(_, slot)| slot.is_idle_candidate(idle_timeout_ms))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Evict idle clients with a slot-local recheck before removing them.
    pub fn evict_idle(
        &self,
        idle_timeout_ms: u64,
    ) -> Vec<(UpstreamEndpointKey, Option<Arc<IpcClient>>)> {
        let keys = self.idle_entries(idle_timeout_ms);
        keys.into_iter()
            .map(|key| {
                let client = self
                    .slot(&key)
                    .and_then(|slot| slot.evict_if_idle(idle_timeout_ms));
                (key, client)
            })
            .collect()
    }

    /// Evict a client — returns old Arc for async close.
    #[cfg(test)]
    pub fn evict(&self, key: &UpstreamEndpointKey) -> Option<Arc<IpcClient>> {
        self.slot(key)?.evict()
    }

    /// Evict a request client only if it is still the current cached client.
    #[cfg(test)]
    pub fn evict_client(
        &self,
        key: &UpstreamEndpointKey,
        client: &Arc<IpcClient>,
    ) -> Option<Arc<IpcClient>> {
        self.slot(key)?.evict_client(client)
    }

    /// Re-attach a freshly connected client.
    #[cfg(test)]
    pub fn reconnect(&self, key: &UpstreamEndpointKey, client: Arc<IpcClient>) {
        if let Some(slot) = self.slot(key) {
            let owner_generation = {
                let mut inner = self.inner.lock();
                next_owner_generation(&mut inner)
            };
            slot.reconnect(client, owner_generation, self.owner_lease_deadline());
        }
    }

    /// Remove entry entirely.
    pub fn remove(&self, key: &UpstreamEndpointKey) -> Option<Arc<IpcClient>> {
        self.inner
            .lock()
            .entries
            .remove(key)
            .and_then(|slot| slot.retire())
    }

    /// List endpoint keys.
    #[cfg(test)]
    pub fn list_connections(&self) -> Vec<UpstreamEndpointKey> {
        self.inner.lock().entries.keys().cloned().collect()
    }

    pub fn matches_owner_token(&self, key: &UpstreamEndpointKey, token: &OwnerToken) -> bool {
        self.slot_matches(key, &token.slot) && token.slot.matches_owner_token(token)
    }

    pub fn renew_owner_lease(&self, key: &UpstreamEndpointKey, token: &OwnerToken) -> bool {
        self.slot_matches(key, &token.slot)
            && token
                .slot
                .renew_owner_lease(token, self.owner_lease_deadline())
    }

    pub fn renew_current_owner_lease(&self, key: &UpstreamEndpointKey) -> bool {
        let Some(slot) = self.slot(key) else {
            return false;
        };
        slot.renew_current_owner_lease(self.owner_lease_deadline())
    }

    pub fn validate_replaceable_owner_token(
        &self,
        key: &UpstreamEndpointKey,
        token: &OwnerToken,
        evidence: OwnerReplacementEvidence,
    ) -> Result<(), OwnerReplaceError> {
        let inner = self.inner.lock();
        let Some(current) = inner.entries.get(key).cloned() else {
            return Err(OwnerReplaceError::StaleToken);
        };
        if !Arc::ptr_eq(&current, &token.slot) {
            return Err(OwnerReplaceError::StaleToken);
        }

        {
            let locked = current.inner.lock();
            if current.key != token.key
                || locked.owner_generation != token.owner_generation
                || locked.owner_lease_epoch != token.owner_lease_epoch
            {
                return Err(OwnerReplaceError::StaleToken);
            }
            if locked.active_requests != 0 {
                return Err(OwnerReplaceError::NotReplaceable);
            }
            if !replacement_evidence_allows_locked(&locked, token, evidence) {
                return Err(OwnerReplaceError::NotReplaceable);
            }
        };
        Ok(())
    }

    #[cfg(test)]
    fn begin_request_for_test(&self, key: &UpstreamEndpointKey) -> Option<UpstreamLease> {
        self.slot(key)?.begin_request_for_test()
    }

    #[cfg(test)]
    pub(crate) fn set_connected_for_test(&self, key: &UpstreamEndpointKey, connected: bool) {
        let slot = self.slot(key).expect("test endpoint must exist");
        let mut inner = slot.inner.lock();
        assert!(inner.client.is_some(), "test endpoint must have a client");
        inner.test_connected_override = Some(connected);
    }
}

fn next_owner_generation(inner: &mut PoolInner) -> u64 {
    let generation = inner.next_owner_generation;
    inner.next_owner_generation = inner.next_owner_generation.saturating_add(1);
    generation
}

#[cfg(test)]
fn close_replaced_client(client: Arc<IpcClient>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            client.retire();
            if let Some(observation) = client.connection_observation() {
                observation.wait_closed(None).await;
            }
        });
    }
}

impl UpstreamSlot {
    fn new(
        key: UpstreamEndpointKey,
        client: Option<Arc<IpcClient>>,
        state: SlotState,
        owner_generation: u64,
        owner_lease_deadline: Option<Instant>,
        #[cfg(test)] test_assume_connected: bool,
    ) -> Self {
        Self {
            key,
            inner: Mutex::new(SlotInner {
                client,
                last_activity: now_millis(),
                active_requests: 0,
                state,
                reconnect_attempt: None,
                owner_generation,
                owner_lease_epoch: 0,
                owner_lease_deadline,
                #[cfg(test)]
                test_connected_override: None,
            }),
            notify: Notify::new(),
            #[cfg(test)]
            test_assume_connected,
        }
    }

    fn client_is_connected(&self, _inner: &SlotInner, client: &IpcClient) -> bool {
        #[cfg(test)]
        if let Some(connected) = _inner.test_connected_override {
            return connected;
        }
        #[cfg(test)]
        if self.test_assume_connected {
            return true;
        }
        client.is_connected()
    }

    #[cfg(test)]
    fn clear_test_connected_override(&self, inner: &mut SlotInner) {
        inner.test_connected_override = None;
    }

    async fn acquire_with<C, Fut>(
        self: Arc<Self>,
        connector: C,
    ) -> Result<UpstreamLease, AcquireError>
    where
        C: Fn(UpstreamEndpointKey) -> Fut,
        Fut: Future<Output = Result<Arc<IpcClient>, c2_ipc::IpcError>>,
    {
        loop {
            let notified = self.notify.notified();
            let mut notified = pin!(notified);
            notified.as_mut().enable();
            let attempt = {
                let mut inner = self.inner.lock();
                match inner.state {
                    SlotState::Retired => return Err(AcquireError::NotFound),
                    SlotState::OwnerOnly => Some(ReconnectGuard::new(&self, &mut inner)),
                    SlotState::Ready => {
                        if let Some(client) = inner.client.clone()
                            && self.client_is_connected(&inner, &client)
                        {
                            inner.active_requests += 1;
                            inner.last_activity = now_millis();
                            return Ok(UpstreamLease {
                                slot: self.clone(),
                                client,
                            });
                        }
                        inner.client = None;
                        #[cfg(test)]
                        self.clear_test_connected_override(&mut inner);
                        Some(ReconnectGuard::new(&self, &mut inner))
                    }
                    SlotState::Evicted | SlotState::Disconnected => {
                        Some(ReconnectGuard::new(&self, &mut inner))
                    }
                    SlotState::Reconnecting => None,
                }
            };

            match attempt {
                Some(attempt) => match connector(self.key.clone()).await {
                    Ok(client) => {
                        let acquire = {
                            let mut inner = self.inner.lock();
                            if !attempt.is_current(&inner) {
                                None
                            } else {
                                inner.reconnect_attempt = None;
                                inner.client = Some(client.clone());
                                #[cfg(test)]
                                self.clear_test_connected_override(&mut inner);
                                inner.state = SlotState::Ready;
                                inner.active_requests += 1;
                                inner.last_activity = now_millis();
                                Some(())
                            }
                        };
                        if acquire.is_none() {
                            // A rejected candidate must start closing even when an
                            // external Arc survives. Only this candidate is fenced;
                            // the replacement slot/client is untouched. Production
                            // connectors published it into the bounded Relay owner
                            // domain before returning it here. An unconfirmed close
                            // leaves that owner intact and is_connected=false makes
                            // its existing observer retry until confirmed cleanup.
                            client.retire();
                            let cleanup_confirmed = match client.connection_observation() {
                                Some(observation) => observation
                                    .wait_closed(Some(Duration::from_millis(50)))
                                    .await
                                    .is_some(),
                                None => true,
                            };
                            tracing::debug!(
                                cleanup_confirmed,
                                "closing rejected upstream candidate"
                            );
                            return Err(AcquireError::NotFound);
                        }
                        self.notify.notify_waiters();
                        return Ok(UpstreamLease {
                            slot: self.clone(),
                            client,
                        });
                    }
                    Err(err) => {
                        let mut inner = self.inner.lock();
                        if !attempt.is_current(&inner) {
                            drop(inner);
                            return Err(AcquireError::NotFound);
                        }
                        inner.client = None;
                        #[cfg(test)]
                        self.clear_test_connected_override(&mut inner);
                        inner.reconnect_attempt = None;
                        inner.state = SlotState::Disconnected;
                        inner.last_activity = now_millis();
                        drop(inner);
                        self.notify.notify_waiters();
                        return Err(AcquireError::Unreachable {
                            endpoint: self.key.clone(),
                            error: err,
                        });
                    }
                },
                None => {
                    notified.as_mut().await;
                }
            }
        }
    }

    fn lookup(&self) -> CachedClient {
        let mut inner = self.inner.lock();
        match inner.state {
            SlotState::OwnerOnly => CachedClient::OwnerOnly {
                endpoint: self.key.clone(),
            },
            SlotState::Ready => match &inner.client {
                Some(client) if self.client_is_connected(&inner, client) => CachedClient::Ready {
                    endpoint: self.key.clone(),
                },
                Some(_) => {
                    inner.client = None;
                    #[cfg(test)]
                    self.clear_test_connected_override(&mut inner);
                    inner.state = SlotState::Disconnected;
                    CachedClient::Disconnected {
                        endpoint: self.key.clone(),
                    }
                }
                None => CachedClient::Evicted {
                    endpoint: self.key.clone(),
                },
            },
            SlotState::Evicted => CachedClient::Evicted {
                endpoint: self.key.clone(),
            },
            SlotState::Disconnected | SlotState::Reconnecting => CachedClient::Disconnected {
                endpoint: self.key.clone(),
            },
            SlotState::Retired => CachedClient::Missing,
        }
    }

    #[cfg(test)]
    fn begin_request_for_test(self: Arc<Self>) -> Option<UpstreamLease> {
        let mut inner = self.inner.lock();
        if inner.state != SlotState::Ready {
            return None;
        }
        let client = inner.client.as_ref()?.clone();
        if !self.client_is_connected(&inner, &client) {
            inner.state = SlotState::Disconnected;
            inner.client = None;
            #[cfg(test)]
            self.clear_test_connected_override(&mut inner);
            return None;
        }
        inner.active_requests += 1;
        inner.last_activity = now_millis();
        drop(inner);
        Some(UpstreamLease { slot: self, client })
    }

    fn end_request(&self) {
        let mut inner = self.inner.lock();
        if inner.active_requests == 0 {
            debug_assert!(false, "end_request called without begin_request");
        } else {
            inner.active_requests -= 1;
        }
        inner.last_activity = now_millis();
    }

    fn address(&self) -> String {
        self.key.address().to_string()
    }

    fn owner_token(self: Arc<Self>) -> Option<OwnerToken> {
        let inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        Some(OwnerToken {
            slot: self.clone(),
            key: self.key.clone(),
            owner_generation: inner.owner_generation,
            owner_lease_epoch: inner.owner_lease_epoch,
        })
    }

    fn matches_owner_token(&self, token: &OwnerToken) -> bool {
        let inner = self.inner.lock();
        self.key == token.key
            && inner.owner_generation == token.owner_generation
            && inner.owner_lease_epoch == token.owner_lease_epoch
            && inner.state != SlotState::Retired
    }

    fn renew_owner_lease(&self, token: &OwnerToken, deadline: Option<Instant>) -> bool {
        let mut inner = self.inner.lock();
        if self.key != token.key
            || inner.owner_generation != token.owner_generation
            || inner.owner_lease_epoch != token.owner_lease_epoch
            || inner.state == SlotState::Retired
        {
            return false;
        }
        inner.owner_lease_epoch = inner.owner_lease_epoch.saturating_add(1);
        inner.owner_lease_deadline = deadline;
        true
    }

    fn renew_current_owner_lease(&self, deadline: Option<Instant>) -> bool {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return false;
        }
        inner.owner_lease_epoch = inner.owner_lease_epoch.saturating_add(1);
        inner.owner_lease_deadline = deadline;
        true
    }

    fn is_idle_candidate(&self, idle_timeout_ms: u64) -> bool {
        let inner = self.inner.lock();
        self.should_evict(&inner, idle_timeout_ms)
    }

    fn evict_if_idle(&self, idle_timeout_ms: u64) -> Option<Arc<IpcClient>> {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        if !self.should_evict(&inner, idle_timeout_ms) {
            return None;
        }
        let client = inner.client.take();
        #[cfg(test)]
        self.clear_test_connected_override(&mut inner);
        if client.is_some() {
            inner.state = SlotState::Evicted;
        }
        client
    }

    #[cfg(test)]
    fn evict(&self) -> Option<Arc<IpcClient>> {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        let client = inner.client.take();
        #[cfg(test)]
        self.clear_test_connected_override(&mut inner);
        if client.is_some() {
            inner.state = SlotState::Evicted;
        }
        client
    }

    fn evict_client(&self, expected: &Arc<IpcClient>) -> Option<Arc<IpcClient>> {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        let current = inner.client.as_ref()?;
        if !Arc::ptr_eq(current, expected) {
            return None;
        }
        let client = inner.client.take();
        #[cfg(test)]
        self.clear_test_connected_override(&mut inner);
        if client.is_some() {
            inner.state = SlotState::Evicted;
        }
        client
    }

    #[cfg(test)]
    fn reconnect(
        &self,
        client: Arc<IpcClient>,
        owner_generation: u64,
        owner_lease_deadline: Option<Instant>,
    ) {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return;
        }
        inner.client = Some(client);
        #[cfg(test)]
        self.clear_test_connected_override(&mut inner);
        inner.reconnect_attempt = None;
        inner.state = SlotState::Ready;
        inner.last_activity = now_millis();
        inner.owner_generation = owner_generation;
        inner.owner_lease_epoch = 0;
        inner.owner_lease_deadline = owner_lease_deadline;
        self.notify.notify_waiters();
    }

    fn retire(&self) -> Option<Arc<IpcClient>> {
        let mut inner = self.inner.lock();
        inner.state = SlotState::Retired;
        inner.reconnect_attempt = None;
        let client = inner.client.take();
        #[cfg(test)]
        self.clear_test_connected_override(&mut inner);
        drop(inner);
        self.notify.notify_waiters();
        client
    }

    fn should_evict(&self, inner: &SlotInner, idle_timeout_ms: u64) -> bool {
        if inner.active_requests > 0 {
            return false;
        }
        let cutoff = now_millis().saturating_sub(idle_timeout_ms);
        match &inner.client {
            Some(client) if !self.client_is_connected(inner, client) => true,
            Some(_) => inner.state == SlotState::Ready && inner.last_activity <= cutoff,
            None => false,
        }
    }
}

fn replacement_evidence_allows_locked(
    inner: &SlotInner,
    token: &OwnerToken,
    evidence: OwnerReplacementEvidence,
) -> bool {
    if inner.owner_generation != token.owner_generation
        || inner.owner_lease_epoch != token.owner_lease_epoch
    {
        return false;
    }

    match evidence {
        OwnerReplacementEvidence::ConfirmedDead
        | OwnerReplacementEvidence::ConfirmedRouteMissing => {
            inner.active_requests == 0 && inner.state.is_replaceable()
        }
    }
}

impl SlotState {
    fn is_replaceable(self) -> bool {
        matches!(
            self,
            SlotState::OwnerOnly | SlotState::Evicted | SlotState::Disconnected
        )
    }
}

impl UpstreamLease {
    pub fn client(&self) -> Arc<IpcClient> {
        self.client.clone()
    }

    pub fn address(&self) -> String {
        self.slot.address()
    }

    pub fn endpoint(&self) -> UpstreamEndpointKey {
        self.slot.key.clone()
    }

    pub fn evict_current_client(&self) -> Option<Arc<IpcClient>> {
        self.slot.evict_client(&self.client)
    }
}

impl Drop for UpstreamLease {
    fn drop(&mut self) {
        self.slot.end_request();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    // Poll the actual acquire future to register its Notify waiter, without
    // relying on task scheduling or sleeps to establish the cancellation race.
    fn poll_pending<F: Future>(future: std::pin::Pin<&mut F>) -> Arc<WakeCounter> {
        let counter = Arc::new(WakeCounter::default());
        let waker = futures::task::waker(counter.clone());
        let mut context = std::task::Context::from_waker(&waker);
        assert!(future.poll(&mut context).is_pending());
        counter
    }

    fn key(address: &str) -> UpstreamEndpointKey {
        UpstreamEndpointKey::new(address, "server-grid", "server-grid-instance")
    }

    #[test]
    fn new_pool_is_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.list_connections().is_empty());
    }

    #[test]
    fn insert_and_lookup_ready() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://test");
        let client = Arc::new(
            IpcClient::with_config("ipc://test", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
    }

    #[test]
    fn regular_pool_uses_physical_client_liveness() {
        let pool = ConnectionPool::with_owner_lease_duration(None);
        let endpoint = key("ipc://physical");
        let client = Arc::new(
            IpcClient::with_config(endpoint.address(), c2_config::ClientIpcConfig::default())
                .unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::Disconnected { .. }
        ));
    }

    #[test]
    fn insert_owner_only_slot_is_not_idle_evictable() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://test");
        pool.insert_owner(endpoint.clone());
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::OwnerOnly { .. }
        ));
        assert!(pool.idle_entries(0).is_empty());
        assert!(pool.evict_idle(0).is_empty());
    }

    #[test]
    fn owner_routes_on_same_endpoint_share_one_slot() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://same");
        pool.insert_owner(endpoint.clone());
        pool.insert_owner(endpoint);

        assert_eq!(
            pool.list_connections().len(),
            1,
            "relay upstream pool must be keyed by endpoint, not route name"
        );
    }

    #[test]
    fn repeated_owner_insert_for_same_endpoint_renews_owner_token_without_repairing_client() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://same");
        pool.insert_owner(endpoint.clone());
        let stale = pool.owner_token(&endpoint).unwrap();
        pool.insert_owner(endpoint.clone());
        assert!(
            !pool.matches_owner_token(&endpoint, &stale),
            "fresh owner activity on the same endpoint must invalidate captured replacement proofs"
        );
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::OwnerOnly { .. }
        ));
    }

    #[test]
    fn lookup_distinguishes_ready_evicted_disconnected_and_missing() {
        let pool = ConnectionPool::new();
        let ready_key = key("ipc://ready");
        let disconnected_key = key("ipc://disconnected");
        let evicted_key = key("ipc://evicted");
        let missing_key = key("ipc://missing");
        let ready = Arc::new(
            IpcClient::with_config("ipc://ready", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(ready_key.clone(), ready);
        let disconnected = Arc::new(
            IpcClient::with_config("ipc://disconnected", c2_config::ClientIpcConfig::default())
                .unwrap(),
        );
        pool.insert(disconnected_key.clone(), disconnected);
        pool.set_connected_for_test(&disconnected_key, false);
        let evicted = Arc::new(
            IpcClient::with_config("ipc://evicted", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(evicted_key.clone(), evicted);
        pool.evict(&evicted_key);

        match pool.lookup(&ready_key) {
            CachedClient::Ready { endpoint, .. } => {
                assert_eq!(endpoint.address(), "ipc://ready");
            }
            _ => panic!("ready connection should be explicit"),
        }
        match pool.lookup(&evicted_key) {
            CachedClient::Evicted { endpoint } => {
                assert_eq!(endpoint.address(), "ipc://evicted");
            }
            _ => panic!("evicted connection should be explicit"),
        }
        match pool.lookup(&disconnected_key) {
            CachedClient::Disconnected { endpoint } => {
                assert_eq!(endpoint.address(), "ipc://disconnected");
            }
            _ => panic!("disconnected connection should be explicit"),
        }
        assert!(matches!(pool.lookup(&missing_key), CachedClient::Missing));
    }

    #[tokio::test]
    async fn owner_only_slot_acquires_data_plane_client_lazily() {
        let pool = Arc::new(ConnectionPool::new());
        let endpoint = key("ipc://lazy");
        pool.insert_owner(endpoint.clone());
        let attempts = Arc::new(AtomicUsize::new(0));
        let lease = pool
            .acquire_with(&endpoint, {
                let attempts = attempts.clone();
                move |endpoint| {
                    let attempts = attempts.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        let client = Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        );
                        Ok(client)
                    }
                }
            })
            .await
            .expect("owner-only slot should connect on first data-plane acquire");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
        drop(lease);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
    }

    #[tokio::test]
    async fn cancelled_connector_wakes_waiters_and_allows_reacquire() {
        for initial in [
            SlotState::OwnerOnly,
            SlotState::Evicted,
            SlotState::Disconnected,
            SlotState::Ready,
        ] {
            let pool = ConnectionPool::new();
            let endpoint = key("ipc://cancelled");
            match initial {
                SlotState::OwnerOnly => pool.insert_owner(endpoint.clone()),
                SlotState::Evicted => {
                    let client = Arc::new(
                        IpcClient::with_config(
                            endpoint.address(),
                            c2_config::ClientIpcConfig::default(),
                        )
                        .unwrap(),
                    );
                    pool.insert(endpoint.clone(), client);
                    pool.evict(&endpoint);
                }
                SlotState::Disconnected | SlotState::Ready => {
                    pool.insert(
                        endpoint.clone(),
                        Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        ),
                    );
                    pool.set_connected_for_test(&endpoint, false);
                    if initial == SlotState::Disconnected {
                        pool.lookup(&endpoint);
                    }
                }
                _ => unreachable!(),
            }
            let owner = pool.owner_token(&endpoint).unwrap();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let finish_rx = Mutex::new(Some(finish_rx));
            let mut connecting = Box::pin(pool.acquire_with(&endpoint, |_| {
                let rx = finish_rx.lock().take().unwrap();
                async move { rx.await.unwrap() }
            }));
            poll_pending(connecting.as_mut());
            assert_eq!(owner.slot.inner.lock().state, SlotState::Reconnecting);
            let attempts = AtomicUsize::new(0);
            let connector = |endpoint: UpstreamEndpointKey| {
                attempts.fetch_add(1, Ordering::SeqCst);
                let client = Arc::new(
                    IpcClient::with_config(
                        endpoint.address(),
                        c2_config::ClientIpcConfig::default(),
                    )
                    .unwrap(),
                );
                std::future::ready(Ok(client))
            };
            let mut waiter_a = Box::pin(pool.acquire_with(&endpoint, &connector));
            let mut waiter_b = Box::pin(pool.acquire_with(&endpoint, &connector));
            let wake_a = poll_pending(waiter_a.as_mut());
            let wake_b = poll_pending(waiter_b.as_mut());
            assert_eq!(attempts.load(Ordering::SeqCst), 0);

            drop(connecting);
            assert!(finish_tx.is_closed(), "connector future must be cancelled");
            assert!(wake_a.0.load(Ordering::SeqCst) > 0);
            assert!(wake_b.0.load(Ordering::SeqCst) > 0);
            let (lease_a, lease_b) = tokio::time::timeout(Duration::from_secs(1), async {
                (waiter_a.await.unwrap(), waiter_b.await.unwrap())
            })
            .await
            .expect("cancelled connector stranded acquire in Reconnecting");
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert!(Arc::ptr_eq(&lease_a.client(), &lease_b.client()));
            assert!(pool.matches_owner_token(&endpoint, &owner));
            assert_eq!(owner.slot.inner.lock().active_requests, 2);
            drop((lease_a, lease_b));
            assert_eq!(owner.slot.inner.lock().active_requests, 0);
        }
    }

    #[tokio::test]
    async fn connector_success_and_error_wake_waiters_and_preserve_owner() {
        for succeed in [true, false] {
            let pool = ConnectionPool::new();
            let endpoint = key("ipc://completion");
            pool.insert_owner(endpoint.clone());
            let owner = pool.owner_token(&endpoint).unwrap();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let finish_rx = Mutex::new(Some(finish_rx));
            let mut connecting = Box::pin(pool.acquire_with(&endpoint, |_| {
                let rx = finish_rx.lock().take().unwrap();
                async move { rx.await.unwrap() }
            }));
            poll_pending(connecting.as_mut());

            let replacement = Arc::new(
                IpcClient::with_config(endpoint.address(), c2_config::ClientIpcConfig::default())
                    .unwrap(),
            );
            let attempts = AtomicUsize::new(0);
            let connector = |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(replacement.clone()))
            };
            let mut waiter = Box::pin(pool.acquire_with(&endpoint, connector));
            let wake = poll_pending(waiter.as_mut());
            let connected = Arc::new(
                IpcClient::with_config(endpoint.address(), c2_config::ClientIpcConfig::default())
                    .unwrap(),
            );
            finish_tx
                .send(if succeed {
                    Ok(connected.clone())
                } else {
                    // Nonretryable: test the slot transition, without retry timers.
                    Err(c2_ipc::IpcError::Io(std::io::Error::new(
                        ErrorKind::InvalidInput,
                        "controlled connector failure",
                    )))
                })
                .unwrap_or_else(|_| panic!("connector receiver dropped"));
            let result = connecting.await;
            if succeed {
                let lease = result.unwrap();
                assert!(Arc::ptr_eq(&lease.client(), &connected));
                drop(lease);
                assert_eq!(owner.slot.inner.lock().state, SlotState::Ready);
            } else {
                assert!(matches!(result, Err(AcquireError::Unreachable { .. })));
                assert_eq!(owner.slot.inner.lock().state, SlotState::Disconnected);
            }
            assert!(wake.0.load(Ordering::SeqCst) > 0);
            assert!(pool.matches_owner_token(&endpoint, &owner));
            let lease = tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("connector completion must wake acquire")
                .unwrap();
            let expected = if succeed { &connected } else { &replacement };
            assert!(Arc::ptr_eq(&lease.client(), expected));
            assert_eq!(attempts.load(Ordering::SeqCst), usize::from(!succeed));
            drop(lease);
            assert_eq!(owner.slot.inner.lock().active_requests, 0);
        }
    }

    #[tokio::test]
    async fn retired_attempt_cancellation_success_and_error_do_not_touch_replacement_slot() {
        // None cancels; Some completes the old connector successfully or with an error.
        for completion in [None, Some(true), Some(false)] {
            let pool = ConnectionPool::new();
            let endpoint = key("ipc://replaced");
            pool.insert_owner(endpoint.clone());
            let old_owner = pool.owner_token(&endpoint).unwrap();
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let finish_rx = Mutex::new(Some(finish_rx));
            let mut connecting = Box::pin(pool.acquire_with(&endpoint, |_| {
                let rx = finish_rx.lock().take().unwrap();
                async move { rx.await.unwrap() }
            }));
            poll_pending(connecting.as_mut());
            let mut waiter = Box::pin(pool.acquire_with(&endpoint, |_| async {
                panic!("retired slot must never reconnect")
            }));
            let wake = poll_pending(waiter.as_mut());
            pool.remove(&endpoint);
            assert!(wake.0.load(Ordering::SeqCst) > 0);
            assert!(matches!(waiter.await, Err(AcquireError::NotFound)));
            pool.insert_owner(endpoint.clone());
            let current_owner = pool.owner_token(&endpoint).unwrap();
            assert!(!Arc::ptr_eq(&old_owner.slot, &current_owner.slot));
            assert!(!pool.matches_owner_token(&endpoint, &old_owner));
            let (new_tx, new_rx) = tokio::sync::oneshot::channel();
            let new_rx = Mutex::new(Some(new_rx));
            let mut current = Box::pin(pool.acquire_with(&endpoint, |_| {
                let rx = new_rx.lock().take().unwrap();
                async move { rx.await.unwrap() }
            }));
            poll_pending(current.as_mut());

            let stale_client = Arc::new(
                IpcClient::with_config(endpoint.address(), c2_config::ClientIpcConfig::default())
                    .unwrap(),
            );
            match completion {
                None => {
                    drop(connecting);
                    assert!(finish_tx.is_closed());
                }
                Some(succeed) => {
                    finish_tx
                        .send(if succeed {
                            Ok(stale_client.clone())
                        } else {
                            Err(c2_ipc::IpcError::Io(std::io::Error::new(
                                ErrorKind::ConnectionReset,
                                "retired connector failure",
                            )))
                        })
                        .unwrap_or_else(|_| panic!("connector receiver dropped"));
                    assert!(matches!(connecting.await, Err(AcquireError::NotFound)));
                }
            }
            assert_eq!(old_owner.slot.inner.lock().state, SlotState::Retired);
            assert_eq!(
                current_owner.slot.inner.lock().state,
                SlotState::Reconnecting
            );
            assert!(pool.matches_owner_token(&endpoint, &current_owner));
            let replacement = Arc::new(
                IpcClient::with_config(endpoint.address(), c2_config::ClientIpcConfig::default())
                    .unwrap(),
            );
            assert!(new_tx.send(Ok(replacement.clone())).is_ok());
            let lease = current.await.unwrap();
            assert!(Arc::ptr_eq(&lease.client(), &replacement));
            drop(lease);
            assert_eq!(current_owner.slot.inner.lock().active_requests, 0);
        }
    }

    #[tokio::test]
    async fn superseded_attempt_does_not_rollback_ready_client_or_new_attempt() {
        for new_attempt in [false, true] {
            for completion in [None, Some(true), Some(false)] {
                let pool = ConnectionPool::new();
                let endpoint = key("ipc://superseded");
                pool.insert_owner(endpoint.clone());
                let old_owner = pool.owner_token(&endpoint).unwrap();
                let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
                let finish_rx = Mutex::new(Some(finish_rx));
                let mut connecting = Box::pin(pool.acquire_with(&endpoint, |_| {
                    let rx = finish_rx.lock().take().unwrap();
                    async move { rx.await.unwrap() }
                }));
                poll_pending(connecting.as_mut());
                let replacement = Arc::new(
                    IpcClient::with_config(
                        endpoint.address(),
                        c2_config::ClientIpcConfig::default(),
                    )
                    .unwrap(),
                );
                pool.reconnect(&endpoint, replacement.clone());
                let current_owner = pool.owner_token(&endpoint).unwrap();
                assert!(Arc::ptr_eq(&old_owner.slot, &current_owner.slot));
                assert!(!pool.matches_owner_token(&endpoint, &old_owner));

                let (new_tx, new_rx) = tokio::sync::oneshot::channel();
                let new_rx = Mutex::new(Some(new_rx));
                let mut current = Box::pin(pool.acquire_with(&endpoint, |_| {
                    let rx = new_rx.lock().take().unwrap();
                    async move { rx.await.unwrap() }
                }));
                if new_attempt {
                    pool.evict(&endpoint);
                    poll_pending(current.as_mut());
                }
                let stale_client = Arc::new(
                    IpcClient::with_config(
                        endpoint.address(),
                        c2_config::ClientIpcConfig::default(),
                    )
                    .unwrap(),
                );
                match completion {
                    None => drop(connecting),
                    Some(succeed) => {
                        finish_tx
                            .send(if succeed {
                                Ok(stale_client.clone())
                            } else {
                                Err(c2_ipc::IpcError::Io(std::io::Error::new(
                                    ErrorKind::ConnectionReset,
                                    "superseded connector failure",
                                )))
                            })
                            .unwrap_or_else(|_| panic!("connector receiver dropped"));
                        assert!(matches!(connecting.await, Err(AcquireError::NotFound)));
                    }
                }
                let expected_state = if new_attempt {
                    SlotState::Reconnecting
                } else {
                    SlotState::Ready
                };
                assert_eq!(current_owner.slot.inner.lock().state, expected_state);
                assert!(pool.matches_owner_token(&endpoint, &current_owner));
                if new_attempt {
                    assert!(new_tx.send(Ok(replacement.clone())).is_ok());
                }
                let lease = current.await.unwrap();
                assert!(Arc::ptr_eq(&lease.client(), &replacement));
                drop(lease);
                assert_eq!(current_owner.slot.inner.lock().active_requests, 0);
            }
        }
    }

    #[test]
    fn lookup_returns_disconnected_for_disconnected() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://test");
        let client = Arc::new(
            IpcClient::with_config("ipc://test", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        // Client starts disconnected.
        pool.insert(endpoint.clone(), client);
        pool.set_connected_for_test(&endpoint, false);
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::Disconnected { .. }
        ));
    }

    #[test]
    fn evict_and_reconnect() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://test");
        let c1 = Arc::new(
            IpcClient::with_config("ipc://test", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), c1);
        pool.evict(&endpoint);
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::Evicted { .. }
        ));

        let c2 = Arc::new(
            IpcClient::with_config("ipc://test", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.reconnect(&endpoint, c2);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
    }

    #[test]
    fn remove_deletes_entry() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://test");
        let client = Arc::new(
            IpcClient::with_config("ipc://test", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        pool.remove(&endpoint);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Missing));
    }

    #[test]
    fn idle_entries_detects_disconnected() {
        let pool = ConnectionPool::new();
        let client = Arc::new(
            IpcClient::with_config("ipc://dead", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(key("ipc://dead"), client);
        pool.set_connected_for_test(&key("ipc://dead"), false);
        assert_eq!(pool.idle_entries(u64::MAX).len(), 1);
    }

    #[test]
    fn idle_entries_do_not_evict_active_connected_client() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://active");
        let client = Arc::new(
            IpcClient::with_config("ipc://active", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);

        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        assert!(pool.idle_entries(0).is_empty());
        drop(lease);
    }

    #[test]
    fn idle_entries_can_evict_inactive_connected_client() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://idle");
        let client = Arc::new(
            IpcClient::with_config("ipc://idle", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);

        assert_eq!(pool.idle_entries(0), vec![endpoint]);
    }

    #[test]
    fn end_request_makes_client_idle_candidate_again() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://active");
        let client = Arc::new(
            IpcClient::with_config("ipc://active", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);

        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        drop(lease);

        assert_eq!(pool.idle_entries(0), vec![endpoint]);
    }

    #[test]
    fn disconnected_client_is_evicted_even_when_not_idle_by_time() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://dead");
        let client = Arc::new(
            IpcClient::with_config("ipc://dead", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        pool.set_connected_for_test(&endpoint, false);

        assert_eq!(pool.idle_entries(u64::MAX), vec![endpoint]);
    }

    #[test]
    fn active_disconnected_client_is_not_evicted_until_request_finishes() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://active-dead");
        let client = Arc::new(
            IpcClient::with_config("ipc://active-dead", c2_config::ClientIpcConfig::default())
                .unwrap(),
        );
        pool.insert(endpoint.clone(), client.clone());

        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        pool.set_connected_for_test(&endpoint, false);
        assert!(pool.idle_entries(u64::MAX).is_empty());
        assert_eq!(pool.evict_idle(u64::MAX).len(), 0);

        drop(lease);
        assert_eq!(pool.idle_entries(u64::MAX), vec![endpoint]);
    }

    #[test]
    fn stale_lease_drop_after_reinsert_does_not_touch_new_slot() {
        let pool = ConnectionPool::new();
        let old_endpoint = key("ipc://old");
        let new_endpoint = key("ipc://new");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(old_endpoint.clone(), old_client);
        let old_lease = pool.begin_request_for_test(&old_endpoint).unwrap();
        pool.remove(&old_endpoint);

        let new_client = Arc::new(
            IpcClient::with_config("ipc://new", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(new_endpoint.clone(), new_client);
        drop(old_lease);

        assert_eq!(pool.idle_entries(0), vec![new_endpoint]);
    }

    #[test]
    fn stale_lease_drop_after_reinsert_does_not_release_new_active_request() {
        let pool = ConnectionPool::new();
        let old_endpoint = key("ipc://old");
        let new_endpoint = key("ipc://new");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(old_endpoint.clone(), old_client);
        let old_lease = pool.begin_request_for_test(&old_endpoint).unwrap();
        pool.remove(&old_endpoint);

        let new_client = Arc::new(
            IpcClient::with_config("ipc://new", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(new_endpoint.clone(), new_client);
        let new_lease = pool.begin_request_for_test(&new_endpoint).unwrap();

        drop(old_lease);
        assert!(pool.idle_entries(0).is_empty());

        drop(new_lease);
        assert_eq!(pool.idle_entries(0), vec![new_endpoint]);
    }

    #[test]
    fn stale_lease_evict_after_reinsert_does_not_evict_new_entry() {
        let pool = ConnectionPool::new();
        let old_endpoint = key("ipc://old");
        let new_endpoint = key("ipc://new");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(old_endpoint.clone(), old_client);
        let old_lease = pool.begin_request_for_test(&old_endpoint).unwrap();
        pool.remove(&old_endpoint);

        let new_client = Arc::new(
            IpcClient::with_config("ipc://new", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(new_endpoint.clone(), new_client);
        assert!(
            pool.evict_client(&new_endpoint, &old_lease.client())
                .is_none()
        );
        assert!(matches!(
            pool.lookup(&new_endpoint),
            CachedClient::Ready { .. }
        ));
    }

    #[test]
    fn matching_client_evict_removes_current_entry_client() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://current");
        let client = Arc::new(
            IpcClient::with_config("ipc://current", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        assert!(pool.evict_client(&endpoint, &lease.client()).is_some());
        assert!(matches!(
            pool.lookup(&endpoint),
            CachedClient::Evicted { .. }
        ));
    }

    #[test]
    fn stale_client_evict_after_reconnect_does_not_evict_reconnected_client() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://old");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let old_lease = pool.begin_request_for_test(&endpoint).unwrap();
        assert!(pool.evict_client(&endpoint, &old_lease.client()).is_some());

        let new_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.reconnect(&endpoint, new_client);
        assert!(pool.evict_client(&endpoint, &old_lease.client()).is_none());
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
    }

    #[test]
    fn stale_lease_drop_after_reconnect_does_not_release_new_active_request() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://old");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let old_lease = pool.begin_request_for_test(&endpoint).unwrap();

        let new_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.reconnect(&endpoint, new_client);
        assert!(pool.idle_entries(0).is_empty());

        let new_lease = pool.begin_request_for_test(&endpoint).unwrap();

        drop(old_lease);
        assert!(pool.idle_entries(0).is_empty());

        drop(new_lease);
        assert_eq!(pool.idle_entries(0), vec![endpoint]);
    }

    #[test]
    fn stale_owner_token_after_reinsert_does_not_match_new_entry() {
        let pool = ConnectionPool::new();
        let old_endpoint = key("ipc://old");
        let current_endpoint = key("ipc://current");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(old_endpoint.clone(), old_client);
        let token = pool.owner_token(&old_endpoint).unwrap();
        pool.remove(&old_endpoint);

        let current_client = Arc::new(
            IpcClient::with_config("ipc://current", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(current_endpoint.clone(), current_client.clone());
        assert!(!pool.matches_owner_token(&current_endpoint, &token));

        let lease = pool.begin_request_for_test(&current_endpoint).unwrap();
        assert!(Arc::ptr_eq(&lease.client(), &current_client));
        assert!(matches!(
            pool.lookup(&current_endpoint),
            CachedClient::Ready { .. }
        ));
    }

    #[test]
    fn owner_token_does_not_match_same_slot_after_reconnect() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://old");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        pool.set_connected_for_test(&endpoint, false);

        let new_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.reconnect(&endpoint, new_client.clone());

        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        assert!(Arc::ptr_eq(&lease.client(), &new_client));
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
        assert!(!pool.matches_owner_token(&endpoint, &token));
    }

    #[test]
    fn owner_token_does_not_match_reinsert_with_same_address() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://same");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://same", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        pool.remove(&endpoint);

        let new_client = Arc::new(
            IpcClient::with_config("ipc://same", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), new_client);
        assert!(!pool.matches_owner_token(&endpoint, &token));
    }

    #[test]
    fn owner_token_does_not_match_after_lease_renewal() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://same-slot");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://same-slot", c2_config::ClientIpcConfig::default())
                .unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        assert!(pool.renew_owner_lease(&endpoint, &token));
        assert!(!pool.matches_owner_token(&endpoint, &token));
    }

    #[test]
    fn current_owner_lease_renewal_is_not_stale_token_based() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://same-slot");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://same-slot", c2_config::ClientIpcConfig::default())
                .unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        assert!(pool.renew_current_owner_lease(&endpoint));
        assert!(pool.renew_current_owner_lease(&endpoint));
        assert!(!pool.matches_owner_token(&endpoint, &token));
    }

    #[test]
    fn disabled_owner_lease_deadline_does_not_block_evidence_based_replacement_validation() {
        let pool = ConnectionPool::with_owner_lease_duration(None);
        let endpoint = key("ipc://old");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        assert!(token.slot.inner.lock().owner_lease_deadline.is_none());
        assert!(pool.renew_current_owner_lease(&endpoint));
        assert!(token.slot.inner.lock().owner_lease_deadline.is_none());
        pool.evict(&endpoint);

        let result = pool.validate_replaceable_owner_token(
            &endpoint,
            &pool.owner_token(&endpoint).unwrap(),
            OwnerReplacementEvidence::ConfirmedDead,
        );
        assert!(result.is_ok());
        assert_eq!(pool.get_address(&endpoint).as_deref(), Some("ipc://old"));
    }

    #[test]
    fn validate_replaceable_owner_token_rejects_active_disconnected_slot() {
        let pool = ConnectionPool::new();
        let endpoint = key("ipc://old");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), old_client);
        let token = pool.owner_token(&endpoint).unwrap();
        let lease = pool.begin_request_for_test(&endpoint).unwrap();
        pool.set_connected_for_test(&endpoint, false);

        let result = pool.validate_replaceable_owner_token(
            &endpoint,
            &token,
            OwnerReplacementEvidence::ConfirmedDead,
        );
        assert!(matches!(result, Err(OwnerReplaceError::NotReplaceable)));
        assert!(pool.matches_owner_token(&endpoint, &token));
        drop(lease);
    }

    #[tokio::test]
    async fn concurrent_acquire_after_eviction_shares_one_reconnect() {
        let pool = Arc::new(ConnectionPool::new());
        let endpoint = key("ipc://shared");
        let client = Arc::new(
            IpcClient::with_config("ipc://shared", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        pool.evict(&endpoint);

        let connect_count = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..16 {
            let pool = pool.clone();
            let endpoint = endpoint.clone();
            let connect_count = connect_count.clone();
            tasks.push(tokio::spawn(async move {
                pool.acquire_with(&endpoint, move |endpoint| {
                    let connect_count = connect_count.clone();
                    async move {
                        connect_count.fetch_add(1, Ordering::SeqCst);
                        let client = Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        );
                        Ok(client)
                    }
                })
                .await
                .expect("acquire should reconnect")
            }));
        }

        let mut leases = Vec::new();
        for task in tasks {
            leases.push(task.await.unwrap());
        }

        assert_eq!(connect_count.load(Ordering::SeqCst), 1);
        let first = leases[0].client();
        assert!(
            leases
                .iter()
                .all(|lease| Arc::ptr_eq(&first, &lease.client()))
        );
    }

    #[tokio::test]
    async fn acquire_after_eviction_retries_one_transient_connector_error() {
        let pool = Arc::new(ConnectionPool::new());
        let endpoint = key("ipc://flaky");
        let client = Arc::new(
            IpcClient::with_config("ipc://flaky", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        pool.evict(&endpoint);
        let attempts = Arc::new(AtomicUsize::new(0));
        let lease = pool
            .acquire_with(&endpoint, {
                let attempts = attempts.clone();
                move |endpoint| {
                    let attempts = attempts.clone();
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            return Err(c2_ipc::IpcError::Io(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "peer reset during idle reconnect",
                            )));
                        }
                        let client = Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        );
                        Ok(client)
                    }
                }
            })
            .await
            .expect("transient reconnect reset should be retried");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(lease.endpoint(), endpoint);
        assert!(matches!(pool.lookup(&endpoint), CachedClient::Ready { .. }));
    }

    #[tokio::test]
    async fn remove_during_reconnect_makes_waiting_acquire_not_found() {
        let pool = Arc::new(ConnectionPool::new());
        let endpoint = key("ipc://removed");
        let client = Arc::new(
            IpcClient::with_config("ipc://removed", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(endpoint.clone(), client);
        pool.evict(&endpoint);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let finish_rx = Arc::new(tokio::sync::Mutex::new(Some(finish_rx)));

        let acquire = {
            let pool = pool.clone();
            let endpoint = endpoint.clone();
            let finish_rx = finish_rx.clone();
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                pool.acquire_with(&endpoint, move |endpoint| {
                    let finish_rx = finish_rx.clone();
                    let started_tx = started_tx.clone();
                    async move {
                        if let Some(started_tx) = started_tx.lock().unwrap().take() {
                            let _ = started_tx.send(());
                        }
                        let rx = finish_rx.lock().await.take().unwrap();
                        rx.await.unwrap();
                        let client = Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        );
                        Ok(client)
                    }
                })
                .await
            })
        };

        started_rx.await.unwrap();
        pool.remove(&endpoint);
        finish_tx.send(()).unwrap();
        assert!(matches!(
            acquire.await.unwrap(),
            Err(AcquireError::NotFound)
        ));
    }

    #[tokio::test]
    async fn insert_retires_replaced_slot_before_waiting_reconnect_completes() {
        let pool = Arc::new(ConnectionPool::new());
        let old_endpoint = key("ipc://old");
        let new_endpoint = key("ipc://new");
        let old_client = Arc::new(
            IpcClient::with_config("ipc://old", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.insert(old_endpoint.clone(), old_client);
        pool.evict(&old_endpoint);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let finish_rx = Arc::new(tokio::sync::Mutex::new(Some(finish_rx)));

        let acquire = {
            let pool = pool.clone();
            let old_endpoint = old_endpoint.clone();
            let finish_rx = finish_rx.clone();
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                pool.acquire_with(&old_endpoint, move |endpoint| {
                    let finish_rx = finish_rx.clone();
                    let started_tx = started_tx.clone();
                    async move {
                        if let Some(started_tx) = started_tx.lock().unwrap().take() {
                            let _ = started_tx.send(());
                        }
                        let rx = finish_rx.lock().await.take().unwrap();
                        rx.await.unwrap();
                        let client = Arc::new(
                            IpcClient::with_config(
                                endpoint.address(),
                                c2_config::ClientIpcConfig::default(),
                            )
                            .unwrap(),
                        );
                        Ok(client)
                    }
                })
                .await
            })
        };

        started_rx.await.unwrap();
        let replacement = Arc::new(
            IpcClient::with_config("ipc://new", c2_config::ClientIpcConfig::default()).unwrap(),
        );
        pool.remove(&old_endpoint);
        pool.insert(new_endpoint.clone(), replacement.clone());
        finish_tx.send(()).unwrap();
        assert!(matches!(
            acquire.await.unwrap(),
            Err(AcquireError::NotFound)
        ));
        let current = pool.begin_request_for_test(&new_endpoint).unwrap();
        assert!(Arc::ptr_eq(&current.client(), &replacement));
    }
}
