//! Minimal POSIX shared memory helpers for reading buddy-allocated data.
//!
//! The relay only needs to **read** from server-side SHM segments (for
//! buddy-allocated reply payloads). It does **not** need to allocate or
//! manage buddy blocks — all allocation is done by the Python ServerV2.

use std::collections::HashMap;
use std::os::unix::io::RawFd;

/// Error type for SHM operations.
#[derive(Debug)]
pub enum ShmError {
    Open(String),
    Mmap(String),
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(msg) => write!(f, "shm_open: {msg}"),
            Self::Mmap(msg) => write!(f, "mmap: {msg}"),
        }
    }
}

impl std::error::Error for ShmError {}

/// A mapped SHM segment — provides read access to buddy-allocated data.
pub struct MappedSegment {
    ptr: *mut u8,
    size: usize,
    #[allow(dead_code)]
    fd: RawFd,
}

// SAFETY: The mapped memory is shared between processes but we only read
// from it, and access is coordinated by the IPC protocol.
unsafe impl Send for MappedSegment {}
unsafe impl Sync for MappedSegment {}

impl MappedSegment {
    /// Open and mmap an existing POSIX SHM segment by name.
    pub fn open(name: &str, size: usize) -> Result<Self, ShmError> {
        use std::ffi::CString;

        let c_name = CString::new(name).map_err(|e| ShmError::Open(e.to_string()))?;

        // shm_open with O_RDONLY
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return Err(ShmError::Open(format!(
                "shm_open({name:?}) failed: {}",
                std::io::Error::last_os_error(),
            )));
        }

        // mmap read-only
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            return Err(ShmError::Mmap(format!(
                "mmap({name:?}, size={size}) failed: {}",
                std::io::Error::last_os_error(),
            )));
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            size,
            fd,
        })
    }

    /// Read `len` bytes from `offset` within this segment.
    ///
    /// # Panics
    ///
    /// Panics if `offset + len > self.size`.
    pub fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        assert!(
            offset + len <= self.size,
            "SHM read out of bounds: offset={offset} + len={len} > size={}",
            self.size,
        );
        let mut buf = vec![0u8; len];
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(offset), buf.as_mut_ptr(), len);
        }
        buf
    }

    /// Read `len` bytes from `offset` into an existing buffer.
    pub fn read_into(&self, offset: usize, buf: &mut [u8]) {
        let len = buf.len();
        assert!(
            offset + len <= self.size,
            "SHM read out of bounds: offset={offset} + len={len} > size={}",
            self.size,
        );
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(offset), buf.as_mut_ptr(), len);
        }
    }
}

impl Drop for MappedSegment {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.size);
                libc::close(self.fd);
            }
        }
    }
}

/// Cache of opened SHM segments, indexed by segment index.
pub struct SegmentCache {
    segments: HashMap<u16, MappedSegment>,
}

impl SegmentCache {
    pub fn new() -> Self {
        Self {
            segments: HashMap::new(),
        }
    }

    /// Open and cache a segment. No-op if already cached.
    pub fn open(&mut self, seg_idx: u16, name: &str, size: usize) -> Result<(), ShmError> {
        if !self.segments.contains_key(&seg_idx) {
            let seg = MappedSegment::open(name, size)?;
            self.segments.insert(seg_idx, seg);
        }
        Ok(())
    }

    /// Read from a cached segment.
    pub fn read(&self, seg_idx: u16, offset: usize, len: usize) -> Option<Vec<u8>> {
        self.segments.get(&seg_idx).map(|seg| seg.read(offset, len))
    }
}

impl Default for SegmentCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod shm_tests {
    use super::*;

    #[test]
    fn segment_cache_basic() {
        let cache = SegmentCache::new();
        // Reading from non-existent segment returns None.
        assert!(cache.read(0, 0, 1).is_none());
    }
}
