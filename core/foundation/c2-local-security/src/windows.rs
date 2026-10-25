use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::fmt::Write as _;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, SE_KERNEL_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID,
    TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSidSubAuthority, GetTokenInformation, NO_INHERITANCE,
    OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenIntegrityLevel, TokenSessionId, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
use windows_sys::Win32::System::SystemServices::SECURITY_MANDATORY_MEDIUM_RID;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

fn current_process() -> BorrowedHandle<'static> {
    // SAFETY: the current-process pseudo-handle is valid for this process's lifetime.
    unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) }
}

fn process_token(process: BorrowedHandle<'_>) -> io::Result<OwnedHandle> {
    let mut token = ptr::null_mut();
    // OpenProcessToken requires PROCESS_QUERY_LIMITED_INFORMATION on real
    // process handles and TOKEN_QUERY for the information read below.
    // https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned one owned token handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

/// Make this server's identity queryable by the same user in other logons and
/// at Medium integrity. Preserve every existing ACE; grant no process-control,
/// token-duplication or impersonation rights. Call before publishing a listener.
pub fn grant_server_identity_query_access() -> io::Result<()> {
    // These process-lifetime grants are installed once. Concurrent first
    // listeners must not resize/read the same process and token ACLs together.
    static INITIALIZED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    let mut initialized = INITIALIZED
        .lock()
        .map_err(|_| io::Error::other("server identity setup lock poisoned"))?;
    if *initialized {
        return Ok(());
    }
    let user: Vec<_> = current_user_sid()?.encode_utf16().chain(Some(0)).collect();
    let mut sid = ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(user.as_ptr(), &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut raw_token = ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | READ_CONTROL | WRITE_DAC,
                &mut raw_token,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
        grant_query_access(token.as_handle(), sid, TOKEN_QUERY)?;
        grant_query_access(
            current_process(),
            sid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )
    })();
    unsafe { LocalFree(sid) };
    result?;
    *initialized = true;
    Ok(())
}

fn grant_query_access(object: BorrowedHandle<'_>, sid: PSID, rights: u32) -> io::Result<()> {
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            object.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let result = (|| {
        // A NULL DACL already permits these rights; replacing it would
        // unexpectedly revoke unrelated access rather than grant a read ACE.
        if dacl.is_null() {
            return Ok(());
        }
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: rights,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
            },
        };
        let mut updated = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &access, dacl, &mut updated) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let status = unsafe {
            SetSecurityInfo(
                object.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                updated,
                ptr::null_mut(),
            )
        };
        unsafe { LocalFree(updated.cast()) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    })();
    unsafe { LocalFree(descriptor) };
    result
}

/// TokenUser SID of the current process's primary token, not a logon group,
/// default object owner, or the calling thread's impersonation identity.
/// Same SID does not imply equal trust or prevent denial of service.
pub fn current_user_sid() -> io::Result<String> {
    process_user_sid(current_process())
}

/// TokenUser SID of a pinned process. The handle needs
/// PROCESS_QUERY_LIMITED_INFORMATION; query errors retain their OS cause.
fn process_user_sid(process: BorrowedHandle<'_>) -> io::Result<String> {
    let token = process_token(process)?;
    token_user_sid(token.as_handle())
}

