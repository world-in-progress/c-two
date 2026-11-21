//! Sealed immutable shared-memory blocks supporting 1-to-N single-owner loans
//! (ADR 0001 + ADR 0005).
//!
//! A [`SealedShmBlock`] owns one buddy or dedicated [`MemHandle`] after its
//! payload bytes have been written once. Multiple physical connections may each
//! mint an independent move-only [`SharedShmLoan`] pointing to the exact same
//! SHM allocation.
//!
//! **ADR 0001 safety invariant:** Dropping a [`SharedShmLoan`] without calling
//! [`SharedShmLoan::settle`] (for example when a connection drops with an
//! unconfirmed published loan) intentionally does **not** decrement
//! `active_refs`, leaving the underlying SHM allocation pinned until pool
//! teardown so no peer can ever read reused memory.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::segment::ChargedMapping;
use crate::{MemHandle, MemPool, PoolAllocation};

struct SharedBlockInner {
    pool: Arc<RwLock<MemPool>>,
    allocation: PoolAllocation,
    mapping: Arc<ChargedMapping>,
    offset: usize,
    len: usize,
    handle: Mutex<Option<MemHandle>>,
    active_refs: AtomicUsize,
}

impl SharedBlockInner {
    fn release_one(&self) {
        let prev = self.active_refs.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "shared SHM block reference count underflow");
        if prev == 1 {
            let handle = self.handle.lock().take();
            if let Some(handle) = handle {
                // Release back to the owning pool while `self.mapping` still
                // pins the OS mapping for the duration of `release_handle`.
                self.pool.write().release_handle(handle);
            }
        }
    }
}

/// Immutable, sealed SHM allocation owned by the publisher process and safe to
/// lend across multiple peer connections concurrently without extra SHM writes.
pub struct SealedShmBlock {
    inner: Arc<SharedBlockInner>,
}

impl fmt::Debug for SealedShmBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedShmBlock")
            .field("allocation", &self.inner.allocation)
            .field("len", &self.inner.len)
            .field("active_refs", &self.active_refs())
            .finish()
    }
}

impl SealedShmBlock {
    /// Seal an already-populated buddy or dedicated [`MemHandle`] into an
    /// immutable multi-loan block. Rejects file-spill handles or handles that
    /// do not belong to `pool`, returning the original handle on failure.
    pub fn seal(pool: Arc<RwLock<MemPool>>, handle: MemHandle) -> Result<Self, (MemHandle, String)> {
        let Some(allocation) = handle.shm_allocation() else {
            return Err((
                handle,
                "cannot seal a non-SHM file-spill allocation for multi-loan".into(),
            ));
        };
        let len = handle.len();
        let pin = {
            let guard = pool.read();
            guard.checked_handle_pin(&handle)
        };
        let (mapping, offset) = match pin {
            Ok(pinned) => pinned,
            Err(error) => return Err((handle, error)),
        };
        Ok(Self {
            inner: Arc::new(SharedBlockInner {
                pool,
                allocation,
                mapping,
                offset,
                len,
                handle: Mutex::new(Some(handle)),
                active_refs: AtomicUsize::new(1),
            }),
        })
    }

    /// Allocate one SHM block from `pool`, populate it outside the pool write
    /// lock using [`crate::ShmMutPin`], and seal it for 1-to-N loans.
    pub fn allocate_and_fill<F>(
        pool: Arc<RwLock<MemPool>>,
        len: usize,
        write_into: F,
    ) -> Result<Self, String>
    where
        F: FnOnce(&mut [u8]) -> Result<(), String>,
    {
        if len == 0 {
            return Err("cannot allocate a zero-byte SHM block".into());
        }
        let mut handle = {
            let mut guard = pool.write();
            guard.try_alloc_shm(len)?
        };
        let copy_result = (|| -> Result<(), String> {
            let mut pin = {
                let guard = pool.read();
                guard.pin_shm_mut(&mut handle)?
            };
            // Populate outside any pool lock so concurrent alloc/release never stall.
            write_into(pin.as_mut_slice())
        })();
        if let Err(error) = copy_result {
            pool.write().release_handle(handle);
            return Err(error);
        }
        Self::seal(pool.clone(), handle).map_err(|(handle, error)| {
            pool.write().release_handle(handle);
            error
        })
    }

    /// Allocate one SHM block from `pool`, copy `payload` into it outside the
    /// pool write lock using [`crate::ShmMutPin`], and seal it for 1-to-N loans.
    pub fn from_bytes(pool: Arc<RwLock<MemPool>>, payload: &[u8]) -> Result<Self, String> {
        Self::allocate_and_fill(pool, payload.len(), |dst| {
            dst.copy_from_slice(payload);
            Ok(())
        })
    }

