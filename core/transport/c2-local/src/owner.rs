//! OS-owned, one-way control liveness handles for explicitly managed child processes.
//!
//! The pair is independent of [`LocalStream`](crate::LocalStream) and carries no application
//! data. Keep the [`OwnerControlKeepalive`] in the controller. Convert the receiver to
//! [`std::process::Stdio`] only on the command for the intended child, then explicitly adopt that
//! inherited handle in the child with [`OwnerControlReceiver::from_inherited_fd`] or
//! [`OwnerControlReceiver::from_inherited_handle`]. Dropping or shutting down the keepalive
//! makes the receiver observe EOF. The first version does not reconnect.
//!
//! # Lifecycle contract
//!
//! Both OS endpoints are created non-inheritable, and neither is registered with an async runtime
//! reactor or completion port by [`owner_control_pair`]. The receiver has exactly one transfer
//! point in the controller and exactly one wait registration in the receiving process:
//!
//! 1. In the controller, [`OwnerControlReceiver::take_stdio`] is the only way to hand the receiver
//!    to a child. It moves the controller's endpoint copy into `Stdio`, so an unrelated child
//!    cannot inherit the capability.
//! 2. In the receiving process, the first call to [`OwnerControlReceiver::wait_closed`] activates
//!    the endpoint. Activation builds the single native watcher (Tokio `AsyncFd` on Unix, Tokio
//!    `NamedPipeServer` on Windows) from the one handle this process owns. Every later wait,
//!    including one that follows a cancelled wait, reuses that watcher; the implementation never
//!    duplicates the handle and never registers the same file object twice.
//! 3. After activation, [`OwnerControlReceiver::take_stdio`] fails with
//!    [`std::io::ErrorKind::InvalidInput`] instead of tearing down or re-registering the live
//!    watcher, and [`OwnerControlReceiver::shutdown`] or `Drop` releases the watcher exactly once.
//!
//! # Terminal closure and trust boundary
//!
//! The capability is one-way and never reconnects. Once a wait has observed the peer end close
//! (`Ok(())`, i.e. EOF or a peer-side broken pipe/no-data status), that observation is the terminal
//! state of the receiver: every later `wait_closed` returns the same `Ok(())` without launching
//! another overlapped operation, so a Windows second read cannot turn it into a fresh
//! `ERROR_PIPE_NOT_CONNECTED` failure. Only a recorded *peer* closure is terminal; a local
//! shutdown, local I/O error, or a non-pipe/type/mode rejection is still reported as an error and
//! is never laundered into a fake peer EOF.
//!
//! The private capability guarantee covers what this module creates: [`owner_control_pair`]
//! produces a non-inheritable pair whose access is restricted to the current logon identity on
//! Windows, and it never publishes the endpoint name, path, or any secret through argv, the
//! environment, or a public address. Explicit adoption ([`OwnerControlReceiver::from_inherited_fd`],
//! [`OwnerControlReceiver::from_inherited_handle`]) deliberately does *not* re-derive that
//! guarantee for an arbitrary caller-supplied descriptor or handle: it validates the OS object
//! type, end, mode, and nonblocking/overlapped properties, but it cannot prove who created the
//! handle or that its ACL is still the private one. Authentication of an adopted source is the
//! trusted launcher's responsibility, and adoption is never applied to ordinary business
//! connections.
//!
//! On Windows the receiving side must be a process that never registered this pipe end on its own
//! completion port; only the explicit `Stdio` transfer makes the child the first registrant. The
//! child duplicates the inherited handle (fd or handle value) and owns only that duplicate.

use std::io;
use std::process::Stdio;

#[cfg(unix)]
#[path = "owner/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "owner/windows.rs"]
mod platform;

/// Create an owned controller endpoint and its matching receiver endpoint.
///
/// Both OS handles are created non-inheritable, and neither is registered with a reactor or
/// completion port. The returned receiver must be explicitly converted to [`Stdio`] for the target
/// child process; ordinary child processes cannot extend the owner's lifetime.
pub fn owner_control_pair() -> io::Result<(OwnerControlKeepalive, OwnerControlReceiver)> {
    let (keepalive, receiver) = platform::pair()?;
    Ok((
        OwnerControlKeepalive(keepalive),
        OwnerControlReceiver(receiver),
    ))
}

fn already_activated() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "owner control receiver is already registered with the async runtime and cannot be \
         transferred as child stdio",
    )
}

/// Controller-side endpoint. It is not cloneable: one owner controls its lifetime.
pub struct OwnerControlKeepalive(platform::Keepalive);

impl OwnerControlKeepalive {
    /// Close the owner endpoint. Repeated calls are harmless.
    pub fn shutdown(&mut self) {
        self.0.shutdown();
    }
}

/// Receiver-side endpoint, normally handed to one child process as explicit stdio.
pub struct OwnerControlReceiver(platform::Receiver);

