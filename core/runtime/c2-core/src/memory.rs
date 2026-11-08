//! Read-only, scope-labelled memory-budget snapshots.
//!
//! These values describe C-Two-owned IPC backing and live reassembly
//! accounting. They are deliberately **not** process RSS: the three cells are
//! independent finite scopes (owner-created SHM backing, owner-created file
//! backing, live reassembly capacity) and their sum must never be presented
//! as physical memory usage.
//!
//! Every accessor here is observation-only. Looking at a snapshot never
//! connects, maps memory, freezes a configuration, or resets accounting.
//!
//! Retired observations retain weak metadata only. Transport budgets and local
//! buffer accounting have independent lifetimes: a hold never retains a pool,
//! connection, mapping, or an otherwise ownerless backing domain.

use std::collections::{BTreeMap, HashMap};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use parking_lot::Mutex;

use c2_config::MemoryBudgetLimits;
use c2_mem::retention::{PayloadBufferObserverWeak, PayloadBufferSnapshot};
use c2_mem::{BudgetCellSnapshot, BudgetObserver, BudgetObserverWeak, BudgetSnapshot};
use c2_server::ServerMemorySnapshot;

/// The SDK scope retaining one local buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalCopyDirection {
    ClientResponse,
    ResourceInput,
}

impl LocalCopyDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientResponse => "client_response",
            Self::ResourceInput => "resource_input",
        }
    }
}

/// Metadata for a local SDK buffer, independent of its transport origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCopyMeta {
    pub route_name: String,
    pub method_name: String,
    pub direction: LocalCopyDirection,
    /// Logical buffer length, not allocation capacity or RSS.
    pub bytes: usize,
}

#[derive(Debug)]
struct LocalCopyEntry {
    meta: LocalCopyMeta,
    created_at: Instant,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalCopyBucketStats {
    pub active_holds: usize,
    pub total_held_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct LocalCopyStats {
    pub active_holds: usize,
    pub total_held_bytes: usize,
    pub oldest_hold_seconds: f64,
    pub by_direction: BTreeMap<LocalCopyDirection, LocalCopyBucketStats>,
}

impl LocalCopyStats {
    /// Compose counters from disjoint trackers.
    pub fn merge(&mut self, other: &Self) {
        self.active_holds = self.active_holds.saturating_add(other.active_holds);
        self.total_held_bytes = self.total_held_bytes.saturating_add(other.total_held_bytes);
        self.oldest_hold_seconds = self.oldest_hold_seconds.max(other.oldest_hold_seconds);
        for (direction, stats) in &other.by_direction {
            let bucket = self.by_direction.entry(*direction).or_default();
            bucket.active_holds = bucket.active_holds.saturating_add(stats.active_holds);
            bucket.total_held_bytes = bucket
                .total_held_bytes
                .saturating_add(stats.total_held_bytes);
        }
    }
}

#[derive(Debug)]
struct LocalCopyTrackerInner {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, LocalCopyEntry>>,
}

#[derive(Debug, Clone)]
pub struct LocalCopyTracker {
    inner: Arc<LocalCopyTrackerInner>,
}

/// Weak observation cannot recover producer authority or retain a buffer.
///
/// ```compile_fail
/// use c2_core::memory::LocalCopyObserver;
/// fn recover_producer(observer: &LocalCopyObserver) {
///     let _producer = observer.upgrade();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct LocalCopyObserver {
    inner: Weak<LocalCopyTrackerInner>,
}

impl LocalCopyObserver {
    pub fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.inner, &other.inner)
    }

    fn upgrade(&self) -> Option<LocalCopyTracker> {
        self.inner.upgrade().map(|inner| LocalCopyTracker { inner })
    }

    pub fn stats(&self) -> Option<LocalCopyStats> {
        self.upgrade().map(|tracker| tracker.stats())
    }
}

/// The local buffer owner keeps only its metadata alive. Drop this guard after
/// invalidating checked views, ending exports, and dropping the buffer.
#[derive(Debug)]
pub struct LocalCopyGuard {
    id: u64,
    tracker: Arc<LocalCopyTrackerInner>,
}

