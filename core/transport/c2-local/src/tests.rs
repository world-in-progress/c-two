use super::*;
use tokio::io::AsyncReadExt;

pub(super) struct TestEndpoint {
    _root: Option<tempfile::TempDir>,
    endpoint: LocalEndpoint,
}

fn isolated_endpoint(label: &str) -> TestEndpoint {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let address = format!(
        "ipc://{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    #[cfg(unix)]
    {
        let root = tempfile::Builder::new()
            .prefix("c2-local-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let endpoint = LocalEndpointContext::with_unix_root(root.path())
            .unwrap()
            .endpoint(&address)
            .unwrap();
        TestEndpoint {
            _root: Some(root),
            endpoint,
        }
    }
    #[cfg(windows)]
    {
        TestEndpoint {
            _root: None,
            endpoint: LocalEndpoint::from_address(&address).unwrap(),
        }
    }
}

pub(super) async fn isolated_pair() -> (TestEndpoint, LocalStream, LocalStream) {
    let fixture = isolated_endpoint("c2-pair-test");
    let mut listener = LocalListener::bind(&fixture.endpoint).unwrap();
    let (client, server) = tokio::try_join!(
        LocalStream::connect(&fixture.endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept()
    )
    .unwrap();
    close_listener(listener);
    (fixture, client, server)
}

#[cfg(unix)]
#[tokio::test]
async fn peer_close_with_unread_data_records_the_native_read_end() {
    let (local, peer) = tokio::net::UnixStream::pair().unwrap();
    let mut local = LocalStream::from_inner(local);
    let mut peer = LocalStream::from_inner(peer);
    // The closing peer leaves its receive queue nonempty. Drain its outgoing
    // bytes first so the following read observes the OS-specific end result.
    peer.write_all(b"sent before close").await.unwrap();
    local.write_all(b"unread by peer").await.unwrap();
    drop(peer);
    let mut payload = [0; 17];
    local.read_exact(&mut payload).await.unwrap();
    assert_eq!(&payload, b"sent before close");
    let result = local.read(&mut [0]).await;
    #[cfg(target_os = "linux")]
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ECONNRESET));
    #[cfg(target_os = "macos")]
    assert_eq!(result.unwrap(), 0);
    assert!(local.peer_ended());
}

pub(super) fn close_listener(listener: LocalListener) {
    let outcome = listener.close();
    assert!(
        matches!(
            outcome,
            EndpointReapResult::Reaped
                | EndpointReapResult::AlreadyAbsent
                | EndpointReapResult::NotApplicable
        ),
        "listener cleanup failed: {outcome:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn explicit_connect_deadline_replaces_the_default_phase_guard() {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let started = tokio::time::Instant::now();
    within_connect_deadline(Some(deadline.into_std()), async {
        tokio::time::sleep(DEFAULT_CONNECT_TIMEOUT + Duration::from_secs(1)).await;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        started.elapsed(),
        DEFAULT_CONNECT_TIMEOUT + Duration::from_secs(1)
    );

    let started = tokio::time::Instant::now();
    let error = within_connect_deadline(None, std::future::pending::<io::Result<()>>())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(started.elapsed(), DEFAULT_CONNECT_TIMEOUT);
}

#[tokio::test]
async fn expired_connection_and_bind_deadlines_create_no_connection_or_listener() {
    let fixture = isolated_endpoint("expired-deadline");
    let deadline = Some(Instant::now());
    assert_eq!(
        LocalStream::connect_until(&fixture.endpoint, deadline)
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::TimedOut,
    );
    assert_eq!(
        LocalListener::bind_until(&fixture.endpoint, deadline)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock,
    );
    close_listener(LocalListener::bind_until(&fixture.endpoint, None).unwrap());
}

#[tokio::test]
async fn native_write_failure_aborts_without_peer_end() {
    let (_fixture, stream, peer) = isolated_pair().await;
    let abort = stream.abort_handle();
    let (reader, mut writer) = stream.into_split();
    {
        let native = peer.owner.stream.lock().unwrap();
        #[cfg(unix)]
        socket2::SockRef::from(&*native)
            .shutdown(std::net::Shutdown::Read)
            .unwrap();
        #[cfg(windows)]
        assert_ne!(
            unsafe {
                windows_sys::Win32::System::Pipes::DisconnectNamedPipe(
                    platform::raw_handle(&native) as windows_sys::Win32::Foundation::HANDLE,
                )
            },
            0
        );
    }
    drop(peer);
    let error = writer
        .write_all_with_timeout(b"native failure", Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::TimedOut);
    assert!(abort.is_aborted());
    assert!(!reader.peer_ended());
}

#[tokio::test]
async fn owned_halves_exchange_bytes_and_abort_wakes_a_pending_read() {
    let (_fixture, client, server) = isolated_pair().await;
    let abort = server.abort_handle();
    let (mut reader, _) = server.into_split();
    let (_, mut writer) = client.into_split();
    writer.write_all(b"hello").await.unwrap();
    let mut bytes = [0; 5];
    reader.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"hello");
    let mut read = Box::pin(reader.read_exact(&mut bytes));
    assert!(futures_util::poll!(&mut read).is_pending());
    abort.abort();
    assert_eq!(
        read.await.unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}

#[tokio::test]
async fn remote_read_guards_retain_the_os_stream_after_abort_and_io_owners_drop() {
    for side in [ConnectionSide::Client, ConnectionSide::Server] {
        let (_fixture, client, server) = isolated_pair().await;
        let (stream, mut peer) = match side {
            ConnectionSide::Client => (client, server),
            ConnectionSide::Server => (server, client),
        };
        let abort = stream.abort_handle();
        let first = stream.retain_for_remote_read();
        let (mut reader, writer) = stream.into_split();
        let last = reader.retain_for_remote_read();
        abort.abort();
        assert_eq!(
            reader.read(&mut [0]).await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(!reader.peer_ended());
        drop(reader);
        drop(writer);
        assert_eq!(Arc::strong_count(&last._owner), 2);
        assert!(futures_util::poll!(std::pin::pin!(peer.read(&mut [0]))).is_pending());
        drop(first);
        assert!(futures_util::poll!(std::pin::pin!(peer.read(&mut [0]))).is_pending());
        drop(last);
        // Retaining the abort handle must not retain the OS connection.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        abort.abort();
    }
}

#[cfg(unix)]
#[test]
fn remote_read_guard_retains_the_os_stream_after_executor_shutdown() {
    use std::io::Read;

    let (local, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    local.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let guard = {
        let _entered = runtime.enter();
        let stream = LocalStream::from_inner(tokio::net::UnixStream::from_std(local).unwrap());
        stream.retain_for_remote_read()
    };
    drop(runtime);
    assert_eq!(
        peer.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(guard);
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[cfg(windows)]
#[tokio::test]
async fn verified_client_stream_pins_noninheritable_server_process() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Threading::GetProcessId;

    let (_fixture, client, _server) = isolated_pair().await;
    let native = client.owner.stream.lock().unwrap();
    let platform::Stream::Client {
        pipe,
        _server_process,
    } = &*native
    else {
        panic!("client stream must retain the verified pipe and process");
    };
    assert_eq!(
        unsafe { GetProcessId(_server_process.as_raw_handle()) },
        std::process::id()
    );
    for handle in [pipe.as_raw_handle(), _server_process.as_raw_handle()] {
        let mut flags = 0;
        assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    }
}

#[cfg(windows)]
#[test]
fn remote_read_guard_retains_native_pipe_handles_after_executor_shutdown() {
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};

    for side in [ConnectionSide::Client, ConnectionSide::Server] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_fixture, client, server) = runtime.block_on(isolated_pair());
        let (stream, peer) = match side {
            ConnectionSide::Client => (client, server),
            ConnectionSide::Server => (server, client),
        };
        let raw = platform::raw_handle(&stream.owner.stream.lock().unwrap());
        let guard = stream.retain_for_remote_read();
        let owner = Arc::downgrade(&guard._owner);
        drop(stream);
        drop(runtime);
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(raw as HANDLE, &mut flags) },
            0
        );
        drop(guard);
        assert!(owner.upgrade().is_none());
        drop(peer);
    }
}

#[tokio::test]
async fn empty_read_buffer_is_not_peer_end() {
    let (_fixture, mut stream, mut peer) = isolated_pair().await;
    peer.write_all(b"x").await.unwrap();
    assert_eq!(stream.read(&mut []).await.unwrap(), 0);
    assert!(!stream.peer_ended());
    assert_eq!(stream.read(&mut [0]).await.unwrap(), 1);
    assert!(!stream.peer_ended());
    drop(peer);
    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    assert!(stream.peer_ended());
}

#[tokio::test]
async fn cancelling_a_partial_write_poisoned_the_connection() {
    let (_fixture, mut client, _server) = isolated_pair().await;
    let abort = client.abort_handle();
    let bytes = vec![0; 16 * 1024 * 1024];
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.write_all(&bytes))
            .await
            .is_err()
    );
    assert!(abort.is_aborted());
    assert!(client.write_all(b"new frame").await.is_err());
}