impl OwnerControlReceiver {
    /// Activate the single runtime watcher, then probe the native endpoint for peer closure.
    /// `true` means no closure was observable at the probe; `false` is peer EOF.
    /// This is an establishment check, not a promise against a later concurrent owner exit.
    pub async fn prepare(&mut self) -> io::Result<bool> {
        self.0.prepare().await
    }

    /// Wait until the controller endpoint closes or the control handle becomes invalid.
    ///
    /// The first call activates the endpoint and creates its single runtime watcher. Dropping this
    /// future cancels only the wait: the watcher stays registered, so a later call resumes waiting
    /// without re-registering or duplicating the OS handle. No background task or blocking worker
    /// is spawned.
    ///
    /// A returned `Ok(())` means the peer end closed. That observation is terminal: later calls
    /// return `Ok(())` again without another overlapped read, which keeps a Windows second wait
    /// stable instead of surfacing a fresh `ERROR_PIPE_NOT_CONNECTED`. Local failures and an
    /// explicit [`OwnerControlReceiver::shutdown`] still report errors.
    pub async fn wait_closed(&mut self) -> io::Result<()> {
        self.0.wait_closed().await
    }

    /// Take this endpoint out as an explicitly selected child-process stdio stream.
    ///
    /// On success the receiver's endpoint moves into `Stdio` (and, for the controller copy, is
    /// closed after the child spawns), leaving this receiver closed. Call it only for the intended
    /// child command, and only before the first [`OwnerControlReceiver::wait_closed`] call: once
    /// activated, the endpoint owns a live runtime watcher and this returns
    /// [`io::ErrorKind::InvalidInput`] without disturbing that watcher or the receiver.
    ///
    /// Taking `&mut self` keeps a rejected call non-destructive: nothing is transferred or closed,
    /// so an activated receiver stays usable for a later wait. A call on an already closed receiver
    /// returns [`io::ErrorKind::BrokenPipe`].
    pub fn take_stdio(&mut self) -> io::Result<Stdio> {
        self.0.take_stdio()
    }

    /// Whether the first [`OwnerControlReceiver::wait_closed`] call has activated the endpoint's
    /// single runtime watcher.
    ///
    /// This is `true` from that activation until [`OwnerControlReceiver::shutdown`] or `Drop`
    /// releases the watcher; a cancelled wait does not clear it.
    pub fn is_activated(&self) -> bool {
        self.0.is_activated()
    }

    /// Adopt a specific inherited file descriptor in the child process.
    ///
    /// The descriptor is borrowed and duplicated into a close-on-exec receiver owned by this
    /// value; the source descriptor keeps its previous owner and is never closed here. The
    /// duplicate is not registered until the first [`OwnerControlReceiver::wait_closed`] call.
    /// The caller must ensure `fd` names a valid descriptor for the duration of this call.
    /// Non-pipe descriptors, write ends, and blocking pipes are rejected. This is deliberately
    /// explicit; the library never adopts process stdin implicitly.
    ///
    /// Only the OS object type and descriptor flags are validated. This call does not prove who
    /// created `fd`, and it does not claim any ACL for an arbitrary external descriptor; the
    /// trusted launcher that selected this endpoint is responsible for its provenance. Ordinary
    /// business connections are never adopted this way.
    ///
    /// # Safety
    ///
    /// `fd` must stay a valid open descriptor for the duration of this call. The call duplicates
    /// it, so the descriptor must not be closed concurrently by another thread.
    #[cfg(unix)]
    pub unsafe fn from_inherited_fd(fd: std::os::fd::RawFd) -> io::Result<Self> {
        // SAFETY: validity during the call is the caller's documented precondition.
        unsafe { platform::adopt(fd) }.map(Self)
    }

    /// Adopt a specific inherited Windows handle in the child process.
    ///
    /// The handle is borrowed and duplicated into a non-inheritable receiver owned by this value;
    /// the source handle keeps its previous owner (for an inherited stdin, the process standard
    /// stream) and is never closed here. The duplicate is not registered on the completion port
    /// until the first [`OwnerControlReceiver::wait_closed`] call. Non-pipe handles, client ends,
    /// message pipes, and synchronous pipes are rejected. This is deliberately explicit; the
    /// library never adopts process stdin implicitly.
    ///
    /// Only the OS object type, pipe end, byte mode, and overlapped property are validated. This
    /// call does not prove who created `handle`, so it does not claim to have ACL-authenticated an
    /// arbitrary external handle; the trusted launcher that selected this endpoint is responsible
    /// for its provenance. Ordinary business connections are never adopted this way.
    ///
    /// # Safety
    ///
    /// `handle` must stay a valid open handle for the duration of this call. The call duplicates
    /// it, so the handle must not be closed concurrently by another thread.
    #[cfg(windows)]
    pub unsafe fn from_inherited_handle(
        handle: std::os::windows::io::RawHandle,
    ) -> io::Result<Self> {
        // SAFETY: validity during the call is the caller's documented precondition.
        unsafe { platform::adopt(handle) }.map(Self)
    }

    /// Close the local receiver endpoint and release its watcher. Repeated calls are harmless.
    pub fn shutdown(&mut self) {
        self.0.shutdown();
    }
}
