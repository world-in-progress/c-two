//! Per-request first-chunk admission ordering for chunked request frames.
//!
//! The connection receive loop dispatches every chunk frame as its own task so a
//! slow route admission on one request cannot serialize the connection. That
//! independence created an ordering hazard: later chunks of a request could
//! reach the chunk registry before the first chunk had reserved its route
//! permit and published its assembly, and were then discarded as "no assembly"
//! while the caller kept waiting. [`ChunkAdmissionGate`] restores the ordering
//! without serializing the connection:
//!
//! - The receive loop creates an entry for the request *before* spawning the
//!   first chunk's task (frame dispatch order is the wire order).
//! - Later chunk tasks wait on that entry (async, no connection-level lock)
//!   until the first chunk established admission: route permit, registry
//!   assembly, and stored route admission.
//! - Every terminal path publishes an outcome and wakes the waiters: admission
//!   success, admission refusal, request abort, and owner teardown on
//!   cancellation. No waiter, permit, or registry entry is left behind.
//!
//! Identity and fencing rules:
//!
//! - A terminal outcome is published through one atomic single-winner
//!   transition ([`ChunkAdmissionEntry::try_publish`]), so a refusal or abort
//!   that races the owner's commit can never be overwritten by `Admitted`.
//! - Removal is identity-checked. A stale owner that was replaced or torn down
//!   can only release the exact entry it began; it can never remove a
//!   successor's same-key entry or wake that successor's waiters.
//! - An entry stays in the gate as a teardown fence until every participant
//!   released it: the owner plus any request-level aborter that is still
//!   releasing published state. That keeps a same-key successor from beginning
//!   while an older generation's rollback is in flight, so an old rollback can
//!   never observe (or delete) a successor's state.
//!
//! Bounds: the receive loop already holds one chunk-processing permit per
//! dispatched chunk frame before it can create an entry, and an entry is
//! removed as soon as admission is terminal and its teardown fences are
//! released, so live entries never exceed `max_total_chunks`. A waiting later
//! chunk keeps its own permit, which is what bounds the number of blocked
//! waiters; the explicit capacity check below is a defensive restatement of
//! that invariant. PING and other control frames are dispatched synchronously
//! in the receive loop and are never blocked by a stalled resource/route gate.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use tokio::sync::watch;

/// Request identity for one in-flight chunked call.
pub(crate) type ChunkRequestKey = (u64, u64);

/// Terminal state of first-chunk admission for one chunked request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkAdmissionOutcome {
    /// First-chunk admission is still in progress.
    Pending,
    /// Admission completed; later chunks may feed the published assembly.
    Admitted,
    /// Admission was refused and the admission owner already wrote the
    /// correlated error reply for this request.
    Refused,
    /// Admission ended without a published reply (disconnect, cancellation, or
    /// owner panic teardown). Later chunks stop without replying.
    Aborted,
}

/// Refusal classes for [`ChunkAdmissionGate::begin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkOrderingError {
    /// A first chunk for this request is already tracked: either awaiting
    /// admission or still tearing its generation down.
    AlreadyPending,
    /// The gate already tracks its configured maximum number of requests.
    Capacity { limit: usize },
}

impl std::fmt::Display for ChunkOrderingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyPending => write!(
                f,
                "duplicate first chunk for a chunked request already awaiting admission"
            ),
            Self::Capacity { limit } => write!(
                f,
                "chunk admission ordering capacity exceeded: max_total_chunks={limit}"
            ),
        }
    }
}

/// One request's admission readiness state plus its teardown fences.
struct ChunkAdmissionEntry {
    outcome: watch::Sender<ChunkAdmissionOutcome>,
    /// One fence for the owner plus one for every request-level aborter that is
    /// still releasing this generation's published state. The entry remains in
    /// the gate map while any fence is held.
    fences: AtomicUsize,
}

impl ChunkAdmissionEntry {
    fn new() -> Self {
        let (outcome, _rx) = watch::channel(ChunkAdmissionOutcome::Pending);
        Self {
            outcome,
            fences: AtomicUsize::new(1),
        }
    }

    fn outcome(&self) -> ChunkAdmissionOutcome {
        *self.outcome.borrow()
    }

    fn is_pending(&self) -> bool {
        self.outcome() == ChunkAdmissionOutcome::Pending
    }

