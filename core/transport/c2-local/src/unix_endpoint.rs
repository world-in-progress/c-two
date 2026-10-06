//! Identity checked operations for the v1 Unix endpoint namespace.

use crate::{
    EndpointCredential, EndpointInspection, EndpointIoError, EndpointReapResult,
    EndpointUnverifiedReason, LocalEndpoint, SweepBatch, SweepBudget, UnixSocketIdentity,
};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, ReadDir};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const RECORD_MAGIC: &[u8; 8] = b"c2sock01";
const RECORD_SIZE: usize = 40;

/// Test-only, per-thread failure injection for bind rollback coverage.
///
/// The injection is deliberately thread local and consumed once so the normal
/// parallel test harness never observes a failure it did not request.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Failure {
        /// `fchmodat` on the freshly bound socket fails.
        SocketPermissions,
        /// The v1 identity record cannot be written.
        RecordWrite,
        /// The record cannot be written because a foreign object replaced the
        /// bound socket entry first.
        RecordWriteAfterReplacement,
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

    pub(crate) fn read(file: &File) -> io::Result<Option<Self>> {
        if file.metadata()?.len() != RECORD_SIZE as u64 {
            return Ok(None);
        }
        let mut record = [0_u8; RECORD_SIZE];
        // Positional I/O keeps the record read independent of shared file
        // offsets, so an ownership handle can be borrowed immutably.
        file.read_exact_at(&mut record, 0)?;
        if &record[..8] != RECORD_MAGIC {
            return Ok(None);
        }
        Ok(Some(Self {
            device: u64::from_le_bytes(record[8..16].try_into().unwrap()),
            inode: u64::from_le_bytes(record[16..24].try_into().unwrap()),
            changed_secs: i64::from_le_bytes(record[24..32].try_into().unwrap()),
            changed_nanos: i64::from_le_bytes(record[32..40].try_into().unwrap()),
        }))
    }

    pub(crate) fn write(self, file: &File) -> io::Result<()> {
        let mut record = [0_u8; RECORD_SIZE];
        record[..8].copy_from_slice(RECORD_MAGIC);
        record[8..16].copy_from_slice(&self.device.to_le_bytes());
        record[16..24].copy_from_slice(&self.inode.to_le_bytes());
        record[24..32].copy_from_slice(&self.changed_secs.to_le_bytes());
        record[32..40].copy_from_slice(&self.changed_nanos.to_le_bytes());
        file.write_all_at(&record, 0)?;
        file.set_len(RECORD_SIZE as u64)
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
    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint, create: bool) -> io::Result<Self> {
        let path = Path::new(endpoint.os_name())
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no parent"))?;
        Self::open(path, create)
    }

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

/// Creates a missing managed namespace with an explicit 0700 mode.
///
/// `fs::create_dir_all` applies the process umask to 0777, so a permissive
/// 002/000 umask would create a group- or world-writable directory that this
/// same module then refuses to open. The explicit mode keeps a new namespace
/// private without ever relaxing a pre-existing directory: an existing unsafe
/// directory remains an explicit error instead of being silently chmod-ed.
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
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
/// This is the macOS primitive. `unix_endpoint::dirfd_relative_socket_path`
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