#[tokio::test]
async fn duplicate_listener_cannot_displace_owner_and_restart_works() {
    let fixture = isolated_endpoint("c2-listener-test");
    let endpoint = &fixture.endpoint;
    let mut first = LocalListener::bind(&endpoint).unwrap();
    assert!(LocalListener::bind(&endpoint).is_err());
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        first.accept()
    )
    .unwrap();
    drop(first);
    close_listener(LocalListener::bind(&endpoint).unwrap());
}

#[tokio::test]
async fn cancelled_accept_can_accept_the_next_client() {
    let fixture = isolated_endpoint("c2-accept-test");
    let endpoint = &fixture.endpoint;
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept()
    )
    .unwrap();
    close_listener(listener);
}

#[tokio::test]
async fn completed_reply_remains_readable_after_server_stream_is_dropped() {
    let (_fixture, mut client, mut server) = isolated_pair().await;
    let expected = b"shutdown initiate acknowledged";
    server.write_all(expected).await.unwrap();
    drop(server);
    let mut actual = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut actual))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn listener_restarts_after_server_closes_even_with_an_old_client_handle() {
    let fixture = isolated_endpoint("c2-restart-test");
    let endpoint = &fixture.endpoint;
    let mut listener = LocalListener::bind(&endpoint).unwrap();
    let (client, server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        listener.accept(),
    )
    .unwrap();
    drop(server);
    drop(listener);
    let mut next = LocalListener::bind(&endpoint).unwrap();
    assert!(LocalListener::bind(&endpoint).is_err());
    let (mut next_client, mut next_server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        next.accept(),
    )
    .unwrap();
    next_server.write_all(b"new listener").await.unwrap();
    let mut bytes = [0; 12];
    next_client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"new listener");
    drop(client);
    close_listener(next);
}

