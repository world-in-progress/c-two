//! Same-process object streams. One async driver owns the original route call
//! slot; only individual resource callbacks occupy blocking workers.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use c2_error::{C2Error, ErrorCode};
use c2_server::{LocalStreamCall, LocalStreamCancellation, StreamCancelCause};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, mpsc, watch};

use crate::Error;
use crate::call_execution::shared_executor;
use crate::host::local_stream_scheduler_error;

/// Resource-owned object continuation for a same-process server stream.
/// Objects are passed directly to the caller without transport encoding.
pub trait LocalObjectStream<T>: Send + 'static {
    fn step(&mut self) -> Result<Option<T>, C2Error>;
    fn finish(self: Box<Self>) -> Result<(), C2Error>;
}

#[derive(Clone)]
struct LocalTerminal {
    result: Result<(), C2Error>,
    complete_items: u64,
}

struct LocalReceiver<T> {
    items: mpsc::Receiver<T>,
}

/// One client-owned view of a local object stream. Cancellation never waits
/// for a blocked callback or for the receiver lock.
pub struct CoreLocalResponseStream<T: Send + 'static> {
    items: Mutex<LocalReceiver<T>>,
    terminal: watch::Receiver<Option<LocalTerminal>>,
    cancellation: LocalStreamCancellation,
    runtime: Handle,
}

/// Observation of the original local call after cancellation intent.
pub struct CoreLocalStreamCloseReceipt {
    pub completed: bool,
    pub terminal: Option<Result<(), Error>>,
    pub complete_items: Option<u64>,
}

impl<T: Send + 'static> CoreLocalResponseStream<T> {
    pub(crate) fn open<F>(
        call: LocalStreamCall,
        factory: F,
        #[cfg(test)] before_first_commit: Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    ) -> Result<Self, Error>
    where
        F: FnOnce() -> Result<Box<dyn LocalObjectStream<T>>, C2Error> + Send + 'static,
    {
        let runtime = shared_executor()?;
        let cancellation = call.cancellation();
        let (items_tx, items) = mpsc::channel(1);
        let (terminal_tx, terminal) = watch::channel(None);
        runtime.spawn(async move {
            let outcome = drive(
                call,
                factory,
                items_tx,
                #[cfg(test)]
                before_first_commit,
            )
            .await;
            terminal_tx.send_replace(Some(outcome));
        });
        Ok(Self {
            items: Mutex::new(LocalReceiver { items }),
            terminal,
            cancellation,
            runtime,
        })
    }

    /// Wait for one owned object. Placing an item in this one-slot receiver
    /// grants the next step; the caller may lag by one further produced item.
    pub fn next_item(&self) -> Result<Option<T>, Error> {
        self.runtime.block_on(async {
            let mut receiver = self.items.lock().await;
            loop {
                match receiver.items.recv().await {
                    Some(item) if self.cancellation.consumer_abandoned() => {
                        drop(item);
                    }
                    Some(item) => return Ok(Some(item)),
                    None => {
                        let terminal = observe_terminal(self.terminal.clone()).await;
                        return terminal.result.map(|()| None).map_err(Error::from);
                    }
                }
            }
        })
    }

    /// Submit cancellation intent without waiting for the current step.
    pub fn cancel(&self) {
        self.cancellation.cancel_consumer();
    }

    pub fn close(&self) -> CoreLocalStreamCloseReceipt {
        self.close_with_timeout(Some(Duration::from_secs(30)))
    }

    /// `None` observes indefinitely. Zero observes once after cancellation.
    /// A timed-out receipt does not stop resource cleanup.
    pub fn close_with_timeout(&self, timeout: Option<Duration>) -> CoreLocalStreamCloseReceipt {
        self.cancel();
        let current = match timeout {
            Some(Duration::ZERO) => self.terminal.borrow().clone(),
            Some(timeout) => self.runtime.block_on(async {
                tokio::time::timeout(timeout, observe_terminal(self.terminal.clone()))
                    .await
                    .ok()
            }),
            None => Some(
                self.runtime
                    .block_on(observe_terminal(self.terminal.clone())),
            ),
        };
        match current {
            Some(terminal) => CoreLocalStreamCloseReceipt {
                completed: true,
                terminal: Some(terminal.result.map_err(Error::from)),
                complete_items: Some(terminal.complete_items),
            },
            None => CoreLocalStreamCloseReceipt {
                completed: false,
                terminal: None,
                complete_items: None,
            },
        }
    }
}

impl<T: Send + 'static> Drop for CoreLocalResponseStream<T> {
    fn drop(&mut self) {
        self.cancellation.cancel_consumer();
    }
}

async fn observe_terminal(mut receiver: watch::Receiver<Option<LocalTerminal>>) -> LocalTerminal {
    loop {
        let current = receiver.borrow_and_update().clone();
        if let Some(terminal) = current {
            return terminal;
        }
        receiver
            .changed()
            .await
            .expect("stream driver retains completion sender until terminal");
    }
}

