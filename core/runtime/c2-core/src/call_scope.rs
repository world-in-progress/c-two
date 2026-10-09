//! One logical call's local deadline and dispatch/completion arbitration.
//!
//! This is metadata only: no timer, executor, I/O, cancellation, payload owner,
//! or retention permit lives here. A future adapter must consult the dispatch
//! guard at first-byte admission, and separately keep real transport/payload
//! owners alive until their work ends, even when the caller stops waiting.
//! `Instant` is process-local and must never be sent to another machine.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use c2_config::CallOptions;

use crate::TransportPhase;

/// Monotonic states of a single logical call. Terminal states never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallState {
    /// Admitted to the scope, but no first byte has been allowed.
    PreDispatch,
    /// First-byte admission won. This does not prove a byte reached the peer.
    Dispatched,
    Succeeded,
    FailedPreDispatch,
    FailedDispatchUncertain,
    ExpiredPreDispatch,
    ExpiredDispatchUncertain,
}

impl CallState {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::PreDispatch | Self::Dispatched)
    }

    fn from_byte(state: u8) -> Self {
        match state {
            0 => Self::PreDispatch,
            1 => Self::Dispatched,
            2 => Self::Succeeded,
            3 => Self::FailedPreDispatch,
            4 => Self::FailedDispatchUncertain,
            5 => Self::ExpiredPreDispatch,
            6 => Self::ExpiredDispatchUncertain,
            _ => unreachable!("only CallState discriminants are stored"),
        }
    }

    fn deadline_error(self) -> Option<CallScopeError> {
        match self {
            Self::ExpiredPreDispatch => Some(CallScopeError::DeadlineExceeded {
                phase: TransportPhase::PreDispatch,
            }),
            Self::ExpiredDispatchUncertain => Some(CallScopeError::DeadlineExceeded {
                phase: TransportPhase::DispatchUncertain,
            }),
            _ => None,
        }
    }
}

/// Core-local arbitration errors, not registered CCError wire codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallScopeError {
    /// The finite policy cannot be represented by the local monotonic clock.
    DeadlineNotRepresentable { timeout: Duration },
    /// Expiry after dispatch is uncertain: it authorizes neither automatic
    /// replay, connection cancellation, nor release of transport memory.
    DeadlineExceeded { phase: TransportPhase },
    /// The operation cannot run in this state (including an existing terminal
    /// result). A rejected/late result remains the real result owner's concern.
    InvalidState { state: CallState },
    /// The native transport's phase proof does not authorize another attempt.
    RetryNotSafe { phase: TransportPhase },
}

impl fmt::Display for CallScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineNotRepresentable { timeout } => write!(
                f,
                "call deadline configuration error: {timeout:?} exceeds the local Instant range"
            ),
            Self::DeadlineExceeded { phase } => {
                write!(f, "call deadline exceeded ({phase:?})")
            }
            Self::InvalidState { state } => write!(f, "call scope operation rejected in {state:?}"),
            Self::RetryNotSafe { phase } => {
                write!(
                    f,
                    "call retry rejected by transport phase proof ({phase:?})"
                )
            }
        }
    }
}

impl std::error::Error for CallScopeError {}

#[derive(Debug)]
struct ScopeState {
    deadline: Option<Instant>,
    state: AtomicU8,
}

/// One independently resolved deadline shared by this call's participants.
///
/// Construct a fresh scope for each call. Clones share arbitration for that
/// same logical call; they do not create or reset its deadline. All updates
/// compete on a single atomic metadata cell. No transport/pool lock is taken,
/// and no user code, I/O, or owner cleanup executes in a state transition.
#[derive(Debug, Clone)]
pub struct CallScope {
    inner: Arc<ScopeState>,
}

#[derive(Clone, Copy)]
enum Operation {
    Observe,
    CheckPreDispatch,
    BeginDispatch,
    BeginRetryDispatch(TransportPhase),
    CompleteSuccess,
    CompleteFailure,
}

impl CallScope {
    /// Resolve policy once, at logical-call entry, against this path's default.
    /// `After(ZERO)` is already expired; `None`/`Unlimited` needs no timer.
    pub fn new(
        options: CallOptions,
        path_default: Option<Duration>,
    ) -> Result<Self, CallScopeError> {
        Self::new_at(options, path_default, Instant::now())
    }

