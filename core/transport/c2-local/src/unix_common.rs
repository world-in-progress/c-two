//! Verified Unix directory, identity and descriptor-relative socket tools.
use crate::{EndpointReapResult, EndpointUnverifiedReason, UnixSocketIdentity};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io;
#[cfg(test)]
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Failure {
        /// `fchmodat` on the freshly bound socket fails.
        SocketPermissions,
        /// A managed-v2 owner record write fails outright.
        ManagedRecordWrite,
        /// A managed-v2 owner record write lands only partially, so the
        /// read-back cannot equal what was written.
        ManagedRecordPartialWrite,
        /// The managed socket entry is replaced by a foreign object before the
        /// owner record is written.
        ManagedRecordWriteAfterReplacement,
    }

    thread_local! {
        static PENDING: Cell<Option<Failure>> = const { Cell::new(None) };
    }

    pub(crate) fn inject(failure: Failure) {
        PENDING.with(|pending| pending.set(Some(failure)));
    }

    pub(crate) fn take_if(expected: Failure) -> bool {
        PENDING.with(|pending| {
            if pending.get() == Some(expected) {
                pending.set(None);
                true
            } else {
                false
            }
        })
    }
}

pub(crate) type SocketIdentity = UnixSocketIdentity;

impl UnixSocketIdentity {
    pub(crate) fn from_stat(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            changed_secs: stat.st_ctime,
            changed_nanos: stat.st_ctime_nsec,
        }
    }
}

#[derive(Debug)]
pub(crate) struct EndpointDirectory {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl EndpointDirectory {
    pub(crate) fn open(path: &Path, create: bool) -> io::Result<Self> {
        if create {
            create_private_directory(path)?;
        }
        let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "directory path contains NUL")
        })?;
        // O_NOFOLLOW applies to the managed namespace directory itself. The
        // system /tmp alias on macOS is an intentional intermediate component.
        let fd = unsafe {
            libc::open(
                path_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: open returned a fresh descriptor and File assumes ownership.
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = fstat(file.as_raw_fd())?;
        verify_directory_stat(&stat)?;
        let directory = Self {
            file,
            path: path.to_owned(),
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        };
        if !directory.path_still_names_open_directory() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed endpoint directory changed while opening",
            ));
        }
        Ok(directory)
    }

    pub(crate) fn fd(&self) -> libc::c_int {
        self.file.as_raw_fd()
    }

    /// Strict managed-namespace check: the directory must be owned by the
    /// current user with exactly `0700` permissions.
    pub(crate) fn strict_private(&self) -> bool {
        let Ok(stat) = fstat(self.fd()) else {
            return false;
        };
        stat.st_uid == unsafe { libc::geteuid() } && stat.st_mode as libc::mode_t & 0o777 == 0o700
    }

    pub(crate) fn path_still_names_open_directory(&self) -> bool {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return false;
        };
        metadata.file_type().is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.mode() & 0o022 == 0
    }

    pub(crate) fn stat(&self, name: &CString) -> io::Result<Option<libc::stat>> {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                self.fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(Some(stat));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }

    pub(crate) fn open_file(
        &self,
        name: &CString,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<Option<File>> {
        let fd = unsafe {
            libc::openat(
                self.fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                mode as libc::c_int,
            )
        };
        if fd >= 0 {
            // SAFETY: openat returned a fresh descriptor and File assumes ownership.
            return Ok(Some(unsafe { File::from_raw_fd(fd) }));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }

    pub(crate) fn unlink(&self, name: &CString) -> io::Result<()> {
        if unsafe { libc::unlinkat(self.fd(), name.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    pub(crate) fn socket_path(&self, socket_name: &OsStr) -> PathBuf {
        self.path.join(socket_name)
    }

    /// Test-only: replace one entry with a foreign regular file so rollback
    /// must refuse to delete it.
    #[cfg(test)]
    pub(crate) fn replace_entry_with_file_for_test(&self, name: &CString) -> io::Result<()> {
        self.unlink(name)?;
        let mut file = self
            .open_file(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "replacement file missing"))?;
        file.write_all(b"replacement-marker")?;
        file.sync_all()
    }
}

pub(crate) fn fstat(fd: libc::c_int) -> io::Result<libc::stat> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } == 0 {
        Ok(stat)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Creates only the requested private leaf directory with an explicit 0700 mode.
///
/// `fs::create_dir_all` applies the process umask to 0777, so a permissive
/// 002/000 umask would create a group- or world-writable directory that this
/// same module then refuses to open. The explicit mode keeps a new namespace
/// private without ever relaxing a pre-existing directory: an existing unsafe
/// directory remains an explicit error instead of being silently chmod-ed.
/// Never create ancestors: only the platform default leaf is initialized by
/// production callers. Custom directories must already exist.
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false).mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn verify_directory_stat(stat: &libc::stat) -> io::Result<()> {
    let mode = stat.st_mode as libc::mode_t;
    if mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed endpoint namespace is not a directory",
        ));
    }
    if stat.st_uid != unsafe { libc::geteuid() } || mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "managed endpoint namespace is not private to the current user (uid {} mode {:o})",
                stat.st_uid,
                mode & 0o7777
            ),
        ));
    }
    Ok(())
}

