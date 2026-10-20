//! Unique allocation owner for buddy, dedicated, and file-spill memory.

use crate::{PoolAllocation, spill::SpillMapping};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Local identity, independent of probabilistic OS backing names. Never reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PoolIncarnation(u64);

impl PoolIncarnation {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(
            NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .expect("pool identity space exhausted"),
        )
    }
}

/// Move-only allocation authority. Coordinates can be inspected but cannot
/// reconstruct this owner. Read through `MemPool::handle_slice`, write through
/// `MemPool::handle_slice_mut`, and consume it with `MemPool::release_handle`.
pub struct MemHandle {
    pub(crate) backing: HandleBacking,
}

pub(crate) enum HandleBacking {
    Shm {
        allocation: PoolAllocation,
        owner: PoolIncarnation,
        len: usize,
    },
    FileSpill {
        mmap: SpillMapping,
        path: PathBuf,
        len: usize,
    },
}

impl MemHandle {
    pub fn len(&self) -> usize {
        match &self.backing {
            HandleBacking::Shm { len, .. } | HandleBacking::FileSpill { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn is_file_spill(&self) -> bool {
        matches!(self.backing, HandleBacking::FileSpill { .. })
    }
    pub fn is_buddy(&self) -> bool {
        self.shm_allocation().is_some_and(|a| !a.is_dedicated)
    }
    pub fn is_dedicated(&self) -> bool {
        self.shm_allocation().is_some_and(|a| a.is_dedicated)
    }

    /// Shrink the logical length without changing the allocation's capacity.
    pub fn trim_to(&mut self, new_len: usize) -> Result<(), String> {
        if new_len > self.len() {
            return Err(format!(
                "cannot grow handle logical length to {new_len} beyond the {} byte allocation",
                self.len()
            ));
        }
        match &mut self.backing {
            HandleBacking::Shm { len, .. } | HandleBacking::FileSpill { len, .. } => *len = new_len,
        }
        Ok(())
    }

    /// Original allocation metadata, without read, write, or release authority.
    pub fn shm_allocation(&self) -> Option<PoolAllocation> {
        match &self.backing {
            HandleBacking::Shm { allocation, .. } => Some(*allocation),
            HandleBacking::FileSpill { .. } => None,
        }
    }

    pub fn file_spill_path(&self) -> Option<&Path> {
        match &self.backing {
            HandleBacking::FileSpill { path, .. } => Some(path),
            HandleBacking::Shm { .. } => None,
        }
    }
}

impl std::fmt::Debug for MemHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.backing {
            HandleBacking::Shm {
                allocation: a, len, ..
            } if !a.is_dedicated => write!(
                f,
                "MemHandle::Buddy(seg={}, gen={}, allocation={}, off={}, capacity={}, len={len})",
                a.seg_idx, a.generation, a.allocation_id, a.offset, a.actual_size
            ),
            HandleBacking::Shm {
                allocation: a, len, ..
            } => write!(f, "MemHandle::Dedicated(seg={}, len={len})", a.seg_idx),
            HandleBacking::FileSpill { path, len, .. } => write!(
                f,
                "MemHandle::FileSpill(path={}, len={len})",
                path.display()
            ),
        }
    }
}