impl LocalCopyTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(LocalCopyTrackerInner {
                next_id: AtomicU64::new(1),
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn observer(&self) -> LocalCopyObserver {
        LocalCopyObserver {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn track(&self, meta: LocalCopyMeta) -> LocalCopyGuard {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.entries.lock().insert(
            id,
            LocalCopyEntry {
                meta,
                created_at: Instant::now(),
            },
        );
        LocalCopyGuard {
            id,
            tracker: Arc::clone(&self.inner),
        }
    }

    pub fn stats(&self) -> LocalCopyStats {
        let now = Instant::now();
        let mut stats = LocalCopyStats::default();
        for entry in self.inner.entries.lock().values() {
            stats.active_holds = stats.active_holds.saturating_add(1);
            stats.total_held_bytes = stats.total_held_bytes.saturating_add(entry.meta.bytes);
            let direction = stats.by_direction.entry(entry.meta.direction).or_default();
            direction.active_holds = direction.active_holds.saturating_add(1);
            direction.total_held_bytes =
                direction.total_held_bytes.saturating_add(entry.meta.bytes);
            stats.oldest_hold_seconds = stats.oldest_hold_seconds.max(
                now.saturating_duration_since(entry.created_at)
                    .as_secs_f64(),
            );
        }
        stats
    }
}

impl Default for LocalCopyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LocalCopyGuard {
    fn drop(&mut self) {
        self.tracker.entries.lock().remove(&self.id);
    }
}

/// Scope labels used when reporting C-Two memory budgets.
pub mod scope {
    /// Per-Runtime outgoing client domain: every cached connection's request
    /// pool and reassembly pool charges this one budget.
    pub const RUNTIME_OUTGOING: &str = "runtime_outgoing";
    /// Per-Server direction: the response pool, the reassembly pool, and
    /// response prewarm share this one budget.
    pub const SERVER: &str = "server";
}

/// One budget cell: finite limit, current usage, peak, and rejections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryCellStats {
    /// Finite limit of this cell in bytes. `0` means no positive allocation
    /// in this cell; it is not unlimited.
    pub limit_bytes: u64,
    /// Currently reserved bytes.
    pub used_bytes: u64,
    /// High-water mark of `used_bytes`; persists across releases.
    pub peak_bytes: u64,
    /// Number of rejected reservations; may saturate.
    pub rejected_allocations: u64,
    /// Bytes rejected across those attempts; may saturate.
    pub rejected_bytes: u64,
}

impl MemoryCellStats {
    /// Project one `c2-mem` cell snapshot without copying live accounting.
    pub const fn from_cell(cell: BudgetCellSnapshot) -> Self {
        Self {
            limit_bytes: cell.limit_bytes,
            used_bytes: cell.used_bytes,
            peak_bytes: cell.peak_bytes,
            rejected_allocations: cell.rejected_allocations,
            rejected_bytes: cell.rejected_bytes,
        }
    }

    pub const fn from_payload_buffer(cell: PayloadBufferSnapshot) -> Self {
        Self {
            limit_bytes: cell.limit_bytes,
            used_bytes: cell.used_bytes,
            peak_bytes: cell.peak_bytes,
            rejected_allocations: cell.rejected_allocations,
            rejected_bytes: cell.rejected_bytes,
        }
    }
}

/// A scope-labelled snapshot of all three budget cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryScopeStats {
    /// Resolved limits the scope's budget enforces.
    pub limits: MemoryBudgetLimits,
    /// Owner-created buddy/dedicated mapped backing, including headers.
    pub shm: MemoryCellStats,
    /// Owner-created file-spill backing length.
    pub file: MemoryCellStats,
    /// Allocated capacity of incomplete and completed-but-retained chunk
    /// assemblies.
    pub reassembly: MemoryCellStats,
}

impl MemoryScopeStats {
    /// Project one `c2-mem` budget snapshot plus its resolved limits.
    pub const fn from_budget(limits: MemoryBudgetLimits, budget: BudgetSnapshot) -> Self {
        Self {
            limits,
            shm: MemoryCellStats::from_cell(budget.shm),
            file: MemoryCellStats::from_cell(budget.file),
            reassembly: MemoryCellStats::from_cell(budget.reassembly),
        }
    }

    /// Project one server-direction snapshot.
    pub const fn from_server(snapshot: ServerMemorySnapshot) -> Self {
        Self::from_budget(snapshot.limits, snapshot.budget)
    }
}

/// Read-only memory statistics for one Core Runtime and its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMemoryStats {
    /// Outgoing client domain of this Runtime. `None` until the first
    /// connection attempt freezes the domain; observing it never freezes.
    pub runtime_outgoing: Option<MemoryScopeStats>,
    /// Server direction of this Runtime's Core host, when one exists.
    pub server: Option<MemoryScopeStats>,
    /// One Runtime's B domain, initialized by business calls or HTTP acquisition.
    pub payload_buffer: Option<MemoryCellStats>,
}