pub(crate) fn same_file(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

// macOS spells the thread-local `fchdir` as `pthread_fchdir_np`. The libc
// crate does not bind it on Apple targets, so it is declared here. The symbol
// has no equivalent on other Unix systems, so the binding is confined to
// Apple targets and every other target compiles the fail-closed stub instead.
//
// `pthread_fchdir_np` changes only the calling thread's working directory; the
// process-wide directory of every other thread is untouched. Callers must
// therefore own the thread they call it on for the whole switch, which is why
// the managed bind runs it on a short-lived dedicated thread that exits
// immediately afterwards: the caller's own thread directory is never read,
// written, or restored, so an existing per-thread cwd cannot be disturbed.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_fchdir_np(fd: libc::c_int) -> libc::c_int;
}

/// Switches the *calling* thread's working directory to `fd` on macOS.
///
/// This is a macOS-only primitive. Linux expresses a descriptor-relative bind
/// through `/proc/self/fd/<fd>` instead, and every other Unix target has no
/// verified way to make the kernel resolve a socket bind against a descriptor,
/// so it reports `Unsupported` and the managed bind fails closed rather than
/// falling back to a path a concurrent rename can redirect.
#[cfg(target_os = "macos")]
pub(crate) fn set_thread_directory(fd: libc::c_int) -> io::Result<()> {
    if unsafe { pthread_fchdir_np(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_thread_directory(_fd: libc::c_int) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a per-thread working directory is not available on this platform",
    ))
}

/// The dirfd-relative socket path to bind, expressed in a form the kernel
/// resolves through the descriptor itself rather than through a directory name.
///
/// Linux exposes the descriptor as `/proc/self/fd/<fd>`. macOS and every other
/// Unix target have no equivalent path (`/dev/fd/<n>/name` returns `ENOENT` for
/// a socket bind on macOS), so they return `None` and the caller must use its
/// platform's descriptor-anchored bind primitive or fail closed. This never
/// returns the absolute path derived from the endpoint, which is exactly the
/// string a concurrent rename can redirect.
pub(crate) fn dirfd_relative_socket_path(
    directory: &EndpointDirectory,
    socket_name: &OsStr,
) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let _ = directory;
        let mut path = PathBuf::from(format!("/proc/self/fd/{}", directory.fd()));
        path.push(socket_name);
        Some(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        let _ = socket_name;
        None
    }
}

