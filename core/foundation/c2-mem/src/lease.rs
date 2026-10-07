use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BufferStorage {
    Inline,
    Shm,
    Handle,
    FileSpill,
}

impl BufferStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Shm => "shm",
            Self::Handle => "handle",
            Self::FileSpill => "file_spill",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseRetention {
    Transient,
    Retained,
}

impl LeaseRetention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Retained => "retained",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeaseDirection {
    ClientResponse,
    ResourceInput,
}

impl LeaseDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientResponse => "client_response",
            Self::ResourceInput => "resource_input",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferLeaseMeta {
    pub route_name: String,
    pub method_name: String,
    pub direction: LeaseDirection,
    pub retention: LeaseRetention,
    pub storage: BufferStorage,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    meta: BufferLeaseMeta,
    created_at: Instant,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageLeaseStats {
    pub active_leases: usize,
    pub active_holds: usize,
    pub total_leased_bytes: usize,
    pub total_held_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectionLeaseStats {
    pub active_leases: usize,
    pub active_holds: usize,
    pub total_leased_bytes: usize,
    pub total_held_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BufferLeaseStats {
    pub active_leases: usize,
    pub active_holds: usize,
    pub total_leased_bytes: usize,
    pub total_held_bytes: usize,
    pub oldest_hold_seconds: f64,
    pub by_storage: BTreeMap<BufferStorage, StorageLeaseStats>,
    pub by_direction: BTreeMap<LeaseDirection, DirectionLeaseStats>,
}

impl BufferLeaseStats {
    /// Merge another tracker's counters into this view.
    ///
    /// Used when one read-only snapshot must cover the live lease tracker plus
    /// trackers retained for retired sessions. The trackers own disjoint
    /// entries, so this only aggregates counters and takes the maximum hold
    /// age. Saturating arithmetic keeps the reporting path safe; observing
    /// never allocates or resets leases.
    pub fn merge(&mut self, other: &BufferLeaseStats) {
        self.active_leases = self.active_leases.saturating_add(other.active_leases);
        self.active_holds = self.active_holds.saturating_add(other.active_holds);
        self.total_leased_bytes = self
            .total_leased_bytes
            .saturating_add(other.total_leased_bytes);
        self.total_held_bytes = self.total_held_bytes.saturating_add(other.total_held_bytes);
        if other.oldest_hold_seconds > self.oldest_hold_seconds {
            self.oldest_hold_seconds = other.oldest_hold_seconds;
        }
        for (storage, stats) in &other.by_storage {
            let entry = self.by_storage.entry(*storage).or_default();
            entry.active_leases = entry.active_leases.saturating_add(stats.active_leases);
            entry.active_holds = entry.active_holds.saturating_add(stats.active_holds);
            entry.total_leased_bytes = entry
                .total_leased_bytes
                .saturating_add(stats.total_leased_bytes);
            entry.total_held_bytes = entry
                .total_held_bytes
                .saturating_add(stats.total_held_bytes);
        }
        for (direction, stats) in &other.by_direction {
            let entry = self.by_direction.entry(*direction).or_default();
            entry.active_leases = entry.active_leases.saturating_add(stats.active_leases);
            entry.active_holds = entry.active_holds.saturating_add(stats.active_holds);
            entry.total_leased_bytes = entry
                .total_leased_bytes
                .saturating_add(stats.total_leased_bytes);
            entry.total_held_bytes = entry
                .total_held_bytes
                .saturating_add(stats.total_held_bytes);
        }
    }
}

#[derive(Debug, Clone)]
pub struct BufferLeaseSnapshot {
    pub id: u64,
    pub route_name: String,
    pub method_name: String,
    pub direction: LeaseDirection,
    pub retention: LeaseRetention,
    pub storage: BufferStorage,
    pub bytes: usize,
    pub age_seconds: f64,
}

#[derive(Debug)]
struct BufferLeaseTrackerInner {
    track_transient: bool,
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, LeaseEntry>>,
}

impl BufferLeaseTrackerInner {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<u64, LeaseEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Clone)]
pub struct BufferLeaseTracker {
    inner: Arc<BufferLeaseTrackerInner>,
}

/// Weak, non-owning view of one tracker's lease metadata.
///
/// A retired observation stores this instead of a tracker `Arc`, so observing
/// never keeps lease metadata alive: the record stays reportable exactly while
/// a real owner — the session, a live proxy's tracker handle, or an
/// outstanding [`BufferLeaseGuard`] — keeps the tracker alive, and it becomes
/// prunable the moment the last such owner is gone. Only copied snapshots are
/// public; an observer cannot recover the producer's `track` authority.
///
/// ```compile_fail
/// use c2_mem::BufferLeaseObserver;
/// fn recover_producer(observer: &BufferLeaseObserver) {
///     let _producer = observer.upgrade();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BufferLeaseObserver {
    inner: Weak<BufferLeaseTrackerInner>,
}

impl BufferLeaseObserver {
    /// Whether some real owner still keeps this tracker's metadata alive.
    pub fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }

    /// Identity comparison for de-duplicating the same tracker observed
    /// through more than one record.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.inner, &other.inner)
    }

    /// Reattach to the live tracker, if any real owner still holds it.
    ///
    /// `None` once every owner of the tracker is gone; such a record carries
    /// no observable metadata anymore and may be pruned.
    fn upgrade(&self) -> Option<BufferLeaseTracker> {
        self.inner
            .upgrade()
            .map(|inner| BufferLeaseTracker { inner })
    }

    /// Copy the current counters while a producer or lease still owns them.
    pub fn stats(&self) -> Option<BufferLeaseStats> {
        self.upgrade().map(|tracker| tracker.stats())
    }

    /// Copy retained-lease metadata without exposing the producer handle.
    pub fn sweep_retained(&self, threshold: Duration) -> Option<Vec<BufferLeaseSnapshot>> {
        self.upgrade()
            .map(|tracker| tracker.sweep_retained(threshold))
    }
}

