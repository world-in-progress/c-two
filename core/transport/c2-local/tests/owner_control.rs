use std::io::{self, BufRead, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use c2_local::{OwnerControlReceiver, owner_control_pair};

const FIXTURE_ACTION: &str = "C2_LOCAL_OWNER_CONTROL_FIXTURE";

fn write_fixture_stdout(bytes: &[u8]) {
    use std::io::Write;
    io::stdout().write_all(bytes).unwrap();
    io::stdout().flush().unwrap();
}

#[test]
fn owner_control_child_fixture() {
    let Ok(action) = std::env::var(FIXTURE_ACTION) else {
        return;
    };
    match action.as_str() {
        "unrelated" => thread::sleep(Duration::from_millis(700)),
        "target" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                #[cfg(unix)]
                // SAFETY: fd 0 is the receiver explicitly configured as this child’s stdin.
                let mut receiver = unsafe { OwnerControlReceiver::from_inherited_fd(0) }.unwrap();
                #[cfg(windows)]
                // SAFETY: the inherited stdin handle was explicitly configured from the receiver.
                let mut receiver = unsafe {
                    use std::os::windows::io::{AsHandle, AsRawHandle};
                    let stdin = std::io::stdin();
                    OwnerControlReceiver::from_inherited_handle(stdin.as_handle().as_raw_handle())
                }
                .unwrap();

                write_fixture_stdout(b"ready\n");
                receiver.wait_closed().await.unwrap();
                write_fixture_stdout(b"closed\n");
                // The receiving process must not be able to hand an activated endpoint on again.
                assert!(receiver.is_activated());
                let rejected = receiver.take_stdio().unwrap_err();
                assert_eq!(rejected.kind(), io::ErrorKind::InvalidInput);
                write_fixture_stdout(b"rejected\n");
            });
        }
        unknown => panic!("unknown child fixture action: {unknown}"),
    }
}

fn fixture_command(action: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "owner_control_child_fixture", "--nocapture"])
        .env(FIXTURE_ACTION, action);
    command
}

#[tokio::test]
async fn closing_keepalive_is_observed_as_eof() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    keepalive.shutdown();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .expect("wait for EOF timed out")
        .unwrap();
}

#[tokio::test]
async fn observed_eof_stays_terminal_across_later_waits() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    keepalive.shutdown();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .expect("wait for EOF timed out")
        .unwrap();

    // This capability never reconnects, so the recorded peer closure is terminal. On Windows the
    // second wait must not start another overlapped read that could surface
    // ERROR_PIPE_NOT_CONNECTED instead of the already observed EOF.
    for attempt in 0..3 {
        let again = tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
            .await
            .unwrap_or_else(|_| panic!("repeated wait {attempt} hung after the observed EOF"));
        again.unwrap_or_else(|error| {
            panic!("repeated wait {attempt} after EOF must return Ok, got {error:?}")
        });
    }
    assert!(receiver.is_activated());
}

#[tokio::test]
async fn receiver_wait_can_be_cancelled_and_retried() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    assert!(
        !receiver.is_activated(),
        "creation must not register a watcher"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.wait_closed())
            .await
            .is_err()
    );
    assert!(
        receiver.is_activated(),
        "the first wait must activate the single watcher"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.wait_closed())
            .await
            .is_err()
    );
    assert!(
        receiver.is_activated(),
        "a cancelled wait must not release or re-register the watcher"
    );
    keepalive.shutdown();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .expect("cancelled receiver wait left no reusable endpoint")
        .unwrap();
}

#[tokio::test]
async fn activated_receiver_rejects_stdio_transfer_and_still_waits() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.wait_closed())
            .await
            .is_err()
    );
    assert!(receiver.is_activated());

    let rejected = receiver.take_stdio().unwrap_err();
    assert_eq!(rejected.kind(), io::ErrorKind::InvalidInput);
    assert!(
        rejected.to_string().contains("cannot be transferred"),
        "unexpected rejection message: {rejected}"
    );

    // The rejected transfer must not consume, close, or re-register the live watcher.
    assert!(receiver.is_activated());
    keepalive.shutdown();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .expect("rejected stdio transfer disturbed the activated watcher")
        .unwrap();
}