    /// Atomically move `Pending` to a terminal outcome.
    ///
    /// `watch::Sender::send_if_modified` runs the transition under the channel's
    /// value lock and leaves the value untouched when the closure returns
    /// `false`, so exactly one racing publisher wins and a loser can never
    /// overwrite the winner. In particular `Admitted` can never be published
    /// after a refusal or abort won.
    fn try_publish(&self, outcome: ChunkAdmissionOutcome) -> bool {
        debug_assert_ne!(outcome, ChunkAdmissionOutcome::Pending);
        self.outcome.send_if_modified(|current| {
            if *current == ChunkAdmissionOutcome::Pending {
                *current = outcome;
                true
            } else {
                false
            }
        })
    }

    async fn wait(&self) -> ChunkAdmissionOutcome {
        let mut rx = self.outcome.subscribe();
        let current = *rx.borrow_and_update();
        if current != ChunkAdmissionOutcome::Pending {
            return current;
        }
        if rx.changed().await.is_err() {
            // The owner vanished without publishing; treat it as an abort so a
            // waiter can never hang.
            return ChunkAdmissionOutcome::Aborted;
        }
        *rx.borrow_and_update()
    }
}

/// Shared gate state. Kept behind an `Arc` so an admission owner can be moved
/// into the chunk task and still tear its entry down on cancellation without
/// borrowing the [`crate::server::Server`].
struct ChunkAdmissionGateInner {
    entries: Mutex<HashMap<ChunkRequestKey, Arc<ChunkAdmissionEntry>>>,
    capacity: usize,
}

/// Bounded per-request first-chunk admission gate.
#[derive(Clone)]
pub(crate) struct ChunkAdmissionGate {
    inner: Arc<ChunkAdmissionGateInner>,
}

impl ChunkAdmissionGate {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(ChunkAdmissionGateInner {
                entries: Mutex::new(HashMap::new()),
                capacity,
            }),
        }
    }

    /// Claim first-chunk admission ownership for `key`.
    ///
    /// The returned owner is the teardown fence: if the owning task disappears
    /// without committing the admission, the entry publishes `Aborted` and is
    /// released so no waiter is stranded. Any existing entry — pending or a
    /// terminal generation that is still tearing down — refuses the claim, so a
    /// successor generation can never begin while an older one still holds
    /// published state.
    pub(crate) fn begin(
        &self,
        key: ChunkRequestKey,
    ) -> Result<ChunkAdmissionOwner, ChunkOrderingError> {
        let mut entries = self.inner.entries.lock();
        if entries.contains_key(&key) {
            return Err(ChunkOrderingError::AlreadyPending);
        }
        if entries.len() >= self.inner.capacity {
            return Err(ChunkOrderingError::Capacity {
                limit: self.inner.capacity,
            });
        }
        let entry = Arc::new(ChunkAdmissionEntry::new());
        entries.insert(key, Arc::clone(&entry));
        drop(entries);
        Ok(ChunkAdmissionOwner {
            gate: self.clone(),
            key,
            entry,
            released: false,
        })
    }

    /// A waiter for `key` when its first chunk is still awaiting admission.
    ///
    /// `None` means no admission is pending: the chunk feeds directly, either
    /// because the assembly is already published or because the request is
    /// already terminal (the feed then reports the latter).
    pub(crate) fn waiter(&self, key: ChunkRequestKey) -> Option<ChunkAdmissionWaiter> {
        let entries = self.inner.entries.lock();
        entries.get(&key).and_then(|entry| {
            entry.is_pending().then(|| ChunkAdmissionWaiter {
                entry: Arc::clone(entry),
            })
        })
    }

    /// Publish an abort for the current entry of `key` and hold a teardown
    /// fence that keeps the entry in place until the returned guard is dropped.
    ///
    /// The caller must have finished (or be about to finish, before dropping
    /// the guard) releasing this request's published assembly and route
    /// admission. `None` means no entry is tracked: the request has no
    /// in-flight first-chunk admission for a successor to race.
    pub(crate) fn abort_key(&self, key: ChunkRequestKey) -> Option<ChunkAdmissionReclaim> {
        let entries = self.inner.entries.lock();
        let entry = Arc::clone(entries.get(&key)?);
        entry.fences.fetch_add(1, Ordering::AcqRel);
        drop(entries);
        entry.try_publish(ChunkAdmissionOutcome::Aborted);
        Some(ChunkAdmissionReclaim {
            gate: self.clone(),
            key,
            entry,
        })
    }

    /// Abort every admission entry owned by a connection and return how many
    /// were torn down (connection disconnect/cancellation cleanup).
    ///
    /// Removes outright: a closing connection dispatches no successor frame, so
    /// no teardown fence is needed.
    pub(crate) fn abort_connection(&self, conn_id: u64) -> usize {
        let mut entries = self.inner.entries.lock();
        let keys: Vec<ChunkRequestKey> = entries
            .keys()
            .filter(|(pending_conn_id, _)| *pending_conn_id == conn_id)
            .copied()
            .collect();
        for key in &keys {
            if let Some(entry) = entries.remove(key) {
                entry.try_publish(ChunkAdmissionOutcome::Aborted);
            }
        }
        keys.len()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.entries.lock().len()
    }

    /// Identity-checked fence release.
    ///
    /// The fence count and the map are read/updated under the same lock so a
    /// concurrent [`ChunkAdmissionGate::abort_key`] either adds its fence
    /// before the last release (and the entry survives) or observes no entry at
    /// all. The removal is identity-checked, so a stale participant can never
    /// release a replacement generation's entry.
    fn release_entry(&self, key: ChunkRequestKey, entry: &Arc<ChunkAdmissionEntry>) {
        let mut entries = self.inner.entries.lock();
        if entry.fences.fetch_sub(1, Ordering::AcqRel) > 1 {
            return;
        }
        if entries
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&key);
        }
    }
}