fn token_user_sid(token: BorrowedHandle<'_>) -> io::Result<String> {
    let mut bytes = 0;
    let result = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut bytes,
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    if (bytes as usize) < size_of::<TOKEN_USER>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TokenUser is truncated",
        ));
    }
    // TOKEN_USER contains a pointer and must be pointer-aligned. Keep this
    // buffer alive until its embedded SID has been converted to owned text.
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation filled a suitably aligned TOKEN_USER buffer.
    unsafe { sid_to_string((*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// Authenticate a local server's primary token. Medium and High may connect
/// across logons; a sandboxed Low/Untrusted process may not impersonate either.
pub fn verify_server_process_identity(process: BorrowedHandle<'_>) -> io::Result<()> {
    let token = process_token(process)?;
    if token_user_sid(token.as_handle())? != current_user_sid()? {
        return Err(crate::peer_identity_mismatch(
            "named pipe server TokenUser does not match the current user",
        ));
    }
    // TokenIntegrityLevel returns TOKEN_MANDATORY_LABEL followed by its SID.
    // The OS label SID is S-1-16-<integrity RID>; one aligned stack buffer fits
    // the header and every Windows SID, with no second token query or cache.
    const WORDS: usize = (size_of::<TOKEN_MANDATORY_LABEL>() + SECURITY_MAX_SID_SIZE as usize)
        .div_ceil(size_of::<usize>());
    let mut storage = [0usize; WORDS];
    let mut bytes = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size_of_val(&storage) as u32,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let level = unsafe {
        let label = &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        *GetSidSubAuthority(label.Label.Sid, 0)
    };
    if level < SECURITY_MANDATORY_MEDIUM_RID as u32 {
        return Err(crate::peer_identity_mismatch(
            "named pipe server integrity is below Medium",
        ));
    }
    Ok(())
}

/// TokenSessionId of the current process, for session-local mapping availability.
pub fn current_session_id() -> io::Result<u32> {
    let token = process_token(current_process())?;
    let mut session = 0u32;
    let mut bytes = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenSessionId,
            (&mut session as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}

/// Verify the owner of the actual open named-pipe object against TokenUser.
/// The borrowed handle needs READ_CONTROL. A mismatch is a policy refusal,
/// without a fabricated OS error; GetSecurityInfo failures retain their code.
/// This check does not authenticate a trusted application within the same SID.
pub fn verify_current_user_pipe_owner(pipe: BorrowedHandle<'_>) -> io::Result<()> {
    let expected = current_user_sid()?;
    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // Named pipes support GetSecurityInfo. Read only the owner, never the name
    // or an unrelated instance. The returned SID lives inside descriptor.
    // https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
    let error = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let actual = if owner.is_null() {
        Err(crate::peer_identity_mismatch("named pipe has no owner SID"))
    } else {
        // SAFETY: owner is in the live descriptor allocated by GetSecurityInfo.
        unsafe { sid_to_string(owner) }
    };
    unsafe { LocalFree(descriptor) };
    if actual? != expected {
        return Err(crate::peer_identity_mismatch(
            "named pipe owner does not match the current TokenUser",
        ));
    }
    Ok(())
}

unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let value = unsafe { String::from_utf16(std::slice::from_raw_parts(text, len)) };
    unsafe { LocalFree(text.cast()) };
    value.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Stable hexadecimal identity for the current TokenUser, shared across logons.
pub fn current_scope_id() -> io::Result<String> {
    Ok(scope_id_from_sid(&current_user_sid()?))
}

fn scope_id_from_sid(sid: &str) -> String {
    let mut identity = String::with_capacity(64);
    for byte in Sha256::digest(sid.as_bytes()) {
        write!(&mut identity, "{byte:02x}").expect("writing to String cannot fail");
    }
    identity
}

/// An owned, non-inheritable security descriptor with an explicit TokenUser owner and a protected, single-user DACL.
pub struct LocalSecurityAttributes {
    attributes: SECURITY_ATTRIBUTES,
    descriptor: *mut c_void,
}

// The descriptor is immutable after construction; access to the attributes
// pointer requires an exclusive borrow and Win32 creation only reads it.
unsafe impl Send for LocalSecurityAttributes {}

impl LocalSecurityAttributes {
    pub fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
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
    fn scope_id_preserves_sha256_hex_golden_value() {
        assert_eq!(
            scope_id_from_sid("S-1-5-21-123-456-789-1001"),
            "cb8d776d80c20cafbae7c0de6735cf6ccbdb97f4eab6d9674d13189a642e968e"
        );
    }

    #[test]
    fn process_token_query_keeps_a_real_os_failure() {
        use std::os::windows::io::AsHandle;
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let error = process_user_sid(file.as_handle()).unwrap_err();
        assert!(error.raw_os_error().is_some(), "{error}");
    }

    #[test]
    fn server_identity_rejects_a_real_low_integrity_process() {
        verify_server_process_identity(current_process()).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::tests::low_integrity_process_fixture",
                "--nocapture",
            ])
            .env("C2_LOCAL_SECURITY_LOW_TOKEN_TEST", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("LOW_SERVER_REJECTED"));
    }

    #[test]
    fn low_integrity_process_fixture() {
        if std::env::var_os("C2_LOCAL_SECURITY_LOW_TOKEN_TEST").is_none() {
            return;
        }
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows_sys::Win32::Security::{
            SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
        };
        use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
        let mut raw_token = ptr::null_mut();
        assert_ne!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_DEFAULT | TOKEN_QUERY,
                    &mut raw_token,
                )
            },
            0
        );
        let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
        let low: Vec<_> = "S-1-16-4096".encode_utf16().chain(Some(0)).collect();
        let mut sid = ptr::null_mut();
        assert_ne!(unsafe { ConvertStringSidToSidW(low.as_ptr(), &mut sid) }, 0);
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        // Only this already-started child lowers its own primary token. It
        // cannot affect the test runner or another test's Windows identity.
        let changed = unsafe {
            SetTokenInformation(
                token.as_raw_handle(),
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            )
        };
        let cause = io::Error::last_os_error();
        unsafe { LocalFree(sid) };
        assert_ne!(changed, 0, "{cause}");
        let error = verify_server_process_identity(current_process()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(crate::is_peer_identity_mismatch(&error));
        assert_eq!(error.raw_os_error(), None);
        assert_eq!(
            error.to_string(),
            "named pipe server integrity is below Medium"
        );
        println!("LOW_SERVER_REJECTED");
    }

    #[test]
    fn scope_and_acl_follow_token_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-"));
        assert!(
            !sid.starts_with("S-1-5-5-"),
            "TokenUser is not a logon group"
        );
        assert_eq!(current_scope_id().unwrap(), scope_id_from_sid(&sid));
        let mut attributes = LocalSecurityAttributes::new().unwrap();
        assert_eq!(unsafe { (*attributes.as_mut_ptr()).bInheritHandle }, 0);
        let descriptor = unsafe { (*attributes.as_mut_ptr()).lpSecurityDescriptor };
        assert!(!descriptor.is_null());
        // Inspect the parsed descriptor, including the explicit owner. An
        // elevated token's default owner may be Administrators, not TokenUser.
        unsafe {
            use windows_sys::Win32::Security::{
                ACCESS_ALLOWED_ACE, GetAce, GetSecurityDescriptorControl,
                GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, SE_DACL_PROTECTED,
            };
            let mut owner = ptr::null_mut();
            let mut defaulted = 0;
            assert_ne!(
                GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted),
                0
            );
            assert_eq!(sid_to_string(owner).unwrap(), sid);
            assert_eq!(defaulted, 0);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            let mut present = 0;
            let mut acl = ptr::null_mut();
            assert_ne!(
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted),
                0
            );
            assert_ne!(present, 0);
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 1);
            let mut ace = ptr::null_mut();
            assert_ne!(GetAce(acl, 0, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
            assert_eq!(ace.Mask, 0x10000000); // GENERIC_ALL before object mapping
            assert_eq!(
                sid_to_string(ptr::addr_of!(ace.SidStart).cast_mut().cast()).unwrap(),
                sid
            );
        }
    }
}
