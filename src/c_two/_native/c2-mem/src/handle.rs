//! Unified memory handle abstracting buddy, dedicated, and file-spill backends.

use std::path::PathBuf;
use memmap2::MmapMut;

/// A handle to an allocated memory region.
///
/// The caller must NOT access the data directly through the handle for
/// Buddy/Dedicated variants — use [`MemPool::handle_slice`] and
/// [`MemPool::handle_slice_mut`] instead.  FileSpill is self-contained.
///
/// Release via [`MemPool::release_handle`].
pub enum MemHandle {
    /// T1/T2: allocation within a buddy segment.
    Buddy {
        seg_idx: u16,
        offset: u32,
        len: usize,
    },
    /// T3: dedicated SHM segment.
    Dedicated {
        seg_idx: u16,
        len: usize,
    },
    /// T4: file-backed mmap (disk spill).
    FileSpill {
        mmap: MmapMut,
        path: PathBuf,
        len: usize,
    },
}

impl MemHandle {
    pub fn len(&self) -> usize {
        match self {
            Self::Buddy { len, .. } => *len,
            Self::Dedicated { len, .. } => *len,
            Self::FileSpill { len, .. } => *len,
        }
    }

    pub fn is_file_spill(&self) -> bool {
        matches!(self, Self::FileSpill { .. })
    }

    pub fn is_buddy(&self) -> bool {
        matches!(self, Self::Buddy { .. })
    }

    pub fn is_dedicated(&self) -> bool {
        matches!(self, Self::Dedicated { .. })
    }

    /// Shrink the logical length of this handle.
    /// Used by ChunkAssembler to trim to actual received data.
    pub fn set_len(&mut self, new_len: usize) {
        match self {
            MemHandle::Buddy { len, .. } => *len = new_len,
            MemHandle::Dedicated { len, .. } => *len = new_len,
            MemHandle::FileSpill { len, .. } => *len = new_len,
        }
    }
}

impl std::fmt::Debug for MemHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buddy { seg_idx, offset, len } => {
                write!(f, "MemHandle::Buddy(seg={seg_idx}, off={offset}, len={len})")
            }
            Self::Dedicated { seg_idx, len } => {
                write!(f, "MemHandle::Dedicated(seg={seg_idx}, len={len})")
            }
            Self::FileSpill { path, len, .. } => {
                write!(f, "MemHandle::FileSpill(path={}, len={len})", path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buddy_handle_len() {
        let h = MemHandle::Buddy { seg_idx: 0, offset: 1024, len: 4096 };
        assert_eq!(h.len(), 4096);
        assert!(h.is_buddy());
        assert!(!h.is_file_spill());
    }

    #[test]
    fn test_dedicated_handle_len() {
        let h = MemHandle::Dedicated { seg_idx: 8, len: 1_000_000 };
        assert_eq!(h.len(), 1_000_000);
        assert!(h.is_dedicated());
    }

    #[test]
    fn test_file_spill_handle() {
        let dir = std::env::temp_dir().join("c2_handle_test");
        let (mmap, path) = crate::spill::create_file_spill(4096, &dir).unwrap();
        let h = MemHandle::FileSpill { mmap, path, len: 4096 };
        assert_eq!(h.len(), 4096);
        assert!(h.is_file_spill());
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("FileSpill"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