    fn new_at(
        options: CallOptions,
        path_default: Option<Duration>,
        start: Instant,
    ) -> Result<Self, CallScopeError> {
        let deadline = options
            .effective_timeout(path_default)
            .map(|timeout| {
                start
                    .checked_add(timeout)
                    .ok_or(CallScopeError::DeadlineNotRepresentable { timeout })
            })
            .transpose()?;
        let state = if deadline.is_some_and(|deadline| start >= deadline) {
            CallState::ExpiredPreDispatch
        } else {
            CallState::PreDispatch
        };
        Ok(Self {
            inner: Arc::new(ScopeState {
                deadline,
                state: AtomicU8::new(state as u8),
            }),
        })
    }

    /// Process-local absolute deadline for a future waiter to observe. `None`
    /// means the waiter must not arrange a deadline timer for this scope.
    pub fn deadline(&self) -> Option<Instant> {
        self.inner.deadline
    }

    /// Metadata snapshot only; use `observe_deadline` to arbitrate expiry.
    pub fn state(&self) -> CallState {
        CallState::from_byte(self.inner.state.load(Ordering::Acquire))
    }

    /// Check whether route acquisition/admission or a known pre-dispatch retry
    /// may continue before this scope has ever allowed a byte. This never
    /// resets the absolute deadline or authorizes sending. After a dispatched
    /// attempt's authoritative refusal, use `try_begin_retry_dispatch` instead.
    /// Do not complete a retryable attempt as a terminal failure.
    pub fn check_pre_dispatch(&self) -> Result<(), CallScopeError> {
        self.transition(Operation::CheckPreDispatch, Instant::now)
            .map(|_| ())
    }

    /// Nonblocking first-byte admission guard for a future transport adapter.
    /// Capture a scope clone in the adapter's guard and call this immediately
    /// at first-byte admission, after pool/route waits, never before queuing.
    ///
    /// `Ok(())` irreversibly marks dispatch allowed. It is not a lock guard or
    /// permission that can be revoked: expiry afterwards is DispatchUncertain,
    /// even if the adapter has not yet written a byte. An expired pre-dispatch
    /// scope never admits a byte, and a second dispatch is always rejected.
    pub fn try_begin_dispatch(&self) -> Result<(), CallScopeError> {
        self.transition(Operation::BeginDispatch, Instant::now)
            .map(|_| ())
    }

    /// Nonblocking admission guard for another attempt after an authoritative
    /// pre-dispatch refusal. The proof must come from the native transport's
    /// classification of the preceding attempt, such as a route-token refusal
    /// proving business dispatch never occurred. Never infer PreDispatch from
    /// a timeout, connection failure, or the caller merely stopping its wait.
    ///
    /// Only a PreDispatch proof and an active scope admit another attempt.
    /// Call this at its first-byte admission, after route/pool waits. Dispatch
    /// remains sticky even after an authoritative refusal: the original local
    /// deadline is unchanged and expiry is still DispatchUncertain once any
    /// byte has been allowed. This starts no retry and resets no scope state;
    /// the adapter is responsible for obtaining a fresh proof for each retry.
    /// Ordinary `try_begin_dispatch` remains a one-time guard.
    pub fn try_begin_retry_dispatch(&self, proof: TransportPhase) -> Result<(), CallScopeError> {
        self.transition(Operation::BeginRetryDispatch(proof), Instant::now)
            .map(|_| ())
    }

    /// Confirm successful completion in Core before expiry. Merely receiving
    /// a result elsewhere before the deadline is insufficient. Equality with
    /// the deadline is expired. Late results are rejected without taking,
    /// invalidating, or releasing their owners. Requires dispatch admission.
    pub fn complete_success(&self) -> Result<(), CallScopeError> {
        self.transition(Operation::CompleteSuccess, Instant::now)
            .map(|_| ())
    }

    /// Confirm final failure, retaining the current dispatch phase. Retryable
    /// attempt failures should instead use the appropriate pre-dispatch or
    /// proof-bearing retry guard without completing the scope.
    /// Expiry wins over a failure first observed at or after the deadline.
    pub fn complete_failure(&self) -> Result<(), CallScopeError> {
        self.transition(Operation::CompleteFailure, Instant::now)
            .map(|_| ())
    }

    /// Arbitrate waiter expiry on the same cell as dispatch/completion. An
    /// early or obsolete timer is harmless. This starts no timer or task and
    /// never cancels work; terminal success/failure survives later observation.
    pub fn observe_deadline(&self) -> CallState {
        self.transition(Operation::Observe, Instant::now)
            .expect("deadline observation never rejects a state")
    }

