use std::sync::Arc;

use c2_error::C2Error;

use super::{HttpCallPhase, HttpError};

/// Per-call guards supplied by the owner of the logical call scope.
///
/// Both callbacks must be synchronous and nonblocking. Capture the same scope
/// (and its original absolute deadline) in both callbacks. This layer owns no
/// deadline, cancellation watcher, executor, or retention budget. The caller's
/// owned task must continue driving a dispatched call even if its waiter leaves.
#[derive(Clone)]
pub struct HttpCallControl {
    before_attempt: Arc<dyn Fn() -> Result<(), C2Error> + Send + Sync>,
    before_dispatch: Arc<dyn Fn(Option<HttpCallPhase>) -> Result<(), C2Error> + Send + Sync>,
}

impl HttpCallControl {
    /// `before_attempt` checks that the original call is still active around
    /// route/probe/pool waits. `before_dispatch` gates a prepared business POST:
    /// `None` for the first POST, `Some(PreDispatch)` only after an authoritative
    /// stale-route rejection of the preceding POST. Resolve and probe are never
    /// business dispatches. A rejection preserves the complete local error and
    /// immediately exits without sending, retrying, or withdrawing a route.
    pub fn new(
        before_attempt: impl Fn() -> Result<(), C2Error> + Send + Sync + 'static,
        before_dispatch: impl Fn(Option<HttpCallPhase>) -> Result<(), C2Error> + Send + Sync + 'static,
    ) -> Self {
        Self {
            before_attempt: Arc::new(before_attempt),
            before_dispatch: Arc::new(before_dispatch),
        }
    }

    pub(crate) fn check_active(&self) -> Result<(), HttpError> {
        (self.before_attempt)().map_err(HttpError::LocalCallRejected)
    }

    pub(crate) fn check_dispatch(&self, previous: Option<HttpCallPhase>) -> Result<(), HttpError> {
        (self.before_dispatch)(previous).map_err(HttpError::LocalCallRejected)
    }
}

pub(crate) fn check_active(control: Option<&HttpCallControl>) -> Result<(), HttpError> {
    control.map_or(Ok(()), HttpCallControl::check_active)
}
