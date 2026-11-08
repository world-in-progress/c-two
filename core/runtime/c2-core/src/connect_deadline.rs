use crate::Error;
use c2_config::ConnectDeadline;
use c2_error::{C2Error, ErrorCode};
use std::collections::BTreeMap;
use std::time::Duration;

/// One Core-owned acquisition budget, including SDK orchestration before I/O.
/// SDKs project this capability; they never calculate or restart its clock.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct ConnectAttempt {
    deadline: ConnectDeadline,
    call: bool,
}

impl ConnectAttempt {
    pub(crate) const fn from_deadline(deadline: ConnectDeadline) -> Self {
        Self {
            deadline,
            call: true,
        }
    }

    pub fn start(options: c2_config::ConnectOptions) -> Result<Self, Error> {
        let deadline = ConnectDeadline::start(options)
            .map_err(|error| crate::LifecycleError::Configuration(error.to_string()))?;
        let attempt = Self {
            deadline,
            call: false,
        };
        attempt.check("connect_start")?;
        Ok(attempt)
    }

    fn expired(&self, stage: &str) -> Error {
        if self.call {
            crate::call_execution::scope_error(crate::CallScopeError::DeadlineExceeded {
                phase: crate::TransportPhase::PreDispatch,
            })
        } else {
            connect_deadline_error(stage)
        }
    }

    pub(crate) fn classify(&self, error: Error) -> Error {
        if self.call
            && matches!(&error, Error::Semantic(error) if error.code == ErrorCode::CallDeadlineExceeded)
        {
            self.expired("call_reacquire")
        } else {
            error
        }
    }

    pub fn check(&self, stage: &str) -> Result<(), Error> {
        self.remaining(stage).map(|_| ())
    }

    pub fn remaining(&self, stage: &str) -> Result<Option<Duration>, Error> {
        self.deadline
            .remaining("connect_attempt")
            .map_err(|_| self.expired(stage))
    }

    pub fn lock<'a, T>(
        &self,
        mutex: &'a parking_lot::Mutex<T>,
        stage: &str,
    ) -> Result<parking_lot::MutexGuard<'a, T>, Error> {
        self.check(stage)?;
        let guard = match self.deadline.instant() {
            None => mutex.lock(),
            Some(instant) => mutex
                .try_lock_until(instant)
                .ok_or_else(|| self.expired(stage))?,
        };
        self.check(stage)?;
        Ok(guard)
    }

    pub(crate) const fn deadline(&self) -> ConnectDeadline {
        self.deadline
    }
}

/// Canonical local connection failure. No business dispatch has occurred.
#[doc(hidden)]
pub fn connect_deadline_error(stage: &str) -> Error {
    C2Error::new(ErrorCode::CallDeadlineExceeded, "connect deadline exceeded")
        .with_details(BTreeMap::from([
            ("operation".into(), "connect".into()),
            ("transport_phase".into(), "pre_dispatch".into()),
            ("stage".into(), stage.into()),
            ("fallback_eligible".into(), "false".into()),
            ("route_withdrawal".into(), "false".into()),
        ]))
        .into()
}

pub(crate) fn check(deadline: ConnectDeadline, stage: &'static str) -> Result<(), Error> {
    deadline
        .check(stage)
        .map_err(|error| connect_deadline_error(error.stage))
}

pub(crate) fn lock<'a, T>(
    mutex: &'a parking_lot::Mutex<T>,
    deadline: ConnectDeadline,
    stage: &'static str,
) -> Result<parking_lot::MutexGuard<'a, T>, Error> {
    check(deadline, stage)?;
    let guard = match deadline.instant() {
        None => mutex.lock(),
        Some(instant) => mutex
            .try_lock_until(instant)
            .ok_or_else(|| connect_deadline_error(stage))?,
    };
    check(deadline, stage)?;
    Ok(guard)
}