    // Clock injection is private: adapters cannot confirm success with an old
    // timestamp. Production samples the clock on every CAS attempt. The state
    // is monotonic, so contention retries are bounded by its finite transitions.
    fn transition(
        &self,
        operation: Operation,
        mut now: impl FnMut() -> Instant,
    ) -> Result<CallState, CallScopeError> {
        loop {
            let current = self.state();
            if current.is_terminal() {
                return if matches!(operation, Operation::Observe) {
                    Ok(current)
                } else {
                    Err(current
                        .deadline_error()
                        .unwrap_or(CallScopeError::InvalidState { state: current }))
                };
            }

            let next = if self
                .inner
                .deadline
                .is_some_and(|deadline| now() >= deadline)
            {
                match current {
                    CallState::PreDispatch => CallState::ExpiredPreDispatch,
                    _ => CallState::ExpiredDispatchUncertain,
                }
            } else {
                match (operation, current) {
                    (Operation::Observe, _)
                    | (Operation::CheckPreDispatch, CallState::PreDispatch) => {
                        return Ok(current);
                    }
                    (Operation::BeginDispatch, CallState::PreDispatch) => CallState::Dispatched,
                    (Operation::BeginRetryDispatch(TransportPhase::PreDispatch), _) => {
                        CallState::Dispatched
                    }
                    (Operation::BeginRetryDispatch(phase), _) => {
                        return Err(CallScopeError::RetryNotSafe { phase });
                    }
                    (Operation::CompleteSuccess, CallState::Dispatched) => CallState::Succeeded,
                    (Operation::CompleteFailure, CallState::PreDispatch) => {
                        CallState::FailedPreDispatch
                    }
                    (Operation::CompleteFailure, CallState::Dispatched) => {
                        CallState::FailedDispatchUncertain
                    }
                    _ => return Err(CallScopeError::InvalidState { state: current }),
                }
            };

            if self
                .inner
                .state
                .compare_exchange(
                    current as u8,
                    next as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return if matches!(operation, Operation::Observe) {
                    Ok(next)
                } else {
                    next.deadline_error().map_or(Ok(next), Err)
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::{CallExecutionLimits, CallTimeout};
    use c2_mem::{RetentionBudget, RetentionPermit};
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn bounded(start: Instant, duration: Duration) -> CallScope {
        CallScope::new_at(
            CallOptions::with_timeout(CallTimeout::After(duration)),
            None,
            start,
        )
        .unwrap()
    }

    fn apply(
        scope: &CallScope,
        operation: Operation,
        at: Instant,
    ) -> Result<CallState, CallScopeError> {
        scope.transition(operation, || at)
    }

    fn expired(phase: TransportPhase) -> CallScopeError {
        CallScopeError::DeadlineExceeded { phase }
    }

    // Pause one contender after loading active metadata, before its CAS.
    // Only tests run barriers in clock injection; production uses Instant::now.
    fn paused_contender(
        scope: CallScope,
        operation: Operation,
        at: Instant,
        entered: Arc<Barrier>,
        resume: Arc<Barrier>,
    ) -> thread::JoinHandle<Result<CallState, CallScopeError>> {
        thread::spawn(move || {
            let mut first = true;
            scope.transition(operation, || {
                if std::mem::replace(&mut first, false) {
                    entered.wait();
                    resume.wait();
                }
                at
            })
        })
    }

    #[test]
    fn policies_resolve_once_inherit_override_and_unlimited() {
        let start = Instant::now();
        let default = Duration::from_secs(10);
        let explicit = Duration::from_secs(3);
        for (options, path_default, expected) in [
            (CallOptions::new(), Some(default), Some(default)),
            (CallOptions::new(), None, None),
            (
                CallOptions::with_timeout(CallTimeout::Unlimited),
                Some(default),
                None,
            ),
            (
                CallOptions::with_timeout(CallTimeout::Unlimited),
                Some(Duration::MAX),
                None,
            ),
            (
                CallOptions::with_timeout(CallTimeout::After(explicit)),
                Some(default),
                Some(explicit),
            ),
        ] {
            let scope = CallScope::new_at(options, path_default, start).unwrap();
            assert_eq!(scope.deadline(), expected.map(|duration| start + duration));
            assert_eq!(scope.state(), CallState::PreDispatch);
            assert_eq!(scope.clone().deadline(), scope.deadline());
        }
    }

    #[test]
    fn zero_is_immediately_expired_and_never_dispatches() {
        let start = Instant::now();
        for options in [
            CallOptions::with_timeout(CallTimeout::After(Duration::ZERO)),
            CallOptions::new(),
        ] {
            let scope = CallScope::new_at(options, Some(Duration::ZERO), start).unwrap();
            assert_eq!(scope.deadline(), Some(start));
            assert_eq!(scope.state(), CallState::ExpiredPreDispatch);
            assert_eq!(
                scope.try_begin_dispatch(),
                Err(expired(TransportPhase::PreDispatch))
            );
            assert_eq!(
                scope.check_pre_dispatch(),
                Err(expired(TransportPhase::PreDispatch))
            );
            assert_eq!(
                scope.complete_success(),
                Err(expired(TransportPhase::PreDispatch))
            );
        }
    }

    #[test]
    fn instant_overflow_is_an_explicit_configuration_error() {
        let start = Instant::now();
        assert!(start.checked_add(Duration::MAX).is_none());
        for options in [
            CallOptions::with_timeout(CallTimeout::After(Duration::MAX)),
            CallOptions::new(),
        ] {
            let error = CallScope::new_at(options, Some(Duration::MAX), start).unwrap_err();
            assert_eq!(
                error,
                CallScopeError::DeadlineNotRepresentable {
                    timeout: Duration::MAX
                }
            );
            assert!(error.to_string().contains("configuration error"));
            let _: &dyn std::error::Error = &error;
        }
    }

    #[test]
    fn unlimited_never_consults_deadline_clock_or_requires_timer() {
        for options in [
            CallOptions::new(),
            CallOptions::with_timeout(CallTimeout::Unlimited),
        ] {
            let scope = CallScope::new(options, None).unwrap();
            assert_eq!(scope.deadline(), None);
            for (operation, expected) in [
                (Operation::Observe, CallState::PreDispatch),
                (Operation::CheckPreDispatch, CallState::PreDispatch),
                (Operation::BeginDispatch, CallState::Dispatched),
                (Operation::CompleteSuccess, CallState::Succeeded),
                (Operation::Observe, CallState::Succeeded),
            ] {
                assert_eq!(
                    scope.transition(operation, || panic!("unlimited consulted the clock")),
                    Ok(expected)
                );
            }
        }
    }

    #[test]
    fn public_guard_is_capturable_and_allows_only_one_dispatch() {
        let scope = CallScope::new(CallOptions::new(), None).unwrap();
        assert_eq!(
            scope.complete_success(),
            Err(CallScopeError::InvalidState {
                state: CallState::PreDispatch
            })
        );
        let captured = scope.clone();
        let guard = move || captured.try_begin_dispatch();
        scope.check_pre_dispatch().unwrap();
        guard().unwrap();
        assert_eq!(
            guard(),
            Err(CallScopeError::InvalidState {
                state: CallState::Dispatched
            })
        );
        assert_eq!(
            scope.check_pre_dispatch(),
            Err(CallScopeError::InvalidState {
                state: CallState::Dispatched
            })
        );
        scope.complete_success().unwrap();
        assert_eq!(scope.observe_deadline(), CallState::Succeeded);
        assert_eq!(
            scope.complete_failure(),
            Err(CallScopeError::InvalidState {
                state: CallState::Succeeded
            })
        );
    }

    #[test]
    fn expiry_wins_and_guard_cannot_admit_first_byte() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let guard = paused_contender(
            scope.clone(),
            Operation::BeginDispatch,
            start,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        assert_eq!(
            apply(&scope, Operation::Observe, deadline),
            Ok(CallState::ExpiredPreDispatch)
        );
        resume.wait();
        assert_eq!(
            guard.join().unwrap(),
            Err(expired(TransportPhase::PreDispatch))
        );
        assert_eq!(
            scope.try_begin_dispatch(),
            Err(expired(TransportPhase::PreDispatch))
        );
    }

    #[test]
    fn dispatch_wins_and_waiter_reclassifies_old_snapshot_as_uncertain() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let waiter = paused_contender(
            scope.clone(),
            Operation::Observe,
            deadline,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        assert_eq!(
            apply(&scope, Operation::BeginDispatch, start),
            Ok(CallState::Dispatched)
        );
        resume.wait();
        assert_eq!(
            waiter.join().unwrap(),
            Ok(CallState::ExpiredDispatchUncertain)
        );
        for operation in [
            Operation::CompleteSuccess,
            Operation::CompleteFailure,
            Operation::BeginDispatch,
            Operation::CheckPreDispatch,
        ] {
            assert_eq!(
                apply(&scope, operation, deadline),
                Err(expired(TransportPhase::DispatchUncertain))
            );
        }
    }

    #[test]
    fn completion_exactly_at_deadline_is_expired_without_a_waiter() {
        let start = Instant::now();
        for operation in [Operation::CompleteSuccess, Operation::CompleteFailure] {
            let scope = bounded(start, Duration::from_secs(1));
            apply(&scope, Operation::BeginDispatch, start).unwrap();
            assert_eq!(
                apply(&scope, operation, scope.deadline().unwrap()),
                Err(expired(TransportPhase::DispatchUncertain))
            );
            assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
        }
    }

    #[test]
    fn early_timer_is_harmless_but_guard_at_deadline_expires_without_waiter() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        assert_eq!(
            apply(
                &scope,
                Operation::Observe,
                deadline - Duration::from_nanos(1)
            ),
            Ok(CallState::PreDispatch)
        );
        assert_eq!(
            apply(&scope, Operation::BeginDispatch, deadline),
            Err(expired(TransportPhase::PreDispatch))
        );
        assert_eq!(scope.state(), CallState::ExpiredPreDispatch);
    }

    #[test]
    fn deadline_wins_against_completion_with_an_old_active_snapshot() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let completion = paused_contender(
            scope.clone(),
            Operation::CompleteSuccess,
            start,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        assert_eq!(
            apply(&scope, Operation::Observe, scope.deadline().unwrap()),
            Ok(CallState::ExpiredDispatchUncertain)
        );
        resume.wait();
        assert_eq!(
            completion.join().unwrap(),
            Err(expired(TransportPhase::DispatchUncertain))
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
    }

    #[test]
    fn success_wins_and_obsolete_timer_cannot_change_terminal_state() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let timer = paused_contender(
            scope.clone(),
            Operation::Observe,
            deadline,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        assert_eq!(
            apply(
                &scope,
                Operation::CompleteSuccess,
                deadline - Duration::from_nanos(1)
            ),
            Ok(CallState::Succeeded)
        );
        resume.wait();
        assert_eq!(timer.join().unwrap(), Ok(CallState::Succeeded));
        assert_eq!(
            apply(
                &scope,
                Operation::Observe,
                deadline + Duration::from_secs(1)
            ),
            Ok(CallState::Succeeded)
        );
    }

    #[test]
    fn simultaneous_deadline_and_completion_have_one_expired_terminal() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let deadline = scope.deadline().unwrap();
        let gate = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for operation in [Operation::Observe, Operation::CompleteSuccess] {
            let scope = scope.clone();
            let gate = gate.clone();
            workers.push(thread::spawn(move || {
                gate.wait();
                apply(&scope, operation, deadline)
            }));
        }
        gate.wait();
        assert_eq!(
            workers.remove(0).join().unwrap(),
            Ok(CallState::ExpiredDispatchUncertain)
        );
        assert_eq!(
            workers.remove(0).join().unwrap(),
            Err(expired(TransportPhase::DispatchUncertain))
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
    }

    #[test]
    fn concurrent_success_and_failure_accept_only_one_terminal_result() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let gate = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for operation in [Operation::CompleteSuccess, Operation::CompleteFailure] {
            let scope = scope.clone();
            let gate = gate.clone();
            workers.push(thread::spawn(move || {
                gate.wait();
                apply(&scope, operation, start)
            }));
        }
        gate.wait();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        let terminal = scope.state();
        assert!(matches!(
            terminal,
            CallState::Succeeded | CallState::FailedDispatchUncertain
        ));
        assert!(outcomes.contains(&Err(CallScopeError::InvalidState { state: terminal })));
        assert_eq!(
            apply(&scope, Operation::Observe, scope.deadline().unwrap()),
            Ok(terminal)
        );
    }

    #[test]
    fn failure_retains_phase_and_is_not_overwritten_by_timer() {
        let start = Instant::now();
        for dispatch in [false, true] {
            let scope = bounded(start, Duration::from_secs(1));
            let expected = if dispatch {
                apply(&scope, Operation::BeginDispatch, start).unwrap();
                CallState::FailedDispatchUncertain
            } else {
                CallState::FailedPreDispatch
            };
            assert_eq!(
                apply(&scope, Operation::CompleteFailure, start),
                Ok(expected)
            );
            assert_eq!(
                apply(&scope, Operation::Observe, scope.deadline().unwrap()),
                Ok(expected)
            );
            assert_eq!(
                scope.try_begin_dispatch(),
                Err(CallScopeError::InvalidState { state: expected })
            );
        }
        let scope = CallScope::new(CallOptions::new(), None).unwrap();
        scope.complete_failure().unwrap();
        assert_eq!(scope.state(), CallState::FailedPreDispatch);
    }

    #[test]
    fn independent_scopes_and_clones_do_not_reset_or_share_deadlines() {
        let start = Instant::now();
        let short = bounded(start, Duration::from_secs(2));
        let long = bounded(start, Duration::from_secs(5));
        let later = bounded(start + Duration::from_secs(1), Duration::from_secs(2));
        let at = start + Duration::from_secs(2);
        assert_eq!(
            apply(&short, Operation::Observe, at),
            Ok(CallState::ExpiredPreDispatch)
        );
        for scope in [&long, &later] {
            assert_eq!(
                apply(scope, Operation::Observe, at),
                Ok(CallState::PreDispatch)
            );
            assert_eq!(
                apply(scope, Operation::BeginDispatch, at),
                Ok(CallState::Dispatched)
            );
        }
        assert_eq!(short.clone().state(), CallState::ExpiredPreDispatch);
        assert_eq!(long.deadline(), Some(start + Duration::from_secs(5)));
        assert_eq!(later.deadline(), Some(start + Duration::from_secs(3)));
    }

    #[test]
    fn pre_dispatch_retries_use_the_original_absolute_deadline() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(10));
        let deadline = scope.deadline();
        for elapsed in [0, 4, 9] {
            assert_eq!(
                apply(
                    &scope,
                    Operation::CheckPreDispatch,
                    start + Duration::from_secs(elapsed)
                ),
                Ok(CallState::PreDispatch)
            );
            assert_eq!(scope.deadline(), deadline);
        }
        assert_eq!(
            apply(&scope, Operation::CheckPreDispatch, deadline.unwrap()),
            Err(expired(TransportPhase::PreDispatch))
        );
        assert_eq!(
            scope.try_begin_dispatch(),
            Err(expired(TransportPhase::PreDispatch))
        );
        assert_eq!(scope.deadline(), deadline);
    }

    #[test]
    fn proof_bearing_retry_guard_is_capturable_and_keeps_ordinary_guard_one_time() {
        let scope = CallScope::new(CallOptions::new(), None).unwrap();
        let captured = scope.clone();
        let retry_guard = move |proof| captured.try_begin_retry_dispatch(proof);
        // A refusal can also precede the first byte of the first attempt.
        retry_guard(TransportPhase::PreDispatch).unwrap();
        assert_eq!(scope.state(), CallState::Dispatched);
        retry_guard(TransportPhase::PreDispatch).unwrap();
        assert_eq!(
            scope.try_begin_dispatch(),
            Err(CallScopeError::InvalidState {
                state: CallState::Dispatched
            })
        );
        assert_eq!(
            retry_guard(TransportPhase::DispatchUncertain),
            Err(CallScopeError::RetryNotSafe {
                phase: TransportPhase::DispatchUncertain
            })
        );
        scope.complete_success().unwrap();
        assert_eq!(
            retry_guard(TransportPhase::PreDispatch),
            Err(CallScopeError::InvalidState {
                state: CallState::Succeeded
            })
        );
        assert_eq!(scope.observe_deadline(), CallState::Succeeded);
    }

    #[test]
    fn authoritative_route_refusal_allows_retry_with_sticky_dispatch_and_original_deadline() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(10));
        let deadline = scope.deadline().unwrap();
        let retry = Operation::BeginRetryDispatch(TransportPhase::PreDispatch);
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        for elapsed in [3, 9] {
            assert_eq!(
                apply(&scope, retry, start + Duration::from_secs(elapsed)),
                Ok(CallState::Dispatched)
            );
            assert_eq!(scope.state(), CallState::Dispatched);
            assert_eq!(scope.deadline(), Some(deadline));
        }
        assert_eq!(
            apply(
                &scope,
                Operation::CheckPreDispatch,
                deadline - Duration::from_nanos(1)
            ),
            Err(CallScopeError::InvalidState {
                state: CallState::Dispatched
            })
        );
        // Proof describes the last attempt, not a rollback of the logical call.
        assert_eq!(
            apply(&scope, retry, deadline),
            Err(expired(TransportPhase::DispatchUncertain))
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
        assert_eq!(scope.deadline(), Some(deadline));
    }