/// Teardown fence for a request-level abort taken through
/// [`ChunkAdmissionGate::abort_key`]. The tracked entry is not removed until
/// this guard is dropped.
pub(crate) struct ChunkAdmissionReclaim {
    gate: ChunkAdmissionGate,
    key: ChunkRequestKey,
    entry: Arc<ChunkAdmissionEntry>,
}

impl Drop for ChunkAdmissionReclaim {
    fn drop(&mut self) {
        self.gate.release_entry(self.key, &self.entry);
    }
}

/// First-chunk admission owner. See [`ChunkAdmissionGate::begin`].
pub(crate) struct ChunkAdmissionOwner {
    gate: ChunkAdmissionGate,
    key: ChunkRequestKey,
    entry: Arc<ChunkAdmissionEntry>,
    released: bool,
}

impl ChunkAdmissionOwner {
    /// Commit admission with the atomic `Pending → Admitted` transition.
    ///
    /// Returns `true` when this owner won: the entry is released immediately
    /// and waiters proceed to feed. Returns `false` when a terminal outcome
    /// (refusal or abort) was already published; the owner must then roll back
    /// its unpublished publications and release the entry, and the terminal
    /// publisher owns the caller reply.
    pub(crate) fn admit(&mut self) -> bool {
        if !self.entry.try_publish(ChunkAdmissionOutcome::Admitted) {
            return false;
        }
        self.release();
        true
    }

    /// Publish a refusal (the correlated error reply was or will be written).
    ///
    /// The entry intentionally stays as a teardown fence until
    /// [`ChunkAdmissionOwner::release`] or drop.
    pub(crate) fn refuse(&mut self) -> bool {
        self.entry.try_publish(ChunkAdmissionOutcome::Refused)
    }

    /// Publish an abort (no reply will be written; disconnect/cancellation).
    pub(crate) fn abort(&mut self) -> bool {
        self.entry.try_publish(ChunkAdmissionOutcome::Aborted)
    }

    /// A waiter over this owner's own terminal state.
    ///
    /// Used to fence the route-admission await: a request-level abort wakes the
    /// waiting owner instead of letting it publish after the caller already got
    /// a terminal failure.
    pub(crate) fn terminal_waiter(&self) -> ChunkAdmissionWaiter {
        ChunkAdmissionWaiter {
            entry: Arc::clone(&self.entry),
        }
    }

    /// Remove this exact entry after the generation's teardown is complete.
    ///
    /// Identity-checked: a stale owner that was already replaced cannot remove
    /// a successor's same-key entry.
    pub(crate) fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.gate.release_entry(self.key, &self.entry);
        }
    }
}

impl Drop for ChunkAdmissionOwner {
    fn drop(&mut self) {
        if !self.released {
            // Cancellation/panic teardown: publish an abort (only if this owner
            // still wins the terminal transition) and release the fence so
            // waiters wake and the key can be admitted again.
            self.entry.try_publish(ChunkAdmissionOutcome::Aborted);
            self.release();
        }
    }
}

impl std::fmt::Debug for ChunkAdmissionOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkAdmissionOwner")
            .field("key", &self.key)
            .field("outcome", &self.entry.outcome())
            .finish()
    }
}

