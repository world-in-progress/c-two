//! Crate-private owned-call execution for the Runtime/Client boundary.
//!
//! Prepare BEFORE SDK serialization; precharge known payload nbytes there.
//! Execute corrects the charge BEFORE materialization, then gives a transport
//! adapter an owned input and a scope clone. The adapter must use that clone
//! at first-byte admission (and only authoritative PreDispatch retry proofs).
//! Execution alone never authorizes dispatch and never selects/withdraws a
//! route. Only this execution concern confirms the final result on the scope.
//! The opaque preparation scope precedes SDK serialization, which remains on
//! the caller. Materialization here is native work over already-owned prepared
//! data; this seam must not run Python/user serializers on I/O workers.
//!
//! Finite calls run on one process-wide two-worker Tokio executor. Each finite
//! preparation takes a slot, so tasks are bounded by their owning domains;
//! there is no per-call thread, timer task, abort, or blocking-task pool.
//! Unlimited unary calls drive the future inline on the same runtime, with
//! neither a deadline timer, a continuation task, nor an operation slot.
//! Every server stream has one operation slot, including unlimited streams.
//! Their owned input still uses B. Closing call-slot admissions leaves B available to
//! already dispatched calls. Transports must be cooperative async
//! futures, and native materialization must be bounded work. A stalled native
//! copy does not block caller expiry, but does occupy its worker; this is not
//! an unlimited worker-performance guarantee or a separate CPU thread pool.
//!
//! The waiter touches only short outcome metadata and CallScope. Publication
//! and scope completion share that metadata lock, closing the success/result
//! gap. Timeout settles only the scope/waiter. Task and input owners live on;
//! arbitrary cleanup runs outside the lock, late responses on the native task.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use c2_config::CallExecutionLimits;
#[cfg(test)]
use c2_config::CallOptions;
use c2_error::{C2Error, ErrorCode};
use c2_mem::retention::{
    BudgetedBuffer, PayloadBufferBudget, PayloadBufferError, PayloadBufferReservation,
    PayloadBufferSnapshot,
};
use c2_mem::{RetentionBudget, RetentionError, RetentionPermit, RetentionSnapshot};
use futures_util::FutureExt;
use parking_lot::{Condvar, Mutex};
use tokio::runtime::{Builder, Handle, Runtime};

use crate::{CallScope, CallScopeError, CallState, Error, LifecycleError, TransportPhase};

/// Pure metadata observation of one Runtime's call-operation domain.
///
/// Capturing or reading this handle never resolves configuration or creates
/// an executor. It can outlive the Runtime and retains only accounting state,
/// never a session, transport, payload, service or permission to execute calls.
/// Different Runtime instances have independent domains and limits.
#[derive(Clone, Debug)]
pub struct CallExecutionObserver {
    state: Arc<Mutex<ObservationState>>,
}

#[derive(Debug, Default)]
struct ObservationState {
    budget: Option<RetentionBudget>,
    closed: bool,
}

/// One consistent metadata observation. An uninitialized open domain remains
/// live: the first payload use may still establish its accounting budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallExecutionObservation {
    pub initialized: bool,
    pub closed: bool,
    pub counters: Option<crate::CallExecutionSnapshot>,
}

