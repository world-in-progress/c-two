//! Read-only, role-labelled capacity snapshots.
//!
//! These values describe C-Two-owned domain accounting — the memory and
//! file-spill cells and the call-admission slots of one role domain. They are
//! deliberately **not** process RSS: each cell is an independent finite scope
//! and their sum must never be presented as physical memory usage.
//!
//! Every accessor here is observation-only. Looking at a snapshot never
//! connects, maps memory, reads the environment, freezes a configuration,
//! creates a domain, or resets accounting.
//!
//! Retired observations retain weak metadata only. A hold never retains a
//! pool, connection, mapping, or an otherwise ownerless backing domain.

use std::collections::{BTreeMap, HashMap};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use parking_lot::Mutex;

use c2_mem::{BudgetCellSnapshot, DomainSnapshot};

use crate::roles::RetiredRoleObserver;

/// One of a Runtime's two independent role domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleKind {
    /// The domain frozen by the first valid remote business connection/call.
    Client,
    /// The domain frozen when a Host starts.
    Server,
}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

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

/// One role domain's read-only memory statistics. Counters are `None` until
/// the role activates; a frozen or retired domain always reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleMemoryStats {
    /// Unique identity of the minted accounting domain; `None` before
    /// activation.
    pub domain_id: Option<u64>,
    /// Advance on every Host restart that reuses the server domain.
    pub generation: u64,
    /// True once the role resolved and minted its domain.
    pub frozen: bool,
    /// True once permanent retirement closed this role's call admission.
    pub closed: bool,
    /// Memory-cell accounting, or `None` while the role is not activated.
    pub memory: Option<MemoryCellStats>,
    /// File-spill-cell accounting, or `None` while the role is not activated.
    pub file_spill: Option<MemoryCellStats>,
}

/// One retired role's projected accounting, in retirement order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredRoleReport {
    pub role: RoleKind,
    pub stats: RoleMemoryStats,
}

/// Read-only memory statistics for one Core Runtime: the symmetric
/// `client`/`server` role domains, retired roles still owned by real
/// in-flight work, and local holds.
#[derive(Debug, Clone)]
pub struct RuntimeMemoryStats {
    pub client: RoleMemoryStats,
    pub server: RoleMemoryStats,
    pub retired: Vec<RetiredRoleReport>,
    pub holds: LocalCopyStats,
}

impl RoleMemoryStats {
    /// Project one live domain snapshot.
    pub(crate) fn from_snapshot(generation: u64, snapshot: &DomainSnapshot) -> Self {
        Self {
            domain_id: Some(snapshot.domain_id),
            generation,
            frozen: true,
            closed: snapshot.closed,
            memory: Some(MemoryCellStats::from_cell(snapshot.memory)),
            file_spill: Some(MemoryCellStats::from_cell(snapshot.file_spill)),
        }
    }

    pub(crate) fn from_role(stats: &crate::roles::RoleDomainStats) -> Self {
        if let Some(snapshot) = &stats.snapshot {
            return Self::from_snapshot(stats.generation, snapshot);
        }
        Self {
            domain_id: None,
            generation: stats.generation,
            frozen: false,
            closed: false,
            memory: None,
            file_spill: None,
        }
    }
}

/// One role's call-admission slot snapshot. Counters are `None` until the
/// role activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleCallExecutionStats {
    pub max_operations: Option<u64>,
    pub used_operations: Option<u64>,
    pub peak_operations: Option<u64>,
    pub rejected_reservations: Option<u64>,
    pub closed: bool,
}

impl RoleCallExecutionStats {
    pub(crate) fn from_snapshot(snapshot: &c2_mem::RetentionSnapshot) -> Self {
        Self {
            max_operations: Some(snapshot.max_operations),
            used_operations: Some(snapshot.used_operations),
            peak_operations: Some(snapshot.peak_operations),
            rejected_reservations: Some(snapshot.rejected_reservations),
            closed: snapshot.closed,
        }
    }
}

/// One retired role's call-admission snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredCallExecutionReport {
    pub role: RoleKind,
    pub stats: RoleCallExecutionStats,
}

/// The symmetric per-role call-execution observation of one Runtime.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeCallExecutionStats {
    pub client: RoleCallExecutionStats,
    pub server: RoleCallExecutionStats,
    pub retired: Vec<RetiredCallExecutionReport>,
}

/// Both symmetric observations captured under one state lock.
#[derive(Debug, Clone)]
pub(crate) struct RoleStatsBundle {
    pub memory: RuntimeMemoryStats,
    pub call_execution: RuntimeCallExecutionStats,
}

/// Project one role domain's call-admission counters; a dormant role reports
/// `None` counters and an open domain.
pub(crate) fn role_call_stats(stats: &crate::roles::RoleDomainStats) -> RoleCallExecutionStats {
    match stats.snapshot.as_ref() {
        Some(snapshot) => RoleCallExecutionStats::from_snapshot(&snapshot.operations),
        None => RoleCallExecutionStats::default(),
    }
}

