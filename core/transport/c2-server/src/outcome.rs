//! Host-wide keyed unary call outcome store (ADR 0006).
//!
//! One short `parking_lot::Mutex` owns the cross-registration keyed-call
//! index `(route_uid, call_id)`, the per-route purge index and the two
//! completion-order queues:
//!
//! - `settled_fifo`: every entry that reached a terminal state enters once;
//!   FIFO eviction of the oldest terminal *metadata* bounds the entry count
//!   even when `outcome_max_retained_bytes == 0`. `PendingInput`/`InFlight`
//!   entries never enter and are never evicted.
//! - `retained_fifo`: only terminal entries holding retained response bytes
//!   (`Arc<SealedShmBlock>` / `Arc<BudgetedBuffer>`) or mutable `C2Error`
//!   detail bytes enter; byte-budget or pressure eviction degrades them in
//!   place to `Succeeded(Evicted)` / `Failed(details_evicted)` while keeping
//!   the terminal metadata and `error_code`.
//!
//! Every eviction collects the displaced owners into a local `Vec` and drops
//! them only after the store lock is released — never inside a
//! `DomainBudget`/`MemPool` lock stack. Waiter wakeups likewise leave the
//! critical section before delivering.
//!
//! The unique resource-entry adjudication point lives here: the original
//! call performs its `CallRecord::enter_resource()` CAS *inside* this lock
//! (`begin_execution`), closing the race between the `PendingInput →
//! InFlight` promotion and connection retirement. A lost CAS revokes the
//! exact `(route_uid, call_id, generation)` record: waiting `recover_call`
//! observers receive `723 CallOutcomeUnknown`, waiting duplicate submits a
//! `NotExecuted` pre-dispatch refusal.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;
use std::sync::Arc;

use c2_error::{C2Error, ErrorCode};
use c2_mem::{BudgetedBuffer, DomainBudget, MemoryReservation, PayloadBufferError, SealedShmBlock};
use c2_wire::rpc::{WireCallOutcome, WireResultUnavailableReason};
use tokio::sync::oneshot;

/// Digest length for the SHA-256 input fingerprint of a keyed call.
pub(crate) const OUTCOME_DIGEST_LEN: usize = 32;

/// `(route_uid, call_id)` identity of one keyed call record.
type OutcomeKey = (Arc<str>, Arc<str>);

/// A completion index identifies the exact generation of one key.
type CompletionIndex = BTreeMap<u64, (OutcomeKey, u64)>;

/// Input fingerprint attached to a waiter. `recover_call` carries no request
/// body, so its waiters match any digest.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum OutcomeDigest {
    Computed([u8; OUTCOME_DIGEST_LEN]),
    Any,
}

/// Which keyed operation registered a waiter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WaiterKind {
    /// A duplicate `KeyedUnary` submit that already released its own input.
    Submit,
    /// A `Recover` OPEN with no request body.
    Recover,
}

/// One immutable delivery handed to a waiter or the original caller. All
/// receivers of one outcome share the same `Arc` owners.
#[derive(Clone)]
pub(crate) enum OutcomeDelivery {
    /// The call succeeded; replay these bytes (or report `722` when the
    /// bytes were not retained / were evicted).
    Success(OutcomeResponse),
    /// The call failed with this error.
    Failure(Arc<OutcomeError>),
    /// The operation was refused without execution (`721` conflict, `723`
    /// unknown, or a revoked pinned generation).
    NotExecuted(C2Error),
}

/// Replayable form of a succeeded keyed call.
#[derive(Clone)]
pub(crate) enum OutcomeResponse {
    /// Zero-byte success: replayable without retained bytes.
    Empty,
    /// Retained inline bytes with their domain-budget charge.
    Inline(Arc<BudgetedBuffer>),
    /// Retained sealed SHM block; one independent loan per connection.
    Shm(Arc<SealedShmBlock>),
    /// Success confirmed but the response bytes cannot be replayed.
    Unavailable(WireResultUnavailableReason),
}

/// The failure delivered to the original caller and every attached waiter.
pub(crate) struct OutcomeError {
    error: C2Error,
    _reservation: Option<MemoryReservation>,
}

impl Deref for OutcomeError {
    type Target = C2Error;

    fn deref(&self) -> &Self::Target {
        &self.error
    }
}

enum OutcomeFailure {
    Full(Arc<OutcomeError>),
    DetailsEvicted { code: ErrorCode },
}

enum EntryState {
    PendingInput,
    InFlight,
    Settled(Settlement),
}

struct Settlement {
    order: u64,
    retained_bytes: u64,
    outcome: SettledOutcome,
}

enum SettledOutcome {
    Succeeded(OutcomeResponse),
    Failed(OutcomeFailure),
}

struct OutcomeEntry {
    generation: u64,
    method_idx: u16,
    digest: Option<[u8; OUTCOME_DIGEST_LEN]>,
    state: EntryState,
    waiters: Vec<OutcomeWaiter>,
}

struct OutcomeWaiter {
    kind: WaiterKind,
    digest: OutcomeDigest,
    sender: oneshot::Sender<OutcomeDelivery>,
}

/// Displaced owners plus parked waiter wakeups that must leave the lock
/// before being dropped or delivered.
#[derive(Default)]
pub(crate) struct EvictedOwners {
    owners: Vec<SettledOutcome>,
    wakeups: Vec<(oneshot::Sender<OutcomeDelivery>, OutcomeDelivery)>,
}

impl EvictedOwners {
    pub(crate) fn is_empty(&self) -> bool {
        self.owners.is_empty() && self.wakeups.is_empty()
    }

    /// Drop the displaced owners and deliver the waiter wakeups; only ever
    /// called with the store lock released.
    pub(crate) fn deliver(self) {
        let Self { owners, wakeups } = self;
        // Owners first: dropping a sealed block or budgeted buffer releases
        // pool capacity and domain charges.
        drop(owners);
        for (sender, delivery) in wakeups {
            let _ = sender.send(delivery);
        }
    }
}

