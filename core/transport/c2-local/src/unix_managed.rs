//! Managed v2 Unix endpoint namespace: one fixed coordinator gate, one lease
//! per live listener, and identity-checked retirement.
//!
//! The namespace is the `managed-v2` directory derived by
//! [`c2_config::LocalEndpoint`] (default `/tmp/c2-<uidhex>`). macOS keeps the
//! default namespace's coordination records in its non-temporary user directory;
//! other namespaces keep them beside the socket. Both locations are accessed
//! through verified directory descriptors:
//!
//! - `.gate` is the single long-lived coordinator lock. Every open, create,
//!   delete, and retire of an endpoint lease happens while holding it.
//! - `.gate.marker` records the gate device/inode and random identity so a
//!   replaced, missing, or damaged gate is detected instead of creating a
//!   second lock.
//! - `<sha256(address)>.lease` is the per-endpoint lease, locked exclusively by
//!   its listener for life. It stores the bounded owner record (protocol,
//!   complete logical address, fresh incarnation, socket identity).
//! - `<32-character endpoint id>` is the Unix socket.
//!
//! While the gate is held, an endpoint lease is only ever *tried*
//! (`LOCK_EX | LOCK_NB`), so a listener that holds its lease and is dropping can
//! never deadlock against a maintenance path that holds the gate.

use crate::unix_common::{
    BoundSocketGuard, EndpointDirectory, EndpointNames, SocketIdentity, SweepLease,
    dirfd_relative_socket_path, fstat, listen_verified_socket, probe_listener_is_live, same_file,
    set_socket_permissions, socket_unverified, stat_is,
};
use crate::{
    EndpointCredential, EndpointInspection, EndpointReapResult, EndpointUnverifiedReason,
    LocalEndpoint, LocalEndpointContext, SweepBatch, SweepBudget,
};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, ReadDir};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};

mod directories;
use directories::{DirectoryEntries, NamespaceDirectories};

pub(crate) const GATE_NAME: &str = ".gate";
pub(crate) const MARKER_NAME: &str = ".gate.marker";
pub(crate) const LEASE_SUFFIX: &str = ".lease";

/// Test-only rendezvous that pauses an opener *inside* the gate-hold window,
/// so a negative test can perform a real concurrent directory rename at the
/// exact instant the P1 blocker describes. It is thread-scoped, one-shot, and
/// bound to one namespace root: the default path never waits, unrelated roots
/// never observe it, and the parallel harness keeps running.
#[cfg(test)]
pub(crate) mod barrier {
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    struct Gate {
        root: Option<PathBuf>,
        entered: bool,
        released: bool,
    }

    fn state() -> &'static (Mutex<Gate>, Condvar) {
        static STATE: OnceLock<(Mutex<Gate>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| {
            (
                Mutex::new(Gate {
                    root: None,
                    entered: false,
                    released: false,
                }),
                Condvar::new(),
            )
        })
    }

    /// Arms the one-shot rendezvous for exactly `root`.
    pub(crate) fn arm(root: &Path) {
        let (lock, cvar) = state();
        let mut gate = lock.lock().unwrap();
        gate.root = Some(root.to_owned());
        gate.entered = false;
        gate.released = false;
        cvar.notify_all();
    }

    /// Disarms the rendezvous and wakes any waiter. Must run even when a test
    /// panics so a stuck opener cannot wedge the whole binary.
    pub(crate) fn disarm() {
        let (lock, cvar) = state();
        let mut gate = lock.lock().unwrap();
        gate.root = None;
        gate.released = true;
        cvar.notify_all();
    }

    /// Blocks until the test observes `entered` and calls `release`. Only the
    /// armed root participates.
    pub(crate) fn after_gate(root: &Path) {
        let (lock, cvar) = state();
        let mut gate = lock.lock().unwrap();
        if gate.root.as_deref() != Some(root) {
            return;
        }
        gate.entered = true;
        cvar.notify_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gate.released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Never hang a test binary: fail open after the bound.
                gate.root = None;
                break;
            }
            let (guard, timeout) = cvar.wait_timeout(gate, remaining).unwrap();
            gate = guard;
            if timeout.timed_out() && !gate.released {
                gate.root = None;
                break;
            }
        }
    }

    /// Waits until an opener is provably parked inside the gate-hold window.
    pub(crate) fn wait_entered() -> bool {
        let (lock, cvar) = state();
        let mut gate = lock.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gate.entered && gate.root.is_some() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = cvar.wait_timeout(gate, remaining).unwrap();
            gate = guard;
        }
        gate.entered
    }

    pub(crate) fn release() {
        let (lock, cvar) = state();
        let mut gate = lock.lock().unwrap();
        gate.released = true;
        cvar.notify_all();
    }
}

const GATE_MAGIC: &[u8; 8] = b"c2mgatv2";
// Version 1 had only dev/ino and could accept a recycled inode. Existing
// markers are never upgraded online: their namespace must remain unverified.
const GATE_VERSION: u8 = 2;
const GATE_IDENTITY_LEN: usize = 8 + 1 + 16;
const MARKER_LEN: usize = 8 + 1 + 8 + 8 + 16;

const RECORD_MAGIC: &[u8; 8] = b"c2mgrec2";
const RECORD_VERSION: u8 = 2;
const RECORD_PROTOCOL: u8 = 2;
const RECORD_ADDRESS_LIMIT: usize = 1024;
const RECORD_FIXED_LEN: usize = 8 + 1 + 1 + 2 + 16 + 8 + 8 + 8 + 8;
const RECORD_MAX_LEN: usize = RECORD_FIXED_LEN + RECORD_ADDRESS_LIMIT;

/// A bounded, self-describing owner record written into an endpoint lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OwnerRecord {
    pub(crate) address: String,
    pub(crate) incarnation: [u8; 16],
    pub(crate) identity: SocketIdentity,
}

impl OwnerRecord {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let address = self.address.as_bytes();
        if address.len() > RECORD_ADDRESS_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed endpoint address exceeds the record bound",
            ));
        }
        let mut bytes = Vec::with_capacity(RECORD_FIXED_LEN + address.len());
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.push(RECORD_VERSION);
        bytes.push(RECORD_PROTOCOL);
        bytes.extend_from_slice(&(address.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&self.incarnation);
        bytes.extend_from_slice(&self.identity.device.to_le_bytes());
        bytes.extend_from_slice(&self.identity.inode.to_le_bytes());
        bytes.extend_from_slice(&self.identity.changed_secs.to_le_bytes());
        bytes.extend_from_slice(&self.identity.changed_nanos.to_le_bytes());
        bytes.extend_from_slice(address);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_FIXED_LEN || bytes.len() > RECORD_MAX_LEN {
            return None;
        }
        if &bytes[..8] != RECORD_MAGIC || bytes[8] != RECORD_VERSION || bytes[9] != RECORD_PROTOCOL
        {
            return None;
        }
        let address_len = u16::from_le_bytes(bytes[10..12].try_into().ok()?) as usize;
        if address_len > RECORD_ADDRESS_LIMIT || bytes.len() != RECORD_FIXED_LEN + address_len {
            return None;
        }
        let mut incarnation = [0_u8; 16];
        incarnation.copy_from_slice(&bytes[12..28]);
        let identity = SocketIdentity {
            device: u64::from_le_bytes(bytes[28..36].try_into().ok()?),
            inode: u64::from_le_bytes(bytes[36..44].try_into().ok()?),
            changed_secs: i64::from_le_bytes(bytes[44..52].try_into().ok()?),
            changed_nanos: i64::from_le_bytes(bytes[52..60].try_into().ok()?),
        };
        let address = std::str::from_utf8(&bytes[RECORD_FIXED_LEN..]).ok()?;
        Some(Self {
            address: address.to_owned(),
            incarnation,
            identity,
        })
    }
}

fn gate_name() -> CString {
    CString::new(GATE_NAME).expect("static gate name")
}

fn marker_name() -> CString {
    CString::new(MARKER_NAME).expect("static marker name")
}

fn read_bounded(file: &File, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let len = file.metadata()?.len();
    if len == 0 || len > limit as u64 {
        return Ok(None);
    }
    let mut bytes = vec![0_u8; len as usize];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(Some(bytes))
}

fn read_record(file: &File) -> io::Result<Option<OwnerRecord>> {
    Ok(read_bounded(file, RECORD_MAX_LEN)?.and_then(|bytes| OwnerRecord::decode(&bytes)))
}

fn write_record(file: &File, record: &OwnerRecord) -> io::Result<()> {
    let bytes = record.encode()?;
    #[cfg(test)]
    if crate::unix_common::fault::take_if(crate::unix_common::fault::Failure::ManagedRecordWrite) {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "injected managed owner record write failure",
        ));
    }
    if let Some(limit) = partial_write_limit(&bytes) {
        file.write_all_at(&bytes[..limit], 0)?;
        return file.set_len(limit as u64);
    }
    file.write_all_at(&bytes, 0)?;
    file.set_len(bytes.len() as u64)
}

