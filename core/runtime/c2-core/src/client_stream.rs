//! Synchronous facade over one original IPC server-stream call record.

use std::time::{Duration, Instant};

use c2_ipc::{IpcError, IpcResponseStream, IpcStreamCancellation, StreamTerminal};
use c2_mem::retention::BudgetedBuffer;
use c2_wire::rpc::CallStatus;
use parking_lot::Mutex;

use crate::call_execution::{scope_error, shared_executor};
use crate::client::normalize_ipc_call_error;
use crate::{CallScope, CallState, Error, HeldResponse, TransportPhase, normalize_ipc_error};

/// One client-owned view of a server stream. The transport record keeps its
/// operation slot until the actual terminal, independently of this facade.
pub struct CoreResponseStream {
    stream: Mutex<IpcResponseStream>,
    cancellation: IpcStreamCancellation,
    scope: CallScope,
    runtime: tokio::runtime::Handle,
}

/// Observation of the original call after cancellation intent is submitted.
/// A timeout bounds only this observation; it never stops cleanup.
pub struct CoreStreamCloseReceipt {
    pub completed: bool,
    pub terminal: Option<Result<(), Error>>,
    pub complete_items: Option<u64>,
}

impl CoreResponseStream {
    pub(crate) fn new(
        stream: IpcResponseStream,
        scope: CallScope,
        cancellation: IpcStreamCancellation,
    ) -> Self {
        Self {
            stream: Mutex::new(stream),
            cancellation,
            scope,
            runtime: shared_executor().expect("stream execution already started on shared runtime"),
        }
    }

    /// Wait for the next complete item under the call's original absolute
    /// deadline. Expiry does not submit CANCEL; the handle remains usable for
    /// terminal observation and its Drop submits cancellation separately.
    pub fn next_item(&self) -> Result<Option<HeldResponse>, Error> {
        self.check_deadline()?;
        let mut stream = self.stream.lock();
        let received = self.runtime.block_on(async {
            match self.scope.deadline() {
                Some(deadline) => {
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.recv())
                        .await
                        .map_err(|_| {
                            scope_error(crate::CallScopeError::DeadlineExceeded {
                                phase: TransportPhase::DispatchUncertain,
                            })
                        })?
                        .map_err(normalize_ipc_call_error)
                }
                None => stream.recv().await.map_err(normalize_ipc_call_error),
            }
        });
        publish_received(&self.scope, received)
    }

    /// SDK typed delivery checks the same deadline after synchronous decoding.
    #[doc(hidden)]
    pub fn check_deadline(&self) -> Result<(), Error> {
        match self.scope.observe_deadline() {
            CallState::ExpiredPreDispatch => {
                return Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                    phase: TransportPhase::PreDispatch,
                }));
            }
            CallState::ExpiredDispatchUncertain => {
                return Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                    phase: TransportPhase::DispatchUncertain,
                }));
            }
            _ => Ok(()),
        }
    }

    /// Submit cancellation intent without waiting for cleanup.
    pub fn cancel(&self) -> Result<(), Error> {
        self.cancellation.cancel();
        Ok(())
    }

    pub fn close(&self) -> CoreStreamCloseReceipt {
        self.close_with_timeout(Some(Duration::from_secs(30)))
    }

    /// `None` waits indefinitely. Zero submits cancellation then observes the
    /// current state once. A timed-out receipt can be observed again later.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> CoreStreamCloseReceipt {
        let started = Instant::now();
        let _ = self.cancel();
        // Cancellation is independent of the single iterator receiver. The
        // observation budget also covers contention with an executing next.
        let stream = match timeout {
            Some(Duration::ZERO) => self.stream.try_lock(),
            Some(timeout) => self.stream.try_lock_for(timeout),
            None => Some(self.stream.lock()),
        };
        let Some(stream) = stream else {
            return CoreStreamCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            };
        };
        let timeout = timeout.map(|timeout| timeout.saturating_sub(started.elapsed()));
        let terminal = match timeout {
            Some(Duration::ZERO) => stream.try_completion(),
            Some(timeout) => self.runtime.block_on(async {
                tokio::time::timeout(timeout, stream.completion())
                    .await
                    .ok()
            }),
            None => Some(self.runtime.block_on(stream.completion())),
        };
        match terminal {
            Some(Ok(terminal)) => {
                let complete_items = terminal.complete_items;
                let outcome = terminal_outcome(terminal);
                if outcome.is_ok() {
                    let _ = self.scope.complete_success();
                } else {
                    let _ = self.scope.complete_failure();
                }
                CoreStreamCloseReceipt {
                    completed: true,
                    terminal: Some(outcome),
                    complete_items: Some(complete_items),
                }
            }
            Some(Err(error)) => {
                let _ = self.scope.complete_failure();
                CoreStreamCloseReceipt {
                    completed: true,
                    terminal: Some(Err(normalize_ipc_call_error(error))),
                    complete_items: None,
                }
            }
            None => CoreStreamCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            },
        }
    }
}

fn publish_received(
    scope: &CallScope,
    received: Result<Option<BudgetedBuffer>, Error>,
) -> Result<Option<HeldResponse>, Error> {
    // timeout_at polls a ready inner future before its timer. Check the same
    // absolute scope at publication so a late item cannot escape that race.
    match scope.observe_deadline() {
        CallState::ExpiredPreDispatch => {
            return Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                phase: TransportPhase::PreDispatch,
            }));
        }
        CallState::ExpiredDispatchUncertain => {
            return Err(scope_error(crate::CallScopeError::DeadlineExceeded {
                phase: TransportPhase::DispatchUncertain,
            }));
        }
        _ => {}
    }
    match received {
        Ok(Some(bytes)) => Ok(Some(HeldResponse::from_budgeted_bytes(bytes))),
        Ok(None) => {
            let _ = scope.complete_success();
            Ok(None)
        }
        Err(error) => {
            let _ = scope.complete_failure();
            Err(error)
        }
    }
}

impl Drop for CoreResponseStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn terminal_outcome(terminal: StreamTerminal) -> Result<(), Error> {
    match terminal.status {
        CallStatus::Success => Ok(()),
        CallStatus::NotExecuted => Err(normalize_ipc_error(
            IpcError::CallRejected(terminal.error.expect("error CALL_END")),
            TransportPhase::PreDispatch,
        )),
        CallStatus::Failed => Err(normalize_ipc_error(
            IpcError::CrmError(terminal.error.expect("error CALL_END").to_wire_bytes()),
            TransportPhase::DispatchUncertain,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::{CallOptions, CallTimeout};
    use c2_error::ErrorCode;
    use c2_mem::retention::PayloadBufferBudget;
    use std::time::Instant;

    #[test]
    fn item_ready_after_absolute_deadline_is_refunded_without_cancellation() {
        let scope = CallScope::new(
            CallOptions::with_timeout(CallTimeout::After(Duration::from_millis(20))),
            None,
        )
        .unwrap();
        scope.try_begin_dispatch().unwrap();
        let budget = PayloadBufferBudget::new(3);
        let item = BudgetedBuffer::new(vec![1, 2, 3], budget.try_reserve(3).unwrap());
        while Instant::now() < scope.deadline().unwrap() {
            std::hint::spin_loop();
        }
        let error = publish_received(&scope, Ok(Some(item)))
            .err()
            .expect("late item refused");
        assert!(
            matches!(error, Error::Semantic(error) if error.code == ErrorCode::CallDeadlineExceeded)
        );
        assert_eq!(scope.state(), CallState::ExpiredDispatchUncertain);
        assert_eq!(budget.snapshot().used_bytes, 0);
    }
}