#[tokio::test]
async fn shutdown_releases_the_activated_watcher_once() {
    let (_keepalive, mut receiver) = owner_control_pair().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.wait_closed())
            .await
            .is_err()
    );
    assert!(receiver.is_activated());

    receiver.shutdown();
    receiver.shutdown();
    assert!(!receiver.is_activated());
    assert_eq!(
        receiver.wait_closed().await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    // A shut-down receiver cannot be handed to a child either.
    assert_eq!(
        receiver.take_stdio().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn shutdown_is_idempotent_and_closes_each_endpoint_independently() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    keepalive.shutdown();
    keepalive.shutdown();
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .unwrap()
        .unwrap();
    receiver.shutdown();
    receiver.shutdown();
    assert_eq!(
        receiver.wait_closed().await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn unrelated_std_process_does_not_keep_the_owner_alive() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    let mut unrelated = fixture_command("unrelated")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    keepalive.shutdown();
    let observed = tokio::time::timeout(Duration::from_millis(250), receiver.wait_closed()).await;
    let child_still_running = unrelated.try_wait().unwrap().is_none();
    if child_still_running {
        let _ = unrelated.kill();
    }
    let _ = unrelated.wait();

    observed
        .expect("unrelated child inherited the owner keepalive")
        .unwrap();
    assert!(child_still_running, "fixture exited before the EOF check");
}

#[tokio::test]
async fn target_child_explicitly_receives_the_owner_control_stdio() {
    let (mut keepalive, mut receiver) = owner_control_pair().unwrap();
    let mut child = fixture_command("target")
        .stdin(receiver.take_stdio().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut before_ready = String::new();
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) => {
                    let _ = ready_tx.send(Err(before_ready.clone()));
                    return (before_ready, Ok(String::new()));
                }
                Ok(_) if line == "ready\n" => {
                    let _ = ready_tx.send(Ok(()));
                    break;
                }
                Ok(_) => before_ready.push_str(&line),
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return (before_ready, Err(error));
                }
            }
        }
        let mut rest = String::new();
        let remainder = stdout.read_to_string(&mut rest).map(|_| rest);
        (before_ready, remainder)
    });

    let readiness = ready_rx.recv_timeout(Duration::from_secs(3));
    if !matches!(readiness, Ok(Ok(()))) {
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        let (before_ready, remainder) = reader.join().unwrap();
        panic!(
            "target child failed before readiness: {readiness:?}, {before_ready:?}, {remainder:?}"
        );
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "owner child exited while kept alive"
    );

    keepalive.shutdown();
    let exit = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok::<_, io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if exit.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (_before_ready, remainder) = reader.join().unwrap();
    let remainder = remainder.unwrap();
    assert!(
        remainder.contains("closed\n"),
        "missing EOF evidence: {remainder:?}"
    );
    assert!(
        remainder.contains("rejected\n"),
        "child did not reject transferring its activated endpoint: {remainder:?}"
    );
    assert!(
        exit.expect("target child did not observe owner EOF")
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn inherited_socket_and_invalid_fd_are_rejected_with_os_errors() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let (socket, _peer) = UnixStream::pair().unwrap();
    // SAFETY: socket owns a live descriptor for the duration of the call.
    let wrong_type = unsafe { OwnerControlReceiver::from_inherited_fd(socket.as_raw_fd()) };
    assert_eq!(
        wrong_type.err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );

    // SAFETY: -1 is intentionally used to exercise fstat's EBADF error path.
    let invalid = unsafe { OwnerControlReceiver::from_inherited_fd(-1) };
    assert_eq!(invalid.err().unwrap().raw_os_error(), Some(libc::EBADF));
}

#[cfg(windows)]
#[test]
fn inherited_file_and_invalid_handle_are_rejected_with_os_errors() {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, INVALID_HANDLE_VALUE};

    let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    // SAFETY: file owns a live handle for the duration of the call.
    let wrong_type =
        unsafe { OwnerControlReceiver::from_inherited_handle(file.as_handle().as_raw_handle()) };
    assert_eq!(
        wrong_type.err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );

    // SAFETY: INVALID_HANDLE_VALUE intentionally exercises GetFileType's real error path.
    let invalid = unsafe { OwnerControlReceiver::from_inherited_handle(INVALID_HANDLE_VALUE) };
    assert_eq!(
        invalid.err().unwrap().raw_os_error(),
        Some(ERROR_INVALID_HANDLE as i32)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_probes_already_closed_without_reactor_turn_and_activates_once() {
    let (keepalive, mut receiver) = owner_control_pair().unwrap();
    drop(keepalive);
    assert!(!receiver.prepare().await.unwrap());
    assert!(receiver.is_activated());
    assert!(!receiver.prepare().await.unwrap());
    receiver.wait_closed().await.unwrap();
    assert!(receiver.take_stdio().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_establishes_live_watcher_and_reuses_it_for_closure() {
    let (keepalive, mut receiver) = owner_control_pair().unwrap();
    assert!(receiver.prepare().await.unwrap());
    assert!(receiver.is_activated());
    assert!(receiver.prepare().await.unwrap());
    drop(keepalive);
    tokio::time::timeout(Duration::from_secs(2), receiver.wait_closed())
        .await
        .unwrap()
        .unwrap();
    assert!(!receiver.prepare().await.unwrap());
    receiver.shutdown();
    assert!(receiver.prepare().await.is_err());
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn adopted_unconnected_overlapped_pipe_prepare_is_cancellable() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let name = format!(
        r"\\.\pipe\LOCAL\c2-unconnected-{}",
        uuid::Uuid::new_v4().simple()
    )
    .encode_utf16()
    .chain(Some(0))
    .collect::<Vec<_>>();
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            0,
            0,
            std::ptr::null(),
        )
    };
    assert!(!raw.is_null() && raw != INVALID_HANDLE_VALUE);
    let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut receiver =
        unsafe { OwnerControlReceiver::from_inherited_handle(owned.as_raw_handle()) }.unwrap();
    drop(owned);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.prepare())
            .await
            .is_err()
    );
    assert!(receiver.is_activated());
    receiver.shutdown();
    assert!(!receiver.is_activated());
    assert!(receiver.wait_closed().await.is_err());
}