/// A partial write is written through the same positional API and truncated, so
/// the subsequent read-back is a real short record rather than a simulated
/// error code.
#[cfg(test)]
fn partial_write_limit(bytes: &[u8]) -> Option<usize> {
    if crate::unix_common::fault::take_if(
        crate::unix_common::fault::Failure::ManagedRecordPartialWrite,
    ) {
        Some(bytes.len().saturating_sub(1).max(1))
    } else {
        None
    }
}

#[cfg(not(test))]
fn partial_write_limit(_bytes: &[u8]) -> Option<usize> {
    None
}

fn endpoint_stem(endpoint: &LocalEndpoint) -> io::Result<OsString> {
    let name = Path::new(endpoint.os_name())
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no file name"))?;
    let stem = name.as_bytes();
    if !is_endpoint_stem(stem) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid endpoint file name",
        ));
    }
    Ok(OsString::from_vec(stem.to_vec()))
}

fn is_endpoint_stem(bytes: &[u8]) -> bool {
    bytes.len() == 32
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Endpoint names for the managed protocol. `lock` carries the per-endpoint
/// lease name so the shared identity-check helpers apply unchanged; the v1
/// `.lock` file is never part of this namespace.
pub(crate) fn managed_names(endpoint: &LocalEndpoint) -> io::Result<EndpointNames> {
    let socket_os = Path::new(endpoint.os_name())
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint has no file name"))?
        .to_owned();
    let stem = endpoint_stem(endpoint)?;
    let mut lease = stem.into_vec();
    lease.extend_from_slice(LEASE_SUFFIX.as_bytes());
    let lease_os = OsString::from_vec(lease);
    Ok(EndpointNames {
        socket: CString::new(socket_os.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket name contains NUL"))?,
        socket_os,
        lock: CString::new(lease_os.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lease name contains NUL"))?,
    })
}

/// Confirms that a record really maps back to the endpoint basename, so a hash
/// basename is never treated as a logical identity by itself. The mapping is
/// re-derived through the canonical `LocalEndpoint` authority instead of
/// reimplementing its digest here.
fn record_matches_endpoint(record: &OwnerRecord, endpoint: &LocalEndpoint) -> bool {
    if record.address != endpoint.address() {
        return false;
    }
    let Ok(derived) = endpoint.context().endpoint(&record.address) else {
        return false;
    };
    derived.os_name() == endpoint.os_name()
}

/// Failure classes for opening the managed namespace.
#[derive(Debug)]
pub(crate) enum NamespaceError {
    Absent,
    Busy,
    Unverified(EndpointUnverifiedReason),
    Io(io::Error),
}

impl From<io::Error> for NamespaceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn directory_error(error: io::Error, create: bool) -> NamespaceError {
    if error.kind() == io::ErrorKind::NotFound && !create {
        NamespaceError::Absent
    } else {
        NamespaceError::Io(error)
    }
}

/// A verified managed namespace with the fixed coordinator gate held.
pub(crate) struct ManagedNamespace {
    directories: NamespaceDirectories,
    gate: File,
}

impl Drop for ManagedNamespace {
    fn drop(&mut self) {
        // Release the coordinator lock before the descriptor closes so an
        // inherited duplicate cannot keep the namespace locked.
        unsafe {
            libc::flock(self.gate.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl ManagedNamespace {
    /// Opens the final owner-controlled directory. Only the platform default
    /// can be created here; custom directories belong to the application.
    pub(crate) fn open_root(
        root: &Path,
        create: bool,
        blocking: bool,
    ) -> Result<Self, NamespaceError> {
        Self::open_root_locked(
            root,
            create,
            if blocking {
                libc::LOCK_EX
            } else {
                libc::LOCK_EX | libc::LOCK_NB
            },
        )
    }

    /// Identity readers share the existing coordinator. They create nothing;
    /// every namespace mutation keeps the original exclusive-lock path.
    fn open_existing_shared(root: &Path) -> Result<Self, NamespaceError> {
        Self::open_root_locked(root, false, libc::LOCK_SH | libc::LOCK_NB)
    }

    fn open_root_locked(
        root: &Path,
        create: bool,
        operation: libc::c_int,
    ) -> Result<Self, NamespaceError> {
        let directories = NamespaceDirectories::open(root, create)?;
        let directory = &directories.records;

        // A marker without its gate must never cause a second coordinator inode
        // to be created. The check happens before any O_CREAT attempt.
        let marker_stat = directory.stat(&marker_name()).map_err(NamespaceError::Io)?;
        let gate_entry = directory.stat(&gate_name()).map_err(NamespaceError::Io)?;
        if marker_stat.is_some() && gate_entry.is_none() {
            return Err(NamespaceError::Unverified(
                EndpointUnverifiedReason::CoordinatorMissing,
            ));
        }
        if !create && marker_stat.is_none() && gate_entry.is_none() {
            // A namespace that was never initialized carries no endpoint
            // identity. An empty one is simply absent; one that already holds
            // endpoint objects is the incomplete-initialization boundary.
            if has_endpoint_entries(directories.entries()?)? {
                return Err(NamespaceError::Unverified(
                    EndpointUnverifiedReason::InitializationIncomplete,
                ));
            }
            return Err(NamespaceError::Absent);
        }

        let gate = open_gate(&directory, create && marker_stat.is_none())?;
        lock_gate(&gate, operation)?;
        if !gate_entry_is_current(&directory, &gate) {
            return Err(NamespaceError::Unverified(
                EndpointUnverifiedReason::CoordinatorReplaced,
            ));
        }
        // The wait for the gate is an unbounded window: after it returns, the
        // directory entries that were verified above may already name different
        // directories. Re-check the final directory through its open descriptor
        // and drop the gate without adopting a redirected name.
        directories.validate().map_err(NamespaceError::Io)?;
        #[cfg(test)]
        barrier::after_gate(root);

        // Re-read the marker while holding the gate: this is the authoritative
        // view, so two concurrent first initializations cannot both write one.
        let marker_stat = directory.stat(&marker_name()).map_err(NamespaceError::Io)?;
        match marker_stat {
            Some(_) => validate_marker(&directory, &gate)?,
            None => {
                if !create {
                    return Err(NamespaceError::Unverified(
                        EndpointUnverifiedReason::InitializationIncomplete,
                    ));
                }
                if has_endpoint_entries(directories.entries()?)? {
                    // An endpoint exists without a marker: the initialization
                    // identity is incomplete. The gate is a fixed inode and is
                    // never unlinked, not even when this call created it: a
                    // drop-then-unlink window would let a third party bind a
                    // fresh coordinator at the same name. Leaving the gate in
                    // place keeps every later open on the same inode and keeps
                    // this namespace visibly Unverified instead of silently
                    // re-initializable.
                    return Err(NamespaceError::Unverified(
                        EndpointUnverifiedReason::InitializationIncomplete,
                    ));
                }
                write_marker(&directory, &gate)?;
                validate_marker(&directory, &gate)?;
            }
        }

        Ok(Self { directories, gate })
    }

    /// The exact directory that must receive this process's socket.
    /// Callers bind relative to this descriptor, never to the absolute path it
    /// was opened from.
    fn directory(&self) -> &EndpointDirectory {
        self.directories
            .sockets
            .as_deref()
            .expect("bind has a socket directory")
    }

    /// Confirms that the final directory still names its open descriptor and
    /// that the gate entry is still
    /// the coordinator inode. A renamed or replaced directory invalidates every
    /// operation that would otherwise act on it.
    fn gate_still_named(&self) -> bool {
        self.directories.validate().is_ok()
            && gate_entry_is_current(&self.directories.records, &self.gate)
    }
}

/// Native context identity, captured only while the verified namespace gate is held.
/// The gate pin is independently opened read-only, so it never shares a flock or a lease.
#[derive(Clone)]
pub(crate) struct ContextBinding {
    root: PathBuf,
    directories: NamespaceDirectories,
    gate_pin: Arc<File>,
    nonce: [u8; 16],
    digest: [u8; 32],
}

impl ContextBinding {
    pub(crate) fn capture(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let root = endpoint.context().unix_root().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "not a Unix endpoint context")
        })?;
        let namespace =
            ManagedNamespace::open_existing_shared(root).map_err(context_binding_error)?;
        Self::from_namespace(root, &namespace)
    }

    fn from_namespace(root: &Path, namespace: &ManagedNamespace) -> io::Result<Self> {
        namespace.directories.validate()?;
        if !namespace.gate_still_named() {
            return Err(changed_context());
        }
        validate_marker(&namespace.directories.records, &namespace.gate)
            .map_err(context_binding_error)?;
        // Do not dup the locked file description: an independent read-only open is the pin.
        let gate_pin = namespace
            .directories
            .records
            .open_file(&gate_name(), libc::O_RDONLY, 0)?
            .ok_or_else(changed_context)?;
        verify_gate_file(&gate_pin).map_err(context_binding_error)?;
        let original = fstat(namespace.gate.as_raw_fd())?;
        let pinned = fstat(gate_pin.as_raw_fd())?;
        let nonce = gate_nonce(&namespace.gate).map_err(context_binding_error)?;
        if !same_file(&original, &pinned)
            || gate_nonce(&gate_pin).map_err(context_binding_error)? != nonce
        {
            return Err(changed_context());
        }
        let digest = context_digest(&namespace.directories, &pinned, nonce)?;
        // Fence both roles and the marker again after opening the independent gate pin.
        namespace.directories.validate()?;
        if !gate_entry_is_current(&namespace.directories.records, &gate_pin) {
            return Err(changed_context());
        }
        validate_marker(&namespace.directories.records, &gate_pin)
            .map_err(context_binding_error)?;
        Ok(Self {
            root: root.to_owned(),
            directories: namespace.directories.clone(),
            gate_pin: Arc::new(gate_pin),
            nonce,
            digest,
        })
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        // Check the old pins first. Reopening the original pathname must never authorize a
        // replacement directory, even if that replacement is a valid managed namespace.
        self.directories.validate()?;
        verify_gate_file(&self.gate_pin).map_err(context_binding_error)?;
        if !gate_entry_is_current(&self.directories.records, &self.gate_pin)
            || gate_nonce(&self.gate_pin).map_err(context_binding_error)? != self.nonce
        {
            return Err(changed_context());
        }
        let namespace =
            ManagedNamespace::open_existing_shared(&self.root).map_err(context_binding_error)?;
        if !self.directories.same_as(&namespace.directories)?
            || !same_file(
                &fstat(self.gate_pin.as_raw_fd())?,
                &fstat(namespace.gate.as_raw_fd())?,
            )
            || gate_nonce(&namespace.gate).map_err(context_binding_error)? != self.nonce
            || context_digest(
                &namespace.directories,
                &fstat(namespace.gate.as_raw_fd())?,
                self.nonce,
            )? != self.digest
        {
            return Err(changed_context());
        }
        validate_marker(&self.directories.records, &self.gate_pin)
            .map_err(context_binding_error)?;
        self.directories.validate()
    }
}

fn context_digest(
    directories: &NamespaceDirectories,
    gate: &libc::stat,
    nonce: [u8; 16],
) -> io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    digest.update(b"c2.owner.unix-context.v1\0");
    // Even a colocated namespace encodes two roles. Paths and directory ctime are excluded:
    // aliases share the native identity and ordinary socket/lease churn must not change it.
    for (role, directory) in [
        (b"sockets\0".as_slice(), directories.socket_directory()?),
        (b"records\0".as_slice(), directories.records.as_ref()),
    ] {
        let stat = fstat(directory.fd())?;
        digest.update(role);
        digest.update((stat.st_dev as u64).to_le_bytes());
        digest.update((stat.st_ino as u64).to_le_bytes());
        digest.update(stat.st_uid.to_le_bytes());
    }
    digest.update(b"gate\0");
    digest.update((gate.st_dev as u64).to_le_bytes());
    digest.update((gate.st_ino as u64).to_le_bytes());
    digest.update(nonce);
    Ok(digest.finalize().into())
}

fn changed_context() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "native endpoint context identity changed",
    )
}

fn context_binding_error(error: NamespaceError) -> io::Error {
    match error {
        NamespaceError::Absent => {
            io::Error::new(io::ErrorKind::NotFound, "endpoint context is absent")
        }
        NamespaceError::Busy => {
            io::Error::new(io::ErrorKind::WouldBlock, "endpoint context gate is busy")
        }
        NamespaceError::Unverified(reason) => io::Error::new(
            io::ErrorKind::InvalidData,
            format!("endpoint context is unverifiable: {reason:?}"),
        ),
        NamespaceError::Io(error) => error,
    }
}

fn open_gate(directory: &EndpointDirectory, create: bool) -> Result<File, NamespaceError> {
    if create {
        match directory.open_file(
            &gate_name(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(Some(file)) => {
                verify_gate_file(&file)?;
                return Ok(file);
            }
            Ok(None) => unreachable!("O_CREAT openat must return a file"),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(NamespaceError::Io(error)),
        }
    }
    let file = directory
        .open_file(&gate_name(), libc::O_RDWR, 0o600)
        .map_err(NamespaceError::Io)?
        .ok_or(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorMissing,
        ))?;
    verify_gate_file(&file)?;
    Ok(file)
}

fn verify_gate_file(file: &File) -> Result<(), NamespaceError> {
    let stat = fstat(file.as_raw_fd()).map_err(NamespaceError::Io)?;
    if stat.st_uid != unsafe { libc::geteuid() } {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::ForeignOwner,
        ));
    }
    if !stat_is(&stat, libc::S_IFREG) || stat.st_mode as libc::mode_t & 0o077 != 0 {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::InvalidOwnership,
        ));
    }
    Ok(())
}