    #[test]
    fn uncertain_proof_rejects_retry_in_either_active_state_without_changing_it() {
        let start = Instant::now();
        for dispatched in [false, true] {
            let scope = bounded(start, Duration::from_secs(1));
            if dispatched {
                apply(&scope, Operation::BeginDispatch, start).unwrap();
            }
            let state = scope.state();
            assert_eq!(
                apply(
                    &scope,
                    Operation::BeginRetryDispatch(TransportPhase::DispatchUncertain),
                    start
                ),
                Err(CallScopeError::RetryNotSafe {
                    phase: TransportPhase::DispatchUncertain
                })
            );
            assert_eq!(scope.state(), state);
            assert_eq!(scope.deadline(), Some(start + Duration::from_secs(1)));
        }
    }

    #[test]
    fn retry_proof_cannot_reopen_expired_or_completed_scopes() {
        let start = Instant::now();
        for terminal in [
            CallState::ExpiredPreDispatch,
            CallState::ExpiredDispatchUncertain,
            CallState::FailedPreDispatch,
            CallState::FailedDispatchUncertain,
            CallState::Succeeded,
        ] {
            let scope = bounded(start, Duration::from_secs(1));
            if matches!(
                terminal,
                CallState::ExpiredDispatchUncertain
                    | CallState::FailedDispatchUncertain
                    | CallState::Succeeded
            ) {
                apply(&scope, Operation::BeginDispatch, start).unwrap();
            }
            match terminal {
                CallState::ExpiredPreDispatch | CallState::ExpiredDispatchUncertain => {
                    apply(&scope, Operation::Observe, scope.deadline().unwrap()).unwrap();
                }
                CallState::Succeeded => {
                    apply(&scope, Operation::CompleteSuccess, start).unwrap();
                }
                _ => {
                    apply(&scope, Operation::CompleteFailure, start).unwrap();
                }
            }
            let expected = terminal
                .deadline_error()
                .unwrap_or(CallScopeError::InvalidState { state: terminal });
            for proof in [
                TransportPhase::PreDispatch,
                TransportPhase::DispatchUncertain,
            ] {
                assert_eq!(
                    apply(
                        &scope,
                        Operation::BeginRetryDispatch(proof),
                        scope.deadline().unwrap()
                    ),
                    Err(expected)
                );
                assert_eq!(scope.state(), terminal);
            }
            assert_eq!(
                apply(&scope, Operation::Observe, scope.deadline().unwrap()),
                Ok(terminal)
            );
        }
    }

