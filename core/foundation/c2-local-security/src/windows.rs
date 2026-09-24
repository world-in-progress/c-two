use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_GROUPS, TOKEN_QUERY, TokenGroups,
};
use windows_sys::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Current process logon SID, suitable for a session-scoped SDDL ACL.
pub fn current_logon_sid() -> io::Result<String> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut bytes = 0;
            GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut bytes);
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            // TOKEN_GROUPS contains pointers and needs pointer alignment.
            let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
            if GetTokenInformation(
                token,
                TokenGroups,
                storage.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let groups = &*storage.as_ptr().cast::<TOKEN_GROUPS>();
            for group in
                std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
            {
                if group.Attributes & SE_GROUP_LOGON_ID as u32 != SE_GROUP_LOGON_ID as u32 {
                    continue;
                }
                let mut text = ptr::null_mut();
                if ConvertSidToStringSidW(group.Sid, &mut text) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut len = 0;
                while *text.add(len) != 0 {
                    len += 1;
                }
                let value = String::from_utf16(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
                return value.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "process token has no logon SID",
            ))
        })();
        CloseHandle(token);
        result
    }
}

/// Stable hexadecimal identity for the current logon session.
pub fn current_scope_id() -> io::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(current_logon_sid()?.as_bytes())
    ))
}

/// An owned, non-inheritable security descriptor allowing only this logon SID.
pub struct LocalSecurityAttributes {
    attributes: SECURITY_ATTRIBUTES,
    descriptor: *mut c_void,
}

// The descriptor is immutable after construction; access to the attributes
// pointer requires an exclusive borrow and Win32 creation only reads it.
unsafe impl Send for LocalSecurityAttributes {}

impl LocalSecurityAttributes {
    pub fn new() -> io::Result<Self> {
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", current_logon_sid()?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = ptr::null_mut();
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
            descriptor,
        })
    }

    /// Valid only while this owner remains alive. Win32 must not retain it.
    pub fn as_mut_ptr(&mut self) -> *mut SECURITY_ATTRIBUTES {
        &mut self.attributes
    }
}

impl Drop for LocalSecurityAttributes {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_acl_follow_the_current_logon() {
        let sid = current_logon_sid().unwrap();
        assert!(sid.starts_with("S-1-5-5-"));
        assert_eq!(current_scope_id().unwrap(), current_scope_id().unwrap());
        let mut attributes = LocalSecurityAttributes::new().unwrap();
        assert_eq!(unsafe { (*attributes.as_mut_ptr()).bInheritHandle }, 0);
        assert!(!unsafe { (*attributes.as_mut_ptr()).lpSecurityDescriptor }.is_null());
    }
}