impl CallExecutionObserver {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ObservationState::default())),
        }
    }

    /// Read counters only; never reads process environment or .env and never
    /// changes configuration, admission or owner lifetimes.
    pub fn snapshot(&self) -> CallExecutionObservation {
        let state = self.state.lock();
        CallExecutionObservation {
            initialized: state.budget.is_some(),
            closed: state.closed,
            counters: state.budget.as_ref().map(RetentionBudget::snapshot),
        }
    }

    /// Retired metadata may be discarded only after closure and real release
    /// of every operation and payload byte. Open, uninitialized domains are live.
    pub fn is_live(&self) -> bool {
        let state = self.state.lock();
        !state.closed
            || state.budget.as_ref().is_some_and(|budget| {
                budget.snapshot().used_operations != 0
                    || budget.payload_budget().snapshot().used_bytes != 0
            })
    }

    pub fn same_domain(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    // RuntimeState's lock serializes publication with configuration/closure.
    // The nested metadata lock makes a captured-before-freeze observer see
    // either the complete new budget or the prior uninitialized state.
    pub(crate) fn publish(&self, context: &CallExecutionContext) {
        let mut state = self.state.lock();
        debug_assert!(state.budget.is_none());
        if state.closed {
            context.close();
        }
        state.budget = Some(context.budget.clone());
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock();
        if let Some(budget) = &state.budget {
            budget.close();
        }
        state.closed = true;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().closed
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CallExecutionContext {
    budget: RetentionBudget,
}

impl CallExecutionContext {
    /// Limits are already resolved by c2-config; no defaults/env policy here.
    pub(crate) fn new(limits: &CallExecutionLimits) -> Self {
        Self {
            budget: RetentionBudget::from_call_limits(limits),
        }
    }

    pub(crate) fn payload_budget(&self) -> PayloadBufferBudget {
        self.budget.payload_budget()
    }

    pub(crate) fn payload_snapshot(&self) -> PayloadBufferSnapshot {
        self.payload_budget().snapshot()
    }

    #[cfg(test)]
    pub(crate) fn prepare(
        &self,
        options: CallOptions,
        path_default: Option<Duration>,
    ) -> Result<PreparingCall, Error> {
        self.prepare_scope(CallScope::new(options, path_default).map_err(scope_error)?)
    }

    #[cfg(test)]
    pub(crate) fn prepare_stream(
        &self,
        options: CallOptions,
        path_default: Option<Duration>,
    ) -> Result<PreparingCall, Error> {
        self.prepare_stream_scope(CallScope::new(options, path_default).map_err(scope_error)?)
    }

    /// The Client establishes its scope at entry, before first-domain config
    /// resolution/metadata waits. Admission checks that original D afterwards.
    pub(crate) fn prepare_scope(&self, scope: CallScope) -> Result<PreparingCall, Error> {
        self.prepare_scope_for(scope, false)
    }

    pub(crate) fn prepare_stream_scope(&self, scope: CallScope) -> Result<PreparingCall, Error> {
        self.prepare_scope_for(scope, true)
    }

    fn prepare_scope_for(&self, scope: CallScope, stream: bool) -> Result<PreparingCall, Error> {
        scope.check_pre_dispatch().map_err(scope_error)?;
        // Reserve before SDK serialization. Unlimited unary has no operation
        // slot; every stream has one. Input still reserves B in either case.
        let permit = if stream || scope.deadline().is_some() {
            Some(
                self.budget
                    .reserve()
                    .map_err(|error| settle_failure(&scope, admission_error(error)))?,
            )
        } else {
            None
        };
        scope.check_pre_dispatch().map_err(scope_error)?;
        Ok(PreparingCall {
            scope,
            permit,
            payload_budget: self.payload_budget(),
            input_reservation: None,
        })
    }

    /// Stops new finite-unary and stream slots. Already admitted calls may still
    /// reserve B for their real storage; this does not cancel work or refund.
    pub(crate) fn close(&self) {
        self.budget.close();
    }

    pub(crate) fn snapshot(&self) -> RetentionSnapshot {
        self.budget.snapshot()
    }
}

pub(crate) struct PreparingCall {
    scope: CallScope,
    permit: Option<RetentionPermit>,
    payload_budget: PayloadBufferBudget,
    input_reservation: Option<PayloadBufferReservation>,
}

impl PreparingCall {
    pub(crate) fn scope_clone(&self) -> CallScope {
        self.scope.clone()
    }

    /// The original stream receive record owns its call slot until it publishes
    /// a real terminal, even after an opening waiter or facade leaves.
    pub(crate) fn take_stream_permit(&mut self) -> RetentionPermit {
        self.permit
            .take()
            .expect("stream preparation reserved a slot")
    }

    pub(crate) fn payload_budget(&self) -> PayloadBufferBudget {
        self.payload_budget.clone()
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> &CallScope {
        &self.scope
    }

    /// Reserve known input bytes before materialization. Only the first
    /// positive-byte charge may wait under the original pre-dispatch deadline.
    /// Growth of an existing positive-byte owner is immediate.
    pub(crate) fn charge_input(&mut self, nbytes: u64) -> Result<(), Error> {
        self.scope.check_pre_dispatch().map_err(scope_error)?;
        if self
            .input_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.bytes() == 0)
        {
            self.input_reservation = None;
        }
        if let Some(reservation) = &mut self.input_reservation {
            let additional = nbytes.saturating_sub(reservation.bytes());
            if additional > 0 {
                reservation
                    .try_grow(additional)
                    .map_err(|error| settle_failure(&self.scope, payload_admission_error(error)))?;
            }
        } else {
            self.input_reservation = Some(
                self.payload_budget
                    .reserve_blocking(nbytes, self.scope.deadline())
                    .map_err(|error| settle_failure(&self.scope, payload_admission_error(error)))?,
            );
        }
        self.scope.check_pre_dispatch().map_err(scope_error)?;
        Ok(())
    }

    /// `materialize` must return exactly `nbytes` bytes. This trusted internal
    /// seam permits SDK prepared-payload materialization without allocating a
    /// second owned copy. An adapter must report its size before allocation;
    /// a mismatched vector is a configuration bug, never a transport loss.
    /// The transport closure is invoked only on the execution side and must
    /// capture the supplied scope as its dispatch guard, not complete it.
    /// Materialization must consume already-owned prepared/native data. SDK
    /// user serialization stays on the caller after prepare, never here.
    pub(crate) fn execute<R, M, T, F>(
        self,
        nbytes: usize,
        materialize: M,
        transport: T,
    ) -> Result<ExecutingCall<R>, Error>
    where
        R: Send + 'static,
        M: FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
        T: FnOnce(BudgetedBuffer, CallScope) -> F + Send + 'static,
        F: Future<Output = Result<R, Error>> + Send + 'static,
    {
        self.execute_on(nbytes, materialize, transport, shared_executor)
    }

    pub(crate) fn execute_stream<R, M, T, F>(
        self,
        nbytes: usize,
        materialize: M,
        transport: T,
        abandon: fn(&R),
        on_waiter_abandon: impl Fn() + Send + Sync + 'static,
    ) -> Result<ExecutingCall<R>, Error>
    where
        R: Send + 'static,
        M: FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
        T: FnOnce(BudgetedBuffer, CallScope) -> F + Send + 'static,
        F: Future<Output = Result<R, Error>> + Send + 'static,
    {
        let scope = self.scope.clone();
        self.start_on(
            nbytes,
            materialize,
            transport,
            shared_executor,
            CompletionMode::StreamOpen,
            Some(abandon),
            Some(Arc::new(on_waiter_abandon)),
        )
        .map_err(|error| settle_failure(&scope, error))
    }

    fn execute_on<R, M, T, F>(
        self,
        nbytes: usize,
        materialize: M,
        transport: T,
        executor: impl FnOnce() -> Result<Handle, Error>,
    ) -> Result<ExecutingCall<R>, Error>
    where
        R: Send + 'static,
        M: FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
        T: FnOnce(BudgetedBuffer, CallScope) -> F + Send + 'static,
        F: Future<Output = Result<R, Error>> + Send + 'static,
    {
        let scope = self.scope.clone();
        self.start_on(
            nbytes,
            materialize,
            transport,
            executor,
            CompletionMode::WholeCall,
            None,
            None,
        )
        .map_err(|error| settle_failure(&scope, error))
    }

    fn start_on<R, M, T, F>(
        mut self,
        nbytes: usize,
        materialize: M,
        transport: T,
        executor: impl FnOnce() -> Result<Handle, Error>,
        mode: CompletionMode,
        abandon: Option<fn(&R)>,
        on_waiter_abandon: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<ExecutingCall<R>, Error>
    where
        R: Send + 'static,
        M: FnOnce() -> Result<Vec<u8>, Error> + Send + 'static,
        T: FnOnce(BudgetedBuffer, CallScope) -> F + Send + 'static,
        F: Future<Output = Result<R, Error>> + Send + 'static,
    {
        let bytes = u64::try_from(nbytes).map_err(|_| configuration("input length exceeds u64"))?;
        self.charge_input(bytes)?;
        let handle = catch_unwind(AssertUnwindSafe(executor))
            .map_err(|_| configuration("call executor initialization panicked"))??;
        self.scope.check_pre_dispatch().map_err(scope_error)?;
        let shared = Arc::new(Completion {
            scope: self.scope,
            mode,
            abandon,
            on_waiter_abandon,
            state: Mutex::new(Outcome {
                result: None,
                caller: true,
            }),
            changed: Condvar::new(),
        });
        // Construct BEFORE spawn: even a stopped runtime dropping an unpolled
        // task owns the completion guard and wakes the caller through Drop.
        let guard = TaskCompletion {
            shared: shared.clone(),
            finished: false,
        };
        let task = async move {
            let mut guard = guard;
            let scope = guard.shared.scope.clone();
            let operation = self.permit;
            let mut input_reservation = self.input_reservation;
            // Materialization is native work too: a slow copy cannot delay
            // the independent caller deadline or destroy its Vec on timeout.
            // The operation slot stays with this task. The B reservation moves
            // into the real input owner when materialization finishes, for
            // both finite and unlimited calls.
            let result = AssertUnwindSafe(async {
                scope.check_pre_dispatch().map_err(scope_error)?;
                let bytes = materialize()?;
                if bytes.len() != nbytes {
                    return Err(configuration(
                        "materialized input length differs from charged length",
                    ));
                }
                let actual_capacity = u64::try_from(bytes.capacity())
                    .map_err(|_| configuration("materialized input capacity exceeds u64"))?;
                let reservation = input_reservation
                    .as_mut()
                    .expect("input was charged before materialization");
                let additional = actual_capacity.saturating_sub(reservation.bytes());
                if additional > 0 {
                    reservation
                        .try_grow(additional)
                        .map_err(payload_admission_error)?;
                }
                let input = BudgetedBuffer::new(
                    bytes,
                    input_reservation.take().expect("input was charged"),
                );
                transport(input, scope).await
            })
            .catch_unwind()
            .await;
            let result = match result {
                Ok(result) => result,
                Err(_) => Err(execution_error(
                    &guard.shared.scope,
                    "native transport or materializer panicked",
                )),
            };
            // A finite unary call releases its slot before publishing a
            // result. A stream transferred its slot to the receive record.
            drop(operation);
            // The input reservation follows its actual last reader, including
            // any request-body slices still owned by the HTTP stack.
            drop(input_reservation);
            guard.finish(result);
        };
        let started = catch_unwind(AssertUnwindSafe(|| {
            if shared.scope.deadline().is_some() {
                // Dropping a Tokio JoinHandle detaches; never abort on timeout.
                drop(handle.spawn(task));
            } else {
                // No background continuation or timer for Unlimited.
                handle.block_on(task);
            }
        }));
        if started.is_err() {
            // Task guard normally already settled during unwind. This also
            // covers a panic before the runtime took responsibility for it.
            shared.finish(Err(execution_error(
                &shared.scope,
                "call executor failed to start",
            )));
        }
        Ok(ExecutingCall { shared })
    }
}

struct Outcome<R> {
    result: Option<Result<R, Error>>,
    caller: bool,
}

#[derive(Clone, Copy)]
enum CompletionMode {
    WholeCall,
    StreamOpen,
}

struct Completion<R> {
    scope: CallScope,
    mode: CompletionMode,
    abandon: Option<fn(&R)>,
    on_waiter_abandon: Option<Arc<dyn Fn() + Send + Sync>>,
    state: Mutex<Outcome<R>>,
    changed: Condvar,
}

impl<R> Completion<R> {
    fn finish(&self, result: Result<R, Error>) {
        // Never replace/drop a real result under the outcome metadata lock.
        let mut pending = Some(result);
        let invalid_success = execution_error(&self.scope, "success without dispatch admission");
        {
            let mut state = self.state.lock();
            let confirmation = match (self.mode, pending.as_ref().unwrap().is_ok()) {
                (CompletionMode::StreamOpen, true) => match self.scope.observe_deadline() {
                    CallState::Dispatched => Ok(()),
                    CallState::ExpiredPreDispatch => Err(CallScopeError::DeadlineExceeded {
                        phase: TransportPhase::PreDispatch,
                    }),
                    CallState::ExpiredDispatchUncertain => Err(CallScopeError::DeadlineExceeded {
                        phase: TransportPhase::DispatchUncertain,
                    }),
                    state => Err(CallScopeError::InvalidState { state }),
                },
                (_, true) => self.scope.complete_success(),
                (_, false) => self.scope.complete_failure(),
            };
            match confirmation {
                Ok(()) if state.caller => state.result = pending.take(),
                Err(CallScopeError::InvalidState {
                    state: CallState::PreDispatch,
                }) => {
                    // A defective internal adapter must not strand Unlimited.
                    if self.scope.complete_failure().is_ok() && state.caller {
                        state.result = Some(Err(invalid_success));
                    }
                }
                _ => {}
            }
            self.changed.notify_all();
        }
        if let (Some(abandon), Some(Ok(value))) = (self.abandon, pending.as_ref()) {
            abandon(value);
        }
        // Late/unreceived HeldResponse, arbitrary error and Vec cleanup occur
        // here on native execution, never under metadata or waiter timeout.
        drop(pending);
    }
}

struct TaskCompletion<R> {
    shared: Arc<Completion<R>>,
    finished: bool,
}

impl<R> TaskCompletion<R> {
    fn finish(&mut self, result: Result<R, Error>) {
        self.shared.finish(result);
        self.finished = true;
    }
}

impl<R> Drop for TaskCompletion<R> {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.finish(Err(execution_error(
                &self.shared.scope,
                "native task ended without an outcome",
            )));
        }
    }
}

/// Independent synchronous waiter. Dropping it only abandons delivery; it
/// neither drops the transport future/input nor aborts/disconnects anything.
pub(crate) struct ExecutingCall<R> {
    shared: Arc<Completion<R>>,
}

impl<R> ExecutingCall<R> {
    pub(crate) fn wait(self) -> Result<R, Error> {
        let mut state = self.shared.state.lock();
        loop {
            if let Some(result) = state.result.take() {
                state.caller = false;
                drop(state);
                return result;
            }
            let observed = self.shared.scope.observe_deadline();
            let phase = match observed {
                CallState::ExpiredPreDispatch => Some(TransportPhase::PreDispatch),
                CallState::ExpiredDispatchUncertain => Some(TransportPhase::DispatchUncertain),
                _ => None,
            };
            if let Some(phase) = phase {
                state.caller = false;
                drop(state);
                if let Some(abandon) = &self.shared.on_waiter_abandon {
                    abandon();
                }
                return Err(deadline_error(phase));
            }
            if let Some(deadline) = self.shared.scope.deadline() {
                self.shared.changed.wait_for(
                    &mut state,
                    deadline.saturating_duration_since(Instant::now()),
                );
            } else {
                self.shared.changed.wait(&mut state);
            }
        }
    }
}

impl<R> Drop for ExecutingCall<R> {
    fn drop(&mut self) {
        let (was_caller, result) = {
            let mut state = self.shared.state.lock();
            let was_caller = state.caller;
            state.caller = false;
            (was_caller, state.result.take())
        };
        if was_caller {
            if let Some(abandon) = &self.shared.on_waiter_abandon {
                abandon();
            }
        }
        // A caller abandoning an already-delivered result owns its cleanup.
        // On timeout there is no published result; native cleanup stays native.
        if let (Some(abandon), Some(Ok(value))) = (self.shared.abandon, result.as_ref()) {
            abandon(value);
        }
        drop(result);
    }
}

pub(crate) fn shared_executor() -> Result<Handle, Error> {
    static EXECUTOR: OnceLock<Result<Runtime, String>> = OnceLock::new();
    let runtime = EXECUTOR.get_or_init(|| {
        catch_unwind(AssertUnwindSafe(|| {
            Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("c2-owned-call")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        }))
        .unwrap_or_else(|_| Err("executor initialization panicked".into()))
    });
    match runtime {
        Ok(runtime) => Ok(runtime.handle().clone()),
        Err(message) => Err(configuration(format!(
            "call executor unavailable: {message}"
        ))),
    }
}

fn configuration(message: impl Into<String>) -> Error {
    LifecycleError::Configuration(message.into()).into()
}

fn phase_name(phase: TransportPhase) -> &'static str {
    match phase {
        TransportPhase::PreDispatch => "pre_dispatch",
        TransportPhase::DispatchUncertain => "dispatch_uncertain",
    }
}

