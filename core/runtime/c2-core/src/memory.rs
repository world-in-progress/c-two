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
//! Retired observations follow one lifecycle rule: **the owner decides**. A
//! retired record stores only weak handles to budget accounting and lease
//! metadata, never the accounting itself, so a record stays reportable
//! exactly while a real owner — a retired session's still-live Runtime/client
//! pool, an old proxy's native client or tracker handle, an in-flight
//! response, or an outstanding lease or reservation guard — keeps that
//! metadata alive. A late held result published through any such owner stays
//! visible; once the last owner is gone the record detaches and is pruned on
//! the next observation. There is deliberately no close-confirmation fence:
//! transport close outcomes cannot prove that no supported SDK producer will
//! publish later (an old proxy remains legitimately usable after a public
//! shutdown), so observable lifetime is bound to producer lifetime instead.

use std::sync::Arc;

use c2_config::MemoryBudgetLimits;
use c2_mem::{
    BudgetCellSnapshot, BudgetObserver, BudgetObserverWeak, BudgetSnapshot, BufferLeaseObserver,
    BufferLeaseStats, BufferLeaseTracker,
};
use c2_server::ServerMemorySnapshot;

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
            .map(|observer| MemoryScopeStats::from_budget(*observer.limits(), observer.snapshot()))
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
    trackers: parking_lot::Mutex<Vec<BufferLeaseObserver>>,
}