/// Waiter for one request's first-chunk admission.
pub(crate) struct ChunkAdmissionWaiter {
    entry: Arc<ChunkAdmissionEntry>,
}

impl ChunkAdmissionWaiter {
    /// Wait until admission is terminal.
    pub(crate) async fn wait(self) -> ChunkAdmissionOutcome {
        self.entry.wait().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waiter_observes_admission_and_refusal() {
        let gate = ChunkAdmissionGate::new(4);
        let mut owner = gate.begin((1, 7)).unwrap();
        let waiter = gate.waiter((1, 7)).expect("pending admission is tracked");
        assert!(owner.admit());
        assert_eq!(waiter.wait().await, ChunkAdmissionOutcome::Admitted);
        assert_eq!(gate.len(), 0);
        assert!(gate.waiter((1, 7)).is_none());

        let mut owner = gate.begin((1, 8)).unwrap();
        let waiter = gate.waiter((1, 8)).unwrap();
        assert!(owner.refuse());
        // The refusal is published immediately; the entry stays as the
        // teardown fence until the owner releases it.
        assert_eq!(waiter.wait().await, ChunkAdmissionOutcome::Refused);
        assert_eq!(gate.len(), 1);
        owner.release();
        assert_eq!(gate.len(), 0);
    }

    #[tokio::test]
    async fn owner_drop_aborts_waiters() {
        let gate = ChunkAdmissionGate::new(4);
        let owner = gate.begin((1, 9)).unwrap();
        let waiter = gate.waiter((1, 9)).unwrap();
        drop(owner);
        assert_eq!(waiter.wait().await, ChunkAdmissionOutcome::Aborted);
        assert_eq!(gate.len(), 0);
    }

    #[tokio::test]
    async fn waiter_is_bounded_and_connection_abort_wakes_all() {
        let gate = ChunkAdmissionGate::new(2);
        let _first = gate.begin((1, 1)).unwrap();
        let _second = gate.begin((1, 2)).unwrap();
        assert_eq!(
            gate.begin((1, 3)).unwrap_err(),
            ChunkOrderingError::Capacity { limit: 2 }
        );
        assert_eq!(
            gate.begin((1, 1)).unwrap_err(),
            ChunkOrderingError::AlreadyPending
        );
        let waiters: Vec<_> = [(1u64, 1u64), (1, 2)]
            .into_iter()
            .map(|key| gate.waiter(key).unwrap())
            .collect();
        assert_eq!(gate.abort_connection(1), 2);
        assert_eq!(gate.len(), 0);
        for waiter in waiters {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), waiter.wait())
                    .await
                    .expect("aborted waiter must wake"),
                ChunkAdmissionOutcome::Aborted
            );
        }
        // A different connection's entries are untouched.
        let _other = gate.begin((2, 1)).unwrap();
        assert_eq!(gate.abort_connection(1), 0);
        assert_eq!(gate.len(), 1);
    }

    #[tokio::test]
    async fn abort_key_wakes_only_that_request() {
        let gate = ChunkAdmissionGate::new(4);
        let mut owner = gate.begin((1, 1)).unwrap();
        let other_owner = gate.begin((1, 2)).unwrap();
        let waiter = gate.waiter((1, 1)).unwrap();
        let reclaim = gate.abort_key((1, 1)).expect("entry is tracked");
        assert_eq!(waiter.wait().await, ChunkAdmissionOutcome::Aborted);
        assert_eq!(gate.len(), 2);
        assert!(
            gate.waiter((1, 2)).is_some(),
            "the other request's pending admission must be untouched"
        );
        // The owner's release alone is not enough: the abort fence still holds
        // the aborted entry until the request teardown finishes.
        owner.release();
        assert_eq!(gate.len(), 2);
        drop(reclaim);
        assert_eq!(gate.len(), 1);
        drop(other_owner);
        assert_eq!(gate.len(), 0);
        assert!(gate.waiter((1, 2)).is_none());
    }

    /// A terminal outcome and the owner's commit race for one atomic
    /// transition; the loser can never overwrite the winner.
    #[tokio::test]
    async fn terminal_transition_has_exactly_one_winner() {
        let gate = ChunkAdmissionGate::new(4);
        let mut owner = gate.begin((1, 1)).unwrap();
        let waiter = gate.waiter((1, 1)).unwrap();
        let reclaim = gate.abort_key((1, 1)).expect("entry is tracked");
        // Abort won first: the owner's late commit must lose and must not turn
        // the request back into an admitted one.
        assert!(!owner.admit());
        assert!(!owner.refuse());
        assert_eq!(waiter.wait().await, ChunkAdmissionOutcome::Aborted);
        owner.release();
        drop(reclaim);

        // Reverse order: admission wins, and a later abort is a no-op (no
        // entry is left to fence).
        let mut owner = gate.begin((1, 2)).unwrap();
        assert!(owner.admit());
        assert!(gate.abort_key((1, 2)).is_none());
        assert!(gate.waiter((1, 2)).is_none());
        assert_eq!(gate.len(), 0);
        drop(owner);
    }

    /// Two publishers racing the same entry: the transition is a single-winner
    /// compare-and-swap, not a check-then-write pair, so both can never report
    /// success and a loser can never overwrite the winner.
    #[test]
    fn terminal_transition_is_a_single_winner_compare_and_swap() {
        use std::sync::Barrier;

        const RACERS: usize = 8;
        const ROUNDS: usize = 512;
        for _ in 0..ROUNDS {
            let gate = ChunkAdmissionGate::new(4);
            let owner = gate.begin((1, 1)).unwrap();
            let entry = Arc::clone(&owner.entry);
            let barrier = Arc::new(Barrier::new(RACERS));
            let mut racers = Vec::with_capacity(RACERS);
            for i in 0..RACERS {
                let entry = Arc::clone(&entry);
                let barrier = Arc::clone(&barrier);
                racers.push(std::thread::spawn(move || {
                    barrier.wait();
                    let outcome = if i % 2 == 0 {
                        ChunkAdmissionOutcome::Admitted
                    } else {
                        ChunkAdmissionOutcome::Aborted
                    };
                    entry.try_publish(outcome)
                }));
            }
            let winners = racers
                .into_iter()
                .map(|racer| racer.join().unwrap())
                .filter(|won| *won)
                .count();
            assert_eq!(
                winners, 1,
                "exactly one terminal publisher must win (round)"
            );
            assert_ne!(entry.outcome(), ChunkAdmissionOutcome::Pending);
        }
    }

    /// A same-key successor cannot begin while the previous generation is still
    /// tearing down, and a stale owner release cannot touch the successor.
    #[tokio::test]
    async fn stale_generation_release_never_removes_the_replacement_entry() {
        let gate = ChunkAdmissionGate::new(4);
        let stale_owner = gate.begin((1, 7)).unwrap();
        // The stale generation's entry disappears through connection teardown
        // while the owner object is still alive in a task that has not returned
        // yet.
        assert_eq!(gate.abort_connection(1), 1);
        assert_eq!(gate.len(), 0);

        // A replacement generation under the same key.
        let mut replacement = gate.begin((1, 7)).unwrap();
        let replacement_waiter = gate.waiter((1, 7)).expect("replacement is tracked");

        // The stale owner's drop must not remove the replacement entry nor
        // publish a terminal outcome on its waiters.
        drop(stale_owner);
        assert_eq!(gate.len(), 1);
        assert!(gate.waiter((1, 7)).is_some());

        // The replacement still commits normally.
        assert!(replacement.admit());
        assert_eq!(
            replacement_waiter.wait().await,
            ChunkAdmissionOutcome::Admitted
        );
        assert_eq!(gate.len(), 0);
    }

    /// The abort fence keeps the entry (and therefore the key) reserved until
    /// both the request-level aborter and the owner released it.
    #[tokio::test]
    async fn abort_fence_blocks_a_successor_until_the_abort_teardown_finishes() {
        let gate = ChunkAdmissionGate::new(4);
        let mut owner = gate.begin((1, 3)).unwrap();
        let reclaim = gate.abort_key((1, 3)).unwrap();
        assert_eq!(
            gate.begin((1, 3)).unwrap_err(),
            ChunkOrderingError::AlreadyPending,
            "a successor must not begin while the abort teardown is in flight"
        );
        owner.release();
        assert_eq!(gate.len(), 1);
        assert_eq!(
            gate.begin((1, 3)).unwrap_err(),
            ChunkOrderingError::AlreadyPending,
            "the abort fence alone still reserves the key"
        );
        drop(reclaim);
        assert_eq!(gate.len(), 0);
        let _successor = gate.begin((1, 3)).expect("key is free after teardown");
        assert_eq!(gate.len(), 1);
    }
}
