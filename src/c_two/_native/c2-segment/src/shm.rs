//! POSIX shared memory region lifecycle.
//!
//! Provides `ShmRegion` — a raw memory-mapped POSIX SHM region
//! with no allocator attached.  The owner creates and unlinks;
//! non-owners open and detach on drop.

use std::ffi::CString;

/// A raw POSIX SHM region.
///
/// Owner process: `create()` → shm_open + ftruncate + mmap.
/// Remote process: `open()` → shm_open + mmap.
/// Drop: munmap, and shm_unlink if owner.
pub struct ShmRegion {
    name: String,
    base: *mut u8,
    size: usize,
    is_owner: bool,
}

unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// Create a new SHM region of exactly `size` bytes.
    ///
    /// The caller is responsible for any size rounding (e.g. page-align).
    pub fn create(name: &str, size: usize) -> Result<Self, String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;

        unsafe {
            // Clean up stale segment if it exists.
            libc::shm_unlink(c_name.as_ptr());

            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
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

            Ok(Self {
                name: name.to_string(),
                base: ptr as *mut u8,
                size,
                is_owner: true,
            })
        }
    }

    /// Open an existing SHM region created by another process.
    ///
    /// Validates that the actual SHM file is at least `expected_size` bytes
    /// to prevent SIGBUS on access.
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

            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("fstat failed: {err}"));
            }
            let actual_size = stat.st_size as usize;

            if actual_size < expected_size {
                libc::close(fd);
                return Err(format!(
                    "SHM region too small: actual {} < expected {}",
                    actual_size, expected_size
                ));
            }
            let map_size = actual_size;

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

            Ok(Self {
                name: name.to_string(),
                base: ptr as *mut u8,
                size: map_size,
                is_owner: false,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn is_owner(&self) -> bool {
        self.is_owner
    }

    /// # Safety
    /// The caller must ensure the full range is within the mapped region.
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base, self.size) }
    }

    /// # Safety
    /// The caller must ensure exclusive access and valid range.
    pub unsafe fn as_slice_mut(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base, self.size) }
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.size);
            if self.is_owner {
                if let Ok(c_name) = CString::new(self.name.as_str()) {
                    libc::shm_unlink(c_name.as_ptr());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_open() {
        let name = format!("/c2test_{}", std::process::id());
        let size = 4096;

        let owner = ShmRegion::create(&name, size).unwrap();
        assert_eq!(owner.size(), size);
        assert!(owner.is_owner());

        // Write some data
        unsafe {
            *owner.base_ptr() = 0x42;
        }

        let reader = ShmRegion::open(&name, size).unwrap();
        assert!(!reader.is_owner());
        assert!(reader.size() >= size);

        // Read back
        unsafe {
            assert_eq!(*reader.base_ptr(), 0x42);
        }

        drop(reader);
        drop(owner); // unlinks
    }

    #[test]
    fn test_open_nonexistent_fails() {
        let result = ShmRegion::open("/c2test_nonexistent_12345", 4096);
        assert!(result.is_err());
    }
}