    #[test]
    fn deadline_wins_against_proven_retry_with_an_old_active_snapshot() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let retry = paused_contender(
            scope.clone(),
            Operation::BeginRetryDispatch(TransportPhase::PreDispatch),
            start,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        assert_eq!(
            apply(&scope, Operation::Observe, scope.deadline().unwrap()),
            Ok(CallState::ExpiredDispatchUncertain)
        );
        resume.wait();
        assert_eq!(
            retry.join().unwrap(),
            Err(expired(TransportPhase::DispatchUncertain))
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
    }

    #[test]
    fn proven_retry_and_success_win_before_old_timer_without_resetting_deadline() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let timer = paused_contender(
            scope.clone(),
            Operation::Observe,
            deadline,
            entered.clone(),
            resume.clone(),
        );
        entered.wait();
        let before_deadline = deadline - Duration::from_nanos(1);
        assert_eq!(
            apply(
                &scope,
                Operation::BeginRetryDispatch(TransportPhase::PreDispatch),
                before_deadline
            ),
            Ok(CallState::Dispatched)
        );
        assert_eq!(scope.deadline(), Some(deadline));
        apply(&scope, Operation::CompleteSuccess, before_deadline).unwrap();
        resume.wait();
        assert_eq!(timer.join().unwrap(), Ok(CallState::Succeeded));
        assert_eq!(
            apply(
                &scope,
                Operation::Observe,
                deadline + Duration::from_secs(1)
            ),
            Ok(CallState::Succeeded)
        );
    }