fn cancelled(cause: StreamCancelCause) -> C2Error {
    C2Error::new(ErrorCode::CallCancelled, cause.message())
}

fn callback<T>(operation: &str, work: impl FnOnce() -> Result<T, C2Error>) -> Result<T, C2Error> {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
        Err(C2Error::new(
            ErrorCode::ResourceFunctionExecuting,
            format!("local stream {operation} panicked"),
        ))
    })
}

fn step_error(
    error: c2_server::SchedulerAcquireError,
    cancel: &LocalStreamCancellation,
) -> C2Error {
    cancel
        .cause()
        .map(cancelled)
        .unwrap_or_else(|| local_stream_scheduler_error(error))
}

type Step<T> = (Result<Option<T>, C2Error>, Option<StreamCancelCause>);

fn advance<T: Send + 'static>(
    stream: &mut dyn LocalObjectStream<T>,
    cancellation: &LocalStreamCancellation,
) -> Step<T> {
    let result = callback("step", || stream.step());
    let decision = if matches!(result, Ok(Some(_))) {
        None
    } else {
        cancellation.decide_terminal()
    };
    (result, decision)
}

async fn drive<T, F>(
    mut call: LocalStreamCall,
    factory: F,
    items: mpsc::Sender<T>,
    #[cfg(test)] before_first_commit: Option<(
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
) -> LocalTerminal
where
    T: Send + 'static,
    F: FnOnce() -> Result<Box<dyn LocalObjectStream<T>>, C2Error> + Send + 'static,
{
    let cancellation = call.cancellation();
    let first_guard = match call.first_step().await {
        Ok(guard) => guard,
        Err(error) => {
            return LocalTerminal {
                result: Err(step_error(error, &cancellation)),
                complete_items: 0,
            };
        }
    };
    call.mark_resource_entry();
    let first_cancel = cancellation.clone();
    let first = tokio::task::spawn_blocking(move || {
        let mut guard = first_guard;
        #[cfg(test)]
        if let Some((entered, proceed)) = before_first_commit {
            entered.send(()).unwrap();
            proceed.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        guard
            .commit()
            .map_err(|error| step_error(error, &first_cancel))?;
        if !first_cancel.start_step() {
            return Err(cancelled(
                first_cancel.cause().expect("cancelled before first step"),
            ));
        }
        let mut stream = callback("factory", factory)?;
        let step = advance(stream.as_mut(), &first_cancel);
        Ok::<_, C2Error>((stream, step))
    })
    .await
    .expect("local stream first callback worker panicked");
    let (mut stream, mut step) = match first {
        Ok(value) => value,
        Err(error) => {
            return LocalTerminal {
                result: Err(error),
                complete_items: 0,
            };
        }
    };
    let mut complete_items = 0;
    let result = loop {
        match step.0 {
            Ok(Some(item)) => {
                if cancellation.consumer_abandoned() {
                    break Err(cancelled(
                        cancellation.cause().unwrap_or(StreamCancelCause::Consumer),
                    ));
                }
                let sent = tokio::select! {
                    biased;
                    _ = cancellation.abandoned() => false,
                    result = items.send(item) => result.is_ok(),
                };
                if !sent {
                    cancellation.cancel_consumer();
                    break Err(cancelled(
                        cancellation.cause().unwrap_or(StreamCancelCause::Consumer),
                    ));
                }
                complete_items += 1;
                if let Some(cause) = cancellation.cause() {
                    break Err(cancelled(cause));
                }
            }
            Ok(None) => {
                break step.1.map_or(Ok(()), |cause| Err(cancelled(cause)));
            }
            Err(error) => break Err(error),
        }

        let guard = match call.next_step().await {
            Ok(guard) => guard,
            Err(error) => break Err(step_error(error, &cancellation)),
        };
        let next_cancel = cancellation.clone();
        let (returned, next) = tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            let result = match guard.commit() {
                Err(error) => Err(step_error(error, &next_cancel)),
                Ok(()) if !next_cancel.start_step() => Err(cancelled(
                    next_cancel.cause().expect("cancelled before continuation"),
                )),
                Ok(()) => {
                    let step = advance(stream.as_mut(), &next_cancel);
                    return (stream, step);
                }
            };
            (stream, (result, None))
        })
        .await
        .expect("local stream step worker panicked");
        stream = returned;
        step = next;
    };

    let cleanup = match call.cleanup_step().await {
        Ok(guard) => tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            guard.commit().map_err(local_stream_scheduler_error)?;
            callback("cleanup", || stream.finish())
        })
        .await
        .expect("local stream cleanup worker panicked"),
        Err(error) => Err(local_stream_scheduler_error(error)),
    };
    LocalTerminal {
        result: cleanup.and(result),
        complete_items,
    }
}