fn lock_gate(gate: &File, operation: libc::c_int) -> Result<(), NamespaceError> {
    if unsafe { libc::flock(gate.as_raw_fd(), operation) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Err(NamespaceError::Busy)
    } else {
        Err(NamespaceError::Io(error))
    }
}

fn gate_entry_is_current(directory: &EndpointDirectory, gate: &File) -> bool {
    let Ok(opened) = fstat(gate.as_raw_fd()) else {
        return false;
    };
    // The coordinator inode is a long-lived constant: it is never unlinked by
    // this protocol and a replacement entry is always unverifiable.
    directory
        .stat(&gate_name())
        .ok()
        .flatten()
        .is_some_and(|current| same_file(&opened, &current))
}

/// Bound on the first-initialization scan. A namespace with no identity marker
/// is only adoptable when it is provably empty of endpoint objects, so the scan
/// stops at the first endpoint entry; the cap keeps a hostile or accidental
/// directory from turning initialization into an unbounded traversal.
const INIT_SCAN_LIMIT: usize = 64;

/// Whether a namespace already holds endpoint objects.
///
/// Bounded to [`INIT_SCAN_LIMIT`] entries. Exceeding the cap fails closed: an
/// unenumerated directory cannot prove "no endpoint objects", and adopting it
/// would risk writing a first marker into a namespace whose real identity is
/// simply too large to inspect.
fn has_endpoint_entries(
    entries: impl Iterator<Item = io::Result<fs::DirEntry>>,
) -> io::Result<bool> {
    for (index, entry) in entries.enumerate() {
        if index >= INIT_SCAN_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed namespace first-initialization scan exceeded its bound",
            ));
        }
        let name = entry?.file_name();
        let bytes = name.as_bytes();
        if is_endpoint_stem(bytes)
            || bytes
                .strip_suffix(LEASE_SUFFIX.as_bytes())
                .is_some_and(is_endpoint_stem)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn gate_nonce(gate: &File) -> Result<[u8; 16], NamespaceError> {
    let bytes = read_bounded(gate, GATE_IDENTITY_LEN).map_err(NamespaceError::Io)?;
    let Some(bytes) = bytes.filter(|bytes| {
        bytes.len() == GATE_IDENTITY_LEN && &bytes[..8] == GATE_MAGIC && bytes[8] == GATE_VERSION
    }) else {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ));
    };
    Ok(bytes[9..25].try_into().unwrap())
}

