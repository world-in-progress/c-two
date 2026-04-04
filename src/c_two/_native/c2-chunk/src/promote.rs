//! FileSpill → SHM promotion.
//! Full implementation in Task 5.

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;

/// Try to move a FileSpill handle into SHM.
/// Returns Ok(shm_handle) on success, Err(original_handle) if SHM unavailable.
pub fn promote_to_shm(
    _pool: &mut MemPool,
    file_handle: MemHandle,
) -> Result<MemHandle, MemHandle> {
    // Placeholder — returns original for now.
    Err(file_handle)
}
