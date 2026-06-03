use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use c2_ipc::IpcClient;
use parking_lot::Mutex;
use tokio::sync::Notify;

const RECONNECT_MAX_ATTEMPTS: usize = 2;
const RECONNECT_RETRY_DELAY_MS: u64 = 10;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_retryable_connect_error(error: &c2_ipc::IpcError) -> bool {
    let c2_ipc::IpcError::Io(error) = error else {
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

/// A single route/address upstream slot.
struct UpstreamSlot {
    inner: Mutex<SlotInner>,
    notify: Notify,
}

struct SlotInner {
    address: String,
    client: Option<Arc<IpcClient>>,
    last_activity: u64,
    active_requests: usize,
    state: SlotState,
    owner_generation: u64,
    owner_lease_epoch: u64,
    owner_lease_deadline: Option<Instant>,
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

#[derive(Debug)]
pub enum AcquireError {
    NotFound,
    Unreachable {
        address: String,
        error: c2_ipc::IpcError,
    },
}

pub struct UpstreamLease {
    slot: Arc<UpstreamSlot>,
    client: Arc<IpcClient>,
}

pub enum CachedClient {
    Ready { address: String },
    OwnerOnly { address: String },
    Evicted { address: String },
    Disconnected { address: String },
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
    address: String,
    owner_generation: u64,
    owner_lease_epoch: u64,
}

pub type OwnerToken = OwnerSlotToken;

struct PoolInner {
    entries: HashMap<String, Arc<UpstreamSlot>>,
    next_owner_generation: u64,
}

/// Pool of IPC connections keyed by route name.
///
/// Separated from RouteTable to keep route metadata independent of
/// connection lifecycle. Supports lazy reconnection and idle eviction.
pub struct ConnectionPool {
    inner: Mutex<PoolInner>,
    owner_lease_duration: Option<Duration>,
}

impl ConnectionPool {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_owner_lease_duration(None)
    }

    pub fn with_owner_lease_duration(owner_lease_duration: Option<Duration>) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                entries: HashMap::new(),
                next_owner_generation: 1,
            }),
            owner_lease_duration,
        }
    }

    fn slot(&self, name: &str) -> Option<Arc<UpstreamSlot>> {
        self.inner.lock().entries.get(name).cloned()
    }

    fn slot_matches(&self, name: &str, expected: &Arc<UpstreamSlot>) -> bool {
        self.slot(name)
            .is_some_and(|current| Arc::ptr_eq(&current, expected))
    }

    fn owner_lease_deadline(&self) -> Option<Instant> {
        self.owner_lease_duration
            .and_then(|duration| Instant::now().checked_add(duration))
    }

    pub async fn acquire_with<C, Fut>(
        &self,
        name: &str,
        connector: C,
    ) -> Result<UpstreamLease, AcquireError>
    where
        C: Fn(String) -> Fut,
        Fut: Future<Output = Result<Arc<IpcClient>, c2_ipc::IpcError>>,
    {
        let Some(slot) = self.slot(name) else {
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

    /// Insert a pre-connected client for a route name.
    #[cfg(test)]
    pub fn insert(&self, name: String, address: String, client: Arc<IpcClient>) {
        let mut inner = self.inner.lock();
        let owner_generation = next_owner_generation(&mut inner);
        let old_slot = inner.entries.insert(
            name,
            Arc::new(UpstreamSlot::new(
                address,
                Some(client),
                SlotState::Ready,
                owner_generation,
                self.owner_lease_deadline(),
            )),
        );
        drop(inner);
        if let Some(old_slot) = old_slot {
            if let Some(client) = old_slot.retire() {
                close_replaced_client(client);
            }
        }
    }

    /// Insert route owner metadata without attaching a relay data-plane client.
    pub fn insert_owner(&self, name: String, address: String) {
        let mut inner = self.inner.lock();
        let owner_generation = next_owner_generation(&mut inner);
        let old_slot = inner.entries.insert(
            name,
            Arc::new(UpstreamSlot::new(
                address,
                None,
                SlotState::OwnerOnly,
                owner_generation,
                self.owner_lease_deadline(),
            )),
        );
        drop(inner);
        if let Some(old_slot) = old_slot {
            if let Some(client) = old_slot.retire() {
                close_replaced_client(client);
            }
        }
    }

    pub fn lookup(&self, name: &str) -> CachedClient {
        let Some(slot) = self.slot(name) else {
            return CachedClient::Missing;
        };
        slot.lookup()
    }

    /// Get stored address for reconnection.
    #[cfg(test)]
    pub fn get_address(&self, name: &str) -> Option<String> {
        self.slot(name).map(|slot| slot.address())
    }

    /// Capture the current owner identity before awaiting a probe.
    pub fn owner_token(&self, name: &str) -> Option<OwnerToken> {
        let slot = self.slot(name)?;
        slot.owner_token()
    }

    /// Names of entries to evict (dead or idle beyond timeout_ms).
    pub fn idle_entries(&self, idle_timeout_ms: u64) -> Vec<String> {
        self.inner
            .lock()
            .entries
            .iter()
            .filter(|(_, slot)| slot.is_idle_candidate(idle_timeout_ms))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Evict idle clients with a slot-local recheck before removing them.
    pub fn evict_idle(&self, idle_timeout_ms: u64) -> Vec<(String, Option<Arc<IpcClient>>)> {
        let names = self.idle_entries(idle_timeout_ms);
        names
            .into_iter()
            .map(|name| {
                let client = self
                    .slot(&name)
                    .and_then(|slot| slot.evict_if_idle(idle_timeout_ms));
                (name, client)
            })
            .collect()
    }

    /// Evict a client — returns old Arc for async close.
    #[cfg(test)]
    pub fn evict(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.slot(name)?.evict()
    }

    /// Evict a request client only if it is still the current cached client.
    #[cfg(test)]
    pub fn evict_client(&self, name: &str, client: &Arc<IpcClient>) -> Option<Arc<IpcClient>> {
        self.slot(name)?.evict_client(client)
    }

    /// Re-attach a freshly connected client.
    #[cfg(test)]
    pub fn reconnect(&self, name: &str, client: Arc<IpcClient>) {
        if let Some(slot) = self.slot(name) {
            let owner_generation = {
                let mut inner = self.inner.lock();
                next_owner_generation(&mut inner)
            };
            slot.reconnect(client, owner_generation, self.owner_lease_deadline());
        }
    }

    /// Remove entry entirely.
    pub fn remove(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.inner
            .lock()
            .entries
            .remove(name)
            .and_then(|slot| slot.retire())
    }

    /// List route names with addresses.
    #[cfg(test)]
    pub fn list_connections(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .entries
            .iter()
            .map(|(n, slot)| (n.clone(), slot.address()))
            .collect()
    }

    pub fn matches_owner_token(&self, name: &str, token: &OwnerToken) -> bool {
        self.slot_matches(name, &token.slot) && token.slot.matches_owner_token(token)
    }

    pub fn renew_owner_lease(&self, name: &str, token: &OwnerToken) -> bool {
        self.slot_matches(name, &token.slot)
            && token
                .slot
                .renew_owner_lease(token, self.owner_lease_deadline())
    }

    pub fn renew_current_owner_lease(&self, name: &str) -> bool {
        let Some(slot) = self.slot(name) else {
            return false;
        };
        slot.renew_current_owner_lease(self.owner_lease_deadline())
    }

    pub fn replace_if_owner_token(
        &self,
        name: &str,
        token: &OwnerToken,
        new_address: String,
        evidence: OwnerReplacementEvidence,
    ) -> Result<Option<Arc<IpcClient>>, OwnerReplaceError> {
        let evidence: OwnerReplacementEvidence = evidence;
        let mut inner = self.inner.lock();
        let Some(current) = inner.entries.get(name).cloned() else {
            return Err(OwnerReplaceError::StaleToken);
        };
        if !Arc::ptr_eq(&current, &token.slot) {
            return Err(OwnerReplaceError::StaleToken);
        }

        let old_client = {
            let mut locked = current.inner.lock();
            if locked.address != token.address
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
            locked.state = SlotState::Retired;
            locked.client.take()
        };

        let owner_generation = next_owner_generation(&mut inner);
        inner.entries.insert(
            name.to_string(),
            Arc::new(UpstreamSlot::new(
                new_address,
                None,
                SlotState::OwnerOnly,
                owner_generation,
                self.owner_lease_deadline(),
            )),
        );
        drop(inner);
        current.notify.notify_waiters();
        Ok(old_client)
    }

    #[cfg(test)]
    fn begin_request_for_test(&self, name: &str) -> Option<UpstreamLease> {
        self.slot(name)?.begin_request_for_test()
    }
}

fn next_owner_generation(inner: &mut PoolInner) -> u64 {
    let generation = inner.next_owner_generation;
    inner.next_owner_generation = inner.next_owner_generation.saturating_add(1);
    generation
}

fn close_replaced_client(client: Arc<IpcClient>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { client.close_shared().await });
    }
}