fn write_marker(directory: &EndpointDirectory, gate: &File) -> Result<(), NamespaceError> {
    // Only first initialization, under the gate and after the bounded scan,
    // may assign an identity. A marker-bearing empty/replaced gate is rejected
    // by validate_marker and never reaches this path.
    if gate.metadata().map_err(NamespaceError::Io)?.len() == 0 {
        let mut identity = Vec::with_capacity(GATE_IDENTITY_LEN);
        identity.extend_from_slice(GATE_MAGIC);
        identity.push(GATE_VERSION);
        identity.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        gate.write_all_at(&identity, 0)
            .map_err(NamespaceError::Io)?;
        gate.sync_all().map_err(NamespaceError::Io)?;
    }
    let nonce = gate_nonce(gate)?;
    let stat = fstat(gate.as_raw_fd()).map_err(NamespaceError::Io)?;
    let mut bytes = Vec::with_capacity(MARKER_LEN);
    bytes.extend_from_slice(GATE_MAGIC);
    bytes.push(GATE_VERSION);
    bytes.extend_from_slice(&(stat.st_dev as u64).to_le_bytes());
    bytes.extend_from_slice(&(stat.st_ino as u64).to_le_bytes());
    bytes.extend_from_slice(&nonce);
    let mut file = directory
        .open_file(
            &marker_name(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(NamespaceError::Io)?
        .ok_or_else(|| {
            NamespaceError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "marker open returned no file",
            ))
        })?;
    let result = (|| -> io::Result<()> {
        file.write_all(&bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        let _ = directory.unlink(&marker_name());
        return Err(NamespaceError::Io(error));
    }
    Ok(())
}

fn validate_marker(directory: &EndpointDirectory, gate: &File) -> Result<(), NamespaceError> {
    let stat = directory
        .stat(&marker_name())
        .map_err(NamespaceError::Io)?
        .ok_or(NamespaceError::Unverified(
            EndpointUnverifiedReason::InitializationIncomplete,
        ))?;
    if stat_is(&stat, libc::S_IFLNK) {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::Symlink,
        ));
    }
    if !stat_is(&stat, libc::S_IFREG)
        || stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_mode as libc::mode_t & 0o077 != 0
    {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ));
    }
    let file = directory
        .open_file(&marker_name(), libc::O_RDONLY, 0)
        .map_err(NamespaceError::Io)?
        .ok_or(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ))?;
    let opened = fstat(file.as_raw_fd()).map_err(NamespaceError::Io)?;
    if !same_file(&stat, &opened) {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ));
    }
    let Some(bytes) = read_bounded(&file, MARKER_LEN).map_err(NamespaceError::Io)? else {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ));
    };
    let gate_stat = fstat(gate.as_raw_fd()).map_err(NamespaceError::Io)?;
    let decoded = bytes.len() == MARKER_LEN
        && &bytes[..8] == GATE_MAGIC
        && bytes[8] == GATE_VERSION
        && u64::from_le_bytes(bytes[9..17].try_into().unwrap()) == gate_stat.st_dev as u64
        && u64::from_le_bytes(bytes[17..25].try_into().unwrap()) == gate_stat.st_ino as u64
        && bytes[25..41] == gate_nonce(gate)?;
    if !decoded {
        return Err(NamespaceError::Unverified(
            EndpointUnverifiedReason::CoordinatorReplaced,
        ));
    }
    Ok(())
}

/// Verifies that a lease entry is a private regular file naming the open file.
fn verify_lease_entry(
    directory: &EndpointDirectory,
    name: &CString,
    file: &File,
) -> Result<libc::stat, EndpointUnverifiedReason> {
    let opened = fstat(file.as_raw_fd()).map_err(|_| EndpointUnverifiedReason::InvalidOwnership)?;
    if stat_is(&opened, libc::S_IFLNK) {
        return Err(EndpointUnverifiedReason::Symlink);
    }
    if !stat_is(&opened, libc::S_IFREG)
        || opened.st_uid != unsafe { libc::geteuid() }
        || opened.st_mode as libc::mode_t & 0o077 != 0
    {
        return Err(EndpointUnverifiedReason::InvalidOwnership);
    }
    let current = directory
        .stat(name)
        .ok()
        .flatten()
        .ok_or(EndpointUnverifiedReason::InvalidOwnership)?;
    if !same_file(&opened, &current) {
        return Err(EndpointUnverifiedReason::InvalidOwnership);
    }
    Ok(opened)
}

fn lease_entry_unverified(stat: &libc::stat) -> Option<EndpointUnverifiedReason> {
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

fn open_lease(directory: &EndpointDirectory, name: &CString) -> io::Result<Option<File>> {
    directory.open_file(name, libc::O_RDWR, 0o600)
}

fn try_lock_lease(file: &File) -> Result<(), EndpointReapResult> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(EndpointReapResult::Busy)
        } else {
            Err(EndpointReapResult::IoError(error))
        }
    }
}

fn namespace_reap_error(error: NamespaceError) -> EndpointReapResult {
    match error {
        NamespaceError::Absent => EndpointReapResult::AlreadyAbsent,
        NamespaceError::Busy => EndpointReapResult::Busy,
        NamespaceError::Unverified(reason) => EndpointReapResult::Unverified(reason),
        NamespaceError::Io(error) => EndpointReapResult::IoError(error),
    }
}

fn namespace_inspection_error(error: NamespaceError) -> EndpointInspection {
    match error {
        NamespaceError::Absent => EndpointInspection::Absent,
        NamespaceError::Busy => EndpointInspection::IoError(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed namespace gate is busy",
        )),
        NamespaceError::Unverified(reason) => EndpointInspection::Unverified(reason),
        NamespaceError::Io(error) => EndpointInspection::IoError(error),
    }
}

fn namespace_bind_error(error: NamespaceError) -> io::Error {
    match error {
        NamespaceError::Absent => io::Error::new(
            io::ErrorKind::NotFound,
            "managed endpoint namespace is absent",
        ),
        NamespaceError::Busy => io::Error::new(
            io::ErrorKind::AddrInUse,
            "managed endpoint namespace gate is busy",
        ),
        NamespaceError::Unverified(_) => io::Error::new(
            io::ErrorKind::AddrInUse,
            "managed endpoint namespace is unverifiable",
        ),
        NamespaceError::Io(error) => error,
    }
}

fn endpoint_in_use() -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        "managed IPC endpoint already has an active or unverifiable listener",
    )
}

fn identity_of(stat: &libc::stat) -> SocketIdentity {
    SocketIdentity::from_stat(stat)
}

fn socket_identity(
    directory: &EndpointDirectory,
    names: &EndpointNames,
) -> io::Result<SocketIdentity> {
    let stat = directory
        .stat(&names.socket)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bound socket disappeared"))?;
    if let Some(reason) = socket_unverified(&stat) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bound managed endpoint is unsafe: {reason:?}"),
        ));
    }
    Ok(identity_of(&stat))
}

