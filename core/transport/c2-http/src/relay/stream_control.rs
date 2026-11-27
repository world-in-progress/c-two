//! Out-of-band control of one relay response stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Waker;

use futures::task::AtomicWaker;
use tokio::sync::Notify;

#[derive(Default)]
pub(crate) struct ResponseStreamControl {
    blocked: AtomicBool,
    aborted: AtomicBool,
    waker: AtomicWaker,
    pub(crate) changed: Notify,
    abort_signal: Notify,
}

impl ResponseStreamControl {
    pub(crate) fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::Acquire)
    }

    pub(crate) fn set_blocked(&self, blocked: bool) {
        if self.blocked.swap(blocked, Ordering::AcqRel) != blocked {
            self.changed.notify_waiters();
            self.changed.notify_one();
        }
    }

    pub(crate) fn abort(&self) {
        if !self.aborted.swap(true, Ordering::AcqRel) {
            self.waker.wake();
            self.abort_signal.notify_waiters();
            self.changed.notify_waiters();
            self.changed.notify_one();
        }
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    pub(crate) async fn aborted(&self) {
        let notification = self.abort_signal.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        if !self.is_aborted() {
            notification.await;
        }
    }

    pub(super) fn register_abort_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    pub(super) fn clear_abort_waker(&self) {
        self.waker.take();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn aborted_completes_both_before_and_after_registration() {
        let already = ResponseStreamControl::default();
        already.abort();
        tokio::time::timeout(Duration::from_secs(1), already.aborted())
            .await
            .unwrap();

        let pending = ResponseStreamControl::default();
        let mut wait = Box::pin(pending.aborted());
        assert!(futures::poll!(wait.as_mut()).is_pending());
        pending.abort();
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap();
    }
}
