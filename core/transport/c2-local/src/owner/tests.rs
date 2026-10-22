use super::*;
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FIXTURE: &str = "C2_LOCAL_DUPLEX_OWNER_FIXTURE";
const REQUEST: &[u8] = b"owner\0request\xff\x01";
const REPLY: &[u8] = b"host\0reply\xfe\x02";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn signal(line: &str) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}").unwrap();
    stdout.flush().unwrap();
}

fn inherited_source_is_open() -> bool {
    #[cfg(unix)]
    {
        (unsafe { libc::fcntl(0, libc::F_GETFD) }) >= 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use windows_sys::Win32::Foundation::GetHandleInformation;
        let mut flags = 0;
        (unsafe { GetHandleInformation(std::io::stdin().as_handle().as_raw_handle(), &mut flags) })
            != 0
    }
}

// Owner server disconnect is EOF on Unix and may report native error 233 on Windows.
fn assert_owner_peer_ended(result: io::Result<usize>) {
    match result {
        Ok(0) => {}
        #[cfg(windows)]
        Err(error)
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED as i32) => {}
        other => panic!("owner peer must end its connection: {other:?}"),
    }
}

#[test]
fn owner_child_fixture() {
    let Ok(action) = std::env::var(FIXTURE) else {
        return;
    };
    #[cfg(unix)]
    // SAFETY: this process was exec'd with the dedicated, previously unregistered bootstrap
    // as stdin. No aliases perform I/O or get registered. Adoption borrows rather than closes 0.
    let mut bootstrap = unsafe { OwnerBootstrap::from_inherited_fd(0) }.unwrap();
    #[cfg(windows)]
    // SAFETY: stdin is the dedicated raw bootstrap moved before any IOCP registration. The
    // original handle stays open but is never used for I/O or registered on any reactor.
    let mut bootstrap = unsafe {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        OwnerBootstrap::from_inherited_handle(std::io::stdin().as_handle().as_raw_handle())
    }
    .unwrap();
    if action == "pending-close" {
        bootstrap.close();
        bootstrap.close();
        assert!(inherited_source_is_open());
        signal("PENDING_ALIAS_OPEN_CLOSED");
        return;
    }
    assert_eq!(action, "duplex");
    runtime().block_on(async {
        let mut stream = bootstrap.activate().await.unwrap();
        assert_eq!(
            bootstrap.activate().await.err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            bootstrap.take_stdio().err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        signal("OWNER_READY");
        let mut request = vec![0; REQUEST.len()];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(request, REQUEST);
        stream.write_all(REPLY).await.unwrap();
        // The explicit acknowledgement proves the peer consumed the reply before Windows
        // DisconnectNamedPipe is allowed to discard any remaining unread pipe data.
        assert_eq!(stream.read_u8().await.unwrap(), 0x7f);
        drop(stream);
        assert!(inherited_source_is_open());
        signal("ACTIVE_ALIAS_OPEN_CLOSED");
    });
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(bootstrap: &mut OwnerBootstrap, action: &str) -> ChildGuard {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "owner::tests::owner_child_fixture",
            "--nocapture",
        ])
        .env(FIXTURE, action)
        .stdin(bootstrap.take_stdio().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let child = command.spawn().unwrap();
    // The parent's Stdio copy closes here. It must not disconnect the child's inherited end.
    drop(command);
    ChildGuard(child)
}

fn wait_child(child: &mut ChildGuard) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "owner fixture failed: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "owner fixture timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn raw_pair_transfers_before_runtime_and_child_closes_with_stdin_alias_open() {
    // Neither endpoint has ever had a reactor here. This also tests real Windows IOCP transfer:
    // registering the server in this process would prevent activation in the exec'd child.
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    let mut child = child(&mut bootstrap, "duplex");
    drop(bootstrap);
    let stdout = child.0.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line == "OWNER_READY" {
                let _ = ready_tx.send(());
            }
            output.push_str(&line);
            output.push('\n');
        }
        output
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("child activation readiness");
    runtime().block_on(async {
        let mut stream = controller.activate().await.unwrap();
        stream.write_all(REQUEST).await.unwrap();
        let mut reply = vec![0; REPLY.len()];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, REPLY);
        // The child consumes this marker and disconnects. Its EOF and exit
        // confirm delivery; flushing after that disconnect can report error 233.
        AsyncWriteExt::write_all(&mut stream, &[0x7f])
            .await
            .unwrap();
        let mut byte = [0; 1];
        let ended = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
            .await
            .expect("child owner end must close within the deadline");
        assert_owner_peer_ended(ended);
    });
    wait_child(&mut child);
    let output = reader.join().unwrap();
    assert!(output.contains("ACTIVE_ALIAS_OPEN_CLOSED"), "{output}");
}

#[test]
fn pending_adoption_disconnects_without_closing_original_stdin() {
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    runtime().block_on(async {
        let mut stream = controller.activate().await.unwrap();
        let mut child = child(&mut bootstrap, "pending-close");
        drop(bootstrap);
        let mut byte = [0; 1];
        let ended = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
            .await
            .expect("child owner end must close within the deadline");
        assert_owner_peer_ended(ended);
        wait_child(&mut child);
        let mut output = String::new();
        child
            .0
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert!(output.contains("PENDING_ALIAS_OPEN_CLOSED"), "{output}");
    });
}