impl UpstreamSlot {
    fn new(
        address: String,
        client: Option<Arc<IpcClient>>,
        state: SlotState,
        owner_generation: u64,
        owner_lease_deadline: Option<Instant>,
    ) -> Self {
        Self {
            inner: Mutex::new(SlotInner {
                address,
                client,
                last_activity: now_millis(),
                active_requests: 0,
                state,
                owner_generation,
                owner_lease_epoch: 0,
                owner_lease_deadline,
            }),
            notify: Notify::new(),
        }
    }

    async fn acquire_with<C, Fut>(
        self: Arc<Self>,
        connector: C,
    ) -> Result<UpstreamLease, AcquireError>
    where
        C: Fn(String) -> Fut,
        Fut: Future<Output = Result<Arc<IpcClient>, c2_ipc::IpcError>>,
    {
        loop {
            let notified = self.notify.notified();
            let mut notified = pin!(notified);
            notified.as_mut().enable();
            let address = {
                let mut inner = self.inner.lock();
                match inner.state {
                    SlotState::Retired => return Err(AcquireError::NotFound),
                    SlotState::OwnerOnly => {
                        inner.state = SlotState::Reconnecting;
                        Some(inner.address.clone())
                    }
                    SlotState::Ready => {
                        if let Some(client) = inner.client.clone() {
                            if client.is_connected() {
                                inner.active_requests += 1;
                                inner.last_activity = now_millis();
                                return Ok(UpstreamLease {
                                    slot: self.clone(),
                                    client,
                                });
                            }
                        }
                        inner.client = None;
                        inner.state = SlotState::Reconnecting;
                        Some(inner.address.clone())
                    }
                    SlotState::Evicted | SlotState::Disconnected => {
                        inner.state = SlotState::Reconnecting;
                        Some(inner.address.clone())
                    }
                    SlotState::Reconnecting => None,
                }
            };

            match address {
                Some(address) => match connector(address.clone()).await {
                    Ok(client) => {
                        let acquire = {
                            let mut inner = self.inner.lock();
                            if inner.state == SlotState::Retired {
                                None
                            } else {
                                inner.client = Some(client.clone());
                                inner.state = SlotState::Ready;
                                inner.active_requests += 1;
                                inner.last_activity = now_millis();
                                Some(())
                            }
                        };
                        if acquire.is_none() {
                            client.close_shared().await;
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
                        if inner.state == SlotState::Retired {
                            drop(inner);
                            return Err(AcquireError::NotFound);
                        }
                        inner.client = None;
                        inner.state = SlotState::Disconnected;
                        inner.last_activity = now_millis();
                        drop(inner);
                        self.notify.notify_waiters();
                        return Err(AcquireError::Unreachable {
                            address,
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
                address: inner.address.clone(),
            },
            SlotState::Ready => match &inner.client {
                Some(client) if client.is_connected() => CachedClient::Ready {
                    address: inner.address.clone(),
                },
                Some(_) => {
                    inner.client = None;
                    inner.state = SlotState::Disconnected;
                    CachedClient::Disconnected {
                        address: inner.address.clone(),
                    }
                }
                None => CachedClient::Evicted {
                    address: inner.address.clone(),
                },
            },
            SlotState::Evicted => CachedClient::Evicted {
                address: inner.address.clone(),
            },
            SlotState::Disconnected | SlotState::Reconnecting => CachedClient::Disconnected {
                address: inner.address.clone(),
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
        if !client.is_connected() {
            inner.state = SlotState::Disconnected;
            inner.client = None;
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
        self.inner.lock().address.clone()
    }

    fn owner_token(self: Arc<Self>) -> Option<OwnerToken> {
        let inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        Some(OwnerToken {
            slot: self.clone(),
            address: inner.address.clone(),
            owner_generation: inner.owner_generation,
            owner_lease_epoch: inner.owner_lease_epoch,
        })
    }

    fn matches_owner_token(&self, token: &OwnerToken) -> bool {
        let inner = self.inner.lock();
        inner.address == token.address
            && inner.owner_generation == token.owner_generation
            && inner.owner_lease_epoch == token.owner_lease_epoch
            && inner.state != SlotState::Retired
    }

    fn renew_owner_lease(&self, token: &OwnerToken, deadline: Option<Instant>) -> bool {
        let mut inner = self.inner.lock();
        if inner.address != token.address
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
        should_evict(&inner, idle_timeout_ms)
    }

    fn evict_if_idle(&self, idle_timeout_ms: u64) -> Option<Arc<IpcClient>> {
        let mut inner = self.inner.lock();
        if inner.state == SlotState::Retired {
            return None;
        }
        if !should_evict(&inner, idle_timeout_ms) {
            return None;
        }
        let client = inner.client.take();
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
        let Some(current) = inner.client.as_ref() else {
            return None;
        };
        if !Arc::ptr_eq(current, expected) {
            return None;
        }
        let client = inner.client.take();
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
        let client = inner.client.take();
        drop(inner);
        self.notify.notify_waiters();
        client
    }
}

fn replacement_evidence_allows_locked(
    inner: &SlotInner,
    token: &OwnerToken,
    evidence: OwnerReplacementEvidence,
) -> bool {
    if inner.address != token.address
        || inner.owner_generation != token.owner_generation
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

fn should_evict(inner: &SlotInner, idle_timeout_ms: u64) -> bool {
    if inner.active_requests > 0 {
        return false;
    }
    let cutoff = now_millis().saturating_sub(idle_timeout_ms);
    match &inner.client {
        Some(client) if !client.is_connected() => true,
        Some(_) => inner.state == SlotState::Ready && inner.last_activity <= cutoff,
        None => false,
    }
}

impl UpstreamLease {
    pub fn client(&self) -> Arc<IpcClient> {
        self.client.clone()
    }

    pub fn address(&self) -> String {
        self.slot.address()
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

    #[test]
    fn new_pool_is_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.list_connections().is_empty());
    }

    #[test]
    fn insert_and_lookup_ready() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://test".into(), client);
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn insert_owner_only_slot_is_not_idle_evictable() {
        let pool = ConnectionPool::new();
        pool.insert_owner("grid".into(), "ipc://test".into());

        assert!(matches!(
            pool.lookup("grid"),
            CachedClient::OwnerOnly { .. }
        ));
        assert!(pool.idle_entries(0).is_empty());
        assert!(pool.evict_idle(0).is_empty());
    }

    #[test]
    fn lookup_distinguishes_ready_evicted_disconnected_and_missing() {
        let pool = ConnectionPool::new();
        let ready = Arc::new(IpcClient::new("ipc://ready"));
        ready.force_connected(true);
        pool.insert("ready".into(), "ipc://ready".into(), ready);
        let disconnected = Arc::new(IpcClient::new("ipc://disconnected"));
        pool.insert(
            "disconnected".into(),
            "ipc://disconnected".into(),
            disconnected,
        );
        let evicted = Arc::new(IpcClient::new("ipc://evicted"));
        evicted.force_connected(true);
        pool.insert("evicted".into(), "ipc://evicted".into(), evicted);
        pool.evict("evicted");

        match pool.lookup("ready") {
            CachedClient::Ready { address, .. } => {
                assert_eq!(address, "ipc://ready");
            }
            _ => panic!("ready connection should be explicit"),
        }
        match pool.lookup("evicted") {
            CachedClient::Evicted { address } => {
                assert_eq!(address, "ipc://evicted");
            }
            _ => panic!("evicted connection should be explicit"),
        }
        match pool.lookup("disconnected") {
            CachedClient::Disconnected { address } => {
                assert_eq!(address, "ipc://disconnected");
            }
            _ => panic!("disconnected connection should be explicit"),
        }
        assert!(matches!(pool.lookup("missing"), CachedClient::Missing));
    }

    #[tokio::test]
    async fn owner_only_slot_acquires_data_plane_client_lazily() {
        let pool = Arc::new(ConnectionPool::new());
        pool.insert_owner("grid".into(), "ipc://lazy".into());

        let attempts = Arc::new(AtomicUsize::new(0));
        let lease = pool
            .acquire_with("grid", {
                let attempts = attempts.clone();
                move |address| {
                    let attempts = attempts.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        let client = Arc::new(IpcClient::new(&address));
                        client.force_connected(true);
                        Ok(client)
                    }
                }
            })
            .await
            .expect("owner-only slot should connect on first data-plane acquire");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(lease.client().is_connected());
        drop(lease);
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn lookup_returns_disconnected_for_disconnected() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        // Client starts disconnected.
        pool.insert("grid".into(), "ipc://test".into(), client);
        assert!(matches!(
            pool.lookup("grid"),
            CachedClient::Disconnected { .. }
        ));
    }

    #[test]
    fn evict_and_reconnect() {
        let pool = ConnectionPool::new();
        let c1 = Arc::new(IpcClient::new("ipc://test"));
        c1.force_connected(true);
        pool.insert("grid".into(), "ipc://test".into(), c1);
        pool.evict("grid");
        assert!(matches!(pool.lookup("grid"), CachedClient::Evicted { .. }));

        let c2 = Arc::new(IpcClient::new("ipc://test"));
        c2.force_connected(true);
        pool.reconnect("grid", c2);
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn remove_deletes_entry() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        pool.insert("grid".into(), "ipc://test".into(), client);
        pool.remove("grid");
        assert!(matches!(pool.lookup("grid"), CachedClient::Missing));
    }

    #[test]
    fn idle_entries_detects_disconnected() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://dead"));
        pool.insert("d".into(), "ipc://dead".into(), client);
        assert_eq!(pool.idle_entries(u64::MAX).len(), 1);
    }

    #[test]
    fn idle_entries_do_not_evict_active_connected_client() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://active"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://active".into(), client);

        let lease = pool.begin_request_for_test("grid").unwrap();

        assert!(pool.idle_entries(0).is_empty());
        drop(lease);
    }

    #[test]
    fn idle_entries_can_evict_inactive_connected_client() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://idle"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://idle".into(), client);

        assert_eq!(pool.idle_entries(0), vec!["grid".to_string()]);
    }

    #[test]
    fn end_request_makes_client_idle_candidate_again() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://active"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://active".into(), client);

        let lease = pool.begin_request_for_test("grid").unwrap();
        drop(lease);

        assert_eq!(pool.idle_entries(0), vec!["grid".to_string()]);
    }

    #[test]
    fn disconnected_client_is_evicted_even_when_not_idle_by_time() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://dead"));
        client.force_connected(false);
        pool.insert("grid".into(), "ipc://dead".into(), client);

        assert_eq!(pool.idle_entries(u64::MAX), vec!["grid".to_string()]);
    }

    #[test]
    fn active_disconnected_client_is_not_evicted_until_request_finishes() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://active-dead"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://active-dead".into(), client.clone());

        let lease = pool.begin_request_for_test("grid").unwrap();
        client.force_connected(false);

        assert!(pool.idle_entries(u64::MAX).is_empty());
        assert_eq!(pool.evict_idle(u64::MAX).len(), 0);

        drop(lease);
        assert_eq!(pool.idle_entries(u64::MAX), vec!["grid".to_string()]);
    }

    #[test]
    fn stale_lease_drop_after_reinsert_does_not_touch_new_slot() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let old_lease = pool.begin_request_for_test("grid").unwrap();

        pool.remove("grid");

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.insert("grid".into(), "ipc://new".into(), new_client);
        drop(old_lease);

        assert_eq!(pool.idle_entries(0), vec!["grid".to_string()]);
    }