/// Binds a Unix stream socket named `socket_name` inside `directory` through a
/// *dedicated short-lived thread* whose own working directory is switched to
/// the verified descriptor for the duration of the `bind` and then destroyed
/// with the thread.
///
/// This is the macOS primitive. `unix_common::dirfd_relative_socket_path`
/// returns `None` there because `/dev/fd/<n>/name` is not resolvable for a
/// socket bind, so the only descriptor-anchored option is
/// `pthread_fchdir_np(dirfd)`. That switch is per-thread, and this function
/// owns the thread it switches: it never consults, alters, or restores the
/// *caller's* thread directory, so a thread that already had its own working
/// directory keeps it exactly. The thread ends immediately after the bind is
/// attempted, which destroys its thread-local directory state with it.
///
/// The raw descriptor is duplicated before it crosses the thread boundary so
/// the bind thread owns a descriptor whose lifetime it fully controls; the
/// duplicate is closed when the thread returns. The freshly bound listener is
/// sent back as a `std::os::unix::net::UnixListener` and registered with the
/// caller's reactor afterwards, so no descriptor is ever polled from two
/// reactors.
///
/// Every target that is not macOS reports `Unsupported`; there is deliberately
/// no fallback to the absolute path, which a concurrent rename can redirect.
#[cfg(target_os = "macos")]
pub(crate) fn bind_in_directory_on_thread(
    directory: &EndpointDirectory,
    socket_name: &OsStr,
) -> io::Result<std::os::unix::net::UnixListener> {
    let name = CString::new(socket_name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket name contains NUL"))?;
    let thread_dir = duplicate_directory_descriptor(directory)?;
    let joined = std::thread::Builder::new()
        .name("c2-managed-bind".to_owned())
        .spawn(move || {
            // `thread_dir` is owned by this thread; the guard closes it when the
            // thread returns, and its thread-local cwd dies with the thread.
            let thread_dir = thread_dir;
            if let Err(error) = set_thread_directory(thread_dir.as_raw_fd()) {
                return Err(error);
            }
            let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = name.as_bytes();
            if bytes.len() >= addr.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "socket name does not fit in sockaddr_un",
                ));
            }
            for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
                *slot = *byte as libc::c_char;
            }
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // Set close-on-exec explicitly: macOS does not accept
            // `SOCK_CLOEXEC` in the socket type.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                }
                return Err(error);
            }
            // SAFETY: `fd` is a fresh descriptor owned here until either the
            // bind fails (closed below) or it is handed to UnixListener.
            let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
            let length = (std::mem::size_of::<libc::sa_family_t>() + 1 + name.as_bytes().len() + 1)
                as libc::socklen_t;
            if unsafe {
                libc::bind(
                    socket.as_raw_fd(),
                    (&addr as *const libc::sockaddr_un).cast(),
                    length,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::listen(socket.as_raw_fd(), 128) } != 0 {
                let error = io::Error::last_os_error();
                // Withdraw the just-bound socket so a failed listen never leaves
                // this call's object behind.
                unsafe {
                    libc::unlinkat(thread_dir.as_raw_fd(), name.as_ptr(), 0);
                }
                return Err(error);
            }
            Ok(std::os::unix::net::UnixListener::from(socket))
        });
    let handle = match joined {
        Ok(handle) => handle,
        Err(error) => return Err(error),
    };
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::Other,
            "managed namespace bind thread panicked",
        )),
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn bind_in_directory_on_thread(
    _directory: &EndpointDirectory,
    _socket_name: &OsStr,
) -> io::Result<std::os::unix::net::UnixListener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-anchored socket bind is only available on macOS",
    ))
}

