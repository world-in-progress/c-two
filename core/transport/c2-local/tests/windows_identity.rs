#![cfg(windows)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::time::Duration;

use c2_local::{LocalEndpoint, LocalListener, LocalStream, OwnerBootstrap, connect_windows_pipe};
use c2_local_security::{
    LocalSecurityAttributes, current_user_sid, verify_current_user_pipe_owner,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};
use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::GetProcessId;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn endpoint() -> LocalEndpoint {
    LocalEndpoint::from_address(&format!(
        "ipc://windows-identity-{}",
        uuid::Uuid::new_v4().simple()
    ))
    .unwrap()
}

#[test]
fn concurrent_listener_startup_preserves_native_identity_setup() {
    const CHILD: &str = "C2_CONCURRENT_IDENTITY_CHILD";
    // Each process starts with its original process/token ACLs. Rebinding
    // repeatedly in an already initialized process misses the first-write race.
    if std::env::var_os(CHILD).is_none() {
        for _ in 0..32 {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "concurrent_listener_startup_preserves_native_identity_setup",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        return;
    }
    let barrier = std::sync::Barrier::new(16);
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let barrier = &barrier;
            scope.spawn(move || {
                let runtime = runtime();
                let _entered = runtime.enter();
                barrier.wait();
                let listener = LocalListener::bind(&endpoint()).unwrap();
                assert!(matches!(
                    listener.close(),
                    c2_local::EndpointReapResult::NotApplicable
                ));
            });
        }
    });
}

fn raw_server(endpoint: &LocalEndpoint, attributes: *mut SECURITY_ATTRIBUTES) -> OwnedHandle {
    let name: Vec<_> = endpoint.os_name().encode_wide().chain(Some(0)).collect();
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            255,
            4096,
            4096,
            0,
            attributes,
        )
    };
    assert!(
        raw != INVALID_HANDLE_VALUE && !raw.is_null(),
        "{}",
        io::Error::last_os_error()
    );
    // SAFETY: this fixture owns the new, unregistered overlapped pipe.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

fn client_impersonation_level(server: &NamedPipeServer) -> io::Result<i32> {
    use windows_sys::Win32::Security::{
        GetTokenInformation, RevertToSelf, TOKEN_QUERY, TokenImpersonationLevel,
    };
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
    if unsafe { ImpersonateNamedPipeClient(server.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // No awaits or assertions while impersonating. Always restore this thread
    // before returning a query error to the test harness.
    let result = (|| {
        let mut raw = std::ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut level = 0i32;
        let mut bytes = 0;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenImpersonationLevel,
                (&mut level as *mut i32).cast(),
                std::mem::size_of::<i32>() as u32,
                &mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(level)
    })();
    if unsafe { RevertToSelf() } == 0 {
        return Err(io::Error::last_os_error());
    }
    result
}

#[test]
fn raw_transfer_remains_unregistered_and_moves_both_handle_owners() {
    let endpoint = endpoint();
    let mut security = LocalSecurityAttributes::new().unwrap();
    let server = raw_server(&endpoint, security.as_mut_ptr());
    let first_runtime = runtime();
    let connection = first_runtime
        .block_on(connect_windows_pipe(&endpoint, Duration::from_secs(2)))
        .unwrap();
    let (pipe, process) = connection.into_parts();
    assert_eq!(
        unsafe { GetProcessId(process.as_raw_handle()) },
        std::process::id()
    );
    for handle in [&pipe, &process] {
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(handle.as_raw_handle(), &mut flags) },
            0
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    }
    // Registration on a different IOCP after the first runtime is gone proves
    // the returned file object was not registered by the connect helper.
    drop(first_runtime);
    runtime().block_on(async move {
        let _process_pin = process;
        // SAFETY: each owned, overlapped file object is transferred exactly once.
        let mut client =
            unsafe { NamedPipeClient::from_raw_handle(pipe.into_raw_handle()) }.unwrap();
        let mut server =
            unsafe { NamedPipeServer::from_raw_handle(server.into_raw_handle()) }.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            server.connect().await.unwrap();
            client.write_all(b"raw transfer").await.unwrap();
            let mut received = [0; 12];
            server.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"raw transfer");
            assert_eq!(
                client_impersonation_level(&server).unwrap(),
                windows_sys::Win32::Security::SecurityIdentification
            );
            server.write_all(b"reply").await.unwrap();
            let mut reply = [0; 5];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"reply");
        })
        .await
        .expect("transferred handles must perform real IO on the second runtime");
    });
}

#[tokio::test]
async fn pipe_busy_obeys_one_deadline_and_an_available_pipe_is_the_control() {
    let endpoint = endpoint();
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    let occupied = connect_windows_pipe(&endpoint, Duration::from_secs(2))
        .await
        .unwrap();
    let error = connect_windows_pipe(&endpoint, Duration::from_millis(30))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    // Accepting creates the next instance, so the identical endpoint connects.
    let _accepted = listener.accept().await.unwrap();
    let _next = connect_windows_pipe(&endpoint, Duration::from_secs(2))
        .await
        .unwrap();
    drop(occupied);
}

#[test]
fn token_user_owner_is_verified_even_when_no_client_has_connected() {
    let endpoint = endpoint();
    let mut security = LocalSecurityAttributes::new().unwrap();
    let server = raw_server(&endpoint, security.as_mut_ptr());
    use std::os::windows::io::AsHandle;
    verify_current_user_pipe_owner(server.as_handle()).unwrap();
    // Bootstrap adoption is a launcher capability. It must not depend on a
    // connected/live controller or query the PID of a missing peer.
    let adopted = unsafe { OwnerBootstrap::from_inherited_handle(server.as_raw_handle()) }.unwrap();
    drop(adopted);
    let mut flags = 0;
    assert_ne!(
        unsafe { GetHandleInformation(server.as_raw_handle(), &mut flags) },
        0
    );
}

// This test creates a real foreign-owned pipe object. Ordinary user tokens
// cannot set Administrators as owner. Run explicitly on an elevated Windows
// runner; it must fail, rather than substitute a fabricated identity, if the
// runner cannot create this fixture.
#[tokio::test]
#[ignore = "requires a Windows token permitted to create Administrators-owned objects; run elevated with --ignored"]
async fn foreign_object_owner_rejects_connect_listener_join_and_bootstrap_adoption() {
    use std::os::windows::io::AsHandle;
    let sddl: Vec<_> = format!("O:BAD:P(A;;GA;;;{})", current_user_sid().unwrap())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    for mode in ["raw", "stream", "listener", "bootstrap"] {
        let endpoint = endpoint();
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let server = raw_server(&endpoint, &mut attributes);
        unsafe { LocalFree(descriptor) };
        let mismatch = verify_current_user_pipe_owner(server.as_handle()).unwrap_err();
        assert_eq!(mismatch.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(mismatch.raw_os_error(), None);
        let error = match mode {
            "raw" => connect_windows_pipe(&endpoint, Duration::from_secs(2))
                .await
                .err()
                .unwrap(),
            "stream" => LocalStream::connect(&endpoint, Duration::from_secs(2))
                .await
                .err()
                .unwrap(),
            "listener" => LocalListener::bind(&endpoint).err().unwrap(),
            "bootstrap" => unsafe { OwnerBootstrap::from_inherited_handle(server.as_raw_handle()) }
                .err()
                .unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "{mode}: {error}"
        );
        assert_eq!(
            error.raw_os_error(),
            None,
            "{mode}: policy refusal has no OS cause"
        );
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(server.as_raw_handle(), &mut flags) },
            0
        );
    }
}