    pub fn allocation(&self) -> PoolAllocation {
        self.inner.allocation
    }

    pub fn len(&self) -> usize {
        self.inner.len
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len == 0
    }

    pub fn is_buddy(&self) -> bool {
        !self.inner.allocation.is_dedicated
    }

    pub fn is_dedicated(&self) -> bool {
        self.inner.allocation.is_dedicated
    }

    /// Number of live host-internal references (unexposed `SealedShmBlock`
    /// owners plus unsettled connection loans).
    pub fn active_refs(&self) -> usize {
        self.inner.active_refs.load(Ordering::Acquire)
    }

    /// Zero-lock read-only view of the sealed immutable SHM bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `&self` guarantees `active_refs >= 1`, so the underlying
        // `MemHandle` cannot be released to `MemPool` while this borrow lives.
        // Sealing consumed exclusive ownership of `MemHandle`, so no mutable
        // reference exists. `self.inner.mapping` pins the OS mapping and
        // `checked_handle_pin` validated `[offset .. offset + len]`.
        unsafe {
            std::slice::from_raw_parts(
                self.inner.mapping.region.base_ptr().wrapping_add(self.inner.offset),
                self.inner.len,
            )
        }
    }

    /// Copy `[offset .. offset + dst.len()]` out of the sealed SHM block for
    /// non-SHM inline/chunked fallback without copying the entire payload.
    pub fn copy_range(&self, offset: usize, dst: &mut [u8]) -> Result<(), String> {
        let end = offset
            .checked_add(dst.len())
            .ok_or_else(|| "sealed SHM range overflow".to_string())?;
        let slice = self.as_slice();
        if end > slice.len() {
            return Err(format!(
                "sealed SHM range {offset}..{end} exceeds length {}",
                slice.len()
            ));
        }
        dst.copy_from_slice(&slice[offset..end]);
        Ok(())
    }

    /// Mint one independent move-only loan token for one physical connection.
    pub fn mint_loan(&self) -> SharedShmLoan {
        let prev = self.inner.active_refs.fetch_add(1, Ordering::AcqRel);
        assert!(prev > 0, "sealed SHM block already released");
        SharedShmLoan {
            inner: Arc::clone(&self.inner),
            settled: false,
            on_settle: None,
        }
    }
}

impl Clone for SealedShmBlock {
    fn clone(&self) -> Self {
        let prev = self.inner.active_refs.fetch_add(1, Ordering::AcqRel);
        assert!(prev > 0, "sealed SHM block already released");
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for SealedShmBlock {
    fn drop(&mut self) {
        self.inner.release_one();
    }
}

/// Move-only per-connection loan authority for a [`SealedShmBlock`].
///
/// Must be explicitly settled via [`Self::settle`] when cancelled before I/O,
/// acknowledged by the peer's `RELEASE` record, or cleared by read-side
/// `peer_end`. Dropping an unsettled loan intentionally leaves its reference
/// pinned (ADR 0001).
pub struct SharedShmLoan {
    inner: Arc<SharedBlockInner>,
    settled: bool,
    on_settle: Option<Box<dyn FnOnce() + Send>>,
}

impl fmt::Debug for SharedShmLoan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedShmLoan")
            .field("allocation", &self.inner.allocation)
            .field("len", &self.inner.len)
            .field("settled", &self.settled)
            .finish()
    }
}

impl SharedShmLoan {
    pub fn allocation(&self) -> PoolAllocation {
        self.inner.allocation
    }

    pub fn len(&self) -> usize {
        self.inner.len
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len == 0
    }

    /// Attach a non-blocking notification invoked outside all locks once this
    /// loan is actually settled (by pre-I/O cancellation, `RELEASE`, or
    /// `peer_end`). Never invoked if an unconfirmed published loan is abandoned.
    pub fn with_on_settle(mut self, callback: impl FnOnce() + Send + 'static) -> Self {
        self.on_settle = Some(Box::new(callback));
        self
    }

    /// Explicitly settle this loan after pre-I/O cancellation, valid peer
    /// `RELEASE`, or read-side `peer_end`.
    pub fn settle(mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.inner.release_one();
        if let Some(callback) = self.on_settle.take() {
            callback();
        }
    }
}

impl Drop for SharedShmLoan {
    fn drop(&mut self) {
        // Intentionally do NOT call `release_one()` when `!self.settled`:
        // an unconfirmed published loan stays pinned until pool teardown.
    }
}
