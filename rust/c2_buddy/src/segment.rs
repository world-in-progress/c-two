//! SHM segment lifecycle management.
//!
//! Wraps POSIX shared memory operations (shm_open, ftruncate, mmap, munmap, shm_unlink)
//! and provides safe Rust abstractions for creating, opening, and destroying SHM segments.

use crate::allocator::BuddyAllocator;
use std::ffi::CString;

/// Represents an open SHM segment with its buddy allocator.
pub struct ShmSegment {
    /// The SHM name (e.g., "/cc3b_deadbeef_01").
    name: String,
    /// Mapped memory base pointer.
    base: *mut u8,
    /// Total mapped size.
    size: usize,
    /// The buddy allocator for this segment.
    allocator: BuddyAllocator,
    /// Whether this process owns (created) the segment.
    is_owner: bool,
}

unsafe impl Send for ShmSegment {}
unsafe impl Sync for ShmSegment {}

impl ShmSegment {
    /// Create a new SHM segment and initialize its buddy allocator.
    pub fn create(name: &str, size: usize, min_block: usize) -> Result<Self, String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;

        unsafe {
            // Clean up stale segment if it exists.
            libc::shm_unlink(c_name.as_ptr());

            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            );
            if fd < 0 {
                return Err(format!(
                    "shm_open failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            if libc::ftruncate(fd, size as libc::off_t) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
                return Err(format!("ftruncate failed: {err}"));
            }

            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);

            if ptr == libc::MAP_FAILED {
                libc::shm_unlink(c_name.as_ptr());
                return Err(format!(
                    "mmap failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let base = ptr as *mut u8;
            let allocator = BuddyAllocator::init(base, size, min_block);

            Ok(Self {
                name: name.to_string(),
                base,
                size,
                allocator,
                is_owner: true,
            })
        }
    }

    /// Open an existing SHM segment (created by another process).
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;

        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                return Err(format!(
                    "shm_open failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            // Get actual size.
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("fstat failed: {err}"));
            }
            let actual_size = stat.st_size as usize;
            let map_size = actual_size.max(expected_size);

            let ptr = libc::mmap(
                std::ptr::null_mut(),
                map_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);

            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "mmap failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let base = ptr as *mut u8;
            let allocator = BuddyAllocator::attach(base, map_size)?;

            Ok(Self {
                name: name.to_string(),
                base,
                size: map_size,
                allocator,
                is_owner: false,
            })
        }
    }

    /// Get a reference to the buddy allocator.
    pub fn allocator(&self) -> &BuddyAllocator {
        &self.allocator
    }

    /// Get the SHM name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the total SHM size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the base pointer (for direct memory access).
    pub fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Whether this process owns (created) the segment.
    pub fn is_owner(&self) -> bool {
        self.is_owner
    }

    /// Get a slice of the data region at given offset and length.
    ///
    /// # Safety
    /// The caller must ensure the offset+len is within a valid allocation.
    pub unsafe fn data_slice(&self, offset: u32, len: usize) -> &[u8] {
        let ptr = self.allocator.data_ptr(offset);
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// Get a mutable slice of the data region at given offset and length.
    ///
    /// # Safety
    /// The caller must ensure exclusive access to this region.
    pub unsafe fn data_slice_mut(&self, offset: u32, len: usize) -> &mut [u8] {
        let ptr = self.allocator.data_ptr(offset);
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.size);
            if self.is_owner {
                let c_name = CString::new(self.name.as_str()).unwrap();
                libc::shm_unlink(c_name.as_ptr());
            }
        }
    }
}

/// Create a dedicated SHM segment for oversized allocations.
/// These bypass the buddy allocator — the entire segment serves one allocation.
pub struct DedicatedSegment {
    name: String,
    base: *mut u8,
    size: usize,
    is_owner: bool,
}

unsafe impl Send for DedicatedSegment {}
unsafe impl Sync for DedicatedSegment {}

impl DedicatedSegment {
    /// Create a dedicated segment for a single large allocation.
    pub fn create(name: &str, size: usize) -> Result<Self, String> {
        // Page-align the size.
        let page_size = 4096;
        let aligned_size = (size + page_size - 1) & !(page_size - 1);
        let c_name = CString::new(name).map_err(|e| e.to_string())?;

        unsafe {
            libc::shm_unlink(c_name.as_ptr());

            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            );
            if fd < 0 {
                return Err(format!(
                    "shm_open failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            if libc::ftruncate(fd, aligned_size as libc::off_t) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
                return Err(format!("ftruncate failed: {err}"));
            }

            let ptr = libc::mmap(
                std::ptr::null_mut(),
                aligned_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);

            if ptr == libc::MAP_FAILED {
                libc::shm_unlink(c_name.as_ptr());
                return Err(format!(
                    "mmap failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            Ok(Self {
                name: name.to_string(),
                base: ptr as *mut u8,
                size: aligned_size,
                is_owner: true,
            })
        }
    }

    /// Open an existing dedicated segment.
    pub fn open(name: &str, size: usize) -> Result<Self, String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;

        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0);
            if fd < 0 {
                return Err(format!(
                    "shm_open failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);

            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "mmap failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            Ok(Self {
                name: name.to_string(),
                base: ptr as *mut u8,
                size,
                is_owner: false,
            })
        }
    }

    /// Get the base data pointer.
    pub fn data_ptr(&self) -> *mut u8 {
        self.base
    }

    /// Get the total size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for DedicatedSegment {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.size);
            if self.is_owner {
                let c_name = CString::new(self.name.as_str()).unwrap();
                libc::shm_unlink(c_name.as_ptr());
            }
        }
    }
}
