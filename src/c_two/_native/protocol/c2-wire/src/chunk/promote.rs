//! FileSpill → SHM promotion.

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;

/// Try to move a FileSpill handle into SHM.
/// Returns `Ok(shm_handle)` on success, `Err(original_handle)` if SHM is
/// unavailable or the input is not a FileSpill.
pub fn promote_to_shm(
    pool: &mut MemPool,
    file_handle: MemHandle,
) -> Result<MemHandle, MemHandle> {
    if !file_handle.is_file_spill() {
        return Err(file_handle);
    }
    let len = file_handle.len();

    // Phase 1: Allocate SHM destination.
    let mut shm_handle = match pool.try_alloc_shm(len) {
        Ok(h) => h,
        Err(_) => return Err(file_handle),
    };

    // Phase 2: Copy data via temp buffer (.to_vec() avoids borrow issues).
    let data = pool.handle_slice(&file_handle)[..len].to_vec();
    pool.handle_slice_mut(&mut shm_handle)[..len].copy_from_slice(&data);

    // Phase 3: Set length and release old handle.
    shm_handle.set_len(len);
    pool.release_handle(file_handle);
    Ok(shm_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;
    use c2_mem::spill::create_file_spill;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn test_pool(cfg: PoolConfig) -> MemPool {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("/cc3p{:04x}{:04x}", std::process::id() as u16, id);
        MemPool::new_with_prefix(cfg, prefix)
    }

    #[test]
    fn promote_file_spill_to_shm() {
        let cfg = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            ..Default::default()
        };
        let mut pool = test_pool(cfg.clone());

        let size = 800;
        let (mut mmap, path) =
            create_file_spill(size, &cfg.spill_dir).expect("create_file_spill");

        // Write known pattern into the mmap.
        for (i, byte) in mmap[..size].iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        let file_handle = MemHandle::FileSpill { mmap, path, len: size };
        assert!(file_handle.is_file_spill());

        let shm_handle = promote_to_shm(&mut pool, file_handle)
            .expect("promote should succeed");

        // Result must NOT be FileSpill.
        assert!(!shm_handle.is_file_spill(), "promoted handle should be SHM");
        // Length must match.
        assert_eq!(shm_handle.len(), size);
        // Data integrity check.
        let data = pool.handle_slice(&shm_handle);
        for i in 0..size {
            assert_eq!(data[i], (i % 251) as u8, "data mismatch at byte {i}");
        }

        pool.release_handle(shm_handle);
    }

    #[test]
    fn promote_returns_original_when_shm_full() {
        // Use a tiny pool so SHM fills up immediately.
        let cfg = PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 0,
            ..Default::default()
        };
        let mut pool = test_pool(cfg.clone());

        // Fill the single 8 KiB segment.
        let fill = pool.try_alloc_shm(8192).expect("fill alloc");

        // Create a FileSpill handle to attempt promotion.
        let size = 512;
        let (mut mmap, path) =
            create_file_spill(size, &cfg.spill_dir).expect("create_file_spill");
        for (i, byte) in mmap[..size].iter_mut().enumerate() {
            *byte = (i % 199) as u8;
        }
        let file_handle = MemHandle::FileSpill { mmap, path, len: size };

        // Promotion should fail — SHM is full.
        let result = promote_to_shm(&mut pool, file_handle);
        assert!(result.is_err(), "should fail when SHM is full");

        let returned = result.unwrap_err();
        // Returned handle must still be FileSpill with correct length.
        assert!(returned.is_file_spill(), "returned handle should be FileSpill");
        assert_eq!(returned.len(), size);
        // Data must be intact.
        let data = pool.handle_slice(&returned);
        for i in 0..size {
            assert_eq!(data[i], (i % 199) as u8, "data mismatch at byte {i}");
        }

        pool.release_handle(returned);
        pool.release_handle(fill);
    }
}