    #[test]
    fn retry_guard_resamples_deadline_after_a_lost_cas_and_keeps_dispatch_sticky() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let deadline = scope.deadline().unwrap();
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let retry = {
            let scope = scope.clone();
            let entered = entered.clone();
            let resume = resume.clone();
            thread::spawn(move || {
                let mut samples = 0;
                let outcome = scope.transition(
                    Operation::BeginRetryDispatch(TransportPhase::PreDispatch),
                    || {
                        samples += 1;
                        if samples == 1 {
                            entered.wait();
                            resume.wait();
                            start
                        } else {
                            deadline
                        }
                    },
                );
                (outcome, samples)
            })
        };
        entered.wait();
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        resume.wait();
        assert_eq!(
            retry.join().unwrap(),
            (Err(expired(TransportPhase::DispatchUncertain)), 2)
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
        assert_eq!(scope.deadline(), Some(deadline));
    }

    struct FakePayloadOwner {
        releases: Arc<AtomicUsize>,
        _permit: RetentionPermit,
    }

    impl Drop for FakePayloadOwner {
        fn drop(&mut self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn scope_expiry_and_drop_do_not_release_payload_or_refund_budget() {
        let start = Instant::now();
        for (dispatched, expire) in [(false, false), (true, false), (false, true), (true, true)] {
            let budget = RetentionBudget::from_call_limits(&CallExecutionLimits {
                max_outstanding_calls: 1,
                retained_input_budget_bytes: 64,
            });
            let releases = Arc::new(AtomicUsize::new(0));
            let owner = FakePayloadOwner {
                releases: releases.clone(),
                _permit: budget.reserve(64).unwrap(),
            };
            let scope = bounded(start, Duration::from_secs(1));
            let guard = scope.clone();
            if dispatched {
                apply(&guard, Operation::BeginDispatch, start).unwrap();
            }
            if expire {
                let expected = if dispatched {
                    CallState::ExpiredDispatchUncertain
                } else {
                    CallState::ExpiredPreDispatch
                };
                assert_eq!(
                    apply(&scope, Operation::Observe, scope.deadline().unwrap()),
                    Ok(expected)
                );
                assert!(scope.complete_success().is_err()); // The late result is still owned below.
            }
            budget.close();
            drop(scope);
            drop(guard);
            assert_eq!(releases.load(Ordering::SeqCst), 0);
            assert_eq!(budget.snapshot().used_operations, 1);
            assert_eq!(budget.snapshot().used_retained_bytes, 64);
            drop(owner); // Only the true continuation owner releases its payload/charge.
            assert_eq!(releases.load(Ordering::SeqCst), 1);
            assert_eq!(budget.snapshot().used_operations, 0);
            assert_eq!(budget.snapshot().used_retained_bytes, 0);
        }
    }

    #[test]
    fn successful_held_owner_survives_old_timer_and_scope_drop() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        let releases = Arc::new(AtomicUsize::new(0));
        let budget = RetentionBudget::new(1, 64);
        let owner = FakePayloadOwner {
            releases: releases.clone(),
            _permit: budget.reserve(64).unwrap(),
        };
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        apply(&scope, Operation::CompleteSuccess, start).unwrap();
        assert_eq!(
            apply(&scope, Operation::Observe, scope.deadline().unwrap()),
            Ok(CallState::Succeeded)
        );
        drop(scope);
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        assert_eq!(budget.snapshot().used_retained_bytes, 64);
        drop(owner);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(budget.snapshot().used_retained_bytes, 0);
    }

    #[test]
    fn waiter_observes_deadline_while_transport_pool_lock_is_held() {
        let start = Instant::now();
        let scope = bounded(start, Duration::from_secs(1));
        apply(&scope, Operation::BeginDispatch, start).unwrap();
        let pool_lock = Arc::new(std::sync::Mutex::new(()));
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let entered = entered.clone();
            let resume = resume.clone();
            thread::spawn(move || {
                let _pool_guard = pool_lock.lock().unwrap();
                entered.wait();
                resume.wait();
            })
        };
        entered.wait();
        assert_eq!(
            apply(&scope, Operation::Observe, scope.deadline().unwrap()),
            Ok(CallState::ExpiredDispatchUncertain)
        );
        resume.wait();
        worker.join().unwrap();
    }
}