#[tokio::test]
async fn duplex_preserves_buffered_control_bytes_and_full_payloads() {
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    let mut client = controller.activate().await.unwrap();
    // Queue legal bytes before host activation. Its arm probe must leave them untouched.
    AsyncWriteExt::write_all(&mut client, REQUEST)
        .await
        .unwrap();
    let mut host = bootstrap.activate().await.unwrap();
    let mut initial = vec![0; REQUEST.len()];
    host.read_exact(&mut initial).await.unwrap();
    assert_eq!(initial, REQUEST);
    let payload: Vec<_> = (0..131_071).map(|i| (i % 251) as u8).collect();
    let mut received = vec![0; payload.len()];
    let (sent, read) = tokio::join!(client.write_all(&payload), host.read_exact(&mut received));
    sent.unwrap();
    read.unwrap();
    assert_eq!(received, payload);
    let (sent, read) = tokio::join!(host.write_all(&payload), client.read_exact(&mut received));
    sent.unwrap();
    read.unwrap();
    assert_eq!(received, payload);
}

#[tokio::test]
async fn activation_is_single_use_and_wrapper_close_does_not_close_transferred_stream() {
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    // Dropping an unpolled activation future does not consume either raw endpoint.
    drop(bootstrap.activate());
    drop(controller.activate());
    let mut client = controller.activate().await.unwrap();
    let mut host = bootstrap.activate().await.unwrap();
    assert_eq!(
        controller.activate().await.err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        bootstrap.activate().await.err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        bootstrap.take_stdio().err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    controller.close();
    bootstrap.close();
    drop(controller);
    drop(bootstrap);
    client.write_all(REQUEST).await.unwrap();
    let mut bytes = vec![0; REQUEST.len()];
    host.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, REQUEST);
}

#[tokio::test]
async fn both_stream_ends_and_pending_endpoints_close_the_relationship() {
    for close_host in [false, true] {
        let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
        let client = controller.activate().await.unwrap();
        let host = bootstrap.activate().await.unwrap();
        let mut remaining = if close_host {
            drop(host);
            client
        } else {
            drop(client);
            host
        };
        let mut byte = [0; 1];
        let ended = tokio::time::timeout(Duration::from_secs(2), remaining.read(&mut byte))
            .await
            .expect("peer owner end must close within the deadline");
        assert_owner_peer_ended(ended);
    }
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    let mut client = controller.activate().await.unwrap();
    bootstrap.close();
    bootstrap.close();
    assert_eq!(
        bootstrap.activate().await.err().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        bootstrap.take_stdio().err().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
    let ended = tokio::time::timeout(Duration::from_secs(2), client.read(&mut [0]))
        .await
        .expect("closed bootstrap must end its peer within the deadline");
    assert_owner_peer_ended(ended);
}

#[tokio::test]
async fn activation_rejects_a_peer_that_closed_before_arm() {
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    controller.close();
    controller.close();
    assert!(bootstrap.activate().await.is_err());
    assert_eq!(
        controller.activate().await.err().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
}

// Windows constructs this boundary with an unregistered synchronous writer in
// owner/windows.rs, so native handle closure precedes activation there too.
#[cfg(unix)]
#[tokio::test]
async fn activation_rejects_a_closed_peer_even_with_buffered_control_bytes() {
    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    let mut client = controller.activate().await.unwrap();
    client.write_all(REQUEST).await.unwrap();
    drop(client);
    assert!(
        bootstrap.activate().await.is_err(),
        "buffered bytes cannot keep a closed owner alive"
    );

    let (mut controller, mut bootstrap) = owner_endpoint_pair().unwrap();
    let mut host = bootstrap.activate().await.unwrap();
    host.write_all(REPLY).await.unwrap();
    drop(host);
    assert!(
        controller.activate().await.is_err(),
        "buffered bytes cannot keep a closed host alive"
    );
}

#[cfg(unix)]
#[test]
fn inherited_wrong_type_mode_and_unconnected_socket_are_rejected() {
    use std::os::fd::AsRawFd;
    let file = tempfile::tempfile().unwrap();
    // SAFETY: intentionally wrong but valid, unregistered descriptors exercise validation.
    assert_eq!(
        unsafe { OwnerBootstrap::from_inherited_fd(file.as_raw_fd()) }
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let (blocking, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert_eq!(
        unsafe { OwnerBootstrap::from_inherited_fd(blocking.as_raw_fd()) }
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let (datagram, _peer) = std::os::unix::net::UnixDatagram::pair().unwrap();
    datagram.set_nonblocking(true).unwrap();
    assert_eq!(
        unsafe { OwnerBootstrap::from_inherited_fd(datagram.as_raw_fd()) }
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
    socket.set_nonblocking(true).unwrap();
    assert_eq!(
        unsafe { OwnerBootstrap::from_inherited_fd(socket.as_raw_fd()) }
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    // Invalid values deliberately exercise the OS rejection path; no handle is adopted.
    assert_eq!(
        unsafe { OwnerBootstrap::from_inherited_fd(-1) }
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::EBADF)
    );
}