/// Opens the coordinator gate with a short, bounded retry instead of an
/// unbounded wait.
///
/// A blocked bind is reachable from server startup, so waiting forever on a
/// coordinator another process holds would stall `Server::start`. The bounded
/// wait lets a transient same-address race resolve (the existing
/// winner/loser semantics), while a persistently held or otherwise busy gate
/// degrades to `Busy` and surfaces as `AddrInUse`. Retrying the whole start
/// attempt, with the caller's own timeout, is left to the layer above.
fn open_root_bounded(root: &Path, create: bool) -> Result<ManagedNamespace, NamespaceError> {
    const ATTEMPTS: usize = 50;
    let mut last = NamespaceError::Busy;
    for attempt in 0..ATTEMPTS {
        match ManagedNamespace::open_root(root, create, false) {
            Ok(namespace) => return Ok(namespace),
            Err(error @ NamespaceError::Busy) => {
                last = error;
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Binds the managed socket inside the exact verified directory.
///
/// The kernel must resolve the address against the directory *descriptor*, not
/// the absolute path that descriptor was opened from. Linux names the
/// descriptor through `/proc/self/fd/<fd>`; macOS has no such path for a socket
/// bind, so it switches a *dedicated short-lived thread's* working directory to
/// the verified descriptor, binds the bare relative name there, and lets the
/// thread exit. The caller's own thread directory is never touched, so an
/// existing per-thread cwd survives a bind unchanged. Every other Unix target
/// reports `Unsupported`. There is deliberately no fallback to the absolute
/// path: if the descriptor cannot carry the bind, the operation fails instead
/// of risking a socket in a replacement directory.
fn bind_in_verified_directory(
    namespace: &ManagedNamespace,
    names: &EndpointNames,
) -> io::Result<socket2::Socket> {
    let directory = namespace.directory();
    if let Some(path) = dirfd_relative_socket_path(directory, &names.socket_os) {
        return crate::unix_common::bind_socket(&path);
    }
    crate::unix_common::bind_in_directory_on_thread(directory, &names.socket_os)
}

/// Removes a stale managed socket only when the lease record proves the exact
/// current inode is the previous owner's. A failed connect never authorizes
/// removal on its own.
fn remove_stale_managed_socket(
    namespace: &ManagedNamespace,
    endpoint: &LocalEndpoint,
    names: &EndpointNames,
    lease: &File,
) -> io::Result<()> {
    let Some(socket_stat) = namespace.directories.socket_stat(&names.socket)? else {
        return Ok(());
    };
    if !stat_is(&socket_stat, libc::S_IFSOCK) || socket_stat.st_uid != unsafe { libc::geteuid() } {
        return Err(endpoint_in_use());
    }
    if probe_listener_is_live(namespace.directories.socket_directory()?, &names.socket_os)? {
        return Err(endpoint_in_use());
    }
    let Some(record) = read_record(lease)? else {
        return Err(endpoint_in_use());
    };
    if !record_matches_endpoint(&record, endpoint) || record.identity != identity_of(&socket_stat) {
        return Err(endpoint_in_use());
    }
    let opened_lock = fstat(lease.as_raw_fd())?;
    match crate::unix_common::unlink_verified(
        namespace.directories.socket_directory()?,
        &namespace.directories.records,
        names,
        &opened_lock,
        record.identity,
    ) {
        EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent => Ok(()),
        EndpointReapResult::IoError(error) => Err(error),
        _ => Err(endpoint_in_use()),
    }
}

/// Retires one endpoint the caller still owns: verifies the recorded identity
/// against the current entries and unlinks socket and lease under the gate.
fn retire_owned(
    namespace: &ManagedNamespace,
    endpoint: &LocalEndpoint,
    lease: &File,
    expected: &OwnerRecord,
) -> EndpointReapResult {
    if !namespace.gate_still_named() {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::CoordinatorReplaced);
    }
    let names = match managed_names(endpoint) {
        Ok(names) => names,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    let opened_lock = match verify_lease_entry(&namespace.directories.records, &names.lock, lease) {
        Ok(stat) => stat,
        Err(reason) => return EndpointReapResult::Unverified(reason),
    };
    match read_record(lease) {
        Ok(Some(record)) if record == *expected => {}
        Ok(Some(_)) => return EndpointReapResult::StaleTarget,
        Ok(None) => return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointReapResult::IoError(error),
    }
    let socket_stat = match namespace.directories.socket_stat(&names.socket) {
        Ok(stat) => stat,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    if let Some(socket_stat) = socket_stat {
        if let Some(reason) = socket_unverified(&socket_stat) {
            return EndpointReapResult::Unverified(reason);
        }
        if identity_of(&socket_stat) != expected.identity {
            return EndpointReapResult::StaleTarget;
        }
        match crate::unix_common::unlink_verified(
            namespace.directory(),
            &namespace.directories.records,
            &names,
            &opened_lock,
            expected.identity,
        ) {
            EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent => {}
            other => return other,
        }
    }
    match namespace.directories.records.unlink(&names.lock) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return EndpointReapResult::IoError(error),
    }
    EndpointReapResult::Reaped
}

/// The per-endpoint lease a listener holds for life.
///
/// `flock` locks are attached to the *open file description*, not the
/// descriptor. A `fork`/`dup`/`try_clone` duplicate therefore shares this
/// listener's OFD, and merely closing this descriptor would leave the flock
/// held until the last unrelated duplicate also closed. Retirement must release
/// ownership for *this* listener deterministically, so `release` issues an
/// explicit `LOCK_UN` on the OFD and then closes the local descriptor: a
/// duplicate that outlives the listener can no longer block a fresh bind or
/// reap, and this listener never extends ownership past its own close.
struct Lease(Option<File>);

impl Lease {
    fn file(&self) -> &File {
        self.0
            .as_ref()
            .expect("an armed lease always owns its descriptor")
    }

    /// Releases the lease flock and closes the local descriptor exactly once.
    ///
    /// `LOCK_UN` is issued before `close` on purpose: `close` alone cannot
    /// release a lock that another duplicate of the same OFD still refers to.
    fn release(&mut self) {
        if let Some(file) = self.0.take() {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            // `file` drops here, closing this listener's descriptor.
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // A lease that is dropped without an explicit retire still must not
        // outlive its listener through a shared OFD.
        self.release();
    }
}

/// Rolls back only the lease created by this bind, while its gate and file
/// descriptor are still held. Existing slots never acquire this authority.
struct BindLeaseGuard<'a> {
    namespace: &'a ManagedNamespace,
    names: &'a EndpointNames,
    lease: &'a File,
    created: bool,
    socket_bound: bool,
}

impl Drop for BindLeaseGuard<'_> {
    fn drop(&mut self) {
        if !self.created
            || !self.namespace.gate_still_named()
            || verify_lease_entry(
                &self.namespace.directories.records,
                &self.names.lock,
                self.lease,
            )
            .is_err()
            || try_lock_lease(self.lease).is_err()
        {
            return;
        }
        // After bind, the socket guard runs first. Preserve the lease if that
        // socket could not be withdrawn, including an unverified replacement.
        if self.socket_bound
            && !matches!(
                self.namespace.directories.socket_stat(&self.names.socket),
                Ok(None)
            )
        {
            return;
        }
        let _ = self.namespace.directories.records.unlink(&self.names.lock);
    }
}

/// A bound managed listener: the socket, its lease handle, and the record
/// written at bind time.
pub(crate) struct ManagedListener {
    inner: Option<tokio::net::UnixListener>,
    endpoint: LocalEndpoint,
    identity: SocketIdentity,
    record: OwnerRecord,
    root: PathBuf,
    lease: Option<Lease>,
    // Pin the original OS coordinator object for the entire listener lifetime,
    // including a close/drop that cannot reacquire its named gate. Namespace
    // Drop releases the shared OFD flock; this descriptor holds only the inode.
    // Field order releases the lease before this last listener pin closes.
    _gate_pin: File,
    directories_pin: Option<NamespaceDirectories>,
    context_binding: Option<ContextBinding>,
    #[cfg(test)]
    gate_identity: SocketIdentity,
    cleaned: bool,
}

impl ManagedListener {
    pub fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
        bind_managed(endpoint, false)
    }
    pub(crate) fn bind_with_context_binding(endpoint: &LocalEndpoint) -> io::Result<Self> {
        bind_managed(endpoint, true)
    }
    pub(crate) fn credential(&self) -> EndpointCredential {
        EndpointCredential::unix_managed(
            self.endpoint.clone(),
            self.identity,
            self.record.incarnation,
        )
    }

    pub(crate) fn context_binding(&self) -> io::Result<ContextBinding> {
        let binding = self.context_binding.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "listener has no owner context pin",
            )
        })?;
        Ok(binding.clone())
    }

    pub(crate) async fn accept(&mut self) -> io::Result<tokio::net::UnixStream> {
        match self.inner.as_mut() {
            Some(inner) => inner.accept().await.map(|(stream, _)| stream),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed listener is already closed",
            )),
        }
    }

    /// Explicit close: acquire the gate with a bounded retry, close the
    /// listening descriptor, verify and withdraw this instance's socket and
    /// lease, then release lease and gate.
    ///
    /// The wait is bounded on purpose. A coordinator held by another process is
    /// not a reason to stall the event loop forever, and an unretired slot is
    /// still safe: the complete record stays for the reaper. The listener
    /// descriptor always closes before the gate attempt, so this can never hold
    /// the listening socket open while waiting.
    pub(crate) fn close(mut self) -> EndpointReapResult {
        self.cleaned = true;
        self.inner.take();
        let namespace = match open_root_bounded(&self.root, false) {
            Ok(namespace) => namespace,
            Err(error) => {
                // Even when the gate cannot be taken, this listener's ownership
                // ends here: release the lease flock and close the descriptor so
                // a duplicate cannot extend it. The complete record stays for
                // the reaper.
                self.lease.take();
                return namespace_reap_error(error);
            }
        };
        if self.directories_pin.as_ref().is_some_and(|directories| {
            !directories.same_as(&namespace.directories).unwrap_or(false)
        }) {
            self.lease.take();
            return EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory);
        }
        match self.lease.as_ref() {
            Some(lease) => retire_owned(&namespace, &self.endpoint, lease.file(), &self.record),
            None => EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidOwnership),
        }
    }

    /// Bounded Drop path: close the listening descriptor in order, then retry
    /// the gate without waiting. A missed gate leaves the lease record for the
    /// reaper instead of starting unbounded background work. Either way the
    /// lease flock is released and this listener's descriptor closed, so a
    /// duplicate of it can never keep ownership alive.
    fn drop_bounded(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let namespace = ManagedNamespace::open_root(&self.root, false, false);
        self.inner.take();
        if let (Ok(namespace), Some(lease)) = (namespace, self.lease.as_ref()) {
            if self.directories_pin.as_ref().is_none_or(|directories| {
                directories.same_as(&namespace.directories).unwrap_or(false)
            }) {
                let _ = retire_owned(&namespace, &self.endpoint, lease.file(), &self.record);
            }
        }
        // Retire (or a missed gate) leaves the record for the reaper; in both
        // cases the lease flock must not survive this listener.
        self.lease.take();
    }
    /// Test-only: simulate a process exit without destructors, leaving the
    /// socket and its complete record behind while releasing the lease flock.
    #[cfg(test)]
    pub(crate) fn abandon_for_test(mut self) -> OwnerRecord {
        self.cleaned = true;
        self.inner.take();
        self.lease.take();
        self.record.clone()
    }

    /// Test-only: the coordinator inode this listener bound under, so a test can
    /// prove every concurrent first initialization shared one gate.
    #[cfg(test)]
    pub(crate) fn gate_identity_for_test(&self) -> SocketIdentity {
        self.gate_identity
    }

    /// Test-only: a real duplicate of *this listener's* lease open file
    /// description, standing in for a descriptor inherited across `fork`. The
    /// returned `File` shares the flock with the listener, so releasing the
    /// listener must be observable while the duplicate stays open.
    #[cfg(test)]
    pub(crate) fn duplicate_lease_fd_for_test(&self) -> File {
        let lease = self
            .lease
            .as_ref()
            .expect("a bound listener always holds its lease")
            .file();
        lease.try_clone().expect("duplicating the lease descriptor")
    }
}

