//! Relay-owned bounded forwarding transactions. HTTP handlers own only waiters.
//!
//! The tracker and admission lock fence shutdown against task publication. The
//! operation permit remains with the actual task until upstream completion,
//! including when its HTTP waiter has disappeared. Payload reservations belong
//! to the actual request and response storage, not the operation permit.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::task::{Context, Poll};

use c2_config::CallExecutionLimits;
use c2_mem::retention::PayloadBufferBudget;
use c2_mem::{RetentionBudget, RetentionError, RetentionPermit, RetentionSnapshot};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) struct ForwardingDomain {
    budget: RetentionBudget,
    tasks: TaskTracker,
    admission: Mutex<()>,
    streams: Mutex<HashMap<String, Arc<StreamCancellation>>>,
    shutdown: CancellationToken,
    #[cfg(test)]
    detached_waiters: Arc<AtomicU64>,
    #[cfg(test)]
    source_faults: [AtomicU64; 3],
}

#[derive(Debug)]
pub(crate) struct ForwardingWaiter<T> {
    result: oneshot::Receiver<T>,
    cancelled: CancellationToken,
    completed: bool,
    #[cfg(test)]
    detached_waiters: Arc<AtomicU64>,
}

impl<T> Future for ForwardingWaiter<T> {
    type Output = Result<T, oneshot::error::RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.result).poll(cx);
        if result.is_ready() {
            this.completed = true;
        }
        result
    }
}