#[derive(Debug)]
pub struct BufferLeaseGuard {
    id: Option<u64>,
    // Strong by design: a retained lease is itself a real owner of its
    // tracker's metadata, so a hold published just before a session swap keeps
    // that metadata — and its observation record — alive without the observer
    // retaining anything. The tracker stores only lease metadata entries and
    // never references its guards, so this cannot cycle or retain payloads.
    tracker: Option<Arc<BufferLeaseTrackerInner>>,
}

impl BufferLeaseTracker {
    pub fn new(track_transient: bool) -> Self {
        Self {
            inner: Arc::new(BufferLeaseTrackerInner {
                track_transient,
                next_id: AtomicU64::new(1),
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Weak, non-owning view of this tracker for read-only observation.
    pub fn observer(&self) -> BufferLeaseObserver {
        BufferLeaseObserver {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn track(&self, meta: BufferLeaseMeta) -> BufferLeaseGuard {
        if meta.retention == LeaseRetention::Transient && !self.inner.track_transient {
            return BufferLeaseGuard {
                id: None,
                tracker: None,
            };
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = LeaseEntry {
            meta,
            created_at: Instant::now(),
        };
        self.inner.entries().insert(id, entry);

        BufferLeaseGuard {
            id: Some(id),
            tracker: Some(Arc::clone(&self.inner)),
        }
    }

    pub fn stats(&self) -> BufferLeaseStats {
        let now = Instant::now();
        let entries = self.inner.entries();
        let mut stats = BufferLeaseStats::default();

        for entry in entries.values() {
            stats.active_leases = stats.active_leases.saturating_add(1);
            stats.total_leased_bytes = stats.total_leased_bytes.saturating_add(entry.meta.bytes);

            let storage = stats.by_storage.entry(entry.meta.storage).or_default();
            storage.active_leases = storage.active_leases.saturating_add(1);
            storage.total_leased_bytes =
                storage.total_leased_bytes.saturating_add(entry.meta.bytes);

            let direction = stats.by_direction.entry(entry.meta.direction).or_default();
            direction.active_leases = direction.active_leases.saturating_add(1);
            direction.total_leased_bytes = direction
                .total_leased_bytes
                .saturating_add(entry.meta.bytes);

            if entry.meta.retention == LeaseRetention::Retained {
                stats.active_holds = stats.active_holds.saturating_add(1);
                stats.total_held_bytes = stats.total_held_bytes.saturating_add(entry.meta.bytes);
                storage.active_holds = storage.active_holds.saturating_add(1);
                storage.total_held_bytes =
                    storage.total_held_bytes.saturating_add(entry.meta.bytes);
                direction.active_holds = direction.active_holds.saturating_add(1);
                direction.total_held_bytes =
                    direction.total_held_bytes.saturating_add(entry.meta.bytes);

                let age = now.duration_since(entry.created_at).as_secs_f64();
                if age > stats.oldest_hold_seconds {
                    stats.oldest_hold_seconds = age;
                }
            }
        }

        stats
    }

    pub fn sweep_retained(&self, threshold: Duration) -> Vec<BufferLeaseSnapshot> {
        let now = Instant::now();
        let entries = self.inner.entries();
        let mut snapshots = Vec::new();

        for (id, entry) in entries.iter() {
            if entry.meta.retention != LeaseRetention::Retained {
                continue;
            }

            let age = now.duration_since(entry.created_at);
            if age >= threshold {
                snapshots.push(BufferLeaseSnapshot {
                    id: *id,
                    route_name: entry.meta.route_name.clone(),
                    method_name: entry.meta.method_name.clone(),
                    direction: entry.meta.direction,
                    retention: entry.meta.retention,
                    storage: entry.meta.storage,
                    bytes: entry.meta.bytes,
                    age_seconds: age.as_secs_f64(),
                });
            }
        }

        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }
}

impl Default for BufferLeaseTracker {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Drop for BufferLeaseGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if let Some(inner) = self.tracker.take() {
            inner.entries().remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn retained_inline_meta() -> BufferLeaseMeta {
        BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "subdivide_grids".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Retained,
            storage: BufferStorage::Inline,
            bytes: 128,
        }
    }

    #[test]
    fn retained_inline_lease_counts_until_guard_drop() {
        let tracker = BufferLeaseTracker::new(false);
        let guard = tracker.track(retained_inline_meta());
        let stats = tracker.stats();
        assert_eq!(stats.active_holds, 1);
        assert_eq!(stats.total_held_bytes, 128);
        assert_eq!(
            stats
                .by_storage
                .get(&BufferStorage::Inline)
                .unwrap()
                .active_holds,
            1
        );
        drop(guard);
        let stats = tracker.stats();
        assert_eq!(stats.active_holds, 0);
        assert_eq!(stats.total_held_bytes, 0);
    }

    #[test]
    fn retained_shm_snapshot_reports_route_method_storage_and_age() {
        let tracker = BufferLeaseTracker::new(false);
        let _guard = tracker.track(BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "subdivide_grids".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Retained,
            storage: BufferStorage::Shm,
            bytes: 8192,
        });
        std::thread::sleep(Duration::from_millis(5));
        let stale = tracker.sweep_retained(Duration::from_millis(1));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].route_name, "grid");
        assert_eq!(stale[0].method_name, "subdivide_grids");
        assert_eq!(stale[0].storage, BufferStorage::Shm);
        assert_eq!(stale[0].bytes, 8192);
        assert!(stale[0].age_seconds > 0.0);
    }

    #[test]
    fn transient_leases_are_noop_by_default_to_keep_view_path_cheap() {
        let tracker = BufferLeaseTracker::new(false);
        let _guard = tracker.track(BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "subdivide_grids".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Transient,
            storage: BufferStorage::Shm,
            bytes: 4096,
        });
        let stats = tracker.stats();
        assert_eq!(stats.active_leases, 0);
        assert_eq!(stats.active_holds, 0);
    }

    #[test]
    fn transient_tracking_can_be_enabled_for_diagnostics() {
        let tracker = BufferLeaseTracker::new(true);
        let guard = tracker.track(BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "subdivide_grids".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Transient,
            storage: BufferStorage::Shm,
            bytes: 4096,
        });
        let stats = tracker.stats();
        assert_eq!(stats.active_leases, 1);
        assert_eq!(stats.active_holds, 0);
        assert_eq!(stats.total_leased_bytes, 4096);
        drop(guard);
        assert_eq!(tracker.stats().active_leases, 0);
    }

    #[test]
    fn retained_leases_are_counted_by_direction() {
        let tracker = BufferLeaseTracker::new(false);
        let _client = tracker.track(BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "read".to_string(),
            direction: LeaseDirection::ClientResponse,
            retention: LeaseRetention::Retained,
            storage: BufferStorage::Inline,
            bytes: 32,
        });
        let _resource = tracker.track(BufferLeaseMeta {
            route_name: "grid".to_string(),
            method_name: "write".to_string(),
            direction: LeaseDirection::ResourceInput,
            retention: LeaseRetention::Retained,
            storage: BufferStorage::Shm,
            bytes: 64,
        });

        let stats = tracker.stats();
        assert_eq!(
            stats
                .by_direction
                .get(&LeaseDirection::ClientResponse)
                .unwrap()
                .active_holds,
            1
        );
        assert_eq!(
            stats
                .by_direction
                .get(&LeaseDirection::ResourceInput)
                .unwrap()
                .total_held_bytes,
            64
        );
    }

    #[test]
    fn a_retained_guard_keeps_its_tracker_metadata_alive() {
        // The tracker's owning session handle goes away, exactly like a
        // session swap dropping the retired RuntimeSession. The outstanding
        // hold is itself a real owner, so the metadata — and any weak
        // observation of it — must stay alive until the hold releases.
        let observer = {
            let tracker = BufferLeaseTracker::new(false);
            let observer = tracker.observer();
            let _hold = tracker.track(retained_inline_meta());
            drop(tracker);
            assert!(observer.is_alive());
            observer
        };
        // The guard dropped with the scope above; nothing owns the tracker
        // anymore, so the weak observation detaches.
        assert!(!observer.is_alive());
    }

    #[test]
    fn observer_is_alive_while_any_owner_holds_the_tracker() {
        let producer = BufferLeaseTracker::new(false);
        let observer = producer.observer();
        let other_observer = producer.observer();
        assert!(observer.is_alive());
        assert!(observer.ptr_eq(&other_observer));

        let session_handle = producer.clone();
        drop(producer);
        assert!(observer.is_alive(), "clone is still a real owner");

        drop(session_handle);
        assert!(!observer.is_alive(), "last owner gone detaches the record");

        let unrelated = BufferLeaseTracker::new(false).observer();
        assert!(!observer.ptr_eq(&unrelated));
    }
}