impl Drop for ManagedListener {
    fn drop(&mut self) {
        self.drop_bounded();
    }
}

/// Binds a managed-v2 endpoint under the fixed coordinator gate.
fn bind_managed(endpoint: &LocalEndpoint, capture_context: bool) -> io::Result<ManagedListener> {
    let root = Path::new(endpoint.os_name()).parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint has no namespace directory",
        )
    })?;
    bind_at(endpoint, root, capture_context)
}

#[cfg(test)]
pub(crate) fn bind_managed_at(
    endpoint: &LocalEndpoint,
    root: &Path,
) -> io::Result<ManagedListener> {
    bind_at(endpoint, root, false)
}

fn bind_at(
    endpoint: &LocalEndpoint,
    root: &Path,
    capture_context: bool,
) -> io::Result<ManagedListener> {
    if Path::new(endpoint.os_name()).parent() != Some(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint context does not match namespace",
        ));
    }
    let namespace = open_root_bounded(root, true).map_err(namespace_bind_error)?;
    let gate_pin = namespace.gate.try_clone()?;
    let names = managed_names(endpoint)?;
    namespace
        .directories
        .verify_slot(&names.lock)
        .map_err(namespace_bind_error)?;
    let (file, created) = match namespace.directories.records.open_file(
        &names.lock,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
    ) {
        Ok(Some(file)) => (file, true),
        Ok(None) => unreachable!("O_CREAT openat must return a lease file"),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (
            open_lease(&namespace.directories.records, &names.lock)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "managed lease disappeared while opening",
                )
            })?,
            false,
        ),
        Err(error) => return Err(error),
    };
    // Declared before both rollback guards: unlock only after they finish.
    let lease = Lease(Some(file));
    let mut lease_guard = BindLeaseGuard {
        namespace: &namespace,
        names: &names,
        lease: lease.file(),
        created,
        socket_bound: false,
    };
    if let Err(reason) =
        verify_lease_entry(&namespace.directories.records, &names.lock, lease.file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("managed lease is unverifiable: {reason:?}"),
        ));
    }
    if let Err(result) = try_lock_lease(lease.file()) {
        return Err(match result {
            EndpointReapResult::Busy => endpoint_in_use(),
            EndpointReapResult::IoError(error) => error,
            _ => endpoint_in_use(),
        });
    }
    #[cfg(test)]
    if crate::unix_common::fault::take_if(crate::unix_common::fault::Failure::LeaseAfterLock) {
        crate::unix_common::fault::retain_duplicate(lease.file());
        return Err(io::Error::other(
            "injected failure immediately after lease lock",
        ));
    }
    #[cfg(test)]
    for (failure, name) in [
        (
            crate::unix_common::fault::Failure::LeaseAfterReplacement,
            names.lock.clone(),
        ),
        (
            crate::unix_common::fault::Failure::LeaseAfterGateReplacement,
            gate_name(),
        ),
    ] {
        if crate::unix_common::fault::take_if(failure) {
            namespace
                .directories
                .records
                .replace_entry_with_file_for_test(&name)?;
            return Err(io::Error::other("injected replacement after lease lock"));
        }
    }
    remove_stale_managed_socket(&namespace, endpoint, &names, lease.file())?;
    // The socket must be created in the directory this call verified. Re-check
    // the gate identity after the stale-socket work, then bind through the
    // verified descriptor: an absolute path could have been redirected to a
    // replacement directory by a concurrent rename in between.
    if !namespace.gate_still_named() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "managed namespace identity changed before bind",
        ));
    }
    let socket = bind_in_verified_directory(&namespace, &names)?;
    lease_guard.socket_bound = true;
    // Everything after a successful bind either completes initialization or
    // withdraws the exact object this call created. The guard never trusts the
    // record it may have failed to write.
    let mut guard = BoundSocketGuard::capture(
        namespace.directory(),
        &namespace.directories.records,
        &names,
        lease.file(),
    )?;
    set_socket_permissions(namespace.directory(), &names, &guard)?;
    listen_verified_socket(&socket, namespace.directory(), &names, &guard)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::os::unix::net::UnixListener = socket.into();
    let inner = tokio::net::UnixListener::from_std(std_listener)?;
    let identity = socket_identity(namespace.directories.socket_directory()?, &names)?;
    let record = OwnerRecord {
        address: endpoint.address().to_owned(),
        incarnation: uuid::Uuid::new_v4().into_bytes(),
        identity,
    };
    #[cfg(test)]
    {
        use crate::unix_common::fault;
        if fault::take_if(fault::Failure::ManagedRecordWriteAfterReplacement) {
            // Simulate a foreign object taking the socket entry between bind and
            // record write. The record then cannot describe this bind's socket,
            // and rollback must leave the foreign object in place.
            namespace
                .directory()
                .replace_entry_with_file_for_test(&names.socket)?;
        }
    }
    write_record(lease.file(), &record)?;
    match read_record(lease.file()) {
        Ok(Some(readback)) if readback == record => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed owner record did not read back",
            ));
        }
        Err(error) => return Err(error),
    }
    // The recorded identity must still name the entry this bind created. A
    // foreign replacement stays in place and is never withdrawn by rollback.
    match socket_identity(namespace.directories.socket_directory()?, &names) {
        Ok(current) if current == identity => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed socket was replaced while recording its identity",
            ));
        }
        Err(error) => return Err(error),
    }
    #[cfg(test)]
    let gate_identity =
        SocketIdentity::from_stat(&fstat(namespace.gate.as_raw_fd()).map_err(|error| error)?);
    let context_binding = capture_context
        .then(|| ContextBinding::from_namespace(root, &namespace))
        .transpose()?;
    guard.disarm();
    drop(guard);
    lease_guard.created = false;
    drop(lease_guard);
    Ok(ManagedListener {
        inner: Some(inner),
        endpoint: endpoint.clone(),
        identity,
        record,
        root: root.to_owned(),
        lease: Some(lease),
        _gate_pin: gate_pin,
        directories_pin: namespace
            .directories
            .split()
            .then(|| namespace.directories.clone()),
        context_binding,
        #[cfg(test)]
        gate_identity,
        cleaned: false,
    })
}

fn slot_name(stem: &OsStr, suffix: &str) -> io::Result<CString> {
    let mut name = stem.as_bytes().to_vec();
    name.extend_from_slice(suffix.as_bytes());
    CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed slot name contains NUL",
        )
    })
}

/// Reads the owner record from a lease entry without locking it.
pub(crate) fn read_lease_record(
    namespace: &ManagedNamespace,
    stem: &OsStr,
) -> Result<Option<OwnerRecord>, EndpointUnverifiedReason> {
    let mut lease = stem.as_bytes().to_vec();
    lease.extend_from_slice(LEASE_SUFFIX.as_bytes());
    let name = CString::new(lease).map_err(|_| EndpointUnverifiedReason::InvalidRecord)?;
    namespace
        .directories
        .verify_slot(&name)
        .map_err(|error| match error {
            NamespaceError::Unverified(reason) => reason,
            _ => EndpointUnverifiedReason::InvalidOwnership,
        })?;
    let stat = match namespace.directories.records.stat(&name) {
        Ok(Some(stat)) => stat,
        Ok(None) => return Ok(None),
        Err(_) => return Err(EndpointUnverifiedReason::InvalidOwnership),
    };
    if let Some(reason) = lease_entry_unverified(&stat) {
        return Err(reason);
    }
    let file = match open_lease(&namespace.directories.records, &name) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(None),
        Err(_) => return Err(EndpointUnverifiedReason::InvalidOwnership),
    };
    let _ = verify_lease_entry(&namespace.directories.records, &name, &file)?;
    match read_record(&file) {
        Ok(Some(record)) => Ok(Some(record)),
        Ok(None) => Err(EndpointUnverifiedReason::InvalidRecord),
        Err(_) => Err(EndpointUnverifiedReason::InvalidOwnership),
    }
}

