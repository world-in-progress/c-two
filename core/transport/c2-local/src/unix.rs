use super::{
    EndpointCredential, EndpointInspection, EndpointReapResult, LocalEndpoint, SweepBatch,
    SweepBudget,
};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::net::UnixStream;

pub type Stream = UnixStream;

pub async fn connect(endpoint: &LocalEndpoint) -> io::Result<Stream> {
    let root = endpoint.context().unix_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint has no Unix directory",
        )
    })?;
    let directory = crate::unix_common::EndpointDirectory::open(root, false)?;
    if !directory.strict_private() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local endpoint directory is not private",
        ));
    }
    let name = Path::new(endpoint.os_name())
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no file name"))?;
    let socket = crate::unix_common::start_connect_in_directory(&directory, name)?;
    let socket: std::os::fd::OwnedFd = socket.into();
    let stream = UnixStream::from_std(std::os::unix::net::UnixStream::from(socket))?;
    // Follow Tokio's connect completion contract: readiness then SO_ERROR.
    // Cancellation drops the stream, even while the connection is pending.
    stream.writable().await?;
    if let Some(error) = stream.take_error()? {
        return Err(error);
    }
    if !directory.path_still_names_open_directory() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local endpoint directory changed during connect",
        ));
    }
    Ok(stream)
}

pub fn raw_handle(stream: &Stream) -> usize {
    stream.as_raw_fd() as usize
}

pub fn cancel(raw: usize) {
    unsafe {
        libc::shutdown(raw as i32, libc::SHUT_RDWR);
    }
}

pub fn poll_flush(stream: &mut Stream, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    Pin::new(stream).poll_flush(cx)
}

pub type Listener = crate::unix_managed::ManagedListener;

pub(crate) fn inspect_endpoint(endpoint: &LocalEndpoint) -> EndpointInspection {
    crate::unix_managed::inspect_managed(endpoint)
}

pub(crate) fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    crate::unix_managed::reap_managed(endpoint, credential)
}

pub(crate) struct EndpointSweep(crate::unix_managed::ManagedSweep);
impl EndpointSweep {
    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        crate::unix_managed::ManagedSweep::for_endpoint(endpoint).map(Self)
    }
    pub(crate) fn for_scope(
        endpoint: &LocalEndpoint,
        targets: &[LocalEndpoint],
    ) -> io::Result<Self> {
        crate::unix_managed::ManagedSweep::for_scope(endpoint, targets).map(Self)
    }
    pub(crate) fn next_batch(&mut self, budget: SweepBudget) -> SweepBatch {
        self.0.next_batch(budget)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;

    fn open_descriptors() -> usize {
        (0..512)
            .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
            .count()
    }

    // An isolated process makes descriptor counts meaningful: unrelated test
    // threads cannot open or reuse descriptors while cancellation is observed.
    #[test]
    fn pending_connect_fixture() {
        if std::env::var_os("C2_LOCAL_PENDING_CONNECT_FIXTURE").is_none() {
            return;
        }
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let context = crate::LocalEndpointContext::with_unix_root(root.path()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // Initialize reactor descriptors before observing ownership.
            drop(tokio::net::UnixStream::pair().unwrap());
            let cwd = std::env::current_dir().unwrap();
            for cancel in [true, false] {
                let endpoint = context
                    .endpoint(if cancel {
                        "ipc://cancel"
                    } else {
                        "ipc://complete"
                    })
                    .unwrap();
                let listener =
                    socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
                        .unwrap();
                listener
                    .bind(&socket2::SockAddr::unix(endpoint.os_name()).unwrap())
                    .unwrap();
                listener.listen(0).unwrap();
                listener.set_nonblocking(true).unwrap();
                let queued =
                    socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
                        .unwrap();
                queued.set_nonblocking(true).unwrap();
                queued
                    .connect(&socket2::SockAddr::unix(endpoint.os_name()).unwrap())
                    .unwrap();
                let before = open_descriptors();
                let mut pending = Box::pin(connect(&endpoint));
                let waker = futures_util::task::noop_waker();
                assert!(
                    pending
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending(),
                    "full backlog must exercise EINPROGRESS"
                );
                assert_eq!(
                    open_descriptors(),
                    before + 2,
                    "pending connect owns directory and socket"
                );
                assert_eq!(std::env::current_dir().unwrap(), cwd);
                if cancel {
                    drop(pending);
                    assert_eq!(
                        open_descriptors(),
                        before,
                        "cancellation must close both descriptors"
                    );
                } else {
                    drop(listener.accept().unwrap());
                    let stream =
                        tokio::time::timeout(std::time::Duration::from_secs(3), &mut pending)
                            .await
                            .unwrap()
                            .unwrap();
                    assert!(stream.peer_addr().is_ok());
                    drop(stream);
                    drop(pending);
                    assert_eq!(open_descriptors(), before);
                }
                drop(queued);
                drop(listener);
                std::fs::remove_file(endpoint.os_name()).unwrap();
            }
            let before = open_descriptors();
            assert_eq!(
                connect(&context.endpoint("ipc://missing").unwrap())
                    .await
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::NotFound
            );
            assert_eq!(
                open_descriptors(),
                before,
                "failed initiation must close its descriptors"
            );
            assert_eq!(std::env::current_dir().unwrap(), cwd);
        });
        println!("PENDING_CONNECT_VERIFIED");
    }

    #[test]
    fn pending_connect_completes_or_cancels_without_descriptor_leaks() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::tests::pending_connect_fixture",
                "--nocapture",
            ])
            .env("C2_LOCAL_PENDING_CONNECT_FIXTURE", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("pending-connect fixture exceeded its deadline: {output:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("PENDING_CONNECT_VERIFIED"),
            "fixture did not execute: {output:?}"
        );
    }
}