/// Duplicates a directory descriptor with close-on-exec so a thread can own it
/// independently of the `EndpointDirectory` that created it.
#[cfg(target_os = "macos")]
fn duplicate_directory_descriptor(directory: &EndpointDirectory) -> io::Result<File> {
    let fd = unsafe { libc::fcntl(directory.fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a fresh descriptor that File now owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn stat_is(stat: &libc::stat, file_type: libc::mode_t) -> bool {
    stat.st_mode as libc::mode_t & libc::S_IFMT == file_type
}

#[derive(Clone)]
pub(crate) struct EndpointNames {
    pub(crate) socket: CString,
    pub(crate) socket_os: OsString,
    pub(crate) lock: CString,
}

fn lock_entry_matches(
    directory: &EndpointDirectory,
    name: &CString,
    opened: &libc::stat,
) -> io::Result<bool> {
    Ok(directory
        .stat(name)?
        .is_some_and(|current| stat_is(&current, libc::S_IFREG) && same_file(opened, &current)))
}

pub(crate) fn socket_unverified(stat: &libc::stat) -> Option<EndpointUnverifiedReason> {
    if stat_is(stat, libc::S_IFLNK) {
        Some(EndpointUnverifiedReason::Symlink)
    } else if !stat_is(stat, libc::S_IFSOCK) {
        Some(EndpointUnverifiedReason::UnexpectedObject)
    } else if stat.st_uid != unsafe { libc::geteuid() } {
        Some(EndpointUnverifiedReason::ForeignOwner)
    } else {
        None
    }
}

fn io_unverified(error: &io::Error) -> Option<EndpointUnverifiedReason> {
    match error.raw_os_error() {
        Some(libc::ELOOP) => Some(EndpointUnverifiedReason::Symlink),
        _ => None,
    }
}

pub(crate) fn unlink_verified(
    directory: &EndpointDirectory,
    names: &EndpointNames,
    opened_lock: &libc::stat,
    expected: SocketIdentity,
) -> EndpointReapResult {
    if !directory.path_still_names_open_directory()
        || !lock_entry_matches(directory, &names.lock, opened_lock).unwrap_or(false)
    {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidOwnership);
    }
    let current = match directory.stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointReapResult::AlreadyAbsent,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    if let Some(reason) = socket_unverified(&current) {
        return EndpointReapResult::Unverified(reason);
    }
    if SocketIdentity::from_stat(&current) != expected {
        return EndpointReapResult::StaleTarget;
    }
    match directory.unlink(&names.socket) {
        Ok(()) => EndpointReapResult::Reaped,
        Err(error) if error.kind() == io::ErrorKind::NotFound => EndpointReapResult::AlreadyAbsent,
        Err(error) => EndpointReapResult::IoError(error),
    }
}

/// Withdraws one freshly bound socket when the rest of initialization fails.
///
/// The guard is armed only after `bind` succeeded and the entry was captured
/// through the verified directory descriptor. It deliberately does not read or
/// trust the owner record: a failed record write is one of the failures it
/// must roll back. The captured identity is device/inode/type/owner rather than
/// the full record identity because `fchmodat` rewrites ctime before the record
/// is written, and the filesystem inode is taken from the directory entry
/// instead of the listening descriptor.
pub(crate) struct BoundSocketGuard<'a> {
    directory: &'a EndpointDirectory,
    names: &'a EndpointNames,
    ownership: &'a File,
    device: u64,
    inode: u64,
    armed: bool,
}

impl<'a> BoundSocketGuard<'a> {
    pub(crate) fn capture(
        directory: &'a EndpointDirectory,
        names: &'a EndpointNames,
        ownership: &'a File,
    ) -> io::Result<Self> {
        let stat = directory
            .stat(&names.socket)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bound socket disappeared"))?;
        if let Some(reason) = socket_unverified(&stat) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bound endpoint is unsafe: {reason:?}"),
            ));
        }
        Ok(Self {
            directory,
            names,
            ownership,
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            armed: true,
        })
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    fn withdraw(&self) -> EndpointReapResult {
        if !self.directory.path_still_names_open_directory() {
            return EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory);
        }
        let current = match self.directory.stat(&self.names.socket) {
            Ok(Some(stat)) => stat,
            Ok(None) => return EndpointReapResult::AlreadyAbsent,
            Err(error) => return EndpointReapResult::IoError(error),
        };
        if !stat_is(&current, libc::S_IFSOCK)
            || current.st_uid != unsafe { libc::geteuid() }
            || current.st_dev as u64 != self.device
            || current.st_ino as u64 != self.inode
        {
            // A replacement object is never removed by rollback.
            return EndpointReapResult::StaleTarget;
        }
        let opened_lock = match fstat(self.ownership.as_raw_fd()) {
            Ok(stat) => stat,
            Err(error) => return EndpointReapResult::IoError(error),
        };
        unlink_verified(
            self.directory,
            self.names,
            &opened_lock,
            SocketIdentity::from_stat(&current),
        )
    }
}