/// One retired scope: a role label plus a weak view of its budget accounting.
///
/// The weak handle shares nothing with the domain owner: it never retains a
/// pool, mapping, connection, cache, Runtime, callback, or even the budget
/// counters. The record is reportable exactly while a real owner of the
/// domain — its Runtime/client pool or an outstanding reservation guard —
/// keeps the accounting state alive.
#[derive(Debug, Clone)]
struct RetiredScope {
    /// Scope label; one of [`scope::RUNTIME_OUTGOING`] or [`scope::SERVER`].
    role: &'static str,
    /// Weak view of the domain's shared accounting state and limits.
    observer: BudgetObserverWeak,
}

impl RetiredScope {
    /// Project this retired scope if its domain is still owned, without
    /// touching live transport authority.
    fn stats(&self) -> Option<MemoryScopeStats> {
        self.observer
            .upgrade()
            .map(|observer| MemoryScopeStats::from_budget(observer.limits(), observer.snapshot()))
    }
}

/// Read-only snapshot of one retired scope in registration order.
///
/// The bundle's scopes live behind an interior lock (they are pruned
/// individually once their owners are gone), so callers receive this
/// projected value instead of a reference into the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredScopeReport {
    /// Scope label; one of [`scope::RUNTIME_OUTGOING`] or [`scope::SERVER`].
    pub role: &'static str,
    /// Scope-labelled budget snapshot at observation time.
    pub stats: MemoryScopeStats,
}

#[derive(Debug, Default)]
struct RetiredMemoryObservationState {
    scopes: parking_lot::Mutex<Vec<RetiredScope>>,
    payload_buffers: parking_lot::Mutex<Vec<PayloadBufferObserverWeak>>,
    trackers: parking_lot::Mutex<Vec<LocalCopyObserver>>,
}

/// Read-only observation bundle for the memory domains of one retired session.
///
/// Built before the owning session shuts down and adopted by its replacement,
/// this keeps retired budget usage and local-buffer metadata observable for
/// exactly as long as real owners keep them alive. It stores only weak
/// handles to budget accounting and local-buffer metadata — never a Runtime, cache,
/// pool, connection, callback, payload, or the accounting/tracker `Arc`s
/// themselves — so observation can never extend a producer's lifetime, and a
/// producer that can still publish (an in-flight response or an outstanding
/// hold) keeps its record observable through its own ownership. Observation never connects, maps, freezes configuration,
/// instantiates a host, or resets accounting.
///
/// ## Lifecycle rule: the owner decides
///
/// An initially-zero record is **not** quiescent, and neither is a drained
/// one: a supported producer may publish later, so records are never dropped
/// for having zero counters. A record is dropped only when its weak handle
/// detaches — the moment no real owner can publish into or use it anymore.
/// That is why there is no close-confirmation fence: a confirmed transport
/// close does not prove future publication is impossible: local copies can
/// outlive their Runtime, while owner lifetime still bounds observation.
#[derive(Debug, Clone, Default)]
pub struct RetiredMemoryObservation {
    state: Arc<RetiredMemoryObservationState>,
}