#[test]
fn listener_process_fixture() {
    let Ok(action) = std::env::var("C2_LOCAL_LISTENER_TEST_ACTION") else {
        return;
    };
    let address = std::env::var("C2_LOCAL_LISTENER_TEST_ADDRESS").unwrap();
    #[cfg(unix)]
    let context = LocalEndpointContext::with_unix_root(std::path::Path::new(
        &std::env::var_os("C2_LOCAL_LISTENER_TEST_ROOT").unwrap(),
    ))
    .unwrap();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();
    let endpoint = context.endpoint(&address).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    match action.as_str() {
        "duplicate" => assert_eq!(
            LocalListener::bind(&endpoint).err().unwrap().kind(),
            io::ErrorKind::AddrInUse,
        ),
        "exit-owner" => {
            let _listener = LocalListener::bind(&endpoint).unwrap();
            // Release kernel handles without Rust destructors, as on process death.
            std::process::exit(0);
        }
        _ => panic!("unknown listener test action: {action}"),
    }
}

fn run_listener_process(endpoint: &LocalEndpoint, action: &str) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "tests::listener_process_fixture", "--nocapture"])
        .env("C2_LOCAL_LISTENER_TEST_ADDRESS", endpoint.address())
        .env("C2_LOCAL_LISTENER_TEST_ACTION", action);
    if let Some(root) = endpoint.context().unix_root() {
        command.env("C2_LOCAL_LISTENER_TEST_ROOT", root);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test]
async fn another_process_cannot_displace_the_listener() {
    let fixture = isolated_endpoint("c2-process-duplicate");
    let endpoint = &fixture.endpoint;
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    run_listener_process(&endpoint, "duplicate");
    let (mut client, mut server) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        owner.accept(),
    )
    .unwrap();
    server.write_all(b"owner").await.unwrap();
    let mut bytes = [0; 5];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"owner");
    close_listener(owner);
}

#[tokio::test]
async fn listener_restarts_after_owner_process_exits_without_destructors() {
    let fixture = isolated_endpoint("c2-process-exit");
    let endpoint = &fixture.endpoint;
    run_listener_process(&endpoint, "exit-owner");
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    let (_, _) = tokio::try_join!(
        LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
        owner.accept(),
    )
    .unwrap();
    close_listener(owner);
}

#[tokio::test]
async fn listener_ownership_can_be_released_on_another_thread() {
    let fixture = isolated_endpoint("c2-thread-drop");
    let endpoint = &fixture.endpoint;
    let owner = LocalListener::bind(&endpoint).unwrap();
    std::thread::spawn(move || drop(owner)).join().unwrap();
    close_listener(LocalListener::bind(&endpoint).unwrap());
}

#[tokio::test]
async fn listener_restarts_after_cancelling_a_pending_accept() {
    let fixture = isolated_endpoint("c2-pending-restart");
    let endpoint = &fixture.endpoint;
    let mut owner = LocalListener::bind(&endpoint).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), owner.accept())
            .await
            .is_err()
    );
    drop(owner);
    let mut next = LocalListener::bind(&endpoint).unwrap();
    let (mut client, mut server) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(
            LocalStream::connect(&endpoint, DEFAULT_CONNECT_TIMEOUT),
            next.accept(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    server.write_all(b"new listener").await.unwrap();
    let mut bytes = [0; 12];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"new listener");
    close_listener(next);
}

#[tokio::test]
async fn connected_pair_allows_same_session_shm_and_abort_is_an_error() {
    let (_fixture, client, server) = isolated_pair().await;
    assert!(client.shared_memory_available().unwrap());
    assert!(server.shared_memory_available().unwrap());
    client.abort_handle().abort();
    assert_eq!(
        client.shared_memory_available().unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}
