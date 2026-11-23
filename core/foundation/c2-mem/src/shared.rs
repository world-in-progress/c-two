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

/// Why [`WritableSlab::try_alloc_with_reason`] could not produce a slab.
///
/// Only [`Self::CapacityExhausted`] describes a mitigable shortage: the pool
/// or its domain budget denied a representable request, so a caller that
/// sheds other retained owners may retry the allocation. The other variants
/// are permanent properties of the requested length and must never trigger
/// shedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlabAllocError {
    /// Zero-length slabs are unrepresentable.
    InvalidLength,
    /// Lengths or aligned backing sizes outside the SHM wire range.
    TooLarge,
    /// Buddy/dedicated capacity or the domain budget denied a representable
    /// request, with the bytes to free for a retry.
    CapacityExhausted { needed_bytes: u64 },
    /// The allocation is unavailable without a known recoverable shortage.
    Unavailable,
    /// Pinning a freshly allocated handle failed.
    Other(String),
}

impl std::fmt::Display for SlabAllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("slab length must be nonzero"),
            Self::TooLarge => f.write_str("slab backing exceeds the SHM wire size limit"),
            Self::CapacityExhausted { .. } => f.write_str("no SHM capacity for the requested slab"),
            Self::Unavailable => f.write_str("SHM is unavailable for the requested slab"),
            Self::Other(message) => write!(f, "slab pin failed: {message}"),
        }
    }
}

impl std::error::Error for SlabAllocError {}

/// An unpublished SHM allocation with exclusive, lock-free write access.
pub struct WritableSlab {
    pool: Arc<RwLock<MemPool>>,
    handle: Option<MemHandle>,
    mapping: Arc<ChargedMapping>,
    offset: usize,
    len: usize,
}

impl WritableSlab {
    pub fn try_alloc(pool: Arc<RwLock<MemPool>>, len: usize) -> Result<Option<Self>, String> {
        match Self::try_alloc_with_reason(pool, len) {
            Ok(slab) => Ok(Some(slab)),
            // Preserve the historical contract: every no-slab outcome except
            // a pin failure reports Ok(None). Callers that must distinguish
            // unrepresentable lengths from mitigable capacity use
            // try_alloc_with_reason.
            Err(SlabAllocError::Other(message)) => Err(message),
            Err(_) => Ok(None),
        }
    }

    /// Allocate like [`Self::try_alloc`] but distinguish why no slab was
    /// produced, so callers never mistake an unrepresentable length for a
    /// mitigable capacity shortage.
    pub fn try_alloc_with_reason(
        pool: Arc<RwLock<MemPool>>,
        len: usize,
    ) -> Result<Self, SlabAllocError> {
        if len == 0 {
            return Err(SlabAllocError::InvalidLength);
        }
        if u32::try_from(len).is_err() {
            return Err(SlabAllocError::TooLarge);
        }
        let handle = pool.write().alloc_slab_handle(len)?;
        match Self::pin(Arc::clone(&pool), handle) {
            Ok(slab) => Ok(slab),
            Err((handle, error)) => {
                pool.write().release_handle(handle);
                Err(SlabAllocError::Other(error))
            }
        }
    }