    #[test]
    fn stale_lease_drop_after_reinsert_does_not_release_new_active_request() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let old_lease = pool.begin_request_for_test("grid").unwrap();

        pool.remove("grid");

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.insert("grid".into(), "ipc://new".into(), new_client);
        let new_lease = pool.begin_request_for_test("grid").unwrap();

        drop(old_lease);
        assert!(pool.idle_entries(0).is_empty());

        drop(new_lease);
        assert_eq!(pool.idle_entries(0), vec!["grid".to_string()]);
    }

    #[test]
    fn stale_lease_evict_after_reinsert_does_not_evict_new_entry() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let old_lease = pool.begin_request_for_test("grid").unwrap();

        pool.remove("grid");

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.insert("grid".into(), "ipc://new".into(), new_client);

        assert!(pool.evict_client("grid", &old_lease.client()).is_none());
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn matching_client_evict_removes_current_entry_client() {
        let pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://current"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://current".into(), client);
        let lease = pool.begin_request_for_test("grid").unwrap();

        assert!(pool.evict_client("grid", &lease.client()).is_some());
        assert!(matches!(pool.lookup("grid"), CachedClient::Evicted { .. }));
    }

    #[test]
    fn stale_client_evict_after_reconnect_does_not_evict_reconnected_client() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let old_lease = pool.begin_request_for_test("grid").unwrap();

        assert!(pool.evict_client("grid", &old_lease.client()).is_some());

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.reconnect("grid", new_client);

        assert!(pool.evict_client("grid", &old_lease.client()).is_none());
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn stale_lease_drop_after_reconnect_does_not_release_new_active_request() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let old_lease = pool.begin_request_for_test("grid").unwrap();

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.reconnect("grid", new_client);
        assert!(pool.idle_entries(0).is_empty());

        let new_lease = pool.begin_request_for_test("grid").unwrap();

        drop(old_lease);
        assert!(pool.idle_entries(0).is_empty());

        drop(new_lease);
        assert_eq!(pool.idle_entries(0), vec!["grid".to_string()]);
    }

    #[test]
    fn stale_owner_token_after_reinsert_does_not_match_new_entry() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let token = pool.owner_token("grid").unwrap();

        pool.remove("grid");

        let current_client = Arc::new(IpcClient::new("ipc://current"));
        current_client.force_connected(true);
        pool.insert(
            "grid".into(),
            "ipc://current".into(),
            current_client.clone(),
        );

        assert!(!pool.matches_owner_token("grid", &token));

        let lease = pool.begin_request_for_test("grid").unwrap();
        assert!(Arc::ptr_eq(&lease.client(), &current_client));
        assert!(matches!(pool.lookup("grid"), CachedClient::Ready { .. }));
    }

    #[test]
    fn owner_token_does_not_match_same_slot_after_reconnect() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let token = pool.owner_token("grid").unwrap();

        let new_client = Arc::new(IpcClient::new("ipc://new"));
        new_client.force_connected(true);
        pool.reconnect("grid", new_client.clone());

        let lease = pool.begin_request_for_test("grid").unwrap();
        assert!(Arc::ptr_eq(&lease.client(), &new_client));
        assert!(!pool.matches_owner_token("grid", &token));
    }

    #[test]
    fn owner_token_does_not_match_reinsert_with_same_address() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://same"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://same".into(), old_client);
        let token = pool.owner_token("grid").unwrap();

        pool.remove("grid");

        let new_client = Arc::new(IpcClient::new("ipc://same"));
        new_client.force_connected(true);
        pool.insert("grid".into(), "ipc://same".into(), new_client);

        assert!(!pool.matches_owner_token("grid", &token));
    }

    #[test]
    fn owner_token_does_not_match_after_lease_renewal() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://same-slot"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://same-slot".into(), old_client);
        let token = pool.owner_token("grid").unwrap();

        assert!(pool.renew_owner_lease("grid", &token));

        assert!(!pool.matches_owner_token("grid", &token));
    }

    #[test]
    fn current_owner_lease_renewal_is_not_stale_token_based() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://same-slot"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://same-slot".into(), old_client);
        let token = pool.owner_token("grid").unwrap();

        assert!(pool.renew_current_owner_lease("grid"));
        assert!(pool.renew_current_owner_lease("grid"));

        assert!(!pool.matches_owner_token("grid", &token));
    }

    #[test]
    fn disabled_owner_lease_deadline_does_not_block_evidence_based_replacement() {
        let pool = ConnectionPool::with_owner_lease_duration(None);
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let token = pool.owner_token("grid").unwrap();
        assert!(token.slot.inner.lock().owner_lease_deadline.is_none());

        assert!(pool.renew_current_owner_lease("grid"));
        assert!(token.slot.inner.lock().owner_lease_deadline.is_none());
        pool.evict("grid");

        let result = pool.replace_if_owner_token(
            "grid",
            &pool.owner_token("grid").unwrap(),
            "ipc://replacement".to_string(),
            OwnerReplacementEvidence::ConfirmedDead,
        );

        assert!(result.is_ok());
        assert_eq!(
            pool.get_address("grid").as_deref(),
            Some("ipc://replacement")
        );
    }

    #[test]
    fn replace_if_owner_token_rejects_active_disconnected_slot() {
        let pool = ConnectionPool::new();
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        let token = pool.owner_token("grid").unwrap();
        let lease = pool.begin_request_for_test("grid").unwrap();
        lease.client().force_connected(false);

        let result = pool.replace_if_owner_token(
            "grid",
            &token,
            "ipc://replacement".to_string(),
            OwnerReplacementEvidence::ConfirmedDead,
        );

        assert!(matches!(result, Err(OwnerReplaceError::NotReplaceable)));
        assert!(pool.matches_owner_token("grid", &token));
        drop(lease);
    }

    #[tokio::test]
    async fn concurrent_acquire_after_eviction_shares_one_reconnect() {
        let pool = Arc::new(ConnectionPool::new());
        let client = Arc::new(IpcClient::new("ipc://shared"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://shared".into(), client);
        pool.evict("grid");

        let connect_count = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..16 {
            let pool = pool.clone();
            let connect_count = connect_count.clone();
            tasks.push(tokio::spawn(async move {
                pool.acquire_with("grid", move |address| {
                    let connect_count = connect_count.clone();
                    async move {
                        connect_count.fetch_add(1, Ordering::SeqCst);
                        let client = Arc::new(IpcClient::new(&address));
                        client.force_connected(true);
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
        let client = Arc::new(IpcClient::new("ipc://flaky"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://flaky".into(), client);
        pool.evict("grid");

        let attempts = Arc::new(AtomicUsize::new(0));
        let lease = pool
            .acquire_with("grid", {
                let attempts = attempts.clone();
                move |address| {
                    let attempts = attempts.clone();
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            return Err(c2_ipc::IpcError::Io(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "peer reset during idle reconnect",
                            )));
                        }
                        let client = Arc::new(IpcClient::new(&address));
                        client.force_connected(true);
                        Ok(client)
                    }
                }
            })
            .await
            .expect("transient reconnect reset should be retried");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(lease.client().is_connected());
    }

    #[tokio::test]
    async fn remove_during_reconnect_makes_waiting_acquire_not_found() {
        let pool = Arc::new(ConnectionPool::new());
        let client = Arc::new(IpcClient::new("ipc://removed"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://removed".into(), client);
        pool.evict("grid");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let finish_rx = Arc::new(tokio::sync::Mutex::new(Some(finish_rx)));

        let acquire = {
            let pool = pool.clone();
            let finish_rx = finish_rx.clone();
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                pool.acquire_with("grid", move |address| {
                    let finish_rx = finish_rx.clone();
                    let started_tx = started_tx.clone();
                    async move {
                        if let Some(started_tx) = started_tx.lock().unwrap().take() {
                            let _ = started_tx.send(());
                        }
                        let rx = finish_rx.lock().await.take().unwrap();
                        rx.await.unwrap();
                        let client = Arc::new(IpcClient::new(&address));
                        client.force_connected(true);
                        Ok(client)
                    }
                })
                .await
            })
        };

        started_rx.await.unwrap();
        pool.remove("grid");
        finish_tx.send(()).unwrap();

        assert!(matches!(
            acquire.await.unwrap(),
            Err(AcquireError::NotFound)
        ));
    }

    #[tokio::test]
    async fn insert_retires_replaced_slot_before_waiting_reconnect_completes() {
        let pool = Arc::new(ConnectionPool::new());
        let old_client = Arc::new(IpcClient::new("ipc://old"));
        old_client.force_connected(true);
        pool.insert("grid".into(), "ipc://old".into(), old_client);
        pool.evict("grid");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let finish_rx = Arc::new(tokio::sync::Mutex::new(Some(finish_rx)));

        let acquire = {
            let pool = pool.clone();
            let finish_rx = finish_rx.clone();
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                pool.acquire_with("grid", move |address| {
                    let finish_rx = finish_rx.clone();
                    let started_tx = started_tx.clone();
                    async move {
                        if let Some(started_tx) = started_tx.lock().unwrap().take() {
                            let _ = started_tx.send(());
                        }
                        let rx = finish_rx.lock().await.take().unwrap();
                        rx.await.unwrap();
                        let client = Arc::new(IpcClient::new(&address));
                        client.force_connected(true);
                        Ok(client)
                    }
                })
                .await
            })
        };

        started_rx.await.unwrap();
        let replacement = Arc::new(IpcClient::new("ipc://new"));
        replacement.force_connected(true);
        pool.insert("grid".into(), "ipc://new".into(), replacement.clone());
        finish_tx.send(()).unwrap();

        assert!(matches!(
            acquire.await.unwrap(),
            Err(AcquireError::NotFound)
        ));
        let current = pool.begin_request_for_test("grid").unwrap();
        assert!(Arc::ptr_eq(&current.client(), &replacement));
    }
}