/// Shared reap core. `expected` carries the credential proof for a public reap;
/// a sweep passes the proof read from the slot record itself.
fn reap_locked(
    namespace: &ManagedNamespace,
    endpoint: &LocalEndpoint,
    expected: Option<(SocketIdentity, [u8; 16])>,
) -> EndpointReapResult {
    if !namespace.gate_still_named() {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::CoordinatorReplaced);
    }
    let names = match managed_names(endpoint) {
        Ok(names) => names,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    if let Err(error) = namespace.directories.verify_slot(&names.lock) {
        return namespace_reap_error(error);
    }
    let socket_stat = match namespace.directories.socket_stat(&names.socket) {
        Ok(stat) => stat,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    if let Some(stat) = &socket_stat {
        if let Some(reason) = socket_unverified(stat) {
            return EndpointReapResult::Unverified(reason);
        }
    }
    let lease_stat = match namespace.directories.records.stat(&names.lock) {
        Ok(stat) => stat,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    let Some(lease_stat) = lease_stat else {
        return match socket_stat {
            None => EndpointReapResult::AlreadyAbsent,
            Some(_) => EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership),
        };
    };
    if let Some(reason) = lease_entry_unverified(&lease_stat) {
        return EndpointReapResult::Unverified(reason);
    }
    let Some(lease) = (match open_lease(&namespace.directories.records, &names.lock) {
        Ok(lease) => lease,
        Err(error) => return EndpointReapResult::IoError(error),
    }) else {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership);
    };
    let opened_lock = match verify_lease_entry(&namespace.directories.records, &names.lock, &lease)
    {
        Ok(stat) => stat,
        Err(reason) => return EndpointReapResult::Unverified(reason),
    };
    if let Err(result) = try_lock_lease(&lease) {
        return result;
    }
    let record = match read_record(&lease) {
        Ok(Some(record)) => record,
        Ok(None) => return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointReapResult::IoError(error),
    };
    if !record_matches_endpoint(&record, endpoint) {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord);
    }
    if let Some((identity, incarnation)) = expected {
        if record.identity != identity || record.incarnation != incarnation {
            return EndpointReapResult::StaleTarget;
        }
    }
    match socket_stat {
        None => {
            // A valid old record whose socket is already gone may still retire
            // its lease; there is no socket object to compare against.
            let current = match namespace.directories.records.stat(&names.lock) {
                Ok(Some(stat)) => stat,
                Ok(None) => return EndpointReapResult::AlreadyAbsent,
                Err(error) => return EndpointReapResult::IoError(error),
            };
            if !namespace.gate_still_named() || !same_file(&opened_lock, &current) {
                return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidOwnership);
            }
            match namespace.directories.records.unlink(&names.lock) {
                Ok(()) => EndpointReapResult::Reaped,
                Err(error) if error.kind() == io::ErrorKind::NotFound => EndpointReapResult::Reaped,
                Err(error) => EndpointReapResult::IoError(error),
            }
        }
        Some(socket_stat) => {
            if let Some(reason) = socket_unverified(&socket_stat) {
                return EndpointReapResult::Unverified(reason);
            }
            if identity_of(&socket_stat) != record.identity {
                return EndpointReapResult::StaleTarget;
            }
            if !namespace.gate_still_named() {
                return EndpointReapResult::Unverified(
                    EndpointUnverifiedReason::CoordinatorReplaced,
                );
            }
            match crate::unix_common::unlink_verified(
                namespace.directory(),
                &namespace.directories.records,
                &names,
                &opened_lock,
                record.identity,
            ) {
                EndpointReapResult::Reaped | EndpointReapResult::AlreadyAbsent => {}
                other => return other,
            }
            match namespace.directories.records.unlink(&names.lock) {
                Ok(()) => EndpointReapResult::Reaped,
                Err(error) if error.kind() == io::ErrorKind::NotFound => EndpointReapResult::Reaped,
                Err(error) => EndpointReapResult::IoError(error),
            }
        }
    }
}

pub(crate) fn reap_managed(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
) -> EndpointReapResult {
    let root = match Path::new(endpoint.os_name()).parent() {
        Some(root) => root.to_owned(),
        None => {
            return EndpointReapResult::IoError(io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint has no namespace directory",
            ));
        }
    };
    reap_managed_at(endpoint, credential, &root)
}

/// Identity-checked reap against an explicit managed namespace root.
///
/// Maintenance never waits for the coordinator gate: another process may hold
/// it indefinitely, and a shared gate wait would stall every other slot behind
/// one busy candidate. A busy gate is reported as `Busy` so the caller keeps
/// its budget and retries on a later pass.
pub(crate) fn reap_managed_at(
    endpoint: &LocalEndpoint,
    credential: &EndpointCredential,
    root: &Path,
) -> EndpointReapResult {
    if credential.endpoint() != endpoint || Path::new(endpoint.os_name()).parent() != Some(root) {
        return EndpointReapResult::StaleTarget;
    }
    let identity = credential.identity();
    let incarnation = credential.incarnation();
    let namespace = match ManagedNamespace::open_root(root, false, false) {
        Ok(namespace) => namespace,
        Err(error) => return namespace_reap_error(error),
    };
    reap_locked(&namespace, endpoint, Some((identity, incarnation)))
}

/// Reaps one slot found by a sweep. The slot record itself is the proof. The
/// gate is only tried, never waited for, so a foreign holder cannot block the
/// round.
#[cfg(test)]
pub(crate) fn reap_slot(context: &LocalEndpointContext, stem: &OsStr) -> EndpointReapResult {
    reap_slot_with_directory(context, stem, None)
}

fn reap_slot_with_directory(
    context: &LocalEndpointContext,
    stem: &OsStr,
    expected_directory: Option<&NamespaceDirectories>,
) -> EndpointReapResult {
    let anchor = match context.endpoint("ipc://c2-endpoint-sweep") {
        Ok(endpoint) => endpoint,
        Err(error) => return EndpointReapResult::IoError(error),
    };
    let root = Path::new(anchor.os_name())
        .parent()
        .expect("derived Unix endpoint has a parent");
    let namespace = match ManagedNamespace::open_root(root, false, false) {
        Ok(namespace) => namespace,
        Err(error) => return namespace_reap_error(error),
    };
    // A sweep must stay attached to the directory it opened. If replacement
    // happens between its batch check and this gate open, do not adopt even a
    // complete record in the replacement namespace.
    if let Some(expected) = expected_directory {
        match expected.same_as(&namespace.directories) {
            Ok(true) => {}
            Ok(false) => {
                return EndpointReapResult::Unverified(EndpointUnverifiedReason::UnsafeDirectory);
            }
            Err(error) => return EndpointReapResult::IoError(error),
        }
    }
    let record = match read_lease_record(&namespace, stem) {
        Ok(Some(record)) => record,
        Ok(None) => {
            // A socket without any lease is an unregistered object, not proof
            // that the slot is already clean.
            let socket = slot_name(stem, "").and_then(|name| {
                let stat = namespace.directories.socket_stat(&name);
                #[cfg(test)]
                let stat = if crate::unix_common::fault::take_if(
                    crate::unix_common::fault::Failure::MissingLeaseSocketStat,
                ) {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    stat
                };
                stat
            });
            return match socket {
                Ok(Some(_)) => {
                    EndpointReapResult::Unverified(EndpointUnverifiedReason::MissingOwnership)
                }
                Ok(None) => EndpointReapResult::AlreadyAbsent,
                Err(error) => EndpointReapResult::IoError(error),
            };
        }
        Err(reason) => return EndpointReapResult::Unverified(reason),
    };
    let address = record.address.clone();
    let Ok(endpoint) = context.endpoint(&address) else {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord);
    };
    let Ok(stem_expected) = endpoint_stem(&endpoint) else {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord);
    };
    if stem_expected != stem {
        return EndpointReapResult::Unverified(EndpointUnverifiedReason::InvalidRecord);
    }
    reap_locked(
        &namespace,
        &endpoint,
        Some((record.identity, record.incarnation)),
    )
}

pub(crate) fn inspect_managed(endpoint: &LocalEndpoint) -> EndpointInspection {
    let root = match Path::new(endpoint.os_name()).parent() {
        Some(root) => root.to_owned(),
        None => {
            return EndpointInspection::IoError(io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint has no namespace directory",
            ));
        }
    };
    inspect_managed_at(endpoint, &root)
}