fn scope_phase(scope: &CallScope) -> TransportPhase {
    match scope.state() {
        CallState::PreDispatch | CallState::FailedPreDispatch | CallState::ExpiredPreDispatch => {
            TransportPhase::PreDispatch
        }
        _ => TransportPhase::DispatchUncertain,
    }
}

fn local_error(
    code: ErrorCode,
    message: impl Into<String>,
    phase: TransportPhase,
    stage: &str,
) -> Error {
    C2Error::new(code, message)
        .with_details(BTreeMap::from([
            ("transport_phase".into(), phase_name(phase).into()),
            ("stage".into(), stage.into()),
            ("fallback_eligible".into(), "false".into()),
            ("route_withdrawal".into(), "false".into()),
        ]))
        .into()
}

fn deadline_error(phase: TransportPhase) -> Error {
    local_error(
        ErrorCode::CallDeadlineExceeded,
        "call deadline exceeded",
        phase,
        "call_deadline",
    )
}

fn admission_error(error: RetentionError) -> Error {
    let mut error_out = local_error(
        ErrorCode::CallCapacityExceeded,
        error.to_string(),
        TransportPhase::PreDispatch,
        "call_execution_admission",
    );
    if let Error::Semantic(semantic) = &mut error_out {
        semantic
            .details
            .insert("admission_reason".into(), format!("{:?}", error.reason));
        semantic
            .details
            .insert("requested_bytes".into(), error.requested_bytes.to_string());
    }
    error_out
}