/// Duplicates a directory descriptor with `dup(2)` so a thread can own it
/// independently of the `EndpointDirectory` that created it.
#[cfg(target_os = "macos")]
fn duplicate_directory_descriptor(directory: &EndpointDirectory) -> io::Result<File> {
    let fd = unsafe { libc::dup(directory.fd()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` returned a fresh descriptor that File now owns.
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

pub(crate) fn endpoint_names(endpoint: &LocalEndpoint) -> io::Result<EndpointNames> {
    let socket_path = Path::new(endpoint.os_name());
    let socket_os = socket_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no file name"))?
        .to_owned();
    let lock_os = socket_path.with_extension("lock");
    let lock_os = lock_os
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no lock name"))?;
    Ok(EndpointNames {
        socket: CString::new(socket_os.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket name contains NUL"))?,
        socket_os,
        lock: CString::new(lock_os.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock name contains NUL"))?,
    })
}

pub(crate) struct Ownership(pub(crate) File);

impl Drop for Ownership {
    fn drop(&mut self) {
        // Release the kernel lock before an inherited duplicate descriptor
        // closes. This preserves the existing listener/restart ordering.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn ownership_lock(
    directory: &EndpointDirectory,
    name: &CString,
) -> io::Result<Ownership> {
    if !directory.path_still_names_open_directory() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed endpoint directory changed before ownership lock open",
        ));
    }
    let file = directory
        .open_file(name, libc::O_RDWR | libc::O_CREAT, 0o600)?
        .expect("O_CREAT openat must return a file");
    verify_lock_file(directory, name, &file)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                "IPC endpoint listener ownership is already held",
            )
        } else {
            error
        });
    }
    Ok(Ownership(file))
}

fn verify_lock_file(
    directory: &EndpointDirectory,
    name: &CString,
    file: &File,
) -> io::Result<libc::stat> {
    let opened = fstat(file.as_raw_fd())?;
    let current = directory.stat(name)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "endpoint ownership file disappeared",
        )
    })?;
    if !stat_is(&opened, libc::S_IFREG)
        || opened.st_uid != unsafe { libc::geteuid() }
        || opened.st_mode as libc::mode_t & 0o077 != 0
        || !same_file(&opened, &current)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint ownership file is not a private regular file",
        ));
    }
    Ok(opened)
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

fn lock_unverified(stat: &libc::stat) -> Option<EndpointUnverifiedReason> {
    if stat_is(stat, libc::S_IFLNK) {
        Some(EndpointUnverifiedReason::Symlink)
    } else if !stat_is(stat, libc::S_IFREG) || stat.st_mode as libc::mode_t & 0o077 != 0 {
        Some(EndpointUnverifiedReason::InvalidOwnership)
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

fn directory_inspection_error(error: io::Error) -> EndpointInspection {
    if error.kind() == io::ErrorKind::NotFound {
        EndpointInspection::Absent
    } else if error.raw_os_error() == Some(libc::ELOOP) {
        EndpointInspection::Unverified(EndpointUnverifiedReason::Symlink)
    } else if error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::InvalidData
    {
        EndpointInspection::Unverified(EndpointUnverifiedReason::UnsafeDirectory)
    } else {
        EndpointInspection::IoError(error)
    }
}

fn directory_reap_error(error: io::Error) -> EndpointReapResult {
    if error.kind() == io::ErrorKind::NotFound {
        EndpointReapResult::AlreadyAbsent
    } else if error.raw_os_error() == Some(libc::ELOOP) {
        EndpointReapResult::Unverified(EndpointUnverifiedReason::Symlink)
    } else if error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::InvalidData
    {
        EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory)
    } else {
        EndpointReapResult::IoError(error)
    }
}

pub(crate) fn inspect_endpoint(endpoint: &LocalEndpoint) -> EndpointInspection {
    let directory = match EndpointDirectory::for_endpoint(endpoint, false) {
        Ok(directory) => directory,
        Err(error) => return directory_inspection_error(error),
    };
    inspect_in(&directory, endpoint)
}

pub(crate) fn inspect_in(
    directory: &EndpointDirectory,
    endpoint: &LocalEndpoint,
) -> EndpointInspection {
    if !directory.path_still_names_open_directory() {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::UnsafeDirectory);
    }
    let names = match endpoint_names(endpoint) {
        Ok(names) => names,
        Err(error) => return EndpointInspection::IoError(error),
    };
    let socket_stat = match directory.stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointInspection::Absent,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointInspection::IoError(error),
                EndpointInspection::Unverified,
            );
        }
    };
    if let Some(reason) = socket_unverified(&socket_stat) {
        return EndpointInspection::Unverified(reason);
    }
    let lock_stat = match directory.stat(&names.lock) {
        Ok(Some(stat)) => stat,
        Ok(None) => {
            return EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership);
        }
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointInspection::IoError(error),
                EndpointInspection::Unverified,
            );
        }
    };
    if let Some(reason) = lock_unverified(&lock_stat) {
        return EndpointInspection::Unverified(reason);
    }
    let Some(lock) = (match directory.open_file(&names.lock, libc::O_RDONLY, 0) {
        Ok(lock) => lock,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointInspection::IoError(error),
                EndpointInspection::Unverified,
            );
        }
    }) else {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership);
    };
    let opened_lock = match verify_lock_file(directory, &names.lock, &lock) {
        Ok(stat) => stat,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointInspection::IoError(error),
                EndpointInspection::Unverified,
            );
        }
    };
    let identity = SocketIdentity::from_stat(&socket_stat);
    let recorded = match SocketIdentity::read(&lock) {
        Ok(Some(recorded)) => recorded,
        Ok(None) => return EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointInspection::IoError(error),
    };
    if recorded != identity {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    let current_socket = match directory.stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointInspection::Absent,
        Err(error) => return EndpointInspection::IoError(error),
    };
    if !same_file(&socket_stat, &current_socket)
        || SocketIdentity::from_stat(&current_socket) != identity
        || !lock_entry_matches(directory, &names.lock, &opened_lock).unwrap_or(false)
        || !directory.path_still_names_open_directory()
    {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    EndpointInspection::Present(EndpointCredential::unix(endpoint.clone(), identity))
}

pub(crate) fn reap_endpoint(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    if credential.endpoint() != endpoint {
        return EndpointReapResult::StaleTarget;
    }
    let directory = match EndpointDirectory::for_endpoint(endpoint, false) {
        Ok(directory) => directory,
        Err(error) => return directory_reap_error(error),
    };
    reap_in(&directory, endpoint, credential)
}

pub(crate) fn reap_in(
    directory: &EndpointDirectory,
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    if credential.endpoint() != endpoint {
        return EndpointReapResult::StaleTarget;
    }
    if !directory.path_still_names_open_directory() {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory);
    }
    let names = match endpoint_names(endpoint) {
        Ok(names) => names,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    let socket_stat = match directory.stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointReapResult::AlreadyAbsent,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    if let Some(reason) = socket_unverified(&socket_stat) {
        return EndpointReapResult::Unverified(reason);
    }
    let identity = SocketIdentity::from_stat(&socket_stat);
    if identity != credential.identity {
        return EndpointReapResult::StaleTarget;
    }
    let lock_stat = match directory.stat(&names.lock) {
        Ok(Some(stat)) => stat,
        Ok(None) => {
            return EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership);
        }
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    if let Some(reason) = lock_unverified(&lock_stat) {
        return EndpointReapResult::Unverified(reason);
    }
    let Some(lock) = (match directory.open_file(&names.lock, libc::O_RDONLY, 0) {
        Ok(lock) => lock,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    }) else {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership);
    };
    let opened_lock = match verify_lock_file(directory, &names.lock, &lock) {
        Ok(stat) => stat,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::WouldBlock {
            EndpointReapResult::Busy
        } else {
            EndpointReapResult::IoError(error)
        };
    }
    let current_socket = match directory.stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointReapResult::AlreadyAbsent,
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    if !same_file(&socket_stat, &current_socket)
        || SocketIdentity::from_stat(&current_socket) != credential.identity
    {
        return EndpointReapResult::StaleTarget;
    }
    if !directory.path_still_names_open_directory()
        || !lock_entry_matches(directory, &names.lock, &opened_lock).unwrap_or(false)
    {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidOwnership);
    }
    match SocketIdentity::read(&lock) {
        Ok(Some(recorded)) if recorded == credential.identity => {}
        Ok(Some(_)) => return EndpointReapResult::StaleTarget,
        Ok(None) => return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointReapResult::IoError(error),
    }
    unlink_verified(directory, &names, &opened_lock, credential.identity)
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

pub(crate) fn cleanup_listener_socket(
    directory: &EndpointDirectory,
    names: &EndpointNames,
    ownership: &File,
    expected: SocketIdentity,
) -> EndpointReapResult {
    let opened_lock = match verify_lock_file(directory, &names.lock, ownership) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership);
        }
        Err(error) => {
            return io_unverified(&error).map_or(
                EndpointReapResult::IoError(error),
                EndpointReapResult::Unverified,
            );
        }
    };
    match SocketIdentity::read(ownership) {
        Ok(Some(recorded)) if recorded == expected => {}
        Ok(Some(_)) => return EndpointReapResult::StaleTarget,
        Ok(None) => return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointReapResult::IoError(error),
    }
    unlink_verified(directory, names, &opened_lock, expected)
}

/// Withdraws one freshly bound socket when the rest of initialization fails.
///
/// The guard is armed only after `bind` succeeded and the entry was captured
/// through the verified directory descriptor. It deliberately does not read or
/// trust the v1 owner record: a failed record write is one of the failures it
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

/// Denial-only replacement probe shared by the v1 and managed protocols.
///
/// `true` means a live or undecidable listener owns the path, so replacement
/// must be refused. `false` means the kernel reported a dead rendezvous; a
/// failed connect is never positive proof of death by itself, which is why the
/// caller still requires a matching owner record for the exact inode.
pub(crate) fn probe_listener_is_live(path: &Path) -> io::Result<bool> {
    let probe = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    probe.set_nonblocking(true)?;
    match probe.connect(&socket2::SockAddr::unix(path)?) {
        Ok(()) => Ok(true),
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

/// Bind-time stale replacement uses the exact same identity, lock-file, and
/// dirfd checks as public reaping. The advisory connect only vetoes cleanup;
/// failure to connect is never the proof that authorizes unlink.
pub(crate) fn remove_stale_socket(
    directory: &EndpointDirectory,
    names: &EndpointNames,
    ownership: &File,
) -> io::Result<()> {
    let socket_stat = match directory.stat(&names.socket)? {
        Some(stat) => stat,
        None => return Ok(()),
    };
    if !stat_is(&socket_stat, libc::S_IFSOCK) || socket_stat.st_uid != unsafe { libc::geteuid() } {
        return Err(endpoint_in_use());
    }
    if probe_listener_is_live(&directory.socket_path(&names.socket_os))? {
        return Err(endpoint_in_use());
    }
    let identity = SocketIdentity::from_stat(&socket_stat);
    if SocketIdentity::read(ownership)? != Some(identity) {
        return Err(endpoint_in_use());
    }
    let names = names.clone();
    let opened_lock = fstat(ownership.as_raw_fd())?;
    match unlink_verified(directory, &names, &opened_lock, identity) {
        EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent => Ok(()),
        EndpointReapResult::IoError(error) => Err(error),
        _ => Err(endpoint_in_use()),
    }
}

fn endpoint_in_use() -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        "IPC endpoint already has an active or unverifiable listener",
    )
}

pub(crate) struct EndpointSweep {
    directory: EndpointDirectory,
    entries: Option<ReadDir>,
    finished: bool,
    interrupted: bool,
    _lease: Option<SweepLease>,
}

static SWEEP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Process-wide lease for explicit maintenance sweeps. Both the v1 and the
/// managed sweep share it so one process never runs two maintenance tasks.
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

/// Region identifier used only to derive the default managed namespace through
/// the same `LocalEndpoint` mapping every production address uses. It is never
/// bound and never names a directory of its own.
pub(crate) const SWEEP_NAMESPACE_PROBE: &str = "c2-endpoint-sweep";

impl EndpointSweep {
    /// Opens the default managed namespace derived from `LocalEndpoint`
    /// authority instead of a second hardcoded directory. A missing namespace
    /// is an error: a sweep never creates the directory it inspects.
    pub(crate) fn open() -> io::Result<Self> {
        let endpoint = LocalEndpoint::from_address(&format!("ipc://{SWEEP_NAMESPACE_PROBE}"))?;
        Self::for_endpoint(&endpoint)
    }

    /// Opens the managed namespace that owns `endpoint`.
    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let path = Path::new(endpoint.os_name()).parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint has no namespace directory",
            )
        })?;
        Self::open_path(path)
    }

    #[cfg(test)]
    pub(crate) fn open_at(path: &Path) -> io::Result<Self> {
        Self::open_path(path)
    }

    fn open_path(path: &Path) -> io::Result<Self> {
        let lease = SweepLease::acquire()?;
        let directory = EndpointDirectory::open(path, false)?;
        let entries = fs::read_dir(path)?;
        if !directory.path_still_names_open_directory() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed endpoint directory changed while opening sweep",
            ));
        }
        Ok(Self {
            directory,
            entries: Some(entries),
            finished: false,
            interrupted: false,
            _lease: Some(lease),
        })
    }

    fn finish(&mut self, interrupted: bool) {
        self.finished = true;
        self.interrupted = interrupted;
        self.entries.take();
        self._lease.take();
    }

    pub(crate) fn next_batch(&mut self, budget: SweepBudget) -> SweepBatch {
        if self.finished {
            // A replaced namespace can never be reported as a completed round,
            // including on every later call to this sweep.
            return SweepBatch {
                round_complete: !self.interrupted,
                round_interrupted: self.interrupted,
                namespace_changed: self.interrupted,
                ..SweepBatch::default()
            };
        }
        if !self.directory.path_still_names_open_directory() {
            self.finish(true);
            return SweepBatch {
                round_interrupted: true,
                namespace_changed: true,
                ..SweepBatch::default()
            };
        }
        let started = Instant::now();
        let limit = budget.max_entries.max(1);
        let mut batch = SweepBatch::default();
        while batch.entries_visited < limit
            && (batch.entries_visited == 0 || started.elapsed() < budget.max_duration)
        {
            let next = self
                .entries
                .as_mut()
                .expect("active sweep has an iterator")
                .next();
            match next {
                None => {
                    self.finish(false);
                    batch.round_complete = true;
                    break;
                }
                Some(Err(_error)) => {
                    batch.entries_visited += 1;
                    batch.io_errors += 1;
                    batch.last_io_error = Some(EndpointIoError::from(&_error));
                }
                Some(Ok(entry)) => {
                    batch.entries_visited += 1;
                    self.inspect_entry(entry.file_name(), &mut batch);
                }
            }
        }
        if !self.directory.path_still_names_open_directory() {
            self.finish(true);
            batch.namespace_changed = true;
            batch.round_interrupted = true;
            batch.round_complete = false;
        }
        batch
    }

    fn inspect_entry(&self, filename: OsString, batch: &mut SweepBatch) {
        let Some(filename) = filename.to_str() else {
            return;
        };
        let Some(region) = filename.strip_suffix(".sock") else {
            return;
        };
        let endpoint = match LocalEndpoint::from_address(&format!("ipc://{region}")) {
            Ok(endpoint) => endpoint,
            Err(_) => {
                batch.endpoints_examined += 1;
                batch.unverified += 1;
                return;
            }
        };
        if !matches!(endpoint_names(&endpoint), Ok(names) if names.socket_os == OsStr::new(filename))
        {
            batch.endpoints_examined += 1;
            batch.unverified += 1;
            return;
        }
        batch.endpoints_examined += 1;
        let inspection = inspect_in(&self.directory, &endpoint);
        let outcome = match inspection {
            EndpointInspection::Present(credential) => {
                reap_in(&self.directory, &endpoint, &credential)
            }
            EndpointInspection::Absent => EndpointReapResult::AlreadyAbsent,
            EndpointInspection::Unverified(reason) => EndpointReapResult::Unverified(reason),
            EndpointInspection::KernelManaged => EndpointReapResult::NotApplicable,
            EndpointInspection::IoError(error) => EndpointReapResult::IoError(error),
        };
        match outcome {
            EndpointReapResult::Reaped => batch.reaped += 1,
            EndpointReapResult::AlreadyAbsent => batch.already_absent += 1,
            EndpointReapResult::Busy => batch.busy += 1,
            EndpointReapResult::StaleTarget => batch.stale_target += 1,
            EndpointReapResult::Unverified(_) => batch.unverified += 1,
            EndpointReapResult::IoError(error) => {
                batch.io_errors += 1;
                batch.last_io_error = Some(EndpointIoError::from(&error));
            }
            EndpointReapResult::NotApplicable => batch.not_applicable += 1,
        }
    }
}

pub(crate) fn endpoint_directory_error_is_unverified(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
        || error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::InvalidData
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

pub(crate) fn record_bound_socket(
    directory: &EndpointDirectory,
    names: &EndpointNames,
    ownership: &File,
) -> io::Result<SocketIdentity> {
    if !directory.path_still_names_open_directory() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint directory changed",
        ));
    }
    let stat = directory
        .stat(&names.socket)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bound socket disappeared"))?;
    if let Some(reason) = socket_unverified(&stat) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bound endpoint is unsafe: {reason:?}"),
        ));
    }
    let identity = SocketIdentity::from_stat(&stat);
    #[cfg(test)]
    if fault::take_if(fault::Failure::RecordWriteAfterReplacement) {
        directory.replace_entry_with_file_for_test(&names.socket)?;
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "injected endpoint record failure after replacement",
        ));
    }
    #[cfg(test)]
    if fault::take_if(fault::Failure::RecordWrite) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "injected endpoint record write failure",
        ));
    }
    identity.write(ownership)?;
    if !directory.path_still_names_open_directory()
        || directory.stat(&names.socket)?.is_none_or(|current| {
            !same_file(&stat, &current) || SocketIdentity::from_stat(&current) != identity
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bound socket was replaced while recording identity",
        ));
    }
    Ok(identity)
}

pub(crate) fn inspect_error_is_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

pub(crate) fn inspection_error(error: io::Error) -> EndpointInspection {
    if inspect_error_is_absent(&error) {
        EndpointInspection::Absent
    } else if endpoint_directory_error_is_unverified(&error) {
        EndpointInspection::Unverified(if error.raw_os_error() == Some(libc::ELOOP) {
            EndpointUnverifiedReason::Symlink
        } else {
            EndpointUnverifiedReason::UnsafeDirectory
        })
    } else {
        EndpointInspection::IoError(error)
    }
}

pub(crate) fn reap_error(error: io::Error) -> EndpointReapResult {
    if inspect_error_is_absent(&error) {
        EndpointReapResult::AlreadyAbsent
    } else if endpoint_directory_error_is_unverified(&error) {
        EndpointReapResult::Unverified(if error.raw_os_error() == Some(libc::ELOOP) {
            EndpointUnverifiedReason::Symlink
        } else {
            EndpointUnverifiedReason::UnsafeDirectory
        })
    } else {
        EndpointReapResult::IoError(error)
    }
}

pub(crate) fn bind_lock_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::InvalidData
        || error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(libc::ELOOP)
    {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            "IPC endpoint ownership is unverifiable",
        )
    } else {
        error
    }
}