struct OutcomeState {
    entries: HashMap<OutcomeKey, OutcomeEntry>,
    /// Purge index: live call IDs grouped by route registration.
    by_route: HashMap<Arc<str>, HashSet<Arc<str>>>,
    /// Completion order of terminal metadata and positive retained charges.
    /// Ordered maps support prompt removal when a route is purged.
    settled_fifo: CompletionIndex,
    retained_fifo: CompletionIndex,
    retained_bytes: u64,
    next_generation: u64,
    next_completion: u64,
}

/// Host outcome limits shared by every registration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutcomeLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_retained_bytes: u64,
}

/// The Host-wide keyed unary outcome store.
pub(crate) struct HostOutcomeStore {
    limits: OutcomeLimits,
    budget: DomainBudget,
    state: parking_lot::Mutex<OutcomeState>,
}

/// Result of the `KeyedUnary` OPEN first-frame adjudication.
pub(crate) enum AdmitOutcome {
    /// No record existed: the caller becomes the original executor of this
    /// fresh generation with the reserved route pending permit.
    Miss { generation: u64 },
    /// The call_id exists with a different `method_idx`: immediate
    /// `721 CallIdConflict`.
    MethodConflict,
    /// The call_id exists with the same method: pin this generation, receive
    /// the complete input, then resolve through `resolve_waiter`.
    Hit { generation: u64 },
}

/// Resolution of a duplicate submit or a recover against a pinned
/// generation.
pub(crate) enum WaiterResolution {
    /// The pinned generation settled with a matching fingerprint: replay
    /// now.
    Replay(OutcomeDelivery),
    /// The entry is still `PendingInput`/`InFlight`: the caller released its
    /// own input and waits on this receiver.
    Attached(oneshot::Receiver<OutcomeDelivery>),
    /// Refused without execution (`721` fingerprint conflict, or the pinned
    /// generation was revoked: `723` for recover, pre-dispatch for submit).
    Refused(C2Error),
}

/// Result of the unique resource-entry adjudication. Both variants carry
/// waiter wakeups that must be delivered outside the store lock.
pub(crate) enum ExecutionAdmission {
    /// The `enter_resource` CAS won and the entry is now `InFlight`; the
    /// owners carry any digest-conflicting observers split off at promotion.
    Won(EvictedOwners),
    /// The CAS lost (connection retired first) or the pinned generation was
    /// revoked; the waiters parked on it ride in the returned owners.
    Lost(EvictedOwners),
}

