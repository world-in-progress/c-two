//! Response payload preparation helpers.

use c2_mem::MemPool;

use crate::dispatcher::ResponseMeta;

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
    response_pool: &parking_lot::RwLock<MemPool>,
    shm_threshold: u64,
    len: usize,
    write_into: F,
) -> Result<Option<ResponseMeta>, String>
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

    let alloc = {
        let mut pool = response_pool.write();
        match pool.alloc(len) {
            Ok(alloc) => alloc,
            Err(_) => return Ok(None),
        }
    };

    if alloc.seg_idx > u16::MAX as u32 {
        let mut pool = response_pool.write();
        let _ = pool.free(&alloc);
        return Err(format!(
            "response segment index {} exceeds buddy response wire limit {}",
            alloc.seg_idx,
            u16::MAX
        ));
    }

    let write_result = {
        let pool = response_pool.read();
        let ptr = match pool.data_ptr(&alloc) {
            Ok(ptr) => ptr,
            Err(err) => {
                drop(pool);
                let mut pool = response_pool.write();
                let _ = pool.free(&alloc);
                return Err(format!("response allocation data pointer failed: {err}"));
            }
        };
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        write_into(dst)
    };

    if let Err(err) = write_result {
        let mut pool = response_pool.write();
        let _ = pool.free(&alloc);
        return Err(err);
    }

    Ok(Some(ResponseMeta::ShmAlloc {
        seg_idx: alloc.seg_idx as u16,
        generation: alloc.generation,
        offset: alloc.offset,
        data_size,
        is_dedicated: alloc.is_dedicated,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use c2_mem::{MemPool, PoolConfig};
    use parking_lot::RwLock;

    use crate::ResponseMeta;

    static TEST_POOL_ID: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix(label: &str) -> String {
        let id = TEST_POOL_ID.fetch_add(1, Ordering::Relaxed);
        format!("/c2r{:04x}{:04x}", std::process::id() as u16, id) + label
    }

    fn test_pool() -> RwLock<MemPool> {
        RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 1,
                max_dedicated_segments: 1,
                dedicated_crash_timeout_secs: 0.0,
                buddy_idle_decay_secs: 60.0,
                spill_threshold: 0.8,
                spill_dir: std::env::temp_dir().join("c2_response_helper_test"),
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

        let meta = super::try_prepare_shm_response(&pool, 1024, payload.len(), |dst| {
            dst.copy_from_slice(&payload);
            Ok(())
        })
        .unwrap()
        .expect("large payload should allocate response SHM");

        let ResponseMeta::ShmAlloc {
            seg_idx,
            generation,
            offset,
            data_size,
            is_dedicated,
        } = meta
        else {
            panic!("expected ShmAlloc response metadata");
        };

        let pool_guard = pool.read();
        let ptr = pool_guard
            .data_ptr_at(seg_idx as u32, generation, offset, is_dedicated)
            .unwrap();
        let actual = unsafe { std::slice::from_raw_parts(ptr, data_size as usize) };
        assert_eq!(actual, payload.as_slice());
    }

    #[test]
    fn write_failure_frees_allocated_response_block() {
        let pool = test_pool();

        let err = super::try_prepare_shm_response(&pool, 1024, 8192, |_dst| {
            Err("copy failed".to_string())
        })
        .expect_err("copy failure should be reported");

        assert!(err.contains("copy failed"));
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    #[test]
    fn allocation_failure_falls_back_to_owned_inline_path() {
        let pool = RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 0,
                max_dedicated_segments: 0,
                dedicated_crash_timeout_secs: 0.0,
                buddy_idle_decay_secs: 60.0,
                spill_threshold: 0.8,
                spill_dir: std::env::temp_dir().join("c2_response_helper_alloc_fail"),
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
