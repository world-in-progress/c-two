//! Unique allocation owner for buddy, dedicated, and file-spill memory.

use crate::{PoolAllocation, spill::SpillMapping};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Local identity, independent of probabilistic OS backing names. Never reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PoolIncarnation(u64);

impl PoolIncarnation {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(
            NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .expect("pool identity space exhausted"),
        )
    }
}

/// A local allocation whose mapping, range and release authority were fixed
/// at pool admission. Scoped borrows need no pool lock. Unlike a transport
/// handle, this owner is not published to a peer or a connection loan book.
pub struct OwnedAllocation {
    pub(crate) backing: OwnedBacking,
    pub(crate) len: usize,
}

pub(crate) enum OwnedBacking {
    Buddy {
        segment: Arc<crate::BuddySegment>,
        allocation: PoolAllocation,
        mapping: Arc<crate::segment::ChargedMapping>,
    },
    Dedicated {
        segment: Arc<crate::DedicatedSegment>,
        allocation: PoolAllocation,
        mapping: Arc<crate::segment::ChargedMapping>,
    },
    File {
        mmap: SpillMapping,
        path: PathBuf,
    },
}

impl OwnedAllocation {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn is_buddy(&self) -> bool {
        matches!(self.backing, OwnedBacking::Buddy { .. })
    }
    pub fn is_dedicated(&self) -> bool {
        matches!(self.backing, OwnedBacking::Dedicated { .. })
    }
    pub fn is_file_spill(&self) -> bool {
        matches!(self.backing, OwnedBacking::File { .. })
    }

    pub fn shm_allocation(&self) -> Option<PoolAllocation> {
        match &self.backing {
            OwnedBacking::Buddy { allocation, .. } | OwnedBacking::Dedicated { allocation, .. } => {
                Some(*allocation)
            }
            OwnedBacking::File { .. } => None,
        }
    }

    pub fn file_spill_path(&self) -> Option<&Path> {
        match &self.backing {
            OwnedBacking::File { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn trim_to(&mut self, len: usize) -> Result<(), String> {
        if len > self.len {
            return Err("cannot grow owned allocation".into());
        }
        self.len = len;
        Ok(())
    }

    /// The private admission boundary checked the span against this exact
    /// mapping. This owner's borrow pins it and excludes release or mutation.
    pub fn as_slice(&self) -> &[u8] {
        let pointer = match &self.backing {
            OwnedBacking::Buddy {
                mapping,
                allocation,
                ..
            } => mapping
                .region
                .base_ptr()
                .wrapping_add(allocation.offset as usize),
            OwnedBacking::Dedicated { mapping, .. } => mapping.region.base_ptr(),
            OwnedBacking::File { mmap, .. } => return &mmap[..self.len],
        };
        // SAFETY: the move-only owner pins the checked mapping and allocation;
        // its logical length can only shrink. No pointer escapes this borrow.
        unsafe { std::slice::from_raw_parts(pointer, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let pointer = match &mut self.backing {
            OwnedBacking::Buddy {
                mapping,
                allocation,
                ..
            } => mapping
                .region
                .base_ptr()
                .wrapping_add(allocation.offset as usize),
            OwnedBacking::Dedicated { mapping, .. } => mapping.region.base_ptr(),
            OwnedBacking::File { mmap, .. } => return &mut mmap[..self.len],
        };
        // SAFETY: admission consumed the unique allocation authority. The
        // exclusive owner borrow excludes all safe readers and other writers.
        unsafe { std::slice::from_raw_parts_mut(pointer, self.len) }
    }
}

impl Drop for OwnedAllocation {
    fn drop(&mut self) {
        match &self.backing {
            OwnedBacking::Buddy {
                segment,
                allocation,
                ..
            } => {
                // SAFETY: this owner is the only release authority and all
                // scoped payload borrows ended before its destructor.
                unsafe { segment.free(allocation.offset, allocation.level) };
            }
            OwnedBacking::Dedicated { segment, .. } => segment.release(),
            OwnedBacking::File { .. } => {}
        }
        // Mapping pins (and their creation charges) drop after local release.
    }
}

impl std::fmt::Debug for OwnedAllocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedAllocation")
            .field("allocation", &self.shm_allocation())
            .field("len", &self.len)
            .field("file", &self.file_spill_path())
            .finish()
    }
}

/// Move-only allocation authority. Coordinates can be inspected but cannot
/// reconstruct this owner. Read through `MemPool::handle_slice`, write through
/// `MemPool::handle_slice_mut`, and consume it with `MemPool::release_handle`.
/// Retain the original pool alongside every SHM handle, including entries in
/// a connection's loan book, until it is released. The handle does not keep
/// that pool alive by itself.
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
                "MemHandle::Buddy(seg={}, gen={}, off={}, capacity={}, len={len})",
                a.seg_idx, a.generation, a.offset, a.actual_size
            ),
            HandleBacking::Shm {
                allocation: a, len, ..
            } => write!(
                f,
                "MemHandle::Dedicated(seg={}, gen={}, len={len})",
                a.seg_idx, a.generation
            ),
            HandleBacking::FileSpill { path, len, .. } => write!(
                f,
                "MemHandle::FileSpill(path={}, len={len})",
                path.display()
            ),
        }
    }
}
