//! Reference-counted pool of [`SyncClient`] instances.
//!
//! Clients connecting to the same server address share a single
//! `SyncClient`. When all references are released the client is
//! kept alive for a grace period before being destroyed.

use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_mem::{MemPool, PoolConfig};

/// Label counter for client pools. MemPool adds its incarnation and owns
/// platform segment-name derivation.
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);
const CONNECT_TRANSIENT_RETRY_ATTEMPTS: usize = 3;

use crate::client::{ClientIpcConfig, IpcError};
use crate::sync_client::SyncClient;

pub(crate) fn pool_config_from_client_config(cfg: &ClientIpcConfig) -> PoolConfig {
    PoolConfig {
        segment_size: cfg.pool_segment_size as usize,
        max_segments: cfg.max_pool_segments as usize,
        ..PoolConfig::default()
    }
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
    address: &str,
    cfg: &ClientIpcConfig,
) -> Result<SyncClient, IpcError> {
    let pool_config = pool_config_from_client_config(cfg);

    for attempt in 0..CONNECT_TRANSIENT_RETRY_ATTEMPTS {
        let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3c{:08x}{:08x}", std::process::id(), counter);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix(
            pool_config.clone(),
            prefix,
        )));

        match SyncClient::connect(address, Some(pool), cfg.clone()) {
            Ok(client) => return Ok(client),
            Err(error)
                if attempt + 1 < CONNECT_TRANSIENT_RETRY_ATTEMPTS
                    && is_transient_connect_error(&error) =>
            {
                std::thread::sleep(Duration::from_millis(10 * (attempt as u64 + 1)));
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("connect retry loop always returns before exhausting attempts")
}

// ── Pool entry ───────────────────────────────────────────────────────────

struct PoolEntry {
    client: Arc<SyncClient>,
    ref_count: usize,
    /// Set to `Some(Instant::now())` when `ref_count` drops to 0.
    last_release: Option<Instant>,
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
}

/// Lifecycle of one retired (detached) client record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetiredState {
    /// A bounded close barrier for this record is currently running.
    Closing,
    /// The last close barrier returned unconfirmed. The record keeps
    /// ownership of the client reachable so a later `close_all` can retry
    /// it and keep reporting it unconfirmed until it actually stops.
    UnconfirmedIdle,
}

