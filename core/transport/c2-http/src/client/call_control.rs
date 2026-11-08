use c2_mem::retention::PayloadBufferBudget;
use std::sync::Arc;
use std::time::Instant;

use c2_error::C2Error;

use super::HttpError;

/// Per-call guards supplied by the owner of the logical call scope.
///
/// Both callbacks must be synchronous and nonblocking. Capture the same scope
/// (and its original absolute deadline) in both callbacks. This layer owns no
/// deadline, cancellation watcher, executor, or retention budget. The caller's
/// owned task must continue driving a dispatched call even if its waiter leaves.
#[derive(Clone)]
pub struct HttpCallControl {
    payload_budget: Option<PayloadBufferBudget>,
    deadline: Option<Instant>,
    before_attempt: Arc<dyn Fn() -> Result<(), C2Error> + Send + Sync>,
    before_dispatch: Arc<dyn Fn() -> Result<(), C2Error> + Send + Sync>,
}

impl HttpCallControl {
    /// `before_attempt` checks that the original call is still active around
    /// route/probe/pool waits. `before_dispatch` gates the prepared business
    /// POST. Resolve and probe never authorize business dispatch. A rejected
    /// guard preserves the complete local error without sending a request.
    pub fn new(
        before_attempt: impl Fn() -> Result<(), C2Error> + Send + Sync + 'static,
        before_dispatch: impl Fn() -> Result<(), C2Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            payload_budget: None,
            deadline: None,
            before_attempt: Arc::new(before_attempt),
            before_dispatch: Arc::new(before_dispatch),
        }
    }

    /// Runtime-owned payload domain and the call's original absolute deadline.
    /// The controlled input already carries its reservation in this domain.
    pub fn with_payload_budget(
        mut self,
        budget: PayloadBufferBudget,
        deadline: Option<Instant>,
    ) -> Self {
        self.payload_budget = Some(budget);
        self.deadline = deadline;
        self
    }

    pub(crate) fn payload_budget(&self) -> Option<&PayloadBufferBudget> {
        self.payload_budget.as_ref()
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn check_active(&self) -> Result<(), HttpError> {
        (self.before_attempt)().map_err(HttpError::LocalCallRejected)
    }

    pub(crate) fn check_dispatch(&self) -> Result<(), HttpError> {
        (self.before_dispatch)().map_err(HttpError::LocalCallRejected)
    }
}

pub(crate) fn check_active(control: Option<&HttpCallControl>) -> Result<(), HttpError> {
    control.map_or(Ok(()), HttpCallControl::check_active)
}
