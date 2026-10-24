use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Stdio;

use crate::LocalStream;

pub(super) struct Controller(Endpoint);
pub(super) struct Bootstrap(Endpoint);

/// One raw socket. Successful synchronous activation transfers its runtime registration.
struct Endpoint {
    pending: Option<OwnedFd>,
    transferred: bool,
}

pub(super) fn pair() -> io::Result<(Controller, Bootstrap)> {
    let mut raw = [-1; 2];
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    let socket_type = libc::SOCK_STREAM;
    // SAFETY: raw has space for the two fresh descriptors returned by socketpair.
    if unsafe { libc::socketpair(libc::AF_UNIX, socket_type, 0, raw.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two distinct owned descriptors.
    let controller = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    // SAFETY: socketpair returned two distinct owned descriptors.
    let bootstrap = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    // Darwin has no SOCK_CLOEXEC/SOCK_NONBLOCK. Set both before publishing either end.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    for socket in [&controller, &bootstrap] {
        let fd = socket.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((
        Controller(Endpoint::new(controller)),
        Bootstrap(Endpoint::new(bootstrap)),
    ))
}

fn verify_socket(fd: RawFd) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized stat on success.
    if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(invalid_socket());
    }
    let mut socket_type = 0;
    let mut size = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(invalid_socket());
    }
    let mut peer = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut size = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { libc::getpeername(fd, peer.as_mut_ptr().cast(), &mut size) } != 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOTCONN) {
            Err(invalid_socket())
        } else {
            Err(error)
        };
    }
    // SAFETY: getpeername initialized the family field on success.
    if unsafe { peer.assume_init() }.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(invalid_socket());
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 || flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(invalid_socket());
    }
    Ok(())
}

fn invalid_socket() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "owner bootstrap requires a connected nonblocking AF_UNIX stream socket",
    )
}

/// SAFETY: the public adoption contract requires a dedicated, unregistered source and aliases.
pub(super) unsafe fn adopt(fd: RawFd) -> io::Result<Bootstrap> {
    verify_socket(fd)?;
    // SAFETY: caller guarantees a valid source throughout validation and duplication.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a fresh close-on-exec owned descriptor.
    Ok(Bootstrap(Endpoint::new(unsafe {
        OwnedFd::from_raw_fd(duplicate)
    })))
}

/// End the relationship even while an inherited stdin alias remains open.
fn disconnect(fd: RawFd) {
    unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
}

fn probe_live(fd: RawFd) -> io::Result<()> {
    // Buffered bytes remain readable after peer close. A successful MSG_PEEK alone cannot
    // arm a live relationship: inspect native hangup status without consuming those bytes.
    let mut status = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        status.events |= libc::POLLRDHUP;
    }
    if unsafe { libc::poll(&mut status, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let terminal = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let terminal = terminal | libc::POLLRDHUP;
    if status.revents & terminal != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "owner socket has terminal status",
        ));
    }
    let mut byte = 0u8;
    loop {
        // MSG_PEEK never consumes a legal control byte. The socket is nonblocking.
        let result = unsafe {
            libc::recv(
                fd,
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "owner peer is already closed",
            ));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock => return Ok(()),
            io::ErrorKind::Interrupted => continue,
            _ => return Err(error),
        }
    }
}

impl Endpoint {
    fn new(fd: OwnedFd) -> Self {
        Self {
            pending: Some(fd),
            transferred: false,
        }
    }

    fn unavailable(&self) -> io::Error {
        if self.transferred {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "owner endpoint was already transferred",
            )
        } else {
            io::Error::new(io::ErrorKind::BrokenPipe, "owner endpoint is closed")
        }
    }

    fn activate(&mut self) -> io::Result<LocalStream> {
        // Activation is synchronous; no registered watcher survives this call.
        // Keep an unregistered alias to disconnect inherited stdio if registration fails.
        let cleanup = self
            .pending
            .as_ref()
            .ok_or_else(|| self.unavailable())?
            .try_clone()?;
        let pending = self.pending.take().expect("pending socket");
        let watcher =
            match tokio::net::UnixStream::from_std(std::os::unix::net::UnixStream::from(pending)) {
                Ok(watcher) => watcher,
                Err(error) => {
                    disconnect(cleanup.as_raw_fd());
                    return Err(error);
                }
            };
        drop(cleanup);
        if let Err(error) = probe_live(watcher.as_raw_fd()) {
            disconnect(watcher.as_raw_fd());
            return Err(error);
        }
        self.transferred = true;
        Ok(LocalStream::from_inner(watcher))
    }

    fn close(&mut self) {
        if let Some(pending) = self.pending.take() {
            disconnect(pending.as_raw_fd());
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.close();
    }
}

impl Controller {
    pub(super) async fn activate(&mut self) -> io::Result<LocalStream> {
        self.0.activate()
    }
    pub(super) fn close(&mut self) {
        self.0.close();
    }
}

impl Bootstrap {
    pub(super) async fn activate(&mut self) -> io::Result<LocalStream> {
        self.0.activate()
    }
    pub(super) fn close(&mut self) {
        self.0.close();
    }
    pub(super) fn take_stdio(&mut self) -> io::Result<Stdio> {
        let pending = self.0.pending.take().ok_or_else(|| self.0.unavailable())?;
        self.0.transferred = true;
        Ok(Stdio::from(pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_fork_copies_use_plain_close_without_disconnecting_the_parent() {
        // The fork child performs only async-signal-safe close/_exit syscalls. In particular
        // it never drops an owner wrapper (which would shut down the shared connection).
        let (mut controller, mut bootstrap) = pair().unwrap();
        let controller_fd = controller.0.pending.as_ref().unwrap().as_raw_fd();
        let bootstrap_fd = bootstrap.0.pending.as_ref().unwrap().as_raw_fd();
        for fd in [controller_fd, bootstrap_fd] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                libc::close(controller_fd);
                libc::close(bootstrap_fd);
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::AsyncReadExt;
            let mut client = controller.activate().await.unwrap();
            let mut host = bootstrap.activate().await.unwrap();
            client.write_all(b"fork copies closed").await.unwrap();
            let mut bytes = [0; 18];
            host.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"fork copies closed");
        });
    }
}
