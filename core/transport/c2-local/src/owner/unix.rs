use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Stdio;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

pub(super) struct Keepalive(Option<OwnedFd>);

/// Receiver endpoint with a two-phase lifecycle.
///
/// `pending` owns the unregistered nonblocking read end until the first [`Receiver::wait_closed`]
/// call. That call moves the same descriptor into `active`, the single reactor registration for
/// this read end; every later wait, including one that follows a cancelled wait, reuses it.
/// Exactly one field owns a descriptor at a time, so `shutdown` and `Drop` close it once.
pub(super) struct Receiver {
    pending: Option<OwnedFd>,
    active: Option<AsyncFd<OwnedFd>>,
}

pub(super) fn pair() -> io::Result<(Keepalive, Receiver)> {
    let mut raw = [-1; 2];
    let result = create_pipe(&mut raw);
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: successful pipe creation returns two distinct owned descriptors.
    let read_end = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    // SAFETY: successful pipe creation returns two distinct owned descriptors.
    let write_end = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    verify_descriptor(read_end.as_raw_fd(), libc::O_RDONLY)?;
    verify_descriptor(write_end.as_raw_fd(), libc::O_WRONLY)?;
    // Deliberately no reactor registration here. `pair` is synchronous and may run outside a
    // runtime, and the controller process must never register the end that its target child
    // receives as stdio.
    Ok((
        Keepalive(Some(write_end)),
        Receiver {
            pending: Some(read_end),
            active: None,
        },
    ))
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn create_pipe(raw: &mut [RawFd; 2]) -> libc::c_int {
    // pipe2 atomically establishes both flags, avoiding an exec race on these platforms.
    unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn create_pipe(raw: &mut [RawFd; 2]) -> libc::c_int {
    // Some Unix libcs (including Darwin) do not expose pipe2. Set the flags before returning
    // either descriptor to callers; the pipe is still anonymous and the published handles are
    // non-inheritable and nonblocking.
    if unsafe { libc::pipe(raw.as_mut_ptr()) } != 0 {
        return -1;
    }
    for fd in raw.iter().copied() {
        let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } < 0
        {
            close_raw_pair(raw);
            return -1;
        }
        let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if status_flags < 0
            || unsafe { libc::fcntl(fd, libc::F_SETFL, status_flags | libc::O_NONBLOCK) } < 0
        {
            close_raw_pair(raw);
            return -1;
        }
    }
    0
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn close_raw_pair(raw: &mut [RawFd; 2]) {
    for fd in raw.iter_mut() {
        if *fd >= 0 {
            unsafe { libc::close(*fd) };
            *fd = -1;
        }
    }
}

fn verify_descriptor(fd: RawFd, expected_access: libc::c_int) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the structure on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner control endpoint must be a pipe",
        ));
    }

    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if descriptor_flags & libc::FD_CLOEXEC == 0
        || status_flags & libc::O_NONBLOCK == 0
        || status_flags & libc::O_ACCMODE != expected_access
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owner control pipe has unexpected descriptor flags",
        ));
    }
    Ok(())
}

pub(super) unsafe fn adopt(fd: RawFd) -> io::Result<Receiver> {
    // SAFETY: caller guarantees this borrowed descriptor stays valid through validation/dup.
    verify_inherited_receiver(fd)?;
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_DUPFD_CLOEXEC returned one new owned descriptor.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    verify_descriptor(duplicate.as_raw_fd(), libc::O_RDONLY)?;
    // The duplicate is nonblocking and close-on-exec, but stays unregistered until the first wait;
    // the borrowed source descriptor remains owned by the caller (for an inherited stdin, by the
    // process standard stream) and is never closed by this receiver.
    Ok(Receiver {
        pending: Some(duplicate),
        active: None,
    })
}

fn verify_inherited_receiver(fd: RawFd) -> io::Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized the structure on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited owner control descriptor is not a pipe",
        ));
    }
    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if status_flags & libc::O_ACCMODE != libc::O_RDONLY || status_flags & libc::O_NONBLOCK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited owner control descriptor is not a nonblocking read end",
        ));
    }
    Ok(())
}

impl Keepalive {
    pub(super) fn shutdown(&mut self) {
        self.0.take();
    }
}

impl Receiver {
    fn activate(&mut self) -> io::Result<()> {
        if self.active.is_none() {
            let pending = self.pending.take().ok_or_else(closed_receiver)?;
            // The first wait activates the endpoint: this is the only reactor registration for
            // the read end, and it stays alive until `shutdown` or `Drop`. A cancelled wait drops
            // only the readiness guard, so the next wait resumes on the same registration.
            // A registration failure is terminal: the descriptor is closed and later waits
            // report the receiver as closed.
            self.active = Some(AsyncFd::with_interest(pending, Interest::READABLE)?);
        }
        Ok(())
    }

    pub(super) async fn prepare(&mut self) -> io::Result<bool> {
        self.activate()?;
        let fd = self
            .active
            .as_ref()
            .ok_or_else(closed_receiver)?
            .get_ref()
            .as_raw_fd();
        loop {
            let mut byte = 0u8;
            // Probe the actual nonblocking pipe, independently of reactor readiness delivery.
            let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            match read {
                0 => return Ok(false),
                1 => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "owner control pipe unexpectedly carried data",
                    ));
                }
                _ => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::WouldBlock {
                        return Ok(true);
                    }
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
    }

    pub(super) async fn wait_closed(&mut self) -> io::Result<()> {
        self.activate()?;
        let receiver = self.active.as_ref().ok_or_else(closed_receiver)?;
        let fd = receiver.get_ref().as_raw_fd();
        receiver
            .async_io(Interest::READABLE, |_| {
                let mut byte = 0u8;
                // This pipe is nonblocking; AsyncFd retries only after readiness.
                let read = unsafe {
                    libc::read(
                        fd,
                        (&mut byte as *mut u8).cast::<libc::c_void>(),
                        std::mem::size_of::<u8>(),
                    )
                };
                match read {
                    0 => Ok(()),
                    1 => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "owner control pipe unexpectedly carried data",
                    )),
                    _ => Err(io::Error::last_os_error()),
                }
            })
            .await
    }

    pub(super) fn take_stdio(&mut self) -> io::Result<Stdio> {
        if self.active.is_some() {
            return Err(super::already_activated());
        }
        let pending = self.pending.take().ok_or_else(closed_receiver)?;
        Ok(Stdio::from(pending))
    }

    pub(super) fn is_activated(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn shutdown(&mut self) {
        self.active.take();
        self.pending.take();
    }
}

fn closed_receiver() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "owner control receiver is closed",
    )
}