impl<T> Drop for ForwardingWaiter<T> {
    fn drop(&mut self) {
        if !self.completed {
            self.cancelled.cancel();
        }
        #[cfg(test)]
        if !self.completed {
            self.detached_waiters.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// One coalesced intent per call. The first cause keeps its meaning.
pub(crate) struct StreamCancellation {
    cause: AtomicU8,
    consumer_requested: AtomicBool,
    changed: Notify,
}

impl StreamCancellation {
    fn new() -> Self {
        Self {
            cause: AtomicU8::new(0),
            consumer_requested: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    pub(crate) fn cancel(&self, cause: c2_wire::rpc::CancelCause) {
        let value = match cause {
            c2_wire::rpc::CancelCause::Consumer => 1,
            c2_wire::rpc::CancelCause::Lifecycle => 2,
        };
        let first = self
            .cause
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        let consumer = cause == c2_wire::rpc::CancelCause::Consumer
            && !self.consumer_requested.swap(true, Ordering::AcqRel);
        if first || consumer {
            self.changed.notify_waiters();
        }
    }

    pub(crate) fn cause(&self) -> Option<c2_wire::rpc::CancelCause> {
        match self.cause.load(Ordering::Acquire) {
            1 => Some(c2_wire::rpc::CancelCause::Consumer),
            2 => Some(c2_wire::rpc::CancelCause::Lifecycle),
            _ => None,
        }
    }

    pub(crate) async fn requested(&self) -> c2_wire::rpc::CancelCause {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(cause) = self.cause() {
                return cause;
            }
            changed.await;
        }
    }

    pub(crate) fn is_consumer_requested(&self) -> bool {
        self.consumer_requested.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_consumer_requested(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_consumer_requested() {
                return;
            }
            changed.await;
        }
    }
}

impl ForwardingDomain {
    pub(crate) fn new(limits: &CallExecutionLimits) -> Self {
        Self {
            budget: RetentionBudget::from_call_limits(limits),
            tasks: TaskTracker::new(),
            admission: Mutex::new(()),
            streams: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            detached_waiters: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            source_faults: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// No queue for operation slots: reject before publishing a native task.
    pub(crate) fn spawn<T, F, Fut>(&self, work: F) -> Result<ForwardingWaiter<T>, RetentionError>
    where
        T: Send + 'static,
        F: FnOnce(RetentionPermit, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let _admission = self.admission.lock();
        let permit = self.budget.reserve()?;
        let (tx, rx) = oneshot::channel();
        let cancelled = CancellationToken::new();
        let task_cancelled = cancelled.clone();
        self.tasks.spawn(async move {
            let result = work(permit, task_cancelled).await;
            // A disconnected waiter drops the response here, invoking its
            // normal carrier/bytes owner release; it never aborts upstream.
            let _ = tx.send(result);
        });
        Ok(ForwardingWaiter {
            result: rx,
            cancelled,
            completed: false,
            #[cfg(test)]
            detached_waiters: self.detached_waiters.clone(),
        })
    }

    pub(crate) fn close(&self) {
        let _admission = self.admission.lock();
        self.budget.close();
        self.shutdown.cancel();
        for stream in self.streams.lock().values() {
            stream.cancel(c2_wire::rpc::CancelCause::Lifecycle);
        }
        self.tasks.close();
    }

    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub(crate) fn register_stream(
        &self,
    ) -> Result<(String, Arc<StreamCancellation>), getrandom::Error> {
        let _admission = self.admission.lock();
        let control = Arc::new(StreamCancellation::new());
        if self.budget.snapshot().closed {
            control.cancel(c2_wire::rpc::CancelCause::Lifecycle);
        }
        let mut streams = self.streams.lock();
        loop {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes)?;
            let token = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if let std::collections::hash_map::Entry::Vacant(entry) = streams.entry(token.clone()) {
                entry.insert(control.clone());
                return Ok((token, control));
            }
        }
    }

    pub(crate) fn cancel_stream(&self, token: &str) -> bool {
        if let Some(control) = self.streams.lock().get(token) {
            control.cancel(c2_wire::rpc::CancelCause::Consumer);
            true
        } else {
            false
        }
    }

    pub(crate) fn remove_stream(&self, token: &str) {
        self.streams.lock().remove(token);
    }

    pub(crate) fn spawn_driver(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(work);
    }

    /// Must be called after close; permanent upstream work remains outstanding.
    /// This observes forwarding tasks. Relay shutdown separately drains the
    /// bounded native-client lifecycle, including request-only pending cleanup.
    /// Physical backing retirement remains with IPC RequestReleaseState/retire
    /// owners and their separate MemoryBudget; zero tasks is not a byte snapshot.
    pub(crate) async fn drain(&self) {
        self.tasks.wait().await;
    }

    pub(crate) fn snapshot(&self) -> RetentionSnapshot {
        self.budget.snapshot()
    }

    pub(crate) fn payload_budget(&self) -> PayloadBufferBudget {
        self.budget.payload_budget()
    }

    #[cfg(test)]
    pub(crate) fn record_source_fault(&self, unknown_length: bool) {
        self.source_faults[usize::from(unknown_length)].fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn source_faults(&self, unknown_length: bool) -> u64 {
        self.source_faults[usize::from(unknown_length)].load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn record_published_source_fault(&self) {
        self.source_faults[2].fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn published_source_faults(&self) -> u64 {
        self.source_faults[2].load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn detached_waiters(&self) -> u64 {
        self.detached_waiters.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::RetentionRejectReason;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn completed_header_waiter_does_not_cancel_response_ownership() {
        let domain = ForwardingDomain::new(&CallExecutionLimits::default());
        let waiter = domain
            .spawn(|_, cancelled| async move { cancelled })
            .unwrap();
        let cancellation = waiter.await.unwrap();
        assert!(!cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn call_tokens_expire_with_the_original_record_and_shutdown_keeps_cause() {
        let domain = ForwardingDomain::new(&CallExecutionLimits::default());
        let (first, first_control) = domain.register_stream().unwrap();
        let (second, second_control) = domain.register_stream().unwrap();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert!(!domain.cancel_stream("not-a-call"));
        assert!(domain.cancel_stream(&first));
        assert_eq!(
            first_control.requested().await,
            c2_wire::rpc::CancelCause::Consumer
        );
        domain.close();
        assert_eq!(
            second_control.requested().await,
            c2_wire::rpc::CancelCause::Lifecycle
        );
        assert_eq!(
            first_control.cause(),
            Some(c2_wire::rpc::CancelCause::Consumer)
        );
        domain.remove_stream(&first);
        assert!(!domain.cancel_stream(&first));
        domain.remove_stream(&second);
        domain.drain().await;
    }

    #[tokio::test]
    async fn detached_waiter_keeps_permit_and_drain_observes_actual_task() {
        let domain = Arc::new(ForwardingDomain::new(&CallExecutionLimits {
            max_outstanding_calls: 1,
            payload_buffer_budget_bytes: 32,
        }));
        let (release, wait) = oneshot::channel();
        let result = domain
            .spawn(|permit, _| async move {
                wait.await.unwrap();
                drop(permit);
            })
            .unwrap();
        drop(result);
        assert_eq!(domain.snapshot().used_operations, 1);
        assert_eq!(
            domain.spawn(|_, _| async {}).unwrap_err().reason,
            RetentionRejectReason::OperationsExhausted
        );
        domain.close();
        assert_eq!(
            domain.spawn(|_, _| async {}).unwrap_err().reason,
            RetentionRejectReason::Closed
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), domain.drain())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), domain.drain())
            .await
            .unwrap();
        assert_eq!(domain.snapshot().used_operations, 0);
        assert_eq!(domain.payload_budget().snapshot().used_bytes, 0);
    }

    #[tokio::test]
    async fn zero_payload_budget_still_admits_an_operation_slot() {
        let domain = ForwardingDomain::new(&CallExecutionLimits {
            max_outstanding_calls: 2,
            payload_buffer_budget_bytes: 0,
        });
        let waiter = domain.spawn(|_, _| async {}).unwrap();
        waiter.await.unwrap();
        assert!(matches!(
            domain.payload_budget().reserve(1).await,
            Err(c2_mem::retention::PayloadBufferError::TooLarge { .. })
        ));
        domain.close();
        domain.drain().await;
        assert_eq!(domain.snapshot().used_operations, 0);
    }
}
