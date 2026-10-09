//! Relay-owned bounded forwarding transactions. HTTP handlers own only waiters.
//!
//! The tracker and admission lock fence shutdown against task publication. The
//! permit remains with the actual task until upstream completion and response
//! materialization, including when its HTTP waiter has disappeared. This is an
//! input retention policy, independent of the existing SHM/file/reassembly budget.

use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::task::{Context, Poll};

use c2_config::CallExecutionLimits;
use c2_mem::{RetentionBudget, RetentionError, RetentionPermit, RetentionSnapshot};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;

pub(crate) struct ForwardingDomain {
    budget: RetentionBudget,
    tasks: TaskTracker,
    admission: Mutex<()>,
    #[cfg(test)]
    detached_waiters: Arc<AtomicU64>,
    #[cfg(test)]
    source_faults: [AtomicU64; 3],
}

#[derive(Debug)]
pub(crate) struct ForwardingWaiter<T> {
    result: oneshot::Receiver<T>,
    #[cfg(test)]
    completed: bool,
    #[cfg(test)]
    detached_waiters: Arc<AtomicU64>,
}

impl<T> Future for ForwardingWaiter<T> {
    type Output = Result<T, oneshot::error::RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.result).poll(cx);
        #[cfg(test)]
        if result.is_ready() {
            this.completed = true;
        }
        result
    }
}

#[cfg(test)]
impl<T> Drop for ForwardingWaiter<T> {
    fn drop(&mut self) {
        if !self.completed {
            self.detached_waiters.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl ForwardingDomain {
    pub(crate) fn new(limits: &CallExecutionLimits) -> Self {
        Self {
            budget: RetentionBudget::from_call_limits(limits),
            tasks: TaskTracker::new(),
            admission: Mutex::new(()),
            #[cfg(test)]
            detached_waiters: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            source_faults: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// No queue: reject before transferring input or publishing a native task.
    pub(crate) fn spawn<T, F, Fut>(
        &self,
        input_bytes: u64,
        work: F,
    ) -> Result<ForwardingWaiter<T>, RetentionError>
    where
        T: Send + 'static,
        F: FnOnce(RetentionPermit) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let _admission = self.admission.lock();
        let permit = self.budget.reserve(input_bytes)?;
        let (tx, rx) = oneshot::channel();
        self.tasks.spawn(async move {
            let result = work(permit).await;
            // A disconnected waiter drops the response here, invoking its
            // normal carrier/bytes owner release; it never aborts upstream.
            let _ = tx.send(result);
        });
        Ok(ForwardingWaiter {
            result: rx,
            #[cfg(test)]
            completed: false,
            #[cfg(test)]
            detached_waiters: self.detached_waiters.clone(),
        })
    }

    pub(crate) fn close(&self) {
        let _admission = self.admission.lock();
        self.budget.close();
        self.tasks.close();
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
    async fn detached_waiter_keeps_permit_and_drain_observes_actual_task() {
        let domain = Arc::new(ForwardingDomain::new(&CallExecutionLimits {
            max_outstanding_calls: 1,
            retained_input_budget_bytes: 32,
        }));
        let (release, wait) = oneshot::channel();
        let result = domain
            .spawn(32, |permit| async move {
                wait.await.unwrap();
                assert_eq!(permit.bytes(), 32);
                drop(permit);
            })
            .unwrap();
        drop(result);
        assert_eq!(domain.snapshot().used_operations, 1);
        assert_eq!(domain.snapshot().used_retained_bytes, 32);
        assert_eq!(
            domain.spawn(0, |_| async {}).unwrap_err().reason,
            RetentionRejectReason::OperationsExhausted
        );
        domain.close();
        assert_eq!(
            domain.spawn(0, |_| async {}).unwrap_err().reason,
            RetentionRejectReason::Closed
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), domain.drain())
                .await
                .is_err()
        );
        assert_eq!(domain.snapshot().used_retained_bytes, 32);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), domain.drain())
            .await
            .unwrap();
        assert_eq!(domain.snapshot().used_operations, 0);
        assert_eq!(domain.snapshot().used_retained_bytes, 0);
    }

    #[tokio::test]
    async fn bytes_zero_and_overflow_reject_without_publishing_work() {
        let domain = ForwardingDomain::new(&CallExecutionLimits {
            max_outstanding_calls: 2,
            retained_input_budget_bytes: 0,
        });
        assert_eq!(
            domain
                .spawn(1, |_| async { panic!("must not execute") })
                .unwrap_err()
                .reason,
            RetentionRejectReason::RetainedBytesExhausted
        );
        domain.spawn(0, |_| async {}).unwrap().await.unwrap();
        domain.close();
        domain.drain().await;
        assert_eq!(domain.snapshot().used_operations, 0);

        let domain = ForwardingDomain::new(&CallExecutionLimits {
            max_outstanding_calls: 2,
            retained_input_budget_bytes: u64::MAX,
        });
        let (release, wait) = oneshot::channel();
        let result = domain
            .spawn(u64::MAX, |permit| async move {
                let _ = wait.await;
                drop(permit);
            })
            .unwrap();
        assert_eq!(
            domain.spawn(1, |_| async {}).unwrap_err().reason,
            RetentionRejectReason::RetainedBytesExhausted
        );
        assert_eq!(domain.snapshot().used_operations, 1);
        domain.close();
        release.send(()).unwrap();
        result.await.unwrap();
        domain.drain().await;
    }
}
