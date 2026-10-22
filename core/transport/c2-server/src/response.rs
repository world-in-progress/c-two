//! Response payload preparation helpers.

use c2_mem::MemPool;

use crate::dispatcher::PreparedShmResponse;

/// The server's fixed response-pool owner.
///
/// Callbacks may inspect its configuration or prepare a response through
/// [`try_prepare_shm_response`]. Safe access cannot replace the pool advertised
/// to connected peers or release a backing still owned by a prepared response.
pub struct ResponsePool {
    pub(crate) inner: parking_lot::RwLock<MemPool>,
}

impl ResponsePool {
    pub(crate) fn new(pool: MemPool) -> Self {
        Self {
            inner: parking_lot::RwLock::new(pool),
        }
    }

    #[inline]
    pub fn read(&self) -> parking_lot::MappedRwLockReadGuard<'_, MemPool> {
        // An ordinary read guard exposes RwLockReadGuard::rwlock, which would
        // let safe callers recover the mutable lock and replace this owner.
        parking_lot::RwLockReadGuard::map(self.inner.read(), |pool| pool)
    }

    /// Raw access for integrations that manage pool storage themselves.
    ///
    /// # Safety
    /// The caller must preserve the advertised pool identity and every live
    /// allocation's access/release authority. In particular it must not replace
    /// or destroy the pool, or free a backing, while the server, a callback,
    /// a prepared response or a connected peer can still use that storage.
    pub unsafe fn write(&self) -> parking_lot::RwLockWriteGuard<'_, MemPool> {
        self.inner.write()
    }
}

/// Return the buddy response wire `data_size` when the payload length is
/// representable by the current buddy payload metadata.
pub(crate) fn buddy_response_data_size(len: usize) -> Option<u32> {
    u32::try_from(len).ok()
}

/// Try to prepare a large response directly in the server response SHM pool.
///
/// Returns `Ok(None)` when the caller should materialise owned inline bytes and
/// let `send_response_meta()` choose inline or chunked fallback. Allocation
/// failure is therefore not a CRM failure. Errors are reserved for cases where
/// an allocation was made but the payload could not be copied safely.
pub fn try_prepare_shm_response<F>(
    response_pool: &ResponsePool,
    shm_threshold: u64,
    len: usize,
    write_into: F,
) -> Result<Option<PreparedShmResponse>, String>
where
    F: FnOnce(&mut [u8]) -> Result<(), String>,
{
    if len == 0 || len as u64 <= shm_threshold {
        return Ok(None);
    }
    let data_size = match buddy_response_data_size(len) {
        Some(size) => size,
        None => return Ok(None),
    };

    // Allocate the backing as the unique write owner. SHM-only: a tier
    // failure returns None so the caller falls back to owned inline bytes
    // and native transport selection.
    let mut handle = {
        let mut pool = response_pool.inner.write();
        match pool.alloc_shm_handle(len) {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        }
    };

    let coordinates = handle
        .shm_allocation()
        .expect("SHM response backing carries coordinates");
    if coordinates.seg_idx > u16::MAX as u32 {
        let mut pool = response_pool.inner.write();
        // Only this exact owner may authorize the raw release.
        if pool.validate_handle(&handle).is_ok() {
            let _ = unsafe { pool.free(&coordinates) };
        }
        return Err(format!(
            "response segment index {} exceeds buddy response wire limit {}",
            coordinates.seg_idx,
            u16::MAX
        ));
    }

    let write_result = {
        let pool = response_pool.read();
        match pool.validate_handle(&handle) {
            Ok(()) => write_into(pool.handle_slice_mut(&mut handle)),
            Err(err) => Err(format!("response allocation data pointer failed: {err}")),
        }
    };

    if let Err(err) = write_result {
        let mut pool = response_pool.inner.write();
        // Only this exact owner may authorize the raw release.
        if pool.validate_handle(&handle).is_ok() {
            let _ = unsafe { pool.free(&coordinates) };
        }
        return Err(err);
    }

    Ok(Some(PreparedShmResponse::new(handle, data_size)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::ResponsePool;
    use c2_mem::{MemPool, PoolConfig};

    use crate::ResponseMeta;

    static TEST_POOL_ID: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix(label: &str) -> String {
        let id = TEST_POOL_ID.fetch_add(1, Ordering::Relaxed);
        format!("/c2r{:04x}{:04x}", std::process::id() as u16, id) + label
    }

    fn test_pool() -> ResponsePool {
        ResponsePool::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                buddy_idle_decay_secs: 60.0,
                spill_threshold: 0.8,
                spill_dir: std::env::temp_dir().join("c2_response_helper_test"),
                ..PoolConfig::default()
            },
            unique_prefix("a"),
        ))
    }

    #[test]
    fn small_payload_stays_owned_inline_without_pool_allocation() {
        let pool = test_pool();

        let meta = super::try_prepare_shm_response(&pool, 1024, 32, |_| {
            panic!("small payload should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn large_payload_is_written_directly_into_response_pool() {
        let pool = test_pool();
        let payload = b"x".repeat(8192);

        let prepared = super::try_prepare_shm_response(&pool, 1024, payload.len(), |dst| {
            dst.copy_from_slice(&payload);
            Ok(())
        })
        .unwrap()
        .expect("large payload should allocate response SHM");

        assert_eq!(prepared.data_size() as usize, payload.len());
        let handle = prepared.into_handle();
        assert_eq!(pool.read().handle_slice(&handle), payload.as_slice());
        // The prepared owner releases cleanly through the response pool.
        pool.inner.write().release_handle(handle).unwrap();
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn write_failure_frees_allocated_response_block() {
        let pool = test_pool();

        let err = super::try_prepare_shm_response(&pool, 1024, 8193, |dst| {
            assert_eq!(dst.len(), 8193);
            Err("copy failed".to_string())
        })
        .expect_err("copy failure should be reported");

        assert!(err.contains("copy failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(pool.read().stats().buddy_occupied_bytes, 0);
    }

    #[test]
    fn allocation_failure_falls_back_to_owned_inline_path() {
        let pool = ResponsePool::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 0,
                max_dedicated_segments: 0,
                dedicated_crash_timeout_secs: 0.0,
                buddy_idle_decay_secs: 60.0,
                spill_threshold: 0.8,
                spill_dir: std::env::temp_dir().join("c2_response_helper_alloc_fail"),
                // max_segments 0 requires a zero retirement floor.
                min_retained_segments: 0,
                ..PoolConfig::default()
            },
            unique_prefix("b"),
        ));

        let meta = super::try_prepare_shm_response(&pool, 1024, 8192, |_| {
            panic!("allocation failure should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn oversized_buddy_wire_payload_falls_back_without_allocation() {
        let pool = test_pool();

        let meta = super::try_prepare_shm_response(&pool, 1024, u32::MAX as usize + 1, |_| {
            panic!("oversized buddy response should not request a SHM destination")
        })
        .unwrap();

        assert!(meta.is_none());
        assert_eq!(pool.read().stats().alloc_count, 0);
    }
}