impl RetiredMemoryObservation {
    /// A new observation bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one retired budget scope under its role label.
    ///
    /// Stores a weak view; the domain owner and its reservation guards decide
    /// how long the record stays observable.
    pub fn push_scope(&self, role: &'static str, observer: BudgetObserver) {
        self.state.scopes.lock().push(RetiredScope {
            role,
            observer: observer.downgrade(),
        });
    }

    pub fn push_payload_buffer(&self, observer: PayloadBufferObserverWeak) {
        let mut buffers = self.state.payload_buffers.lock();
        if !buffers.iter().any(|existing| existing.ptr_eq(&observer)) {
            buffers.push(observer);
        }
    }

    pub fn compose_payload_buffer_reports(
        bundles: &[Arc<RetiredMemoryObservation>],
    ) -> Vec<MemoryCellStats> {
        let mut seen = Vec::<PayloadBufferObserverWeak>::new();
        let mut reports = Vec::new();
        for bundle in bundles {
            for observer in bundle.state.payload_buffers.lock().iter() {
                if seen.iter().any(|existing| existing.ptr_eq(observer)) {
                    continue;
                }
                if let Some(snapshot) = observer.snapshot() {
                    seen.push(observer.clone());
                    reports.push(MemoryCellStats::from_payload_buffer(snapshot));
                }
            }
        }
        reports
    }

    /// Register one Rust-owned local-buffer tracker; duplicates by identity
    /// are skipped so one tracker can never be observed twice.
    pub fn push_tracker(&self, tracker: Arc<LocalCopyTracker>) {
        let observer = tracker.observer();
        let mut trackers = self.state.trackers.lock();
        if !trackers.iter().any(|existing| existing.ptr_eq(&observer)) {
            trackers.push(observer);
        }
    }

    /// Projected snapshots of the still-owned retired scopes, in registration
    /// order. Detached scopes — whose last owner is gone — report nothing and
    /// are not listed.
    pub fn scope_reports(&self) -> Vec<RetiredScopeReport> {
        self.state
            .scopes
            .lock()
            .iter()
            .filter_map(|scope| {
                scope.stats().map(|stats| RetiredScopeReport {
                    role: scope.role,
                    stats,
                })
            })
            .collect()
    }

    /// Local-buffer counters composed across every still-owned tracker in
    /// this bundle. Detached trackers contribute nothing.
    pub fn local_copy_stats(&self) -> LocalCopyStats {
        let mut stats = LocalCopyStats::default();
        for observer in self.state.trackers.lock().iter() {
            if let Some(snapshot) = observer.stats() {
                stats.merge(&snapshot);
            }
        }
        stats
    }

    /// Drop records whose last real owner is gone, and de-duplicate the rest.
    ///
    /// Records with zero counters are deliberately kept: zero is not
    /// quiescent while any owner — and therefore any supported producer —
    /// remains. Repeated observation of an ownerless session therefore
    /// converges to an empty bundle instead of accumulating records.
    pub fn prune_detached(&self) {
        self.state
            .scopes
            .lock()
            .retain(|scope| scope.observer.is_alive());
        self.state
            .payload_buffers
            .lock()
            .retain(PayloadBufferObserverWeak::is_alive);
        let mut trackers = self.state.trackers.lock();
        trackers.retain(|observer| observer.is_alive());
        let mut deduped: Vec<LocalCopyObserver> = Vec::with_capacity(trackers.len());
        for observer in trackers.iter() {
            if !deduped.iter().any(|existing| existing.ptr_eq(observer)) {
                deduped.push(observer.clone());
            }
        }
        *trackers = deduped;
    }

    /// Whether every record was pruned away, so the bundle itself can go.
    pub fn is_empty(&self) -> bool {
        self.state.scopes.lock().is_empty()
            && self.state.payload_buffers.lock().is_empty()
            && self.state.trackers.lock().is_empty()
    }