    fn pin(pool: Arc<RwLock<MemPool>>, handle: MemHandle) -> Result<Self, (MemHandle, String)> {
        let pinned = pool.read().checked_handle_pin(&handle);
        let (mapping, offset) = match pinned {
            Ok(pinned) => pinned,
            Err(error) => return Err((handle, error)),
        };
        Ok(Self {
            pool,
            len: handle.len(),
            handle: Some(handle),
            mapping,
            offset,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Check transport ownership without exposing the mutable pool.
    pub fn belongs_to(&self, pool: &Arc<RwLock<MemPool>>) -> bool {
        Arc::ptr_eq(&self.pool, pool)
    }

    /// # Safety
    /// Preserve the original pool and all live allocation and loan authority.
    pub unsafe fn pool(&self) -> &Arc<RwLock<MemPool>> {
        &self.pool
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the checked mapping pins this initialized span and the slab
        // still holds its exclusive allocation handle.
        unsafe { std::slice::from_raw_parts(self.ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the unique slab owns the unpublished span.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.len) }
    }

    fn ptr(&self) -> *mut u8 {
        self.mapping.region.base_ptr().wrapping_add(self.offset)
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr()
    }

    pub fn seal(mut self) -> Result<SealedShmBlock, String> {
        let handle = self
            .handle
            .take()
            .expect("writable slab owns its allocation");
        let allocation = handle.shm_allocation().expect("pinned slab is SHM");
        Ok(SealedShmBlock {
            inner: Arc::new(SharedBlockInner {
                pool: Arc::clone(&self.pool),
                allocation,
                mapping: Arc::clone(&self.mapping),
                offset: self.offset,
                len: self.len,
                handle: Mutex::new(Some(handle)),
                active_refs: AtomicUsize::new(1),
            }),
        })
    }

    /// Transfer the unique unpublished allocation to a single-loan owner.
    ///
    /// # Safety
    /// Preserve the original pool and associate the handle with it until
    /// transferring it to the connection loan book or releasing it locally.
    pub unsafe fn into_parts(mut self) -> (Arc<RwLock<MemPool>>, MemHandle) {
        let handle = self
            .handle
            .take()
            .expect("writable slab owns its allocation");
        (Arc::clone(&self.pool), handle)
    }
}

impl AsRef<[u8]> for WritableSlab {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Drop for WritableSlab {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.pool.write().release_handle(handle);
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
    pub fn seal(
        pool: Arc<RwLock<MemPool>>,
        handle: MemHandle,
    ) -> Result<Self, (MemHandle, String)> {
        let slab = WritableSlab::pin(pool, handle)?;
        Ok(slab.seal().expect("pinned allocation seals"))
    }

    /// Allocate one SHM block from `pool`, populate it outside the pool write
    /// lock using [`WritableSlab`], and seal it for 1-to-N loans.
    pub fn allocate_and_fill<F>(
        pool: Arc<RwLock<MemPool>>,
        len: usize,
        write_into: F,
    ) -> Result<Self, String>
    where
        F: FnOnce(&mut [u8]) -> Result<(), String>,
    {
        let mut slab = WritableSlab::try_alloc(pool, len)?
            .ok_or_else(|| format!("no SHM allocation available for {len} bytes"))?;
        write_into(slab.as_mut_slice())?;
        slab.seal()
    }

    /// Allocate one SHM block from `pool`, copy `payload` into it outside the
    /// pool write lock using [`WritableSlab`], and seal it for 1-to-N loans.
    pub fn from_bytes(pool: Arc<RwLock<MemPool>>, payload: &[u8]) -> Result<Self, String> {
        Self::allocate_and_fill(pool, payload.len(), |dst| {
            dst.copy_from_slice(payload);
            Ok(())
        })
    }

    pub fn allocation(&self) -> PoolAllocation {
        self.inner.allocation
    }

    /// The block's actual charged quota (buddy `actual_size`, or the
    /// dedicated backing size), not the logical payload length. Retention
    /// budgets count sealed blocks by this value.
    pub fn quota_bytes(&self) -> usize {
        self.inner.allocation.actual_size as usize
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
                self.inner
                    .mapping
                    .region
                    .base_ptr()
                    .wrapping_add(self.inner.offset),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_slab_transfers_or_releases_its_one_allocation() {
        fn assert_thread_safe<T: Send + Sync + 'static>() {}
        assert_thread_safe::<WritableSlab>();

        let pool = Arc::new(RwLock::new(
            MemPool::new(crate::PoolConfig::primary_for_budget(8 * 1024 * 1024)).unwrap(),
        ));
        let len = 64 * 1024;
        let mut slab = WritableSlab::try_alloc(pool.clone(), len)
            .unwrap()
            .expect("SHM");
        assert_eq!(slab.len(), len);
        assert!(!slab.is_empty());
        slab.as_mut_slice().fill(0x37);
        let ptr = slab.as_mut_ptr().cast_const();
        assert_eq!(slab.as_slice().as_ptr(), ptr);
        let sealed = slab.seal().unwrap();
        assert_eq!(sealed.as_slice().as_ptr(), ptr);
        assert!(sealed.as_slice().iter().all(|&byte| byte == 0x37));
        assert_eq!(pool.read().stats().alloc_count, 1);
        drop(sealed);
        assert_eq!(pool.read().stats().alloc_count, 0);

        let mut slab = WritableSlab::try_alloc(pool.clone(), len)
            .unwrap()
            .expect("SHM");
        slab.as_mut_slice()[0] = 5;
        // SAFETY: the original pool stays in place and releases its handle.
        let (owner, handle) = unsafe { slab.into_parts() };
        assert!(Arc::ptr_eq(&pool, &owner));
        assert_eq!(owner.read().handle_slice(&handle)[0], 5);
        owner.write().release_handle(handle);
        assert_eq!(pool.read().stats().alloc_count, 0);

        let unsealed = WritableSlab::try_alloc(pool.clone(), len)
            .unwrap()
            .expect("SHM");
        assert_eq!(pool.read().stats().alloc_count, 1);
        drop(unsealed);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn writable_slab_returns_none_for_unrepresentable_or_unfunded_requests() {
        let pool = Arc::new(RwLock::new(
            MemPool::new(crate::PoolConfig::primary_for_budget(0)).unwrap(),
        ));
        assert!(WritableSlab::try_alloc(pool.clone(), 0).unwrap().is_none());
        #[cfg(target_pointer_width = "64")]
        assert!(
            WritableSlab::try_alloc(pool.clone(), u32::MAX as usize + 1)
                .unwrap()
                .is_none()
        );
        // Explicit zero budget so admission fails before attempting an OS map.
        let budget = crate::DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 0,
            ..Default::default()
        });
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_budget(crate::PoolConfig::primary_for_budget(0), budget).unwrap(),
        ));
        assert!(WritableSlab::try_alloc(pool, 64 * 1024).unwrap().is_none());
    }

    #[test]
    fn classified_slab_failure_separates_unrepresentable_from_capacity() {
        fn classified(pool: &Arc<RwLock<MemPool>>, len: usize) -> SlabAllocError {
            match WritableSlab::try_alloc_with_reason(pool.clone(), len) {
                Err(error) => error,
                Ok(_) => panic!("len {len} must fail slab allocation"),
            }
        }
        let pool = Arc::new(RwLock::new(
            MemPool::new(crate::PoolConfig::primary_for_budget(0)).unwrap(),
        ));
        assert_eq!(
            classified(&pool, 0),
            SlabAllocError::InvalidLength,
            "zero-length must be an unrepresentable length, not capacity"
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            classified(&pool, u32::MAX as usize + 1),
            SlabAllocError::TooLarge
        );
        assert_eq!(
            classified(&pool, u32::MAX as usize),
            SlabAllocError::TooLarge
        );
        let budget = crate::DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 64 * 1024,
            ..Default::default()
        });
        let reserved = budget.try_reserve_memory(128).unwrap();
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_budget(
                crate::PoolConfig {
                    pool_enabled: false,
                    spill_threshold: 1.0,
                    ..crate::PoolConfig::default()
                },
                budget,
            )
            .unwrap(),
        ));
        assert_eq!(classified(&pool, 65 * 1024), SlabAllocError::Unavailable);
        assert_eq!(
            classified(&pool, 64 * 1024),
            SlabAllocError::CapacityExhausted { needed_bytes: 128 },
        );
        drop(reserved);
        assert!(WritableSlab::try_alloc_with_reason(pool, 64 * 1024).is_ok());
    }

    #[test]
    fn sealed_block_reports_its_buddy_quota_bytes() {
        let pool = Arc::new(RwLock::new(
            MemPool::new(crate::PoolConfig::primary_for_budget(8 * 1024 * 1024)).unwrap(),
        ));
        let len = 65 * 1024; // between buddy levels: quota rounds up
        let sealed = SealedShmBlock::from_bytes(pool, &vec![7u8; len]).unwrap();
        assert_eq!(sealed.len(), len);
        assert!(sealed.quota_bytes() >= len);
        assert_eq!(sealed.quota_bytes() % (64 * 1024), 0);
    }

    #[test]
    fn unwinding_a_fill_releases_the_unsealed_block() {
        let budget = crate::DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 8 * 1024 * 1024,
            file_spill_budget_bytes: 0,
            max_outstanding_calls: 1,
        });
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_budget(
                crate::PoolConfig::primary_for_budget(8 * 1024 * 1024),
                budget.clone(),
            )
            .unwrap(),
        ));
        pool.write().set_idle_backing_retention(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            SealedShmBlock::allocate_and_fill(Arc::clone(&pool), 64 * 1024, |bytes| {
                bytes[0] = 42;
                panic!("aborted producer");
            })
        }));
        assert!(result.is_err(), "expected producer panic, got {result:?}");
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
    }
}