/// One client detached from the cache whose bounded close is owned by the
/// coordinator. Records are inserted in the same critical section that
/// removes the entry from `CacheState.entries` and removed only after a
/// close confirms, so a `close_all` can never observe an empty cache with
/// unaccounted detached work.
struct RetiredClose {
    address: String,
    client: Arc<SyncClient>,
    state: RetiredState,
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
    retired: HashMap<u64, RetiredClose>,
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
/// so a later `close_all` can retry it and keep reporting it honestly.
/// Returns `Err(address)` when the close did not confirm.
fn close_retired_ticket(
    txn: &(Mutex<CloseTxnState>, Condvar),
    ticket: u64,
    deadline: Instant,
) -> Result<(), String> {
    let client = {
        let txn_state = txn.0.lock();
        match txn_state.retired.get(&ticket) {
            Some(record) if record.state == RetiredState::Closing => {
                Arc::clone(&record.client)
            }
            // Already removed by a confirming retry elsewhere, or never
            // registered: nothing to close, nothing to report.
            _ => return Ok(()),
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let confirmed = client.close_shared(remaining);
    let unconfirmed_address = {
        let mut txn_state = txn.0.lock();
        match txn_state.retired.get_mut(&ticket) {
            Some(record) => {
                if confirmed {
                    // Remove the exact record only after confirmation.
                    txn_state.retired.remove(&ticket);
                    None
                } else {
                    record.state = RetiredState::UnconfirmedIdle;
                    Some(record.address.clone())
                }
            }
            None => None, // A concurrent retry already resolved this record.
        }
    };
    txn.1.notify_all();
    match unconfirmed_address {
        Some(address) => Err(address),
        None => Ok(()),
    }
}

/// Close a batch of registered retired tickets with the detached-close
/// deadline. Used by non-drain paths (acquire sweeps/stale/loser cleanup);
/// unconfirmed records stay registered for a later `close_all` to retry.
fn run_retired_closes(txn: &(Mutex<CloseTxnState>, Condvar), tickets: &[u64]) {
    let deadline = Instant::now() + DETACHED_CLOSE_TIMEOUT;
    for ticket in tickets {
        let _ = close_retired_ticket(txn, *ticket, deadline);
    }
}

impl ClientPool {
    /// Create a new pool with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self {
            state: Mutex::new(CacheState {
                entries: HashMap::new(),
                epoch: 0,
                closing_generation: None,
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

    /// Set the default IPC config for newly created clients.
    pub fn set_default_config(&self, config: ClientIpcConfig) {
        *self.default_config.lock() = Some(config);
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
        let ticket = txn.next_ticket;
        txn.next_ticket += 1;
        txn.retired.insert(
            ticket,
            RetiredClose {
                address: address.to_string(),
                client,
                state: RetiredState::Closing,
            },
        );
        drop(txn);
        self.close_txn.1.notify_all();
        ticket
    }

    /// Acquire a client for `address`. Creates and connects if needed.
    /// Increments reference count.
    ///
    /// The connect runs outside the pool lock under an epoch fence: if a
    /// concurrent `close_all` drains the cache while this acquire is
    /// connecting, the fresh connection is closed explicitly and the acquire
    /// fails instead of inserting an entry after the drain.
    pub fn acquire(
        &self,
        address: &str,
        config: Option<&ClientIpcConfig>,
    ) -> Result<Arc<SyncClient>, IpcError> {
        let mut retired: Vec<u64> = Vec::new();
        let epoch;
        let cfg;
        {
            let mut state = self.state.lock();
            if state.closing_generation.is_some() {
                return Err(IpcError::Pool(
                    "client pool acquire rejected: cache is closing".to_string(),
                ));
            }
            epoch = state.epoch;
            // Detach expired entries before potentially creating a new one;
            // each detachment registers its pending close in this same
            // critical section.
            for (expired_address, client) in
                Self::sweep_expired_locked(&mut state, self.grace_period)
            {
                retired.push(self.retire_locked(&expired_address, client));
            }
            if let Some(entry) = state.entries.get_mut(address) {
                if entry.client.is_connected() {
                    entry.ref_count += 1;
                    entry.last_release = None;
                    let client = Arc::clone(&entry.client);
                    drop(state);
                    run_retired_closes(&self.close_txn, &retired);
                    return Ok(client);
                }
                // Stale — detach and fall through to create a new one.
                if let Some(entry) = state.entries.remove(address) {
                    retired.push(self.retire_locked(address, entry.client));
                }
            }
            // Resolve config: explicit > default > ClientIpcConfig::default().
            cfg = match config {
                Some(c) => c.clone(),
                None => self.default_config.lock().clone().unwrap_or_default(),
            };
        }
        // Drop the pool lock before connecting (connect may block).
        run_retired_closes(&self.close_txn, &retired);

        let client = Arc::new(connect_with_transient_retry(address, &cfg)?);

        let mut state = self.state.lock();
        if state.closing_generation.is_some() || state.epoch != epoch {
            // A drain completed while this connect was in flight. The entry
            // cannot join the drained generation; register and close it
            // explicitly while still holding the state lock so the barrier
            // is accounted before we release it.
            let ticket = self.retire_locked(address, client);
            drop(state);
            let _ = close_retired_ticket(
                &self.close_txn,
                ticket,
                Instant::now() + DETACHED_CLOSE_TIMEOUT,
            );
            return Err(IpcError::Pool(
                "client pool acquire rejected: cache closed while connecting".to_string(),
            ));
        }
        if let Some(entry) = state.entries.get_mut(address)
            && entry.client.is_connected()
        {
            // Another thread raced and inserted the same address; its client
            // wins and this fresh connection is the loser. Bump the winner
            // under the lock, register the loser's close, then run it
            // outside the lock.
            entry.ref_count += 1;
            entry.last_release = None;
            let winner = Arc::clone(&entry.client);
            let loser_ticket = self.retire_locked(address, client);
            drop(state);
            let _ = close_retired_ticket(
                &self.close_txn,
                loser_ticket,
                Instant::now() + DETACHED_CLOSE_TIMEOUT,
            );
            return Ok(winner);
        }
        let replaced = state.entries.insert(
            address.to_owned(),
            PoolEntry {
                client: Arc::clone(&client),
                ref_count: 1,
                last_release: None,
            },
        );
        // A stale entry raced back in and lost; register its close in the
        // same critical section that replaced it.
        let replaced_ticket = replaced.map(|old| self.retire_locked(address, old.client));
        drop(state);
        if let Some(ticket) = replaced_ticket {
            let _ = close_retired_ticket(
                &self.close_txn,
                ticket,
                Instant::now() + DETACHED_CLOSE_TIMEOUT,
            );
        }
        Ok(client)
    }

    /// Decrement reference count. When it reaches 0, mark for grace-period
    /// cleanup.
    pub fn release(&self, address: &str) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(address) {
            if entry.ref_count == 0 {
                eprintln!("ClientPool::release: ref_count already 0 for {address}");
                return;
            }
            entry.ref_count -= 1;
            if entry.ref_count == 0 {
                entry.last_release = Some(Instant::now());
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
        if entry.ref_count == 0 {
            return false;
        }
        entry.ref_count -= 1;
        if entry.ref_count == 0 {
            entry.last_release = Some(Instant::now());
        }
        true
    }

    /// Remove one observed unusable client without evicting a racing replacement.
    ///
    /// Callers must use this only after a pre-dispatch operation proves that
    /// the exact acquired connection can no longer serve requests. The
    /// detached client's bounded close is registered in the same critical
    /// section that removes the entry, then runs outside the pool lock.
    pub fn discard_if_same(&self, address: &str, observed: &Arc<SyncClient>) -> bool {
        let ticket = {
            let mut state = self.state.lock();
            let is_same = state
                .entries
                .get(address)
                .is_some_and(|entry| Arc::ptr_eq(&entry.client, observed));
            if is_same {
                let entry = state
                    .entries
                    .remove(address)
                    .expect("entry presence checked above");
                Some(self.retire_locked(address, entry.client))
            } else {
                None
            }
        };
        match ticket {
            Some(ticket) => {
                let _ = close_retired_ticket(
                    &self.close_txn,
                    ticket,
                    Instant::now() + DETACHED_CLOSE_TIMEOUT,
                );
                true
            }
            None => false,
        }
    }

    /// Detach entries that have been unreferenced longer than `grace_period`.
    fn sweep_expired_locked(
        state: &mut CacheState,
        grace: Duration,
    ) -> Vec<(String, Arc<SyncClient>)> {
        let mut detached = Vec::new();
        state.entries.retain(|address, entry| {
            if entry.ref_count == 0
                && let Some(released_at) = entry.last_release
                && released_at.elapsed() >= grace
            {
                detached.push((address.clone(), Arc::clone(&entry.client)));
                return false;
            }
            true
        });
        detached
    }

    /// Sweep expired entries and close them outside the pool lock.
    /// Call this periodically from SDK bindings or before acquire.
    pub fn sweep_expired(&self) {
        let tickets = {
            let mut state = self.state.lock();
            Self::sweep_expired_locked(&mut state, self.grace_period)
                .into_iter()
                .map(|(address, client)| self.retire_locked(&address, client))
                .collect::<Vec<u64>>()
        };
        run_retired_closes(&self.close_txn, &tickets);
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
                if txn.active_generation.is_none()
                    && !txn
                        .retired
                        .values()
                        .any(|record| record.state == RetiredState::Closing)
                {
                    break;
                }
                let now = Instant::now();
                let blocked_by_drain = txn.active_generation.is_some();
                if now >= deadline {
                    report.error = Some(Self::drain_wait_error(blocked_by_drain));
                    return report;
                }
                let result = txn_condvar.wait_for(&mut txn, deadline - now);
                if result.timed_out()
                    && (txn.active_generation.is_some()
                        || txn
                            .retired
                            .values()
                            .any(|record| record.state == RetiredState::Closing))
                {
                    report.error =
                        Some(Self::drain_wait_error(txn.active_generation.is_some()));
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
                        RetiredClose {
                            address,
                            client,
                            state: RetiredState::Closing,
                        },
                    );
                    ticket
                })
                .collect();
            let retry_tickets: Vec<u64> = txn
                .retired
                .iter_mut()
                .filter(|(_, record)| record.state == RetiredState::UnconfirmedIdle)
                .map(|(ticket, record)| {
                    record.state = RetiredState::Closing;
                    *ticket
                })
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
                .any(|record| record.state == RetiredState::Closing)
            {
                let now = Instant::now();
                if now >= deadline {
                    report.error = Some(Self::drain_wait_error(false));
                    break;
                }
                let result = txn_condvar.wait_for(&mut txn, deadline - now);
                if result.timed_out()
                    && txn
                        .retired
                        .values()
                        .any(|record| record.state == RetiredState::Closing)
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
            for record in txn.retired.values() {
                let already_reported = report.unconfirmed.iter().any(|address| *address == record.address);
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
            .map_or(0, |e| e.ref_count)
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
                .any(|record| record.state == RetiredState::Closing)
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

    /// Number of retired records still registered (pending or
    /// unconfirmed-idle). Zero means every detached client's close has
    /// confirmed.
    pub(crate) fn retired_records_for_test(&self) -> usize {
        self.close_txn.0.lock().retired.len()
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
                    ref_count: 0,
                    last_release: Some(Instant::now() - Duration::from_millis(200)),
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
                    ref_count: 0,
                    last_release: Some(Instant::now()),
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
                    ref_count: 2,
                    last_release: None,
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
                    ref_count: 0,
                    last_release: Some(Instant::now()),
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
                ref_count: 1,
                last_release: None,
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
                ref_count: 1,
                last_release: None,
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
                    ref_count: 1,
                    last_release: None,
                },
            );
            state.entries.insert(
                "ipc://b".to_owned(),
                PoolEntry {
                    client: Arc::new(c2),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
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
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
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
        // frame length prefix and goes silent: the client receive task is
        // deterministically blocked mid-frame. The close barrier must abort
        // the stalled stream, let the receive task finish within its bounded
        // join slice, and confirm — without waiting for the peer.
        let address = unique_ipc_address("pool_close_receiver_blocked");
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
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

        let client = SyncClient::connect(&address, None, ClientIpcConfig::default())
            .expect("connect against the real handshake");
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
            let server = Arc::new(Server::new(&reopen_address, ServerIpcConfig::default()).unwrap());
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
            let probe_result = tokio::task::spawn_blocking(move || {
                probe_pool.acquire(&probe_address, None)
            })
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
            let (first_report, first_elapsed) =
                drain_thread.join().expect("first drain thread");
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
            let reopened = tokio::task::spawn_blocking(move || {
                reopen_pool.acquire(&reopen_address, None)
            })
            .await
            .expect("reopen acquire task")
            .expect("acquire after drain");
            assert!(reopened.is_connected());
            let final_pool = Arc::clone(&pool);
            let final_report = tokio::task::spawn_blocking(move || {
                final_pool.close_all(Duration::from_secs(2))
            })
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
        // One client whose receive task is blocked mid-frame; two threads
        // close it concurrently. The close gate serializes the barriers: the
        // loser cannot observe a taken receive handle and mistake it for a
        // terminal task state, both callers stay bounded, at least one
        // reports an honest unconfirmed close, and the aborted task stays
        // observable — a subsequent close retries the restored handle and
        // confirms.
        let address = unique_ipc_address("pool_double_close_receiver");
        let endpoint = LocalEndpoint::from_address(&address).unwrap();
        let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
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
            SyncClient::connect(&address, None, ClientIpcConfig::default())
                .expect("connect against the real handshake"),
        );

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
        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(
                result_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("both concurrent closes must return"),
            );
        }
        for (confirmed, elapsed) in &results {
            assert!(
                elapsed < &Duration::from_secs(8),
                "each concurrent close must stay bounded, took {elapsed:?}"
            );
            let _ = confirmed;
        }
        assert!(
            results.iter().any(|(confirmed, _)| !confirmed),
            "the barrier that consumed its receive budget must report an honest unconfirmed close: {results:?}"
        );
        for thread in close_threads {
            thread.join().expect("close thread");
        }

        // Retryability: the aborted receive task is still observable through
        // its restored handle; a subsequent close joins it and confirms.
        let retry = client.close_shared(Duration::from_secs(2));
        assert!(retry, "a later close must confirm after the serialized barriers");
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
            let discard_thread =
                thread::spawn(move || discard_pool.discard_if_same(&discard_address, &discard_client));
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
            let clean = tokio::task::spawn_blocking(move || {
                clean_pool.close_all(Duration::from_secs(2))
            })
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
            let second = tokio::task::spawn_blocking(move || {
                second_pool.close_all(Duration::from_secs(2))
            })
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
                            ref_count: 1,
                            last_release: None,
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
        };
        pool.set_default_config(cfg);
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
        };

        let pc = pool_config_from_client_config(&cfg);

        assert_eq!(pc.segment_size, 65_536);
        assert_eq!(pc.max_segments, 3);
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