impl Drop for BoundSocketGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Rollback is best effort: the original initialization error is the
            // result the caller must see. An unprovable entry stays in place and
            // remains visible as Unverified rather than being guessed away.
            let _ = self.withdraw();
            self.armed = false;
        }
    }
}

/// Initiates exactly one nonblocking connect using the verified directory.
/// Pending connections are completed on the caller's reactor, never in the
/// macOS directory thread. RAII owns every descriptor through the handoff.
pub(crate) fn start_connect_in_directory(
    directory: &EndpointDirectory,
    socket_name: &OsStr,
) -> io::Result<socket2::Socket> {
    if let Some(path) = dirfd_relative_socket_path(directory, socket_name) {
        return start_nonblocking_connect(&path);
    }
    #[cfg(target_os = "macos")]
    {
        let thread_dir = duplicate_directory_descriptor(directory)?;
        let name = socket_name.to_owned();
        std::thread::Builder::new()
            .name("c2-local-connect".to_owned())
            .spawn(move || {
                set_thread_directory(thread_dir.as_raw_fd())?;
                start_nonblocking_connect(Path::new(&name))
            })?
            .join()
            .map_err(|_| io::Error::other("local endpoint connect thread panicked"))?
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative socket connect is unavailable",
        ))
    }
}

fn start_nonblocking_connect(path: &Path) -> io::Result<socket2::Socket> {
    // SockAddr validates the actual short argument against sun_path capacity.
    let address = socket2::SockAddr::unix(path)?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&address) {
        Ok(()) => Ok(socket),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINPROGRESS | libc::EALREADY)
            ) =>
        {
            Ok(socket)
        }
        Err(error) => Err(error),
    }
}

/// Denial-only native socket replacement probe.
///
/// `true` means a live or undecidable listener owns the path, so replacement
/// must be refused. `false` means the kernel reported a dead rendezvous; a
/// failed connect is never positive proof of death by itself, which is why the
/// caller still requires a matching owner record for the exact inode.
pub(crate) fn probe_listener_is_live(
    directory: &EndpointDirectory,
    socket_name: &OsStr,
) -> io::Result<bool> {
    match start_connect_in_directory(directory, socket_name) {
        Ok(_socket) => Ok(true),
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || matches!(
                    error.raw_os_error(),
                    Some(libc::EINPROGRESS | libc::EALREADY)
                ) =>
        {
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

static SWEEP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Process-wide lease so one process never runs two maintenance sweeps.
pub(crate) struct SweepLease;

impl SweepLease {
    pub(crate) fn acquire() -> io::Result<Self> {
        SWEEP_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "an endpoint maintenance sweep is already active in this process",
                )
            })
    }
}

impl Drop for SweepLease {
    fn drop(&mut self) {
        SWEEP_ACTIVE.store(false, Ordering::Release);
    }
}

pub(crate) fn set_socket_permissions(
    directory: &EndpointDirectory,
    names: &EndpointNames,
) -> io::Result<()> {
    #[cfg(test)]
    if fault::take_if(fault::Failure::SocketPermissions) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected socket permission failure",
        ));
    }
    if !directory.path_still_names_open_directory() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint directory changed",
        ));
    }
    let current = directory
        .stat(&names.socket)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bound socket disappeared"))?;
    if let Some(reason) = socket_unverified(&current) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bound endpoint is unsafe: {reason:?}"),
        ));
    }
    if unsafe {
        libc::fchmodat(
            directory.fd(),
            names.socket.as_ptr(),
            0o600,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let changed = directory
        .stat(&names.socket)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bound socket disappeared"))?;
    if !same_file(&current, &changed) || !stat_is(&changed, libc::S_IFSOCK) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bound socket was replaced",
        ));
    }
    Ok(())
}