    /// Prune detached records inside bundles and drop emptied bundles from a
    /// session's adopted list.
    ///
    /// This is the one list-maintenance rule for retired observations: it is
    /// shared by every native projection so per-record pruning and bundle
    /// cardinality behave identically everywhere.
    pub fn retain_live_bundles(bundles: &mut Vec<Arc<RetiredMemoryObservation>>) {
        for bundle in bundles.iter() {
            bundle.prune_detached();
        }
        bundles.retain(|bundle| !bundle.is_empty());
    }

    /// Projected snapshots of the still-owned scopes across several bundles,
    /// de-duplicated by accounting-domain identity in first-seen order.
    ///
    /// Capture is retry-safe, so the same domain may legitimately be
    /// registered in more than one adopted bundle; composition must count it
    /// once.
    pub fn compose_scope_reports(
        bundles: &[Arc<RetiredMemoryObservation>],
    ) -> Vec<RetiredScopeReport> {
        let mut reports = Vec::new();
        let mut seen: Vec<BudgetObserverWeak> = Vec::new();
        for bundle in bundles {
            for scope in bundle.state.scopes.lock().iter() {
                if !scope.observer.is_alive() {
                    continue;
                }
                if seen.iter().any(|existing| existing.ptr_eq(&scope.observer)) {
                    continue;
                }
                if let Some(observer) = scope.observer.upgrade() {
                    seen.push(scope.observer.clone());
                    reports.push(RetiredScopeReport {
                        role: scope.role,
                        stats: MemoryScopeStats::from_budget(
                            observer.limits(),
                            observer.snapshot(),
                        ),
                    });
                }
            }
        }
        reports
    }

    /// Local-buffer counters across several bundles, de-duplicated by
    /// tracker identity so one tracker observed through two bundles is
    /// counted once.
    pub fn compose_local_copy_stats(bundles: &[Arc<RetiredMemoryObservation>]) -> LocalCopyStats {
        let mut stats = LocalCopyStats::default();
        let mut seen: Vec<LocalCopyObserver> = Vec::new();
        for bundle in bundles {
            for observer in bundle.state.trackers.lock().iter() {
                if !observer.is_alive() {
                    continue;
                }
                if seen.iter().any(|existing| existing.ptr_eq(observer)) {
                    continue;
                }
                if let Some(snapshot) = observer.stats() {
                    seen.push(observer.clone());
                    stats.merge(&snapshot);
                }
            }
        }
        stats
    }
}

/// One retirement handoff: the bundle created by a retiring session plus
/// every earlier bundle that session had adopted.
///
/// The retiring session captures the handoff without consuming its own
/// records, and capture is retry-safe: every call re-captures the session's
/// current own domains and tracker, so a replacement attempt that fails
/// construction or adoption loses nothing, and a later retry observes scopes
/// that only came into existence after the failed attempt. Installing the
/// handoff into a replacement session's fresh list is identity-deduplicated,
/// and composition de-duplicates by domain/tracker identity, so the same
/// session captured twice can never double-count.
#[derive(Debug, Clone)]
pub struct RetirementHandoff {
    pending: Arc<RetiredMemoryObservation>,
    carried: Vec<Arc<RetiredMemoryObservation>>,
}

impl RetirementHandoff {
    /// Capture one retirement: this session's pending bundle and clones of
    /// every bundle the session had already adopted.
    pub fn new(
        pending: Arc<RetiredMemoryObservation>,
        carried: Vec<Arc<RetiredMemoryObservation>>,
    ) -> Self {
        Self { pending, carried }
    }

    /// The bundle created by this retirement event.
    pub fn pending(&self) -> &Arc<RetiredMemoryObservation> {
        &self.pending
    }

