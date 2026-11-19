//! Reusable capacity reserved for the lifetime of a forwarding window.

use crate::budget::AsyncWaiters;
use crate::{MemoryReservation, PayloadBufferError};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

#[derive(Debug)]
struct WindowState {
    available: u64,
    waiters: AsyncWaiters,
}

#[derive(Debug)]
struct Window {
    reservation: MemoryReservation,
    state: Mutex<WindowState>,
}

/// A private capacity window backed by one domain reservation. A slice
/// returns its capacity to the window when dropped. The domain gets the
/// entire reservation back when the window and all issued slices are gone.
#[derive(Debug)]
pub struct ReusableWindow {
    inner: Arc<Window>,
}

impl ReusableWindow {
    pub(crate) fn new(reservation: MemoryReservation) -> Self {
        let available = reservation.bytes();
        Self {
            inner: Arc::new(Window {
                reservation,
                state: Mutex::new(WindowState {
                    available,
                    waiters: AsyncWaiters::default(),
                }),
            }),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.inner.reservation.bytes()
    }

    /// Wait for capacity inside this window before allocating a chunk.
    pub fn reserve(&self, bytes: u64) -> WindowReserveFuture<'_> {
        WindowReserveFuture {
            window: self,
            bytes,
            waiter: None,
        }
    }
}

/// Cancelling a pending future removes only its wakeup.
#[must_use = "poll or await the slice before allocating a chunk"]
pub struct WindowReserveFuture<'a> {
    window: &'a ReusableWindow,
    bytes: u64,
    waiter: Option<u64>,
}

impl Future for WindowReserveFuture<'_> {
    type Output = Result<WindowSlice, PayloadBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let limit = this.window.capacity();
        if this.bytes > limit {
            return Poll::Ready(Err(PayloadBufferError::TooLarge {
                requested: this.bytes.into(),
                limit,
            }));
        }

        let waker = cx.waker().clone();
        let inner = &this.window.inner;
        let mut state = inner
            .state
            .lock()
            .expect("window accounting mutex poisoned");
        if this.bytes <= state.available {
            state.available -= this.bytes;
            let old_waker = state.waiters.remove(&mut this.waiter);
            drop(state);
            drop(old_waker);
            Poll::Ready(Ok(WindowSlice {
                inner: Arc::clone(inner),
                bytes: this.bytes,
            }))
        } else {
            let old_waker = state.waiters.register(&mut this.waiter, waker);
            drop(state);
            drop(old_waker);
            Poll::Pending
        }
    }
}

impl Drop for WindowReserveFuture<'_> {
    fn drop(&mut self) {
        let waker = self
            .window
            .inner
            .state
            .lock()
            .expect("window accounting mutex poisoned")
            .waiters
            .remove(&mut self.waiter);
        drop(waker);
    }
}

#[must_use = "keep the slice with its chunk storage"]
#[derive(Debug)]
pub struct WindowSlice {
    inner: Arc<Window>,
    bytes: u64,
}

impl WindowSlice {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn into_buffer(self, bytes: Vec<u8>) -> WindowChunk {
        assert!(
            bytes.capacity() as u64 <= self.bytes,
            "chunk allocation exceeds its window slice"
        );
        WindowChunk {
            bytes,
            _slice: self,
        }
    }
}

impl Drop for WindowSlice {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let waiters = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("window accounting mutex poisoned");
            state.available += self.bytes;
            state.waiters.take()
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

/// Chunk owner suitable for `Bytes::from_owner`. Storage drops before the
/// slice refunds its capacity.
#[derive(Debug)]
pub struct WindowChunk {
    bytes: Vec<u8>,
    _slice: WindowSlice,
}

impl AsRef<[u8]> for WindowChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
