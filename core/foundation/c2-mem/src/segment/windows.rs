//! Session-local named mappings with TokenUser-scoped names and ACLs. The handle and view share one RAII owner.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use c2_local_security::{LocalSecurityAttributes, current_scope_id};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEM_COMMIT, MEM_MAPPED,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
    PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};

struct MappedView {
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

impl Drop for MappedView {
    fn drop(&mut self) {
        // SAFETY: the view is owned here and is unmapped exactly once.
        unsafe { UnmapViewOfFile(self.address) };
    }
}

/// One named shared region. The owner has a read/write view; peers have a
/// read-only view.
///
/// The logical name remains portable. Its OS name is encoded inside the
/// current TokenUser scope; the DACL permits only that user SID. Windows removes
/// the mapping after the last handle and view close, including on process exit.
pub struct ShmRegion {
    name: String,
    // Field order is intentional: unmap before releasing the mapping handle.
    view: MappedView,
    _mapping: OwnedHandle,
    is_owner: bool,
}

// The region owns its view, not access to its bytes. Callers synchronize all
// shared access through the allocator protocol, as with the Unix backend.
unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// Exclusively create a committed mapping. Existing objects are rejected
    /// before any allocator metadata can be initialized.
    pub fn create(name: &str, size: usize) -> Result<Self, String> {
        validate_size(size)?;
        let os_name = mapping_name(name)?;
        let mut security = LocalSecurityAttributes::new().map_err(|error| error.to_string())?;
        let size64 = size as u64;
        // Source: CreateFileMappingW documents both ERROR_ALREADY_EXISTS and
        // default SEC_COMMIT for paging-file-backed mappings.
        // https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-createfilemappingw
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                security.as_mut_ptr(),
                PAGE_READWRITE,
                (size64 >> 32) as u32,
                size64 as u32,
                os_name.as_ptr(),
            )
        };
        let error = unsafe { GetLastError() };
        if raw.is_null() {
            return Err(format!(
                "CreateFileMappingW failed: {}",
                std::io::Error::from_raw_os_error(error as i32)
            ));
        }
        // SAFETY: the API returned a newly owned, non-null handle.
        let mapping = unsafe { OwnedHandle::from_raw_handle(raw) };
        if error == ERROR_ALREADY_EXISTS {
            return Err("shared memory mapping already exists".to_string());
        }
        let mut view = map_full_view(&mapping, size, true)?;
        // Creators know the exact requested backing size. Openers report the
        // committed view capacity, which Windows rounds to a page boundary.
        view.size = size;
        Ok(Self {
            name: name.to_string(),
            view,
            _mapping: mapping,
            is_owner: true,
        })
    }

    /// Map the entire backing read-only. `expected_size` is a minimum
    /// capacity, not the length to map.
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String> {
        validate_size(expected_size)?;
        let os_name = mapping_name(name)?;
        let raw = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, os_name.as_ptr()) };
        if raw.is_null() {
            return Err(format!(
                "OpenFileMappingW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: OpenFileMappingW returned a newly owned handle.
        let mapping = unsafe { OwnedHandle::from_raw_handle(raw) };
        let view = map_full_view(&mapping, expected_size, false)?;
        Ok(Self {
            name: name.to_string(),
            view,
            _mapping: mapping,
            is_owner: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Raw mapping address. Only the owner may write through this pointer;
    /// `open()` returns an OS-enforced read-only view.
    pub fn base_ptr(&self) -> *mut u8 {
        self.view.address.Value.cast()
    }

    pub fn size(&self) -> usize {
        self.view.size
    }

    pub fn is_owner(&self) -> bool {
        self.is_owner
    }
}

fn validate_size(size: usize) -> Result<(), String> {
    if size == 0 || size > isize::MAX as usize {
        return Err("shared memory size must be between 1 and isize::MAX".to_string());
    }
    Ok(())
}

fn mapping_name(name: &str) -> Result<Vec<u16>, String> {
    if name.is_empty() || name.len() > 64 || name.as_bytes().contains(&0) {
        return Err("shared memory name must contain 1 to 64 bytes and no NUL".to_string());
    }
    let scope = current_scope_id().map_err(|error| error.to_string())?;
    // Hex encoding is injective and avoids backslashes/case folding without
    // allowing callers to select the Global kernel namespace. Cross-session
    // pipes must negotiate SHM off before any Local mapping is advertised.
    let mut encoded = String::with_capacity(name.len() * 2);
    use std::fmt::Write;
    for byte in name.as_bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("Local\\c_two_mem_{scope}_{encoded}")
        .encode_utf16()
        .chain(Some(0))
        .collect())
}

fn map_full_view(
    mapping: &OwnedHandle,
    expected_size: usize,
    writable: bool,
) -> Result<MappedView, String> {
    // A zero length maps the complete object.
    // https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-mapviewoffile
    let address = unsafe {
        MapViewOfFile(
            mapping.as_raw_handle(),
            if writable {
                FILE_MAP_READ | FILE_MAP_WRITE
            } else {
                FILE_MAP_READ
            },
            0,
            0,
            0,
        )
    };
    if address.Value.is_null() {
        return Err(format!(
            "MapViewOfFile failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut view = MappedView { address, size: 0 };
    view.size = committed_view_size(address.Value, writable)?;
    if view.size < expected_size {
        return Err(format!(
            "SHM region too small: actual {} < expected {}",
            view.size, expected_size
        ));
    }
    Ok(view)
}

fn committed_view_size(base: *mut c_void, writable: bool) -> Result<usize, String> {
    let base_address = base as usize;
    let mut cursor = base_address;
    loop {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            VirtualQuery(
                cursor as *const c_void,
                &mut info,
                std::mem::size_of_val(&info),
            )
        };
        if queried == 0 {
            return Err(format!(
                "VirtualQuery failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if info.AllocationBase != base {
            break;
        }
        // VirtualQuery splits allocations when page state/protection differ.
        // Refuse reserved, guarded, copy-on-write or inaccessible subregions.
        // https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualquery
        if info.BaseAddress as usize != cursor
            || info.RegionSize == 0
            || info.State != MEM_COMMIT
            || info.Type != MEM_MAPPED
            || info.Protect
                != if writable {
                    PAGE_READWRITE
                } else {
                    PAGE_READONLY
                }
        {
            return Err("shared memory view has unexpected protection or state".to_string());
        }
        cursor = cursor
            .checked_add(info.RegionSize)
            .ok_or_else(|| "shared memory view address overflow".to_string())?;
        if cursor - base_address > isize::MAX as usize {
            return Err("shared memory view exceeds isize::MAX".to_string());
        }
    }
    let size = cursor - base_address;
    validate_size(size)?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

    fn name() -> String {
        format!(
            "/c2win_{}_{}",
            std::process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn mapping_uses_token_user_scope_in_session_local_namespace() {
        let logical = name();
        let wide = mapping_name(&logical).unwrap();
        let os_name = String::from_utf16(&wide[..wide.len() - 1]).unwrap();
        let scope = c2_local_security::current_scope_id().unwrap();
        assert!(os_name.starts_with(&format!(r"Local\c_two_mem_{scope}_")));
        assert!(!os_name.starts_with(r"Global\"));
        let owner = ShmRegion::create(&logical, 4096).unwrap();
        let peer = ShmRegion::open(&logical, 4096).unwrap();
        unsafe { *owner.base_ptr() = 0x39 };
        assert_eq!(unsafe { *peer.base_ptr() }, 0x39);
    }

    #[test]
    fn duplicate_create_preserves_existing_mapping() {
        let name = name();
        let owner = ShmRegion::create(&name, 4096).unwrap();
        unsafe { *owner.base_ptr() = 0x42 };
        assert!(ShmRegion::create(&name, 8192).is_err());
        let reader = ShmRegion::open(&name, 4096).unwrap();
        assert_eq!(unsafe { *reader.base_ptr() }, 0x42);
    }

    #[test]
    fn full_view_includes_bytes_beyond_advertised_data_capacity() {
        let name = name();
        let owner = ShmRegion::create(&name, 3 * 4096).unwrap();
        unsafe { *owner.base_ptr().add(2 * 4096) = 0x63 };
        let reader = ShmRegion::open(&name, 4096).unwrap();
        assert!(reader.size() >= 3 * 4096);
        assert_eq!(unsafe { *reader.base_ptr().add(2 * 4096) }, 0x63);
        assert!(ShmRegion::open(&name, 4 * 4096).is_err());
    }

    #[test]
    fn peer_keeps_backing_alive_until_last_close() {
        let name = name();
        let owner = ShmRegion::create(&name, 4096).unwrap();
        unsafe { *owner.base_ptr() = 0x77 };
        let reader = ShmRegion::open(&name, 4096).unwrap();
        drop(owner);
        assert!(ShmRegion::create(&name, 4096).is_err());
        assert_eq!(unsafe { *reader.base_ptr() }, 0x77);
        drop(reader);
        assert!(ShmRegion::open(&name, 4096).is_err());
        assert!(ShmRegion::create(&name, 4096).is_ok());
    }

    #[test]
    fn owner_and_peer_views_have_os_enforced_protection() {
        let logical = name();
        let owner = ShmRegion::create(&logical, 4096).unwrap();
        let peer = ShmRegion::open(&logical, 4096).unwrap();

        for (region, expected_protection) in [(&owner, PAGE_READWRITE), (&peer, PAGE_READONLY)] {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let queried = unsafe {
                VirtualQuery(
                    region.base_ptr().cast(),
                    &mut info,
                    std::mem::size_of_val(&info),
                )
            };
            assert_eq!(queried, std::mem::size_of_val(&info));
            assert_eq!(info.State, MEM_COMMIT);
            assert_eq!(info.Type, MEM_MAPPED);
            assert_eq!(info.Protect, expected_protection);
        }
    }

    #[test]
    fn invalid_size_and_missing_objects_fail() {
        assert!(ShmRegion::create(&name(), 0).is_err());
        assert!(ShmRegion::open(&name(), 4096).is_err());
    }
}