    /// Install the pending and carried bundles into a replacement session's
    /// (fresh) adopted list, de-duplicated by bundle identity.
    ///
    /// Nothing is discarded here: whether a record currently reports zero or
    /// holds live charges, its observable lifetime is decided by its real
    /// owners, never by adoption.
    pub fn adopt_into(&self, retired: &mut Vec<Arc<RetiredMemoryObservation>>) {
        fn push_deduped(
            bundle: Arc<RetiredMemoryObservation>,
            retired: &mut Vec<Arc<RetiredMemoryObservation>>,
        ) {
            if !retired
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &bundle))
            {
                retired.push(bundle);
            }
        }
        push_deduped(Arc::clone(&self.pending), retired);
        for bundle in &self.carried {
            push_deduped(Arc::clone(bundle), retired);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::retention::PayloadBufferBudget;
    use c2_mem::{BudgetKind, MemoryBudget};

    #[test]
    fn retired_payload_buffer_reports_live_storage_once_until_last_owner_drops() {
        let budget = PayloadBufferBudget::new(8);
        let reservation = budget.reserve_blocking(5, None).unwrap();
        let first = Arc::new(RetiredMemoryObservation::new());
        let second = Arc::new(RetiredMemoryObservation::new());
        first.push_payload_buffer(budget.downgrade());
        second.push_payload_buffer(budget.downgrade());
        drop(budget);
        let bundles = vec![first, second];
        let reports = RetiredMemoryObservation::compose_payload_buffer_reports(&bundles);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].used_bytes, 5);
        drop(reservation);
        assert!(RetiredMemoryObservation::compose_payload_buffer_reports(&bundles).is_empty());
    }

    fn limits() -> MemoryBudgetLimits {
        MemoryBudgetLimits {
            shm_backing_budget_bytes: 4096,
            file_backing_budget_bytes: 0,
            live_reassembly_budget_bytes: 8192,
        }
    }

    fn retained_meta(bytes: usize) -> LocalCopyMeta {
        LocalCopyMeta {
            route_name: "grid".to_string(),
            method_name: "echo".to_string(),
            direction: LocalCopyDirection::ClientResponse,
            bytes,
        }
    }

    #[test]
    fn local_buffers_count_logical_bytes_by_direction_until_release() {
        let tracker = LocalCopyTracker::new();
        let response = tracker.track(retained_meta(17));
        let input = tracker.track(LocalCopyMeta {
            direction: LocalCopyDirection::ResourceInput,
            bytes: 31,
            ..retained_meta(0)
        });
        let stats = tracker.stats();
        assert_eq!(stats.active_holds, 2);
        assert_eq!(stats.total_held_bytes, 48);
        assert_eq!(
            stats.by_direction[&LocalCopyDirection::ClientResponse].total_held_bytes,
            17
        );
        assert_eq!(
            stats.by_direction[&LocalCopyDirection::ResourceInput].total_held_bytes,
            31
        );
        assert!(stats.oldest_hold_seconds >= 0.0);
        drop((response, input));
        assert_eq!(tracker.stats().active_holds, 0);
        assert_eq!(tracker.stats().total_held_bytes, 0);
    }

    #[test]
    fn local_hold_does_not_keep_an_ownerless_retired_backing_domain() {
        let budget = MemoryBudget::from_limits(&limits());
        let tracker = Arc::new(LocalCopyTracker::new());
        let hold = tracker.track(retained_meta(128));
        let observation = RetiredMemoryObservation::new();
        observation.push_scope(scope::SERVER, BudgetObserver::new(budget.clone()));
        observation.push_tracker(tracker.clone());
        drop((budget, tracker));
        observation.prune_detached();
        assert!(observation.scope_reports().is_empty());
        assert_eq!(observation.local_copy_stats().total_held_bytes, 128);
        drop(hold);
        observation.prune_detached();
        assert!(observation.is_empty());
    }

    #[test]
    fn retired_observation_reports_live_charges_after_its_owner_drops() {
        let limits = limits();
        let budget = MemoryBudget::from_limits(&limits);
        let reservation = budget.reserve(BudgetKind::Shm, 1024).expect("reserve");
        let tracker = Arc::new(LocalCopyTracker::new());
        let hold = tracker.track(retained_meta(64));

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(scope::RUNTIME_OUTGOING, BudgetObserver::new(budget.clone()));
        observation.push_tracker(Arc::clone(&tracker));
        // Simulate the failed-capture path registering the same tracker twice.
        observation.push_tracker(Arc::clone(&tracker));

        assert!(!observation.is_empty());
        let retired = &observation.scope_reports()[0];
        assert_eq!(retired.role, scope::RUNTIME_OUTGOING);
        assert_eq!(retired.stats.limits, limits);
        assert_eq!(retired.stats.shm.used_bytes, 1024);
        assert_eq!(retired.stats.shm.limit_bytes, 4096);
        assert_eq!(observation.local_copy_stats().active_holds, 1);
        assert_eq!(observation.local_copy_stats().total_held_bytes, 64);

        // The owning session drops its own handles, exactly like a session
        // swap releasing the retired RuntimeSession. The outstanding charge
        // and hold are themselves real owners, so both records stay
        // observable and keep reporting live values.
        drop(budget);
        drop(tracker);
        observation.prune_detached();
        assert_eq!(observation.scope_reports()[0].stats.shm.used_bytes, 1024);
        assert_eq!(observation.local_copy_stats().active_holds, 1);

        // The last owners finish: records detach and the bundle empties.
        drop(reservation);
        drop(hold);
        observation.prune_detached();
        assert!(observation.is_empty());
        assert!(observation.scope_reports().is_empty());
    }

    #[test]
    fn zero_records_stay_observable_while_a_producer_owner_exists() {
        // Initially zero is not quiescent: while the session (or any producer
        // handle it handed out) still owns the domain and tracker, an
        // in-flight response can still create backing or a held result, so a
        // zero record must stay observable.
        let limits = limits();
        let budget = MemoryBudget::from_limits(&limits);
        let tracker = Arc::new(LocalCopyTracker::new());

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(scope::RUNTIME_OUTGOING, BudgetObserver::new(budget.clone()));
        observation.push_tracker(Arc::clone(&tracker));

        observation.prune_detached();
        assert_eq!(observation.scope_reports().len(), 1);
        assert_eq!(observation.scope_reports()[0].stats.shm.used_bytes, 0);
        assert!(!observation.is_empty());

        // The session handle goes away; the zero records detach immediately
        // because no producer can publish into them anymore.
        drop(budget);
        drop(tracker);
        observation.prune_detached();
        assert!(observation.is_empty());
    }

    #[test]
    fn a_late_hold_published_after_the_owner_handle_drops_stays_visible() {
        // A producer handle the retired session handed out — what an old
        // proxy or an in-flight response holds — keeps the tracker alive, so
        // a hold published after the session itself is gone must remain
        // observable through the same record.
        let limits = limits();
        let budget = MemoryBudget::from_limits(&limits);
        let session_tracker = Arc::new(LocalCopyTracker::new());
        let producer_handle = session_tracker.clone();

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(scope::RUNTIME_OUTGOING, BudgetObserver::new(budget.clone()));
        observation.push_tracker(Arc::clone(&session_tracker));
        drop((budget, session_tracker));

        let late_charge = producer_handle.track(retained_meta(48));
        assert_eq!(observation.local_copy_stats().active_holds, 1);
        assert_eq!(observation.local_copy_stats().total_held_bytes, 48);

        drop(late_charge);
        drop(producer_handle);
        observation.prune_detached();
        assert!(observation.is_empty());
    }

    #[test]
    fn repeated_session_swaps_keep_one_live_hold_without_growing_records() {
        // One long-lived hold outlives repeated session swaps. Each swap's
        // own records detach when that session's handles drop, so only the
        // bundle whose owners are still alive survives and the adopted list
        // never grows.
        let limits = limits();
        let charged = MemoryBudget::from_limits(&limits);
        let reservation = charged.reserve(BudgetKind::Shm, 2048).expect("reserve");
        let held_tracker = Arc::new(LocalCopyTracker::new());
        let hold = held_tracker.track(retained_meta(96));

        let mut current: Vec<Arc<RetiredMemoryObservation>> = Vec::new();
        for swap in 0..4 {
            let pending = Arc::new(RetiredMemoryObservation::new());
            if swap == 0 {
                // The first session owns the outstanding charge and hold.
                pending.push_scope(
                    scope::RUNTIME_OUTGOING,
                    BudgetObserver::new(charged.clone()),
                );
                pending.push_tracker(Arc::clone(&held_tracker));
            } else {
                // Later sessions are empty: fresh tracker, no scopes. Their
                // session handles drop with the swap, so their records
                // detach.
                pending.push_tracker(Arc::new(LocalCopyTracker::new()));
            }
            let handoff = RetirementHandoff::new(pending, current.clone());
            let mut replacement = Vec::new();
            handoff.adopt_into(&mut replacement);
            current = replacement;
            RetiredMemoryObservation::retain_live_bundles(&mut current);

            assert_eq!(
                current.len(),
                1,
                "swap {swap}: only the bundle with live owners may survive"
            );
            let mut stats = LocalCopyStats::default();
            for bundle in &current {
                stats.merge(&bundle.local_copy_stats());
            }
            assert_eq!(stats.active_holds, 1);
            assert_eq!(stats.total_held_bytes, 96);
            let reports = RetiredMemoryObservation::compose_scope_reports(&current);
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].role, scope::RUNTIME_OUTGOING);
            assert_eq!(reports[0].stats.shm.used_bytes, 2048);
        }

        // The hold ends: the retirement drains completely and the last bundle
        // is pruned away record by record.
        drop(reservation);
        drop(hold);
        drop(charged);
        drop(held_tracker);
        RetiredMemoryObservation::retain_live_bundles(&mut current);
        assert!(current.is_empty());
    }

    #[test]
    fn capture_is_retry_safe_and_composition_never_double_counts() {
        // A failed replacement attempt captures but never adopts; the retry
        // re-captures the session's own records — including scopes created
        // only after the failed attempt — and composition de-duplicates the
        // same tracker/domain observed through two adopted bundles.
        let limits = limits();
        let budget = MemoryBudget::from_limits(&limits);
        let tracker = Arc::new(LocalCopyTracker::new());
        let hold = tracker.track(retained_meta(64));

        // First retirement succeeded earlier and was adopted.
        let mut adopted: Vec<Arc<RetiredMemoryObservation>> = Vec::new();
        let earlier = Arc::new(RetiredMemoryObservation::new());
        earlier.push_tracker(Arc::clone(&tracker));
        RetirementHandoff::new(Arc::clone(&earlier), vec![]).adopt_into(&mut adopted);

        // Failed attempt: captured, then dropped without adoption. The
        // session keeps its records, so nothing is lost.
        let failed = RetirementHandoff::new(Arc::new(RetiredMemoryObservation::new()), vec![]);
        drop(failed);

        // The session's own outgoing domain is created only after the failed
        // attempt; the retry's fresh capture must include it.
        let retry = Arc::new(RetiredMemoryObservation::new());
        retry.push_scope(scope::RUNTIME_OUTGOING, BudgetObserver::new(budget.clone()));
        retry.push_tracker(Arc::clone(&tracker));
        let handoff = RetirementHandoff::new(retry, adopted.clone());
        let mut replacement = Vec::new();
        handoff.adopt_into(&mut replacement);
        handoff.adopt_into(&mut replacement);
        assert_eq!(
            replacement.len(),
            2,
            "bundle adoption is identity-deduplicated"
        );

        // The same tracker lives in both bundles; composition counts it once.
        let mut stats = LocalCopyStats::default();
        stats.merge(&RetiredMemoryObservation::compose_local_copy_stats(
            &replacement,
        ));
        assert_eq!(stats.active_holds, 1);
        assert_eq!(stats.total_held_bytes, 64);
        let reports = RetiredMemoryObservation::compose_scope_reports(&replacement);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].role, scope::RUNTIME_OUTGOING);

        drop(hold);
        drop(tracker);
        drop(budget);
        RetiredMemoryObservation::retain_live_bundles(&mut replacement);
        assert!(replacement.is_empty());
    }
}