impl HostOutcomeStore {
    pub(crate) fn new(limits: OutcomeLimits, budget: DomainBudget) -> Self {
        assert!(
            limits.max_entries > 0,
            "outcome entry limit must be positive"
        );
        Self {
            limits,
            budget,
            state: parking_lot::Mutex::new(OutcomeState {
                entries: HashMap::new(),
                by_route: HashMap::new(),
                settled_fifo: BTreeMap::new(),
                retained_fifo: BTreeMap::new(),
                retained_bytes: 0,
                next_generation: 1,
                next_completion: 1,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn admit(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        method_idx: u16,
    ) -> AdmitOutcome {
        self.admit_with(route_uid, call_id, method_idx, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("infallible test admission")
        .0
    }

    /// Adjudicate the first frame after route, contract and method validation.
    /// Only a miss reserves a route pending permit. It does so before another
    /// keyed call can enter this admission path, preserving ticket order.
    pub(crate) fn admit_with<T, E>(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        method_idx: u16,
        reserve: impl FnOnce() -> Result<T, E>,
    ) -> Result<(AdmitOutcome, Option<T>), E> {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let mut state = self.state.lock();
        match state.entries.get(&key) {
            Some(entry) if entry.method_idx != method_idx => {
                Ok((AdmitOutcome::MethodConflict, None))
            }
            Some(entry) => Ok((
                AdmitOutcome::Hit {
                    generation: entry.generation,
                },
                None,
            )),
            None => {
                let permit = reserve()?;
                let generation = state.next_generation;
                state.next_generation = generation
                    .checked_add(1)
                    .expect("keyed call generation exhausted");
                state.entries.insert(
                    key.clone(),
                    OutcomeEntry {
                        generation,
                        method_idx,
                        digest: None,
                        state: EntryState::PendingInput,
                        waiters: Vec::new(),
                    },
                );
                state.by_route.entry(key.0).or_default().insert(key.1);
                Ok((AdmitOutcome::Miss { generation }, Some(permit)))
            }
        }
    }

    /// Undo a `PendingInput` record whose execution never began: admission
    /// failure, input truncation, connection loss before dispatch or a lost
    /// route/commit check. Waiters parked on the exact generation are woken
    /// (`723` for recover, pre-dispatch for a duplicate submit) and returned
    /// for delivery outside the lock. A no-op when the entry already moved
    /// on (InFlight/Settled) or vanished.
    pub(crate) fn revoke_pending(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
    ) -> EvictedOwners {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let mut state = self.state.lock();
        let mut owners = EvictedOwners::default();
        if matches!(
            state.entries.get(&key),
            Some(entry)
                if entry.generation == generation
                    && matches!(entry.state, EntryState::PendingInput)
        ) {
            state.remove_entry(&key, &mut owners);
        }
        owners
    }

    #[cfg(test)]
    fn probe(&self, route_uid: &Arc<str>, call_id: &str) -> Option<(u64, u16)> {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        self.state
            .lock()
            .entries
            .get(&key)
            .map(|entry| (entry.generation, entry.method_idx))
    }

    /// Look up a recovery, reserve its operation slot, and attach or replay
    /// under the same store lock. A miss or conflict consumes no capacity.
    pub(crate) fn recover_with<T>(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        method_idx: u16,
        reserve: impl FnOnce() -> Result<T, C2Error>,
    ) -> Result<(WaiterResolution, T), C2Error> {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let mut closed = Vec::new();
        let mut state = self.state.lock();
        let Some(entry) = state.entries.get_mut(&key) else {
            return Err(revoked_error(WaiterKind::Recover));
        };
        if entry.method_idx != method_idx {
            return Err(conflict_error());
        }
        let permit = reserve()?;
        let resolution = entry.resolve(OutcomeDigest::Any, WaiterKind::Recover, &mut closed);
        drop(state);
        drop(closed);
        Ok((resolution, permit))
    }

    /// Resolve a duplicate submit after receiving and digesting its complete
    /// input. It can attach or replay only against the pinned generation.
    pub(crate) fn resolve_waiter(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
        digest: OutcomeDigest,
        kind: WaiterKind,
    ) -> WaiterResolution {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let mut closed = Vec::new();
        let mut state = self.state.lock();
        let Some(entry) = state.entries.get_mut(&key) else {
            return WaiterResolution::Refused(revoked_error(kind));
        };
        if entry.generation != generation {
            return WaiterResolution::Refused(revoked_error(kind));
        }
        let resolution = entry.resolve(digest, kind, &mut closed);
        drop(state);
        drop(closed);
        resolution
    }

    /// The unique resource-entry adjudication: run the original call's
    /// `enter_resource` CAS inside the store lock. On success the exact
    /// `(route_uid, call_id, generation)` becomes `InFlight` and observers
    /// whose fingerprints conflict with the original's input are split off
    /// for out-of-lock `721` delivery. On failure the record is revoked and
    /// its waiters are returned for out-of-lock delivery (`723` for
    /// recover, pre-dispatch for a duplicate submit).
    pub(crate) fn begin_execution(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
        digest: [u8; OUTCOME_DIGEST_LEN],
        enter_resource: impl FnOnce() -> bool,
    ) -> ExecutionAdmission {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let mut state = self.state.lock();
        let Some(entry) = state.entries.get_mut(&key) else {
            return ExecutionAdmission::Lost(EvictedOwners::default());
        };
        if entry.generation != generation || !matches!(entry.state, EntryState::PendingInput) {
            return ExecutionAdmission::Lost(EvictedOwners::default());
        }
        if !enter_resource() {
            let mut owners = EvictedOwners::default();
            state.remove_entry(&key, &mut owners);
            return ExecutionAdmission::Lost(owners);
        }
        entry.digest = Some(digest);
        entry.state = EntryState::InFlight;
        let mut owners = EvictedOwners::default();
        for waiter in entry
            .waiters
            .extract_if(.., |waiter| !digest_matches(Some(&digest), &waiter.digest))
        {
            owners.wakeups.push((
                waiter.sender,
                OutcomeDelivery::NotExecuted(conflict_error()),
            ));
        }
        ExecutionAdmission::Won(owners)
    }

    /// Publish the terminal success of the original executor. Sealing and
    /// every pre-publication validation must already be complete. The
    /// `delivery` is the immutable response the original and every
    /// already-attached waiter receive — even when the store itself retains
    /// nothing (`outcome_max_retained_bytes == 0`) or the entry is evicted
    /// immediately after. Later observers replay from the retained state
    /// instead. Returns parked waiter wakeups plus owners displaced by
    /// eviction, all for delivery outside the lock.
    pub(crate) fn publish_success(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
        delivery: OutcomeResponse,
    ) -> EvictedOwners {
        let key = (Arc::clone(route_uid), Arc::from(call_id));
        let bytes = match &delivery {
            OutcomeResponse::Empty => 0,
            OutcomeResponse::Inline(buffer) => buffer.capacity() as u64,
            OutcomeResponse::Shm(block) => block.quota_bytes() as u64,
            OutcomeResponse::Unavailable(_) => unreachable!("executor owns its response"),
        };
        let (retained, bytes) = if bytes <= self.limits.max_retained_bytes {
            (delivery.clone(), bytes)
        } else {
            (
                OutcomeResponse::Unavailable(WireResultUnavailableReason::NotRetained),
                0,
            )
        };
        self.settle(
            key,
            generation,
            SettledOutcome::Succeeded(retained),
            bytes,
            OutcomeDelivery::Success(delivery),
        )
    }

    /// Settle an executor failure when the original connection needs no delivery.
    pub(crate) fn publish_failure(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
        error: C2Error,
    ) -> EvictedOwners {
        self.publish_failure_with_delivery(route_uid, call_id, generation, error)
            .1
    }

    /// Settle an executor failure and return its shared original delivery.
    /// Retention charges the actual string allocations for as long as a
    /// delivery still owns them. An error that cannot fit still reaches the
    /// original caller and attached waiters with all its details.
    pub(crate) fn publish_failure_with_delivery(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        generation: u64,
        error: C2Error,
    ) -> (Arc<OutcomeError>, EvictedOwners) {
        let key = (Arc::clone(route_uid), Arc::from(call_id));
        let bytes = error_variable_bytes(&error);
        let can_reserve = bytes > 0
            && bytes <= self.limits.max_retained_bytes
            && self.state.lock().is_in_flight(&key, generation);
        let reservation = if can_reserve {
            self.reserve_error_details(bytes)
        } else {
            None
        };
        let can_retain = bytes == 0 || reservation.is_some();
        let delivery = Arc::new(OutcomeError {
            error,
            _reservation: reservation,
        });
        let (failure, retained_bytes) = if can_retain {
            (OutcomeFailure::Full(Arc::clone(&delivery)), bytes)
        } else {
            (
                OutcomeFailure::DetailsEvicted {
                    code: delivery.code,
                },
                0,
            )
        };
        let owners = self.settle(
            key,
            generation,
            SettledOutcome::Failed(failure),
            retained_bytes,
            OutcomeDelivery::Failure(Arc::clone(&delivery)),
        );
        (delivery, owners)
    }

    fn reserve_error_details(&self, bytes: u64) -> Option<MemoryReservation> {
        match self.budget.try_reserve_memory(bytes) {
            Ok(reservation) => Some(reservation),
            Err(PayloadBufferError::InsufficientCapacity { available, .. }) => {
                self.shed_for_pressure(bytes - available).deliver();
                self.budget.try_reserve_memory(bytes).ok()
            }
            Err(_) => None,
        }
    }

    fn settle(
        &self,
        key: OutcomeKey,
        generation: u64,
        outcome: SettledOutcome,
        bytes: u64,
        delivery: OutcomeDelivery,
    ) -> EvictedOwners {
        let mut owners = EvictedOwners::default();
        let mut state = self.state.lock();
        if !state.is_in_flight(&key, generation) {
            return owners;
        }

        // Park the exact immutable delivery before evicting any metadata.
        let entry = state
            .entries
            .get_mut(&key)
            .expect("checked in-flight entry");
        for waiter in std::mem::take(&mut entry.waiters) {
            owners.wakeups.push((waiter.sender, delivery.clone()));
        }
        let order = state.next_completion;
        state.next_completion = order
            .checked_add(1)
            .expect("keyed call completion order exhausted");
        state.entries.get_mut(&key).expect("entry still live").state =
            EntryState::Settled(Settlement {
                order,
                retained_bytes: bytes,
                outcome,
            });
        state.settled_fifo.insert(order, (key.clone(), generation));

        // The new terminal record is newest, and the configured cap is positive.
        if state.settled_fifo.len() > self.limits.max_entries {
            let (&oldest_order, (oldest_key, oldest_generation)) = state
                .settled_fifo
                .first_key_value()
                .expect("terminal entry");
            let oldest_key = oldest_key.clone();
            state.check_settlement(&oldest_key, *oldest_generation, oldest_order);
            state.remove_entry(&oldest_key, &mut owners);
        }
        if bytes != 0 {
            let remaining = self.limits.max_retained_bytes - bytes;
            while state.retained_bytes > remaining {
                state
                    .shed_oldest_retained(&mut owners)
                    .expect("retained bytes indexed");
            }
            state.retained_bytes += bytes;
            state.retained_fifo.insert(order, (key, generation));
        }
        owners
    }

    /// Read-only status snapshot for the non-blocking query path. The
    /// `method_name` resolver maps the entry's `method_idx` through the live
    /// route contract; a miss after full contract validation reports
    /// `unknown`.
    pub(crate) fn query(
        &self,
        route_uid: &Arc<str>,
        call_id: &str,
        method_name: impl FnOnce(u16) -> Option<String>,
    ) -> WireCallOutcome {
        let key: OutcomeKey = (Arc::clone(route_uid), Arc::from(call_id));
        let state = self.state.lock();
        let Some(entry) = state.entries.get(&key) else {
            return WireCallOutcome::unknown();
        };
        let Some(name) = method_name(entry.method_idx) else {
            return WireCallOutcome::unknown();
        };
        match &entry.state {
            EntryState::PendingInput | EntryState::InFlight => WireCallOutcome::in_flight(name),
            EntryState::Settled(settlement) => match &settlement.outcome {
                SettledOutcome::Succeeded(OutcomeResponse::Unavailable(reason)) => {
                    WireCallOutcome::succeeded(name, false, Some(*reason))
                }
                SettledOutcome::Succeeded(_) => WireCallOutcome::succeeded(name, true, None),
                SettledOutcome::Failed(OutcomeFailure::Full(error)) => {
                    WireCallOutcome::failed(name, error.code, true)
                }
                SettledOutcome::Failed(OutcomeFailure::DetailsEvicted { code }) => {
                    WireCallOutcome::failed(name, *code, false)
                }
            },
        }
    }

    /// Remove every record of one route registration. Called after the
    /// route drained during unregister; retained owners of terminal entries
    /// are dropped by the caller outside the lock.
    pub(crate) fn purge_route(&self, route_uid: &str) -> EvictedOwners {
        let mut owners = EvictedOwners::default();
        let mut state = self.state.lock();
        let Some((route_uid, keys)) = state.by_route.remove_entry(route_uid) else {
            return owners;
        };
        for call_id in keys {
            state.remove_entry(&(Arc::clone(&route_uid), call_id), &mut owners);
        }
        owners
    }

    /// Degrade the oldest retained terminal entries until at least
    /// `needed_bytes` of retained outcome bytes were released or no
    /// retained entry remains. Terminal metadata survives; displaced owners
    /// are dropped by the caller outside this lock. Only ever called after
    /// the pool/budget lock stack unwound.
    pub(crate) fn shed_for_pressure(&self, needed_bytes: u64) -> EvictedOwners {
        let mut owners = EvictedOwners::default();
        let mut state = self.state.lock();
        let mut remaining = needed_bytes;
        while remaining != 0 {
            let Some(bytes) = state.shed_oldest_retained(&mut owners) else {
                break;
            };
            remaining = remaining.saturating_sub(bytes);
        }
        owners
    }
}

impl OutcomeEntry {
    fn resolve(
        &mut self,
        digest: OutcomeDigest,
        kind: WaiterKind,
        closed: &mut Vec<OutcomeWaiter>,
    ) -> WaiterResolution {
        if !digest_matches(self.digest.as_ref(), &digest) {
            return WaiterResolution::Refused(conflict_error());
        }
        if let EntryState::Settled(settlement) = &self.state {
            return WaiterResolution::Replay(settled_delivery(&settlement.outcome));
        }
        closed.extend(
            self.waiters
                .extract_if(.., |waiter| waiter.sender.is_closed()),
        );
        let (sender, receiver) = oneshot::channel();
        self.waiters.push(OutcomeWaiter {
            kind,
            digest,
            sender,
        });
        WaiterResolution::Attached(receiver)
    }
}

impl OutcomeState {
    fn is_in_flight(&self, key: &OutcomeKey, generation: u64) -> bool {
        matches!(
            self.entries.get(key),
            Some(entry)
                if entry.generation == generation && matches!(entry.state, EntryState::InFlight)
        )
    }

    fn check_settlement(&self, key: &OutcomeKey, generation: u64, order: u64) {
        let entry = self.entries.get(key).expect("indexed outcome exists");
        assert_eq!(entry.generation, generation);
        assert!(
            matches!(&entry.state, EntryState::Settled(settlement) if settlement.order == order)
        );
    }

    fn remove_entry(&mut self, key: &OutcomeKey, owners: &mut EvictedOwners) {
        let entry = self.entries.remove(key).expect("removed outcome exists");
        if let Some(keys) = self.by_route.get_mut(&key.0) {
            keys.remove(&key.1);
            if keys.is_empty() {
                self.by_route.remove(&key.0);
            }
        }
        for waiter in entry.waiters {
            owners.wakeups.push((
                waiter.sender,
                OutcomeDelivery::NotExecuted(revoked_error(waiter.kind)),
            ));
        }
        if let EntryState::Settled(settlement) = entry.state {
            self.settled_fifo
                .remove(&settlement.order)
                .expect("settled registration index");
            if settlement.retained_bytes != 0 {
                self.retained_fifo
                    .remove(&settlement.order)
                    .expect("retained registration index");
                self.retained_bytes -= settlement.retained_bytes;
            }
            owners.owners.push(settlement.outcome);
        }
    }

    fn shed_oldest_retained(&mut self, owners: &mut EvictedOwners) -> Option<u64> {
        let (order, (key, generation)) = self.retained_fifo.pop_first()?;
        self.check_settlement(&key, generation, order);
        let entry = self.entries.get_mut(&key).expect("checked retained entry");
        let EntryState::Settled(settlement) = &mut entry.state else {
            unreachable!("retention index contains terminal entries");
        };
        let degraded = match &settlement.outcome {
            SettledOutcome::Succeeded(_) => SettledOutcome::Succeeded(
                OutcomeResponse::Unavailable(WireResultUnavailableReason::Evicted),
            ),
            SettledOutcome::Failed(OutcomeFailure::Full(error)) => {
                SettledOutcome::Failed(OutcomeFailure::DetailsEvicted { code: error.code })
            }
            SettledOutcome::Failed(OutcomeFailure::DetailsEvicted { .. }) => {
                unreachable!("retention index contains only live bytes");
            }
        };
        let bytes = std::mem::take(&mut settlement.retained_bytes);
        self.retained_bytes -= bytes;
        owners
            .owners
            .push(std::mem::replace(&mut settlement.outcome, degraded));
        Some(bytes)
    }
}

/// Outcome-control error constructors. Only the Host outcome store mints
/// these codes.
pub(crate) fn conflict_error() -> C2Error {
    C2Error::new(
        ErrorCode::CallIdConflict,
        "call_id is already used by another method or a different input",
    )
}

pub(crate) fn revoked_error(kind: WaiterKind) -> C2Error {
    match kind {
        WaiterKind::Recover => C2Error::new(
            ErrorCode::CallOutcomeUnknown,
            "no keyed-call record matches this recovery attempt in the current registration",
        ),
        WaiterKind::Submit => C2Error::new(
            ErrorCode::ResourceClosed,
            "the pinned keyed-call record is no longer available; the submission was not executed",
        ),
    }
}

fn digest_matches(original: Option<&[u8; OUTCOME_DIGEST_LEN]>, candidate: &OutcomeDigest) -> bool {
    match (original, candidate) {
        (_, OutcomeDigest::Any) => true,
        (Some(original), OutcomeDigest::Computed(digest)) => original == digest,
        // The original's digest is unknown until promotion; a duplicate that
        // attached during PendingInput is re-checked at begin_execution.
        (None, OutcomeDigest::Computed(_)) => true,
    }
}

fn error_variable_bytes(error: &C2Error) -> u64 {
    error.message.capacity() as u64
        + error
            .details
            .iter()
            .map(|(key, value)| key.capacity() as u64 + value.capacity() as u64)
            .sum::<u64>()
}

fn settled_delivery(settled: &SettledOutcome) -> OutcomeDelivery {
    match settled {
        SettledOutcome::Succeeded(response) => OutcomeDelivery::Success(response.clone()),
        SettledOutcome::Failed(OutcomeFailure::Full(error)) => {
            OutcomeDelivery::Failure(Arc::clone(error))
        }
        SettledOutcome::Failed(OutcomeFailure::DetailsEvicted { code }) => {
            let error = C2Error::new(
                *code,
                "the original error details were evicted from the outcome store",
            )
            .with_details(BTreeMap::from([(
                "details_evicted".to_string(),
                "true".to_string(),
            )]));
            OutcomeDelivery::Failure(Arc::new(OutcomeError {
                error,
                _reservation: None,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(max_entries: usize, max_retained_bytes: u64) -> HostOutcomeStore {
        HostOutcomeStore::new(
            OutcomeLimits {
                max_entries,
                max_retained_bytes,
            },
            budget(),
        )
    }

    fn budget() -> DomainBudget {
        DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 1 << 20,
            ..Default::default()
        })
    }

    fn route() -> Arc<str> {
        Arc::from("route-uid-1")
    }

    fn assert_unknown(query: WireCallOutcome) {
        assert!(matches!(
            query,
            WireCallOutcome {
                status: c2_wire::rpc::WireCallOutcomeStatus::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn miss_then_hit_pins_one_generation_until_revoke() {
        let store = store(8, 1024);
        let route = route();
        let AdmitOutcome::Miss { generation } = store.admit(&route, "op-1", 3) else {
            panic!("expected miss");
        };
        assert!(matches!(
            store.admit(&route, "op-1", 5),
            AdmitOutcome::MethodConflict
        ));
        assert!(matches!(
            store.admit(&route, "op-1", 3),
            AdmitOutcome::Hit {
                generation: pinned
            } if pinned == generation
        ));
    }

    #[test]
    fn only_a_miss_reserves_a_pending_ticket() {
        let store = store(8, 0);
        let route = route();
        assert!(matches!(
            store.admit_with(&route, "ticketed", 0, || Err::<u64, _>("closed")),
            Err("closed")
        ));
        assert_eq!(store.probe(&route, "ticketed"), None);
        let (AdmitOutcome::Miss { generation }, Some(9)) = store
            .admit_with(&route, "ticketed", 0, || Ok::<_, ()>(9))
            .unwrap()
        else {
            panic!("only the successful admission gets a ticket");
        };
        assert!(matches!(
            store.admit_with(&route, "ticketed", 0, || Err::<u64, _>("must not reserve")),
            Ok((AdmitOutcome::Hit { generation: pinned }, None)) if pinned == generation
        ));
        assert!(matches!(
            store.admit_with(&route, "ticketed", 1, || Err::<u64, _>("must not reserve")),
            Ok((AdmitOutcome::MethodConflict, None))
        ));
    }

    #[test]
    fn recovery_checks_method_and_attaches_during_capacity_reservation() {
        let store = store(8, 0);
        let route = route();
        assert!(matches!(
            store.recover_with::<()>(&route, "recover", 0, || panic!("miss reserves no slot")),
            Err(error) if error.code == ErrorCode::CallOutcomeUnknown
        ));
        let generation = start(&store, &route, "recover");
        assert!(matches!(
            store.recover_with::<()>(&route, "recover", 1, || {
                panic!("conflicting method reserves no slot")
            }),
            Err(error) if error.code == ErrorCode::CallIdConflict
        ));
        let (WaiterResolution::Attached(mut waiter), 7) = store
            .recover_with(&route, "recover", 0, || {
                assert!(store.state.try_lock().is_none());
                Ok(7)
            })
            .unwrap()
        else {
            panic!("recovery holds capacity and attaches to the live call");
        };
        store
            .publish_success(&route, "recover", generation, OutcomeResponse::Empty)
            .deliver();
        assert!(matches!(
            waiter.try_recv(),
            Ok(OutcomeDelivery::Success(OutcomeResponse::Empty))
        ));
        assert!(matches!(
            store.recover_with(&route, "recover", 0, || Ok(8)),
            Ok((
                WaiterResolution::Replay(OutcomeDelivery::Success(OutcomeResponse::Empty)),
                8
            ))
        ));
    }

    #[test]
    fn begin_execution_revoke_wakes_waiters_by_kind() {
        let store = store(8, 1024);
        let route = route();
        let AdmitOutcome::Miss { generation } = store.admit(&route, "op-2", 0) else {
            panic!("expected miss");
        };
        // One recover and one duplicate submit attach during PendingInput.
        let mut recover = match store.resolve_waiter(
            &route,
            "op-2",
            generation,
            OutcomeDigest::Any,
            WaiterKind::Recover,
        ) {
            WaiterResolution::Attached(receiver) => receiver,
            _ => panic!("expected attach"),
        };
        let mut submit = match store.resolve_waiter(
            &route,
            "op-2",
            generation,
            OutcomeDigest::Computed([9u8; 32]),
            WaiterKind::Submit,
        ) {
            WaiterResolution::Attached(receiver) => receiver,
            _ => panic!("expected attach"),
        };
        let digest = [7u8; 32];
        let ExecutionAdmission::Lost(owners) =
            store.begin_execution(&route, "op-2", generation, digest, || false)
        else {
            panic!("lost CAS must lose");
        };
        owners.deliver();
        match recover.try_recv() {
            Ok(OutcomeDelivery::NotExecuted(error)) => {
                assert_eq!(error.code, ErrorCode::CallOutcomeUnknown);
            }
            _ => panic!("recover waiter must be woken with 723"),
        }
        match submit.try_recv() {
            Ok(OutcomeDelivery::NotExecuted(error)) => {
                assert_eq!(error.code, ErrorCode::ResourceClosed);
            }
            _ => panic!("submit waiter must be woken with NotExecuted"),
        }
        // The record is gone: a later query observes unknown.
        assert_unknown(store.query(&route, "op-2", |_| Some("m".into())));
    }

    #[test]
    fn digest_conflict_waiter_split_at_promotion() {
        let store = store(8, 1024);
        let route = route();
        let AdmitOutcome::Miss { generation } = store.admit(&route, "op-3", 0) else {
            panic!("expected miss");
        };
        let mut conflicting = match store.resolve_waiter(
            &route,
            "op-3",
            generation,
            OutcomeDigest::Computed([1u8; 32]),
            WaiterKind::Submit,
        ) {
            WaiterResolution::Attached(receiver) => receiver,
            _ => panic!("expected attach"),
        };
        let digest = [2u8; 32];
        let ExecutionAdmission::Won(owners) =
            store.begin_execution(&route, "op-3", generation, digest, || true)
        else {
            panic!("won CAS must win");
        };
        owners.deliver();
        match conflicting.try_recv() {
            Ok(OutcomeDelivery::NotExecuted(error)) => {
                assert_eq!(error.code, ErrorCode::CallIdConflict);
            }
            _ => panic!("conflicting waiter must be split off with 721"),
        }
        // A post-promotion duplicate with the same digest attaches; a
        // different digest is refused.
        assert!(matches!(
            store.resolve_waiter(
                &route,
                "op-3",
                generation,
                OutcomeDigest::Computed(digest),
                WaiterKind::Submit
            ),
            WaiterResolution::Attached(_)
        ));
        match store.resolve_waiter(
            &route,
            "op-3",
            generation,
            OutcomeDigest::Computed([3u8; 32]),
            WaiterKind::Submit,
        ) {
            WaiterResolution::Refused(error) => assert_eq!(error.code, ErrorCode::CallIdConflict),
            _ => panic!("expected refusal"),
        }
    }

    #[test]
    fn zero_retention_and_entry_cap_evict_terminal_metadata_only() {
        let store = store(2, 0);
        let route = route();
        for index in 0..3 {
            let call_id = format!("op-{index}");
            let AdmitOutcome::Miss { generation } = store.admit(&route, &call_id, 0) else {
                panic!("expected miss");
            };
            let digest = [index as u8; 32];
            assert!(matches!(
                store.begin_execution(&route, &call_id, generation, digest, || true),
                ExecutionAdmission::Won(_)
            ));
            store
                .publish_success(&route, &call_id, generation, OutcomeResponse::Empty)
                .deliver();
        }
        // Zero-byte success replays as Empty even though nothing is retained.
        match store.resolve_waiter(
            &route,
            "op-2",
            3,
            OutcomeDigest::Computed([2u8; 32]),
            WaiterKind::Recover,
        ) {
            WaiterResolution::Replay(OutcomeDelivery::Success(OutcomeResponse::Empty)) => {}
            _ => panic!("expected replay"),
        }
        // The oldest entry (op-0) was metadata-evicted: recover misses → 723.
        match store.resolve_waiter(
            &route,
            "op-0",
            1,
            OutcomeDigest::Computed([0u8; 32]),
            WaiterKind::Recover,
        ) {
            WaiterResolution::Refused(error) => {
                assert_eq!(error.code, ErrorCode::CallOutcomeUnknown)
            }
            _ => panic!("expected refusal"),
        }

        let generation = start(&store, &route, "inline");
        let mut waiter = attach(&store, &route, "inline", generation, WaiterKind::Recover);
        publish_inline(&store, &route, "inline", generation, b"delivery").deliver();
        assert_eq!(
            store
                .query(&route, "inline", |_| Some("m".into()))
                .result_unavailable_reason,
            Some(WireResultUnavailableReason::NotRetained)
        );
        assert!(matches!(
            waiter.try_recv(),
            Ok(OutcomeDelivery::Success(OutcomeResponse::Inline(buffer)))
                if buffer.as_ref().as_ref() == b"delivery"
        ));
    }

    #[test]
    fn shed_for_pressure_degrades_oldest_retained_without_deleting_metadata() {
        let store = store(8, 10_000);
        let route = route();
        let newer = start(&store, &route, "op-1");
        let older = start(&store, &route, "op-0");
        for (call_id, generation) in [("op-0", older), ("op-1", newer)] {
            let owners = publish_inline(&store, &route, call_id, generation, &[1; 300]);
            assert!(owners.is_empty());
            owners.deliver();
        }
        // The first completion degrades even though the other call entered first.
        store.shed_for_pressure(300).deliver();
        match store.resolve_waiter(
            &route,
            "op-0",
            older,
            OutcomeDigest::Any,
            WaiterKind::Recover,
        ) {
            WaiterResolution::Replay(OutcomeDelivery::Success(OutcomeResponse::Unavailable(
                WireResultUnavailableReason::Evicted,
            ))) => {}
            _ => panic!("expected evicted replay"),
        }
        // The newest entry still replays its bytes.
        match store.resolve_waiter(
            &route,
            "op-1",
            newer,
            OutcomeDigest::Any,
            WaiterKind::Recover,
        ) {
            WaiterResolution::Replay(OutcomeDelivery::Success(OutcomeResponse::Inline(_))) => {}
            _ => panic!("expected retained replay"),
        }
    }

    fn budgeted(bytes: &[u8]) -> BudgetedBuffer {
        let reservation = budget().try_reserve_memory(bytes.len() as u64).unwrap();
        BudgetedBuffer::new(bytes.to_vec(), reservation)
    }

    fn start(store: &HostOutcomeStore, route: &Arc<str>, call_id: &str) -> u64 {
        let AdmitOutcome::Miss { generation } = store.admit(route, call_id, 0) else {
            panic!("expected a new generation");
        };
        let ExecutionAdmission::Won(owners) =
            store.begin_execution(route, call_id, generation, [0; OUTCOME_DIGEST_LEN], || true)
        else {
            panic!("expected entry into the resource");
        };
        owners.deliver();
        generation
    }

    fn attach(
        store: &HostOutcomeStore,
        route: &Arc<str>,
        call_id: &str,
        generation: u64,
        kind: WaiterKind,
    ) -> oneshot::Receiver<OutcomeDelivery> {
        let WaiterResolution::Attached(receiver) =
            store.resolve_waiter(route, call_id, generation, OutcomeDigest::Any, kind)
        else {
            panic!("expected waiting delivery");
        };
        receiver
    }

    fn publish_inline(
        store: &HostOutcomeStore,
        route: &Arc<str>,
        call_id: &str,
        generation: u64,
        bytes: &[u8],
    ) -> EvictedOwners {
        let buffer = Arc::new(budgeted(bytes));
        store.publish_success(route, call_id, generation, OutcomeResponse::Inline(buffer))
    }

    #[test]
    fn attached_delivery_survives_4097th_settlement_and_call_id_reuse() {
        let store = store(4096, 16);
        let route = route();
        let first = start(&store, &route, "same-id");
        let mut recover = attach(&store, &route, "same-id", first, WaiterKind::Recover);
        let mut submit = attach(&store, &route, "same-id", first, WaiterKind::Submit);
        let first_delivery = publish_inline(&store, &route, "same-id", first, b"old");

        for index in 1..4096 {
            let call_id = format!("op-{index}");
            let generation = start(&store, &route, &call_id);
            store
                .publish_success(&route, &call_id, generation, OutcomeResponse::Empty)
                .deliver();
        }
        assert_eq!(store.probe(&route, "same-id"), Some((first, 0)));
        let neighbor = start(&store, &route, "neighbor");
        publish_inline(&store, &route, "neighbor", neighbor, b"next").deliver();
        assert_unknown(store.query(&route, "same-id", |_| Some("m".into())));

        let new = start(&store, &route, "same-id");
        assert_ne!(new, first);
        publish_inline(&store, &route, "same-id", first, b"stale").deliver();
        assert_eq!(
            store.query(&route, "same-id", |_| Some("m".into())),
            WireCallOutcome::in_flight("m".into()),
        );
        publish_inline(&store, &route, "same-id", new, b"new").deliver();
        for (kind, code) in [
            (WaiterKind::Recover, ErrorCode::CallOutcomeUnknown),
            (WaiterKind::Submit, ErrorCode::ResourceClosed),
        ] {
            assert!(matches!(
                store.resolve_waiter(&route, "same-id", first, OutcomeDigest::Any, kind),
                WaiterResolution::Refused(error) if error.code == code
            ));
        }

        // Neither the old metadata eviction nor its retained FIFO slot
        // may select the replacement. Pressure must degrade the neighbor.
        store.shed_for_pressure(1).deliver();
        let query = store.query(&route, "neighbor", |_| Some("m".into()));
        assert_eq!(
            query.result_unavailable_reason,
            Some(WireResultUnavailableReason::Evicted)
        );
        assert!(
            store
                .query(&route, "same-id", |_| Some("m".into()))
                .result_available
        );

        first_delivery.deliver();
        let Ok(OutcomeDelivery::Success(OutcomeResponse::Inline(recovered))) = recover.try_recv()
        else {
            panic!("attached recovery lost the original delivery");
        };
        let Ok(OutcomeDelivery::Success(OutcomeResponse::Inline(duplicated))) = submit.try_recv()
        else {
            panic!("attached duplicate lost the original delivery");
        };
        assert_eq!(recovered.as_ref().as_ref(), b"old");
        assert!(Arc::ptr_eq(&recovered, &duplicated));
    }

    #[test]
    fn failed_details_charge_string_capacity_until_the_last_delivery_drops() {
        let store = store(8, 1024);
        let route = route();
        let generation = start(&store, &route, "failed");
        let mut recover = attach(&store, &route, "failed", generation, WaiterKind::Recover);
        let mut message = String::with_capacity(128);
        message.push('m');
        let mut detail_key = String::with_capacity(32);
        detail_key.push('k');
        let mut detail_value = String::with_capacity(64);
        detail_value.push('v');
        let allocated =
            (message.capacity() + detail_key.capacity() + detail_value.capacity()) as u64;
        let error = C2Error::new(ErrorCode::Unknown, message)
            .with_details(BTreeMap::from([(detail_key, detail_value)]));

        let (original, wakeups) =
            store.publish_failure_with_delivery(&route, "failed", generation, error);
        drop(original);
        assert_eq!(store.budget.snapshot().memory.used_bytes, allocated);
        assert!(
            store
                .query(&route, "failed", |_| Some("m".into()))
                .error_details_available
        );
        wakeups.deliver();
        let Ok(OutcomeDelivery::Failure(attached)) = recover.try_recv() else {
            panic!("attached recovery lost failure");
        };
        let WaiterResolution::Replay(OutcomeDelivery::Failure(replayed)) = store.resolve_waiter(
            &route,
            "failed",
            generation,
            OutcomeDigest::Any,
            WaiterKind::Recover,
        ) else {
            panic!("expected retained failure");
        };
        assert!(Arc::ptr_eq(&attached, &replayed));
        drop(attached);
        drop(replayed);

        let displaced = store.shed_for_pressure(1);
        assert_eq!(store.budget.snapshot().memory.used_bytes, allocated);
        assert_eq!(
            store.query(&route, "failed", |_| Some("m".into())),
            WireCallOutcome::failed("m".into(), ErrorCode::Unknown, false)
        );
        displaced.deliver();
        assert_eq!(store.budget.snapshot().memory.used_bytes, 0);
    }

    #[test]
    fn failed_details_over_retention_keep_attached_delivery_full() {
        let store = store(8, 16);
        let route = route();
        let generation = start(&store, &route, "failed");
        let mut recover = attach(&store, &route, "failed", generation, WaiterKind::Recover);
        let mut message = String::with_capacity(32);
        message.push('m');
        let (original, wakeups) = store.publish_failure_with_delivery(
            &route,
            "failed",
            generation,
            C2Error::new(ErrorCode::Unknown, message),
        );
        wakeups.deliver();
        assert!(matches!(
            recover.try_recv(),
            Ok(OutcomeDelivery::Failure(attached)) if Arc::ptr_eq(&attached, &original)
        ));
        assert_eq!(original.message, "m");
        assert_eq!(store.budget.snapshot().memory.used_bytes, 0);
        let WaiterResolution::Replay(OutcomeDelivery::Failure(replayed)) = store.resolve_waiter(
            &route,
            "failed",
            generation,
            OutcomeDigest::Any,
            WaiterKind::Recover,
        ) else {
            panic!("expected failed outcome without its original details");
        };
        assert_eq!(replayed.code, original.code);
        assert_eq!(
            replayed.details.get("details_evicted").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn disconnected_waiters_are_replaced_without_advancing_generation() {
        let store = store(8, 0);
        let route = route();
        let generation = start(&store, &route, "reconnect");
        for _ in 0..16 {
            drop(attach(
                &store,
                &route,
                "reconnect",
                generation,
                WaiterKind::Recover,
            ));
        }
        let mut live = attach(&store, &route, "reconnect", generation, WaiterKind::Recover);
        let key = (Arc::clone(&route), Arc::from("reconnect"));
        assert_eq!(store.state.lock().entries[&key].waiters.len(), 1);
        store
            .publish_success(&route, "reconnect", generation, OutcomeResponse::Empty)
            .deliver();
        assert!(matches!(
            live.try_recv(),
            Ok(OutcomeDelivery::Success(OutcomeResponse::Empty))
        ));
    }

    #[test]
    fn purge_route_drops_every_record_of_the_registration() {
        let store = store(8, 1024);
        let route = route();
        let AdmitOutcome::Miss { generation } = store.admit(&route, "op-9", 0) else {
            panic!("expected miss");
        };
        assert!(matches!(
            store.begin_execution(&route, "op-9", generation, [5u8; 32], || true),
            ExecutionAdmission::Won(_)
        ));
        store
            .publish_failure(
                &route,
                "op-9",
                generation,
                C2Error::new(ErrorCode::Unknown, "x"),
            )
            .deliver();
        let displaced = store.purge_route(&route);
        assert_unknown(store.query(&route, "op-9", |_| Some("m".into())));
        let state = store.state.lock();
        assert!(state.entries.is_empty());
        assert!(state.by_route.is_empty());
        assert!(state.settled_fifo.is_empty());
        assert!(state.retained_fifo.is_empty());
        assert_eq!(state.retained_bytes, 0);
        drop(state);
        assert_eq!(store.budget.snapshot().memory.used_bytes, 1);
        displaced.deliver();
        assert_eq!(store.budget.snapshot().memory.used_bytes, 0);
    }
}