/// Read-only observation bundle for the memory domains of one retired session.
///
/// Built before the owning session shuts down and adopted by its replacement,
/// this keeps retired budget usage and retained-lease metadata observable for
/// exactly as long as real owners keep them alive. It stores only weak
/// handles to budget accounting and lease metadata — never a Runtime, cache,
/// pool, connection, callback, payload, or the accounting/tracker `Arc`s
/// themselves — so observation can never extend a producer's lifetime, and a
/// producer that can still publish (a live old proxy, an in-flight response,
/// an outstanding hold) automatically keeps its record observable through its
/// own ownership. Observation never connects, maps, freezes configuration,
/// instantiates a host, or resets accounting.
///
/// ## Lifecycle rule: the owner decides
///
/// An initially-zero record is **not** quiescent, and neither is a drained
/// one: a supported producer may publish later, so records are never dropped
/// for having zero counters. A record is dropped only when its weak handle
/// detaches — the moment no real owner can publish into or use it anymore.
/// That is why there is no close-confirmation fence: a confirmed transport
/// close does not prove future publication is impossible (old proxies stay
/// legitimately usable after a public shutdown), while owner lifetime does.
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

    /// Register one Rust-owned retained-lease tracker; duplicates by identity
    /// are skipped so one tracker can never be observed twice.
    pub fn push_tracker(&self, tracker: Arc<BufferLeaseTracker>) {
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

    /// Retained-lease counters composed across every still-owned tracker in
    /// this bundle. Detached trackers contribute nothing.
    pub fn lease_stats(&self) -> BufferLeaseStats {
        let mut stats = BufferLeaseStats::default();
        for observer in self.state.trackers.lock().iter() {
            if let Some(snapshot) = observer.stats() {
                stats.merge(&snapshot);
            }
        }
        stats
    }

    /// Retained-lease snapshots older than `threshold`, composed across every
    /// still-owned tracker in this bundle without exposing the trackers.
    pub fn sweep_retained_snapshots(
        &self,
        threshold: std::time::Duration,
    ) -> Vec<c2_mem::BufferLeaseSnapshot> {
        let mut snapshots = Vec::new();
        for observer in self.state.trackers.lock().iter() {
            if let Some(retained) = observer.sweep_retained(threshold) {
                snapshots.extend(retained);
            }
        }
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
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
        let mut trackers = self.state.trackers.lock();
        trackers.retain(|observer| observer.is_alive());
        let mut deduped: Vec<BufferLeaseObserver> = Vec::with_capacity(trackers.len());
        for observer in trackers.iter() {
            if !deduped.iter().any(|existing| existing.ptr_eq(observer)) {
                deduped.push(observer.clone());
            }
        }
        *trackers = deduped;
    }

    /// Whether every record was pruned away, so the bundle itself can go.
    pub fn is_empty(&self) -> bool {
        self.state.scopes.lock().is_empty() && self.state.trackers.lock().is_empty()
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
                            *observer.limits(),
                            observer.snapshot(),
                        ),
                    });
                }
            }
        }
        reports
    }

    /// Retained-lease counters across several bundles, de-duplicated by
    /// tracker identity so one tracker observed through two bundles is
    /// counted once.
    pub fn compose_lease_stats(bundles: &[Arc<RetiredMemoryObservation>]) -> BufferLeaseStats {
        let mut stats = BufferLeaseStats::default();
        let mut seen: Vec<BufferLeaseObserver> = Vec::new();
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

    /// Retained-lease snapshots older than `threshold` across several
    /// bundles, de-duplicated by tracker identity.
    pub fn compose_sweep_snapshots(
        bundles: &[Arc<RetiredMemoryObservation>],
        threshold: std::time::Duration,
    ) -> Vec<c2_mem::BufferLeaseSnapshot> {
        let mut snapshots = Vec::new();
        let mut seen: Vec<BufferLeaseObserver> = Vec::new();
        for bundle in bundles {
            for observer in bundle.state.trackers.lock().iter() {
                if !observer.is_alive() {
                    continue;
                }
                if seen.iter().any(|existing| existing.ptr_eq(observer)) {
                    continue;
                }
                if let Some(retained) = observer.sweep_retained(threshold) {
                    seen.push(observer.clone());
                    snapshots.extend(retained);
                }
            }
        }
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
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
    use c2_mem::{
        BudgetKind, BufferLeaseMeta, BufferStorage, LeaseDirection, LeaseRetention, MemoryBudget,
    };

    fn limits() -> MemoryBudgetLimits {
        MemoryBudgetLimits {
            shm_backing_budget_bytes: 4096,
            file_backing_budget_bytes: 0,
            live_reassembly_budget_bytes: 8192,
        }
    }

    fn retained_meta(bytes: usize) -> BufferLeaseMeta {
        BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "echo".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Retained,
            storage: BufferStorage::Shm,
            bytes,
        }
    }

    #[test]
    fn retired_observation_reports_live_charges_after_its_owner_drops() {
        let limits = limits();
        let budget = MemoryBudget::from_limits(&limits);
        let reservation = budget.reserve(BudgetKind::Shm, 1024).expect("reserve");
        let tracker = Arc::new(BufferLeaseTracker::new(false));
        let hold = tracker.track(retained_meta(64));

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(
            scope::RUNTIME_OUTGOING,
            BudgetObserver::new(limits, budget.clone()),
        );
        observation.push_tracker(Arc::clone(&tracker));
        // Simulate the failed-capture path registering the same tracker twice.
        observation.push_tracker(Arc::clone(&tracker));

        assert!(!observation.is_empty());
        let retired = &observation.scope_reports()[0];
        assert_eq!(retired.role, scope::RUNTIME_OUTGOING);
        assert_eq!(retired.stats.limits, limits);
        assert_eq!(retired.stats.shm.used_bytes, 1024);
        assert_eq!(retired.stats.shm.limit_bytes, 4096);
        assert_eq!(observation.lease_stats().active_holds, 1);
        assert_eq!(observation.lease_stats().total_held_bytes, 64);

        // The owning session drops its own handles, exactly like a session
        // swap releasing the retired RuntimeSession. The outstanding charge
        // and hold are themselves real owners, so both records stay
        // observable and keep reporting live values.
        drop(budget);
        drop(tracker);
        observation.prune_detached();
        assert_eq!(observation.scope_reports()[0].stats.shm.used_bytes, 1024);
        assert_eq!(observation.lease_stats().active_holds, 1);

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
        let tracker = Arc::new(BufferLeaseTracker::new(false));

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(
            scope::RUNTIME_OUTGOING,
            BudgetObserver::new(limits, budget.clone()),
        );
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
        let session_tracker = Arc::new(BufferLeaseTracker::new(false));
        let producer_handle = session_tracker.clone();

        let observation = RetiredMemoryObservation::new();
        observation.push_scope(
            scope::RUNTIME_OUTGOING,
            BudgetObserver::new(limits, budget.clone()),
        );
        observation.push_tracker(Arc::clone(&session_tracker));
        drop((budget, session_tracker));

        let late_charge = producer_handle.track(retained_meta(48));
        assert_eq!(observation.lease_stats().active_holds, 1);
        assert_eq!(observation.lease_stats().total_held_bytes, 48);

        drop(late_charge);
        drop(producer_handle);
        observation.prune_detached();
        assert!(observation.is_empty());
    }

    #[test]
    fn sweep_snapshots_are_composed_without_exposing_trackers() {
        let tracker = Arc::new(BufferLeaseTracker::new(false));
        let _hold = tracker.track(retained_meta(16));

        let observation = RetiredMemoryObservation::new();
        observation.push_tracker(Arc::clone(&tracker));
        observation.push_tracker(Arc::clone(&tracker));

        let snapshots = observation.sweep_retained_snapshots(std::time::Duration::ZERO);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].route_name, "grid");
        assert_eq!(snapshots[0].bytes, 16);

        drop(_hold);
        drop(tracker);
        observation.prune_detached();
        assert!(
            observation
                .sweep_retained_snapshots(std::time::Duration::ZERO)
                .is_empty()
        );
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
        let held_tracker = Arc::new(BufferLeaseTracker::new(false));
        let hold = held_tracker.track(retained_meta(96));

        let mut current: Vec<Arc<RetiredMemoryObservation>> = Vec::new();
        for swap in 0..4 {
            let pending = Arc::new(RetiredMemoryObservation::new());
            if swap == 0 {
                // The first session owns the outstanding charge and hold.
                pending.push_scope(
                    scope::RUNTIME_OUTGOING,
                    BudgetObserver::new(limits, charged.clone()),
                );
                pending.push_tracker(Arc::clone(&held_tracker));
            } else {
                // Later sessions are empty: fresh tracker, no scopes. Their
                // session handles drop with the swap, so their records
                // detach.
                pending.push_tracker(Arc::new(BufferLeaseTracker::new(false)));
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
            let mut stats = BufferLeaseStats::default();
            for bundle in &current {
                stats.merge(&bundle.lease_stats());
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
        let tracker = Arc::new(BufferLeaseTracker::new(false));
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
        retry.push_scope(
            scope::RUNTIME_OUTGOING,
            BudgetObserver::new(limits, budget.clone()),
        );
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
        let mut stats = BufferLeaseStats::default();
        stats.merge(&RetiredMemoryObservation::compose_lease_stats(&replacement));
        assert_eq!(stats.active_holds, 1);
        assert_eq!(stats.total_held_bytes, 64);
        let reports = RetiredMemoryObservation::compose_scope_reports(&replacement);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].role, scope::RUNTIME_OUTGOING);
        let sweeps = RetiredMemoryObservation::compose_sweep_snapshots(
            &replacement,
            std::time::Duration::ZERO,
        );
        assert_eq!(sweeps.len(), 1);

        drop(hold);
        drop(tracker);
        drop(budget);
        RetiredMemoryObservation::retain_live_bundles(&mut replacement);
        assert!(replacement.is_empty());
    }
}