fn payload_admission_error(error: PayloadBufferError) -> Error {
    match error {
        PayloadBufferError::DeadlineExceeded => deadline_error(TransportPhase::PreDispatch),
        PayloadBufferError::TooLarge { requested, limit } => {
            let mut error_out = local_error(
                ErrorCode::CallCapacityExceeded,
                error.to_string(),
                TransportPhase::PreDispatch,
                "payload_buffer_admission",
            );
            if let Error::Semantic(semantic) = &mut error_out {
                semantic
                    .details
                    .insert("requested_bytes".into(), requested.to_string());
                semantic
                    .details
                    .insert("limit_bytes".into(), limit.to_string());
            }
            error_out
        }
        PayloadBufferError::InsufficientCapacity {
            requested,
            available,
        } => {
            let mut error_out = local_error(
                ErrorCode::CallCapacityExceeded,
                error.to_string(),
                TransportPhase::PreDispatch,
                "payload_buffer_admission",
            );
            if let Error::Semantic(semantic) = &mut error_out {
                semantic
                    .details
                    .insert("requested_bytes".into(), requested.to_string());
                semantic
                    .details
                    .insert("available_bytes".into(), available.to_string());
            }
            error_out
        }
    }
}

fn execution_error(scope: &CallScope, message: &str) -> Error {
    local_error(
        ErrorCode::ClientCallingResource,
        message,
        scope_phase(scope),
        "call_execution",
    )
}

// Early local rejection participates in the same terminal arbitration as
// native completion. Deadline wins if admission/startup consumed the budget.
fn settle_failure(scope: &CallScope, error: Error) -> Error {
    match scope.complete_failure() {
        Err(CallScopeError::DeadlineExceeded { phase }) => deadline_error(phase),
        _ => error,
    }
}

pub(crate) fn scope_error(error: CallScopeError) -> Error {
    match error {
        CallScopeError::DeadlineExceeded { phase } => deadline_error(phase),
        CallScopeError::DeadlineNotRepresentable { .. } => configuration(error.to_string()),
        _ => configuration(format!("invalid call execution scope: {error}")),
    }
}