/// The retired-role observation list of one session.
///
/// Stores only weak handles to role-domain accounting and local-buffer
/// metadata — never a Runtime, cache, pool, connection, callback, payload, or
/// the accounting/tracker `Arc`s themselves — so observation can never extend
/// a producer's lifetime, and a producer that can still publish (an in-flight
/// response or an outstanding hold) keeps its record observable through its
/// own ownership. Observation never connects, maps, freezes configuration,
/// instantiates a host, or resets accounting.
///
/// ## Lifecycle rule: the owner decides
///
/// An initially-zero record is **not** quiescent, and neither is a drained
/// one: a supported producer may publish later, so records are never dropped
/// for having zero counters. A record is dropped only when its weak handle
/// detaches — the moment no real owner can publish into or use it anymore.
#[derive(Debug, Default)]
pub(crate) struct RetiredRoles {
    roles: Vec<RetiredRoleObserver>,
    trackers: Vec<LocalCopyObserver>,
}

impl RetiredRoles {
    pub(crate) fn reports(&self) -> (Vec<RetiredRoleReport>, Vec<RetiredCallExecutionReport>) {
        self.roles
            .iter()
            .filter_map(|observer| {
                let snapshot = observer.snapshot()?;
                Some((
                    RetiredRoleReport {
                        role: observer.role,
                        stats: RoleMemoryStats::from_snapshot(observer.generation, &snapshot),
                    },
                    RetiredCallExecutionReport {
                        role: observer.role,
                        stats: RoleCallExecutionStats::from_snapshot(&snapshot.operations),
                    },
                ))
            })
            .unzip()
    }

    pub(crate) fn local_copy_stats(&self) -> LocalCopyStats {
        let mut stats = LocalCopyStats::default();
        for observer in &self.trackers {
            if let Some(snapshot) = observer.stats() {
                stats.merge(&snapshot);
            }
        }
        stats
    }

    pub(crate) fn capture(&self) -> (Vec<RetiredRoleObserver>, Vec<LocalCopyObserver>) {
        (
            self.roles
                .iter()
                .filter(|role| role.is_alive())
                .cloned()
                .collect(),
            self.trackers
                .iter()
                .filter(|tracker| tracker.is_alive())
                .cloned()
                .collect(),
        )
    }
}

/// One retirement handoff: the retired-role records of a retiring session,
/// its adopted predecessors, and both roles' explicit code-override intent.
///
/// The retiring session captures the handoff without consuming its own
/// records, and capture is retry-safe. Installing the handoff into a
/// replacement session is identity-deduplicated, and composition
/// de-duplicates by domain/tracker identity, so the same session captured
/// twice can never double-count. An auto-generated server id never travels
/// in the handoff; only explicit code overrides do.
#[derive(Clone)]
pub struct RetirementHandoff {
    roles: Vec<RetiredRoleObserver>,
    trackers: Vec<LocalCopyObserver>,
    client_overrides: c2_config::DomainLimitsOverrides,
    server_overrides: c2_config::ServerConfigOverrides,
    admission_hook: Option<Arc<dyn crate::AdmissionHook>>,
}

impl std::fmt::Debug for RetirementHandoff {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetirementHandoff")
            .field("roles", &self.roles)
            .field("trackers", &self.trackers)
            .field("client_overrides", &self.client_overrides)
            .field("server_overrides", &self.server_overrides)
            .field("has_admission_hook", &self.admission_hook.is_some())
            .finish()
    }
}

impl RetirementHandoff {
    /// Capture one retirement's records and both roles' override intent.
    pub(crate) fn new(
        roles: Vec<RetiredRoleObserver>,
        trackers: Vec<LocalCopyObserver>,
        client_overrides: c2_config::DomainLimitsOverrides,
        server_overrides: c2_config::ServerConfigOverrides,
        admission_hook: Option<Arc<dyn crate::AdmissionHook>>,
    ) -> Self {
        Self {
            roles,
            trackers,
            client_overrides,
            server_overrides,
            admission_hook,
        }
    }

    pub(crate) fn admission_hook(&self) -> Option<&Arc<dyn crate::AdmissionHook>> {
        self.admission_hook.as_ref()
    }

    /// Both roles' explicit code-override intent for the replacement session.
    pub fn override_intent(
        &self,
    ) -> (
        &c2_config::DomainLimitsOverrides,
        &c2_config::ServerConfigOverrides,
    ) {
        (&self.client_overrides, &self.server_overrides)
    }

    /// Install the retired records into a replacement session's fresh list,
    /// de-duplicated by role-domain identity. Whether a record currently
    /// reports zero or holds live charges, its observable lifetime is decided
    /// by its real owners, never by adoption.
    pub(crate) fn adopt_into(&self, retired: &mut RetiredRoles) {
        retired.roles.retain(RetiredRoleObserver::is_alive);
        retired.trackers.retain(LocalCopyObserver::is_alive);
        for observer in self.roles.iter().filter(|role| role.is_alive()) {
            if !retired
                .roles
                .iter()
                .any(|existing| existing.same_role_domain(observer))
            {
                retired.roles.push(observer.clone());
            }
        }
        for tracker in self.trackers.iter().filter(|tracker| tracker.is_alive()) {
            if !retired
                .trackers
                .iter()
                .any(|existing| existing.ptr_eq(tracker))
            {
                retired.trackers.push(tracker.clone());
            }
        }
    }
}