/// Read-only inspection against an explicit managed namespace root. Like the
/// reaper it only tries the gate, so a busy coordinator becomes an observable
/// `WouldBlock` inspection instead of an unbounded wait.
pub(crate) fn inspect_managed_at(endpoint: &LocalEndpoint, root: &Path) -> EndpointInspection {
    if Path::new(endpoint.os_name()).parent() != Some(root) {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    let namespace = match ManagedNamespace::open_root(root, false, false) {
        Ok(namespace) => namespace,
        Err(error) => return namespace_inspection_error(error),
    };
    if !namespace.gate_still_named() {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::CoordinatorReplaced);
    }
    let names = match managed_names(endpoint) {
        Ok(names) => names,
        Err(error) => return EndpointInspection::IoError(error),
    };
    if let Err(error) = namespace.directories.verify_slot(&names.lock) {
        return namespace_inspection_error(error);
    }
    let socket_stat = match namespace.directories.socket_stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointInspection::Absent,
        Err(error) => return EndpointInspection::IoError(error),
    };
    if let Some(reason) = socket_unverified(&socket_stat) {
        return EndpointInspection::Unverified(reason);
    }
    let lease_stat = match namespace.directories.records.stat(&names.lock) {
        Ok(Some(stat)) => stat,
        Ok(None) => {
            return EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership);
        }
        Err(error) => return EndpointInspection::IoError(error),
    };
    if let Some(reason) = lease_entry_unverified(&lease_stat) {
        return EndpointInspection::Unverified(reason);
    }
    let Some(lease) = (match open_lease(&namespace.directories.records, &names.lock) {
        Ok(lease) => lease,
        Err(error) => return EndpointInspection::IoError(error),
    }) else {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::MissingOwnership);
    };
    let opened_lock = match verify_lease_entry(&namespace.directories.records, &names.lock, &lease)
    {
        Ok(stat) => stat,
        Err(reason) => return EndpointInspection::Unverified(reason),
    };
    let record = match read_record(&lease) {
        Ok(Some(record)) => record,
        Ok(None) => return EndpointInspection::Unverified(EndpointUnverifiedReason::InvalidRecord),
        Err(error) => return EndpointInspection::IoError(error),
    };
    if !record_matches_endpoint(&record, endpoint) || record.identity != identity_of(&socket_stat) {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    // Re-verify both entries after reading so a concurrent replacement cannot
    // be reported as a present endpoint.
    #[cfg(test)]
    if crate::unix_common::fault::take_if(
        crate::unix_common::fault::Failure::InspectSocketReplacementThenLeaseStat,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let path = namespace.directory().path().join(&names.socket_os);
        fs::remove_file(&path).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        crate::unix_common::fault::inject(
            crate::unix_common::fault::Failure::InspectFinalLeaseStat,
        );
    }
    let current_socket = match namespace.directories.socket_stat(&names.socket) {
        Ok(Some(stat)) => stat,
        Ok(None) => return EndpointInspection::Absent,
        Err(error) => return EndpointInspection::IoError(error),
    };
    let current_lock = namespace.directories.records.stat(&names.lock);
    #[cfg(test)]
    let current_lock = if crate::unix_common::fault::take_if(
        crate::unix_common::fault::Failure::InspectFinalLeaseStat,
    ) {
        Err(io::Error::from_raw_os_error(libc::EIO))
    } else {
        current_lock
    };
    // A socket identity already known to be wrong takes precedence over a
    // concurrent error while re-reading the lease entry.
    if !same_file(&socket_stat, &current_socket) || identity_of(&current_socket) != record.identity
    {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    let current_lock = match current_lock {
        Ok(stat) => stat,
        Err(error) => return EndpointInspection::IoError(error),
    };
    if current_lock.is_none_or(|current| !same_file(&opened_lock, &current))
        || !namespace.gate_still_named()
    {
        return EndpointInspection::Unverified(EndpointUnverifiedReason::RecordMismatch);
    }
    EndpointInspection::Present(EndpointCredential::unix_managed(
        endpoint.clone(),
        record.identity,
        record.incarnation,
    ))
}

/// Bounded, explicitly driven maintenance over the managed-v2 namespace. The
/// gate is acquired per candidate and never held across the traversal.
pub(crate) struct ManagedSweep {
    context: LocalEndpointContext,
    directories: NamespaceDirectories,
    entries: Option<DirectoryEntries>,
    finished: bool,
    interrupted: bool,
    _lease: Option<SweepLease>,
    selected: Option<std::collections::HashSet<OsString>>,
}

impl ManagedSweep {
    pub(crate) fn for_endpoint(endpoint: &LocalEndpoint) -> io::Result<Self> {
        let root = Path::new(endpoint.os_name())
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "endpoint has no namespace directory",
                )
            })?
            .to_owned();
        Self::open_root(&root, endpoint.context())
    }

    pub(crate) fn for_scope(
        endpoint: &LocalEndpoint,
        targets: &[LocalEndpoint],
    ) -> io::Result<Self> {
        if targets
            .iter()
            .any(|target| target.context() != endpoint.context())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint sweep target is outside its context",
            ));
        }
        let selected = Self::target_names(targets)?;
        let mut sweep = Self::for_endpoint(endpoint)?;
        sweep.selected = Some(selected);
        Ok(sweep)
    }

    fn target_names(targets: &[LocalEndpoint]) -> io::Result<std::collections::HashSet<OsString>> {
        let mut selected = std::collections::HashSet::new();
        for target in targets {
            let names = managed_names(target)?;
            selected.insert(names.socket_os);
            selected.insert(OsStr::from_bytes(names.lock.as_bytes()).to_owned());
        }
        Ok(selected)
    }

    fn open_root(root: &Path, context: &LocalEndpointContext) -> io::Result<Self> {
        let lease = SweepLease::acquire()?;
        let directories = NamespaceDirectories::open(root, false).map_err(namespace_bind_error)?;
        let entries = directories.entries()?;
        if directories.validate().is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed namespace changed while opening sweep",
            ));
        }
        Ok(Self {
            context: context.clone(),
            directories,
            entries: Some(entries),
            finished: false,
            interrupted: false,
            _lease: Some(lease),
            selected: None,
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
            return SweepBatch {
                round_complete: !self.interrupted,
                round_interrupted: self.interrupted,
                namespace_changed: self.interrupted,
                ..SweepBatch::default()
            };
        }
        if self.directories.validate().is_err() {
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
                    batch.last_io_error = Some(crate::EndpointIoError::from(&_error));
                }
                Some(Ok(entry)) => {
                    batch.entries_visited += 1;
                    self.inspect_entry(&entry, &mut batch);
                }
            }
        }
        if self.directories.validate().is_err() {
            self.finish(true);
            batch.namespace_changed = true;
            batch.round_interrupted = true;
            batch.round_complete = false;
        }
        batch
    }

    fn inspect_entry(&self, entry: &fs::DirEntry, batch: &mut SweepBatch) {
        let filename = entry.file_name();
        let filename = filename.as_os_str();
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !selected.contains(filename))
        {
            return;
        }
        let bytes = filename.as_bytes();
        if bytes == GATE_NAME.as_bytes() || bytes == MARKER_NAME.as_bytes() {
            return;
        }
        let in_records = entry.path().parent() == Some(self.directories.records.path());
        if is_endpoint_stem(bytes) {
            if self.directories.split() && in_records {
                return;
            }
            batch.endpoints_examined += 1;
            let stem = OsStr::from_bytes(bytes);
            self.count(
                batch,
                reap_slot_with_directory(&self.context, stem, Some(&self.directories)),
                false,
            );
        } else if let Some(stem) = bytes
            .strip_suffix(LEASE_SUFFIX.as_bytes())
            .filter(|stem| is_endpoint_stem(stem))
        {
            if self.directories.split() && !in_records {
                // Historical colocated leases belong to the cold-switch
                // boundary. Never report one absent by reading the new root.
                batch.endpoints_examined += 1;
                batch.unverified += 1;
                return;
            }
            // A socket slot is handled through its short entry, which retires
            // both objects. Only a socket-less slot is a lease-only candidate.
            let socket = match CString::new(stem) {
                Ok(socket) => socket,
                Err(_) => {
                    batch.unverified += 1;
                    return;
                }
            };
            match self.directories.socket_stat(&socket) {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(error) => {
                    self.count(batch, EndpointReapResult::IoError(error), false);
                    return;
                }
            }
            batch.endpoints_examined += 1;
            let stem = OsStr::from_bytes(stem);
            self.count(
                batch,
                reap_slot_with_directory(&self.context, stem, Some(&self.directories)),
                true,
            );
        }
    }

    fn count(&self, batch: &mut SweepBatch, outcome: EndpointReapResult, lease_only: bool) {
        match outcome {
            EndpointReapResult::Reaped => {
                batch.reaped += 1;
                if lease_only {
                    batch.leases_retired += 1;
                }
            }
            EndpointReapResult::AlreadyAbsent => batch.already_absent += 1,
            EndpointReapResult::Busy => batch.busy += 1,
            EndpointReapResult::StaleTarget => batch.stale_target += 1,
            EndpointReapResult::Unverified(_) => batch.unverified += 1,
            EndpointReapResult::IoError(error) => {
                batch.io_errors += 1;
                batch.last_io_error = Some(crate::EndpointIoError::from(&error));
            }
            EndpointReapResult::NotApplicable => batch.not_applicable += 1,
        }
    }
}

/// Creates a fresh managed namespace root for tests without going through the
/// production UID path.
#[cfg(test)]
pub(crate) fn test_namespace_root(root: &Path) -> io::Result<()> {
    drop(EndpointDirectory::open(root, true)?);
    ManagedNamespace::open_root(root, true, true)
        .map(|namespace| drop(namespace))
        .map_err(|error| match error {
            NamespaceError::Io(error) => error,
            _ => io::Error::new(io::ErrorKind::InvalidData, "unverifiable test namespace"),
        })
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "macos"))]
mod split_tests;