/// Transport callbacks require the canonical semantic authority, never an
/// untyped guard rejection that could trigger route fallback or withdrawal.
pub(crate) fn guard_error(error: CallScopeError) -> C2Error {
    match scope_error(error) {
        Error::Semantic(error) => error,
        other => C2Error::new(ErrorCode::ClientCallingResource, other.to_string()).with_details(
            BTreeMap::from([
                ("fallback_eligible".into(), "false".into()),
                ("route_withdrawal".into(), "false".into()),
                ("stage".into(), "call_execution_guard".into()),
            ]),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeldResponse, TransportError, TransportKind};
    use c2_config::CallTimeout;
    use std::fmt;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Weak, mpsc};
    use std::task::{Context, Poll};
    use std::thread;
    use tokio::sync::oneshot;

    const FINITE: Duration = Duration::from_millis(500);
    const TEST_WAIT: Duration = Duration::from_secs(5);
    // Fault-injection fixtures verify panic conversion, refund-once, and
    // no-strand semantics, never expiry: a loaded CI scheduler may legally
    // consume a short budget between prepare and the injected fault, and the
    // deadline must win then. Their budget therefore stays finite but large
    // enough that only the injected fault decides the outcome. Expiry and
    // budget-boundary tests keep FINITE or their own explicit deadlines.
    const INJECTION_BUDGET: Duration = Duration::from_secs(10);

    fn context(slots: u64, bytes: u64) -> CallExecutionContext {
        CallExecutionContext::new(&CallExecutionLimits {
            max_outstanding_calls: slots,
            payload_buffer_budget_bytes: bytes,
        })
    }

    fn prepare(context: &CallExecutionContext) -> PreparingCall {
        context
            .prepare(CallOptions::with_timeout(CallTimeout::After(FINITE)), None)
            .unwrap()
    }

    fn prepare_injection(context: &CallExecutionContext) -> PreparingCall {
        context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(INJECTION_BUDGET)),
                None,
            )
            .unwrap()
    }

    fn semantic(error: &Error, code: ErrorCode, phase: &str, stage: &str) {
        let Error::Semantic(error) = error else {
            panic!("expected semantic error: {error:?}");
        };
        assert_eq!(error.code, code);
        assert_eq!(error.details["transport_phase"], phase);
        assert_eq!(error.details["stage"], stage);
        assert_eq!(error.details["fallback_eligible"], "false");
        assert_eq!(error.details["route_withdrawal"], "false");
    }

    fn eventually(mut check: impl FnMut() -> bool) {
        let until = Instant::now() + TEST_WAIT;
        while !check() {
            assert!(Instant::now() < until, "native work did not settle");
            thread::yield_now();
        }
    }

    // Owns an actual HeldResponse containing local response bytes. The probe
    // observes destruction after dropping that owner, and re-enters outcome
    // metadata to assert cleanup is outside its lock and on native execution.
    struct TrackedResponse {
        held: Option<HeldResponse>,
        drops: Arc<AtomicUsize>,
        cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
    }

    impl TrackedResponse {
        fn new(drops: Arc<AtomicUsize>, cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
            Self {
                held: Some(HeldResponse::from_owned_bytes(vec![9; 4096])),
                drops,
                cleanup: Some(Box::new(cleanup)),
            }
        }
    }

    impl Drop for TrackedResponse {
        fn drop(&mut self) {
            drop(self.held.take());
            if let Some(cleanup) = self.cleanup.take() {
                cleanup();
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl fmt::Debug for TrackedResponse {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("TrackedResponse")
        }
    }
    impl fmt::Display for TrackedResponse {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("owned response error")
        }
    }
    impl std::error::Error for TrackedResponse {}

    #[test]
    fn zero_and_clock_overflow_never_start_materialization_or_future() {
        let context = context(1, 10);
        let zero = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::ZERO)),
                None,
            )
            .err()
            .unwrap();
        semantic(
            &zero,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        let overflow = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::MAX)),
                None,
            )
            .err()
            .unwrap();
        assert!(matches!(
            overflow,
            Error::Lifecycle(LifecycleError::Configuration(_))
        ));
        let snap = context.snapshot();
        assert_eq!(snap.peak_operations, 0);
        assert_eq!(snap.rejected_reservations, 0);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
    }

    #[test]
    fn slot_oversize_and_closed_rejections_precede_owned_copy_and_transport() {
        let context = context(1, 4);
        let mut first = prepare(&context);
        assert_eq!(context.snapshot().used_operations, 1);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
        let slot = context
            .prepare(CallOptions::new(), Some(FINITE))
            .err()
            .unwrap();
        semantic(
            &slot,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "call_execution_admission",
        );
        first.charge_input(2).unwrap();
        assert_eq!(context.payload_snapshot().used_bytes, 2);
        let first_scope = first.scope().clone();
        let error = first
            .execute::<(), _, _, _>(
                5,
                || panic!("copied after rejection"),
                |_, _| async { panic!("transport after rejection") },
            )
            .err()
            .unwrap();
        semantic(
            &error,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "payload_buffer_admission",
        );
        assert_eq!(first_scope.state(), CallState::FailedPreDispatch);
        assert_eq!(context.snapshot().used_operations, 0);
        assert_eq!(context.snapshot().rejected_reservations, 1);
        assert_eq!(context.payload_snapshot().rejected_allocations, 1);
        let prepared = prepare(&context);
        context.close();
        let call = prepared
            .execute::<(), _, _, _>(
                1,
                || Ok(vec![1]),
                |input, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    assert_eq!(input.as_ref().as_ref(), &[1]);
                    Ok(())
                },
            )
            .unwrap();
        call.wait().unwrap();
        let closed = context
            .prepare(CallOptions::new(), Some(FINITE))
            .err()
            .unwrap();
        semantic(
            &closed,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "call_execution_admission",
        );
        assert_eq!(context.snapshot().used_operations, 0);
        assert_eq!(context.snapshot().rejected_reservations, 2);
        assert_eq!(context.payload_snapshot().peak_bytes, 2);
    }

    #[test]
    fn byte_wait_expiry_is_pre_dispatch_and_does_not_count_as_rejection() {
        let context = context(2, 4);
        let mut first = prepare(&context);
        first.charge_input(4).unwrap();
        let mut second = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::from_millis(20))),
                None,
            )
            .unwrap();
        let error = second.charge_input(1).unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.payload_snapshot().rejected_allocations, 0);
        drop(second);
        assert_eq!(context.snapshot().used_operations, 1);
        drop(first);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
    }

    #[test]
    fn partial_inputs_cannot_wait_for_each_others_growth() {
        let context = context(2, 8);
        let mut first = prepare(&context);
        let mut second = prepare(&context);
        first.charge_input(4).unwrap();
        second.charge_input(4).unwrap();
        let error = first.charge_input(5).unwrap_err();
        semantic(
            &error,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "payload_buffer_admission",
        );
        assert_eq!(context.payload_snapshot().rejected_allocations, 1);
        assert_eq!(context.payload_snapshot().rejected_bytes, 5);
        drop(first);
        second.charge_input(5).unwrap();
        assert_eq!(context.payload_snapshot().used_bytes, 5);
    }

    #[test]
    fn materialized_input_actual_capacity_is_charged_before_transport() {
        let context = context(1, 4);
        let mut preparing = prepare(&context);
        preparing.charge_input(4).unwrap();
        let error = preparing
            .execute::<(), _, _, _>(
                4,
                || {
                    let mut bytes = Vec::with_capacity(8);
                    bytes.extend_from_slice(&[1; 4]);
                    Ok(bytes)
                },
                |_, _| async { panic!("transport after actual capacity rejection") },
            )
            .unwrap()
            .wait()
            .unwrap_err();
        semantic(
            &error,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "payload_buffer_admission",
        );
        assert_eq!(context.payload_snapshot().rejected_allocations, 1);
        assert_eq!(context.payload_snapshot().rejected_bytes, 8);
        eventually(|| context.payload_snapshot().used_bytes == 0);
    }

    #[test]
    fn zero_precharge_does_not_turn_first_positive_charge_into_immediate_rejection() {
        let context = context(2, 4);
        let mut first = prepare(&context);
        first.charge_input(4).unwrap();
        let mut second = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::from_millis(20))),
                None,
            )
            .unwrap();
        second.charge_input(0).unwrap();
        let error = second.charge_input(1).unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.payload_snapshot().rejected_allocations, 0);
    }

    #[test]
    fn final_size_grows_precharge_before_materialize_and_last_body_arc_refunds() {
        let context = context(1, 10);
        let mut preparing = prepare(&context);
        preparing.charge_input(2).unwrap();
        let seen = context.clone();
        let (body_tx, body_rx) = mpsc::channel();
        let call = preparing
            .execute(
                7,
                move || {
                    assert_eq!(seen.payload_snapshot().used_bytes, 7);
                    assert_eq!(seen.snapshot().used_operations, 1);
                    Ok(vec![1; 7])
                },
                move |input, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    let body: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(input);
                    body_tx.send(body).unwrap();
                    Ok(HeldResponse::from_owned_bytes(vec![2; 3]))
                },
            )
            .unwrap();
        let native_done = Arc::downgrade(&call.shared);
        let response = call.wait().unwrap();
        eventually(|| native_done.upgrade().is_none());
        let body = body_rx.recv_timeout(TEST_WAIT).unwrap();
        assert_eq!(body.as_ref().as_ref(), &[1; 7]);
        assert_eq!(response.bytes(), &[2; 3]);
        // Outcome has completed; HTTP-like body still owns the full charge.
        assert_eq!(context.payload_snapshot().used_bytes, 7);
        assert_eq!(context.snapshot().used_operations, 0);
        context.close();
        drop(response);
        assert_eq!(context.snapshot().used_operations, 0);
        drop(body);
        eventually(|| context.snapshot().used_operations == 0);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
    }

    #[test]
    fn success_before_deadline_survives_old_deadline_and_retains_held_owner() {
        let context = context(1, 4);
        let preparing = prepare(&context);
        let scope = preparing.scope().clone();
        let drops = Arc::new(AtomicUsize::new(0));
        let captured = drops.clone();
        let call = preparing
            .execute(
                4,
                || Ok(vec![3; 4]),
                move |_, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    Ok(TrackedResponse::new(captured, || {}))
                },
            )
            .unwrap();
        // Deliberately leave a published outcome waiting until the obsolete
        // deadline. Tests the completion/publication handshake, not a timer.
        eventually(|| scope.state() == CallState::Succeeded);
        let (_tx, rx) = mpsc::channel::<()>();
        assert!(
            rx.recv_timeout(
                scope
                    .deadline()
                    .unwrap()
                    .saturating_duration_since(Instant::now())
            )
            .is_err()
        );
        assert_eq!(scope.observe_deadline(), CallState::Succeeded);
        let response = call.wait().unwrap();
        assert_eq!(response.held.as_ref().unwrap().bytes().len(), 4096);
        assert!(!response.held.as_ref().unwrap().is_released());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(response);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        eventually(|| context.snapshot().used_operations == 0);
    }

    #[test]
    fn timeout_leaves_native_future_running_other_call_completes_and_late_success_drops_once() {
        let context = context(2, 8);
        let (release, pending) = oneshot::channel();
        let (entered, started) = mpsc::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let captured = drops.clone();
        let preparing = prepare(&context);
        let scope = preparing.scope().clone();
        let call = preparing
            .execute(
                4,
                || Ok(vec![3; 4]),
                move |input, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    let input = Arc::new(input);
                    entered.send(Arc::downgrade(&input)).unwrap();
                    pending.await.unwrap();
                    assert_eq!(input.as_ref().as_ref(), &[3; 4]);
                    Ok(TrackedResponse::new(captured, || {
                        assert_eq!(thread::current().name(), Some("c2-owned-call"));
                    }))
                },
            )
            .unwrap();
        let shared: Weak<Completion<TrackedResponse>> = Arc::downgrade(&call.shared);
        let input = started.recv_timeout(TEST_WAIT).unwrap();
        let error = call.wait().unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "dispatch_uncertain",
            "call_deadline",
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
        assert!(input.upgrade().is_some());
        assert_eq!(context.snapshot().used_operations, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let second = prepare(&context)
            .execute(
                4,
                || Ok(vec![5; 4]),
                |input, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    Ok(input.as_ref().as_ref()[0])
                },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(second, 5);
        assert!(shared.upgrade().is_some());
        release.send(()).unwrap();
        eventually(|| drops.load(Ordering::SeqCst) == 1 && input.upgrade().is_none());
        eventually(|| context.snapshot().used_operations == 0);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
        assert_eq!(
            scope.observe_deadline(),
            CallState::ExpiredDispatchUncertain
        );
    }

    #[test]
    fn late_error_destroys_real_payload_once_outside_metadata_on_native_task() {
        let context = context(1, 4);
        let (release, pending) = oneshot::channel();
        let (entered, started) = mpsc::channel();
        let (cleanup_tx, cleanup_rx) = oneshot::channel::<Weak<Completion<()>>>();
        let drops = Arc::new(AtomicUsize::new(0));
        let captured = drops.clone();
        let call = prepare(&context)
            .execute::<(), _, _, _>(
                4,
                || Ok(vec![3; 4]),
                move |_, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    entered.send(()).unwrap();
                    pending.await.unwrap();
                    let shared = cleanup_rx.await.unwrap();
                    let response = TrackedResponse::new(captured, move || {
                        assert_eq!(thread::current().name(), Some("c2-owned-call"));
                        if let Some(shared) = shared.upgrade() {
                            assert!(
                                shared.state.try_lock().is_some(),
                                "payload drop under metadata lock"
                            );
                        }
                    });
                    Err(Error::Transport(TransportError::new(
                        TransportPhase::DispatchUncertain,
                        TransportKind::Http,
                        response,
                    )))
                },
            )
            .unwrap();
        cleanup_tx.send(Arc::downgrade(&call.shared)).ok().unwrap();
        started.recv_timeout(TEST_WAIT).unwrap();
        assert!(call.wait().is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        eventually(|| drops.load(Ordering::SeqCst) == 1);
        eventually(|| context.snapshot().used_operations == 0);
    }

    #[test]
    fn abandoned_result_drops_after_slot_release() {
        let context = context(1, 0);
        let (release, pending) = oneshot::channel();
        let (entered, started) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();
        let next_context = context.clone();
        let call = prepare(&context)
            .execute(
                0,
                || Ok(Vec::new()),
                move |_, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    entered.send(()).unwrap();
                    pending.await.unwrap();
                    Ok(TrackedResponse::new(
                        Arc::new(AtomicUsize::new(0)),
                        move || {
                            // The caller abandoned delivery. Its response is destroyed
                            // inside Completion::finish on the native task, so this
                            // checks the exact publication/slot-release boundary.
                            let next = next_context
                                .prepare(CallOptions::new(), Some(FINITE))
                                .is_ok();
                            observed_tx.send(next).unwrap();
                        },
                    ))
                },
            )
            .unwrap();
        started.recv_timeout(TEST_WAIT).unwrap();
        drop(call);
        release.send(()).unwrap();
        assert!(observed_rx.recv_timeout(TEST_WAIT).unwrap());
    }

    #[test]
    fn unlimited_stream_reserves_outstanding_call_slot() {
        let context = context(1, 0);
        let first = context.prepare_stream(CallOptions::new(), None).unwrap();
        assert_eq!(context.snapshot().used_operations, 1);
        assert!(context.prepare_stream(CallOptions::new(), None).is_err());
        drop(first);
        assert!(context.prepare_stream(CallOptions::new(), None).is_ok());
    }

    #[test]
    fn dropping_caller_does_not_drop_inflight_input_or_native_future() {
        let context = context(1, 4);
        let (release, pending) = oneshot::channel();
        let (entered, started) = mpsc::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let captured = drops.clone();
        let call = prepare(&context)
            .execute(
                4,
                || Ok(vec![3; 4]),
                move |input, scope| async move {
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    let input = Arc::new(input);
                    entered.send(Arc::downgrade(&input)).unwrap();
                    pending.await.unwrap();
                    Ok(TrackedResponse::new(captured, || {}))
                },
            )
            .unwrap();
        let weak = started.recv_timeout(TEST_WAIT).unwrap();
        drop(call);
        context.close();
        assert!(weak.upgrade().is_some());
        assert_eq!(context.payload_snapshot().used_bytes, 4);
        assert_eq!(context.snapshot().used_operations, 1);
        release.send(()).unwrap();
        eventually(|| drops.load(Ordering::SeqCst) == 1 && weak.upgrade().is_none());
        eventually(|| context.snapshot().used_operations == 0);
    }

    #[test]
    fn scope_expiry_does_not_wait_for_fake_pool_lock_or_allow_late_dispatch() {
        let context = context(1, 4);
        let pool = Arc::new(std::sync::Mutex::new(()));
        let held_lock = pool.lock().unwrap();
        let captured_pool = pool.clone();
        let (entered, started) = mpsc::channel();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let captured = dispatched.clone();
        let call = prepare(&context)
            .execute::<(), _, _, _>(
                4,
                || Ok(vec![3; 4]),
                move |_, scope| async move {
                    entered.send(()).unwrap();
                    let _pool = captured_pool.lock().unwrap();
                    scope.try_begin_dispatch().map_err(scope_error)?;
                    captured.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
        started.recv_timeout(TEST_WAIT).unwrap();
        let (done, received) = mpsc::channel();
        let waiter = thread::spawn(move || done.send(call.wait()).unwrap());
        // The pool remains locked until the synchronous waiter proves expiry.
        let error = received.recv_timeout(TEST_WAIT).unwrap().unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.snapshot().used_operations, 1);
        drop(held_lock);
        waiter.join().unwrap();
        eventually(|| context.snapshot().used_operations == 0);
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unlimited_and_inherit_none_bypass_operation_slots_but_charge_input() {
        let context = context(0, 4);
        // An un-driven current-thread runtime makes any accidental background
        // spawn observable: execute must finish the future before returning.
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let caller = thread::current().id();
        for closed in [false, true] {
            if closed {
                context.close();
            }
            for (options, path_default) in [
                (
                    CallOptions::with_timeout(CallTimeout::Unlimited),
                    Some(Duration::ZERO),
                ),
                (CallOptions::new(), None), // Default direct IPC policy.
            ] {
                let mut preparing = context.prepare(options, path_default).unwrap();
                let scope = preparing.scope().clone();
                assert_eq!(scope.deadline(), None);
                preparing.charge_input(4).unwrap();
                assert_eq!(context.payload_snapshot().used_bytes, 4);
                assert_eq!(context.snapshot().used_operations, 0);
                let handle = runtime.handle().clone();
                let call = preparing
                    .execute_on(
                        4,
                        move || {
                            assert_eq!(thread::current().id(), caller);
                            Ok(vec![8; 4])
                        },
                        move |input, scope| async move {
                            assert_eq!(thread::current().id(), caller);
                            scope.try_begin_dispatch().map_err(scope_error)?;
                            tokio::task::yield_now().await;
                            assert_eq!(thread::current().id(), caller);
                            // A real escaping body owner keeps B until its
                            // final reference, without consuming a call slot.
                            let body: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(input);
                            Ok(body)
                        },
                        || Ok(handle),
                    )
                    .unwrap();
                assert_eq!(
                    scope.state(),
                    CallState::Succeeded,
                    "background continuation was created"
                );
                assert_eq!(context.payload_snapshot().used_bytes, 4);
                let body = call.wait().unwrap();
                assert_eq!(body.as_ref().as_ref(), &[8; 4]);
                assert_eq!(context.payload_snapshot().used_bytes, 4);
                drop(body);
                assert_eq!(context.payload_snapshot().used_bytes, 0);
            }
        }
    }

    #[test]
    fn finite_zero_slot_still_rejects_and_zero_deadline_wins_before_budget() {
        let context = context(0, 0);
        let error = context
            .prepare(CallOptions::new(), Some(FINITE))
            .err()
            .unwrap();
        semantic(
            &error,
            ErrorCode::CallCapacityExceeded,
            "pre_dispatch",
            "call_execution_admission",
        );
        let before = context.snapshot();
        assert_eq!(before.rejected_reservations, 1);
        assert_eq!(before.peak_operations, 0);
        assert_eq!(context.payload_snapshot().peak_bytes, 0);
        // Closed and zero-slot budget must not mask After(ZERO)'s first expiry.
        context.close();
        let before = context.snapshot();
        let error = context
            .prepare(
                CallOptions::with_timeout(CallTimeout::After(Duration::ZERO)),
                None,
            )
            .err()
            .unwrap();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.snapshot(), before);
        // Neither error yields PreparingCall, so materialize/transport cannot
        // be invoked; no executor/native continuation is even constructed.
    }

    #[test]
    fn materialization_stays_native_when_caller_expires_mid_copy() {
        // This exercises a stalled native-copy boundary, not SDK/user
        // serialization or a worker-throughput/performance guarantee.
        // Independent runtime avoids occupying both global workers when this
        // test and the fake-pool test execute in normal parallel mode.
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let context = context(1, 4);
        let (entered, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let caller = thread::current().id();
        let (transport_tx, transport_rx) = mpsc::channel();
        let call = prepare(&context)
            .execute_on::<(), _, _, _>(
                4,
                move || {
                    assert_ne!(thread::current().id(), caller);
                    entered.send(()).unwrap();
                    resume.recv_timeout(TEST_WAIT).unwrap();
                    Ok(vec![1; 4])
                },
                move |input, scope| async move {
                    assert_eq!(input.as_ref().as_ref(), &[1; 4]);
                    let error = scope.try_begin_dispatch().unwrap_err();
                    transport_tx.send(()).unwrap();
                    Err(scope_error(error))
                },
                || Ok(handle),
            )
            .unwrap();
        let weak = Arc::downgrade(&call.shared);
        started.recv_timeout(TEST_WAIT).unwrap();
        let error = call.wait().unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.snapshot().used_operations, 1);
        assert_eq!(context.payload_snapshot().used_bytes, 4);
        release.send(()).unwrap();
        transport_rx.recv_timeout(TEST_WAIT).unwrap();
        eventually(|| weak.upgrade().is_none());
        assert_eq!(context.snapshot().used_operations, 0);
    }

    #[test]
    fn queued_expiry_never_materializes_or_constructs_transport() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let handle = runtime.handle().clone();
        let context = context(1, 4);
        let call = prepare(&context)
            .execute_on::<(), _, _, _>(
                4,
                || panic!("expired queued call copied"),
                |_, _| async { panic!("expired queued call constructed transport") },
                || Ok(handle),
            )
            .unwrap();
        let weak = Arc::downgrade(&call.shared);
        let error = call.wait().unwrap_err();
        semantic(
            &error,
            ErrorCode::CallDeadlineExceeded,
            "pre_dispatch",
            "call_deadline",
        );
        assert_eq!(context.snapshot().used_operations, 1);
        runtime.block_on(async {
            tokio::task::yield_now().await;
        });
        assert!(weak.upgrade().is_none());
        assert_eq!(context.snapshot().used_operations, 0);
        assert_eq!(context.payload_snapshot().used_bytes, 0);
    }

    struct PanicFuture {
        input: BudgetedBuffer,
        scope: CallScope,
        on_drop: mpsc::Sender<()>,
    }
    impl Future for PanicFuture {
        type Output = Result<(), Error>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.scope.try_begin_dispatch().unwrap();
            assert_eq!(self.input.as_ref().as_ref(), &[1]);
            panic!("future poll panic");
        }
    }
    impl Drop for PanicFuture {
        fn drop(&mut self) {
            self.on_drop.send(()).unwrap();
        }
    }

    struct DropPanicFuture {
        input: BudgetedBuffer,
        scope: CallScope,
        on_drop: mpsc::Sender<()>,
    }
    impl Future for DropPanicFuture {
        type Output = Result<(), Error>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.scope.try_begin_dispatch().unwrap();
            assert_eq!(self.input.as_ref().as_ref(), &[1]);
            Poll::Ready(Ok(()))
        }
    }
    impl Drop for DropPanicFuture {
        fn drop(&mut self) {
            self.on_drop.send(()).unwrap();
            panic!("transport future Drop panicked after producing a result");
        }
    }

    #[test]
    fn panic_in_materializer_constructor_poll_and_future_drop_wakes_and_refunds() {
        for boundary in 0..4 {
            let context = context(1, 1);
            let preparing = prepare_injection(&context);
            let scope = preparing.scope().clone();
            let (drop_tx, drop_rx) = mpsc::channel();
            let call = preparing
                .execute::<(), _, _, _>(
                    1,
                    move || {
                        if boundary == 0 {
                            panic!("materializer panic");
                        }
                        Ok(vec![1])
                    },
                    move |input, scope| -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> {
                        if boundary == 1 {
                            panic!("constructor panic");
                        }
                        if boundary == 3 {
                            Box::pin(DropPanicFuture { input, scope, on_drop: drop_tx })
                        } else {
                            Box::pin(PanicFuture { input, scope, on_drop: drop_tx })
                        }
                    },
                )
                .unwrap();
            let error = call.wait().unwrap_err();
            semantic(
                &error,
                ErrorCode::ClientCallingResource,
                if boundary < 2 {
                    "pre_dispatch"
                } else {
                    "dispatch_uncertain"
                },
                "call_execution",
            );
            if boundary >= 2 {
                drop_rx.recv_timeout(TEST_WAIT).unwrap();
            }
            eventually(|| context.snapshot().used_operations == 0);
            assert_eq!(context.payload_snapshot().used_bytes, 0);
            assert_eq!(
                scope.state(),
                if boundary < 2 {
                    CallState::FailedPreDispatch
                } else {
                    CallState::FailedDispatchUncertain
                }
            );
        }
    }

    #[test]
    fn executor_start_failure_and_unpolled_task_drop_do_not_strand_slots() {
        let context = context(1, 1);
        let error = prepare_injection(&context)
            .execute_on::<(), _, _, _>(
                1,
                || panic!("materialized without executor"),
                |_, _| async { panic!("transport without executor") },
                || Err(configuration("injected startup failure")),
            )
            .err()
            .unwrap();
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::Configuration(_))
        ));
        assert_eq!(context.snapshot().used_operations, 0);
        let preparing = prepare_injection(&context);
        let scope = preparing.scope().clone();
        let error = preparing
            .execute_on::<(), _, _, _>(
                1,
                || panic!("materialized after startup panic"),
                |_, _| async { panic!("transport after startup panic") },
                || panic!("executor start panic"),
            )
            .err()
            .unwrap();
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::Configuration(_))
        ));
        assert_eq!(scope.state(), CallState::FailedPreDispatch);
        assert_eq!(context.snapshot().used_operations, 0);
        // Real Tokio stopped handle drops an unpolled task, exercising the
        // pre-spawn guard rather than a mirrored helper simulation.
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let handle = runtime.handle().clone();
        runtime.shutdown_background();
        let call = prepare_injection(&context)
            .execute_on::<(), _, _, _>(
                1,
                || panic!("materialized on stopped runtime"),
                |_, _| async { panic!("transport on stopped runtime") },
                || Ok(handle),
            )
            .unwrap();
        let error = call.wait().unwrap_err();
        semantic(
            &error,
            ErrorCode::ClientCallingResource,
            "pre_dispatch",
            "call_execution",
        );
        assert_eq!(context.snapshot().used_operations, 0);
    }

    #[test]
    fn invalid_success_and_materialized_size_mismatch_are_terminal() {
        let context = context(1, 2);
        let preparing = prepare(&context);
        let scope = preparing.scope().clone();
        let call = preparing
            .execute(
                1,
                || Ok(vec![1]),
                |_, _| async { Ok(HeldResponse::from_owned_bytes(vec![2])) },
            )
            .unwrap();
        let error = call.wait().err().unwrap();
        semantic(
            &error,
            ErrorCode::ClientCallingResource,
            "pre_dispatch",
            "call_execution",
        );
        assert_eq!(scope.state(), CallState::FailedPreDispatch);
        eventually(|| context.snapshot().used_operations == 0);
        let preparing = prepare(&context);
        let scope = preparing.scope().clone();
        let error = preparing
            .execute::<(), _, _, _>(
                1,
                || Ok(vec![1, 2]),
                |_, _| async { panic!("incorrect length dispatched") },
            )
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Lifecycle(LifecycleError::Configuration(_))
        ));
        assert_eq!(scope.state(), CallState::FailedPreDispatch);
        eventually(|| context.snapshot().used_operations == 0);
    }
}
