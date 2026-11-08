//! Synchronous facade over one original server-stream call record.

use crate::client::ClientTarget;
use std::sync::Arc;
use std::time::{Duration, Instant};

use c2_http::client::{HttpError, HttpResponseStream, HttpStreamCancellation};
use c2_http::stream_wire::{HttpStreamStatus, HttpStreamTerminal};
use c2_ipc::{IpcError, IpcResponseStream, IpcStreamCancellation, StreamTerminal};
use c2_mem::retention::BudgetedBuffer;
use c2_wire::rpc::CallStatus;
use tokio::sync::Mutex;

use crate::call_execution::{scope_error, shared_executor};
use crate::client::normalize_ipc_call_error;
use crate::{CallScope, CallState, Error, HeldResponse, TransportPhase, normalize_ipc_error};

/// One client-owned view of a server stream. The transport record keeps its
/// operation slot until the actual terminal, independently of this facade.
pub struct CoreResponseStream {
    stream: Mutex<StreamTransport>,
    target: Arc<ClientTarget>,
    cancellation: IpcStreamCancellation,
    http_cancellation: HttpStreamCancellation,
    scope: CallScope,
    runtime: tokio::runtime::Handle,
}

pub(crate) enum StreamTransport {
    Ipc(IpcResponseStream),
    Http(HttpResponseStream),
}

impl StreamTransport {
    pub(crate) fn cancel(&self) {
        match self {
            Self::Ipc(stream) => {
                let _ = stream.cancel();
            }
            Self::Http(stream) => stream.cancel(),
        }
    }

    async fn recv(&mut self) -> Result<Option<BudgetedBuffer>, Error> {
        match self {
            Self::Ipc(stream) => stream.recv().await.map_err(normalize_ipc_call_error),
            Self::Http(stream) => stream.recv().await.map_err(normalize_http_stream_error),
        }
    }

    async fn completion(&mut self) -> Result<CoreTerminal, Error> {
        match self {
            Self::Ipc(stream) => stream
                .completion()
                .await
                .map(CoreTerminal::Ipc)
                .map_err(normalize_ipc_call_error),
            Self::Http(stream) => stream
                .completion_after_cancel()
                .await
                .map(CoreTerminal::Http)
                .map_err(normalize_http_stream_error),
        }
    }

    fn discard_buffered(&mut self) {
        if let Self::Http(stream) = self {
            stream.discard_buffered();
        }
    }

    fn try_completion(&self) -> Option<Result<CoreTerminal, Error>> {
        match self {
            Self::Ipc(stream) => stream.try_completion().map(|result| {
                result
                    .map(CoreTerminal::Ipc)
                    .map_err(normalize_ipc_call_error)
            }),
            Self::Http(stream) => stream.try_completion().map(|result| {
                result
                    .map(CoreTerminal::Http)
                    .map_err(normalize_http_stream_error)
            }),
        }
    }
}

enum CoreTerminal {
    Ipc(StreamTerminal),
    Http(HttpStreamTerminal),
}

impl CoreTerminal {
    fn complete_items(&self) -> Option<u64> {
        match self {
            Self::Ipc(terminal) => Some(terminal.complete_items),
            Self::Http(terminal) if terminal.status == HttpStreamStatus::NotExecuted => Some(0),
            Self::Http(terminal) => terminal.complete_items,
        }
    }

    fn outcome(self) -> Result<(), Error> {
        match self {
            Self::Ipc(terminal) => terminal_outcome(terminal),
            Self::Http(terminal) => match terminal.status {
                HttpStreamStatus::Success => Ok(()),
                HttpStreamStatus::NotExecuted | HttpStreamStatus::Failed => {
                    Err(Error::Semantic(terminal.error.expect("error CALL_END")))
                }
            },
        }
    }
}

fn normalize_http_stream_error(error: HttpError) -> Error {
    crate::normalize_http_error(error, TransportPhase::DispatchUncertain)
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
        (stream, target): (StreamTransport, Arc<ClientTarget>),
        scope: CallScope,
        cancellation: IpcStreamCancellation,
        http_cancellation: HttpStreamCancellation,
    ) -> Self {
        Self {
            stream: Mutex::new(stream),
            target,
            cancellation,
            http_cancellation,
            scope,
            runtime: shared_executor().expect("stream execution already started on shared runtime"),
        }
    }

    /// Wait for the next complete item under the call's original absolute
    /// deadline. Expiry does not submit CANCEL; the handle remains usable for
    /// terminal observation and its Drop submits cancellation separately.
    pub fn next_item(&self) -> Result<Option<HeldResponse>, Error> {
        self.runtime.block_on(self.next_item_async())
    }

    /// Async projection of the original receiver; it owns no second read loop.
    #[doc(hidden)]
    pub async fn next_item_async(&self) -> Result<Option<HeldResponse>, Error> {
        self.check_deadline()?;
        let mut stream = self.stream.lock().await;
        let received = match self.scope.deadline() {
            Some(deadline) => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.recv())
                    .await
                    .map_err(|_| {
                        scope_error(crate::CallScopeError::DeadlineExceeded {
                            phase: TransportPhase::DispatchUncertain,
                        })
                    })?
            }
            None => stream.recv().await,
        };
        if let Err(error) = &received {
            self.target.observe_error(error);
        }
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
        self.http_cancellation.cancel();
        Ok(())
    }

    pub fn close(&self) -> CoreStreamCloseReceipt {
        self.close_with_timeout(Some(Duration::from_secs(30)))
    }

    /// `None` waits indefinitely. Zero submits cancellation then observes the
    /// current state once. A timed-out receipt can be observed again later.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> CoreStreamCloseReceipt {
        self.runtime
            .block_on(self.close_with_timeout_async(timeout))
    }

    #[doc(hidden)]
    pub async fn close_with_timeout_async(
        &self,
        timeout: Option<Duration>,
    ) -> CoreStreamCloseReceipt {
        let started = Instant::now();
        let _ = self.cancel();
        let stream = match timeout {
            Some(Duration::ZERO) => self.stream.try_lock().ok(),
            Some(timeout) => tokio::time::timeout(timeout, self.stream.lock()).await.ok(),
            None => Some(self.stream.lock().await),
        };
        let Some(mut stream) = stream else {
            return CoreStreamCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            };
        };
        stream.discard_buffered();
        let timeout = timeout.map(|timeout| timeout.saturating_sub(started.elapsed()));
        let terminal = match timeout {
            Some(Duration::ZERO) => stream.try_completion(),
            Some(timeout) => tokio::time::timeout(timeout, stream.completion())
                .await
                .ok(),
            None => Some(stream.completion().await),
        };
        match terminal {
            Some(Ok(terminal)) => {
                let complete_items = terminal.complete_items();
                let outcome = terminal.outcome();
                if let Err(error) = &outcome {
                    self.target.observe_error(error);
                }
                if outcome.is_ok() {
                    let _ = self.scope.complete_success();
                } else {
                    let _ = self.scope.complete_failure();
                }
                CoreStreamCloseReceipt {
                    completed: true,
                    terminal: Some(outcome),
                    complete_items,
                }
            }
            Some(Err(error)) => {
                self.target.observe_error(&error);
                let _ = self.scope.complete_failure();
                CoreStreamCloseReceipt {
                    completed: true,
                    terminal: Some(Err(error)),
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
        self.http_cancellation.cancel();
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
