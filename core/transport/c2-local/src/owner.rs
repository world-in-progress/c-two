//! Private duplex owner endpoints for a trusted launcher and its child.
//!
//! [`owner_endpoint_pair`] creates non-inheritable, unregistered OS endpoints without starting
//! a runtime. The controller activates on its long-lived client runtime; the child activates
//! the bootstrap on its host runtime. Activation transfers the single registered stream to the
//! caller. It never consumes control bytes or creates a second reader.
//!
//! A bootstrap may instead be moved into child stdio before activation. Adoption duplicates
//! the inherited descriptor/handle and leaves the original stdin owned by the process. Closing
//! the adopted bootstrap or stream ends the connection even while that alias remains open.
//! The inherited source must be dedicated to this protocol; OS type checks cannot authenticate
//! an arbitrary external handle or prove that it has never been registered with a reactor.
//!
//! Unix close-on-exec does not prevent raw fork from copying a descriptor. Trusted launchers
//! must use ordinary `close` on unrelated fork copies before exec (never connection-wide
//! `shutdown`). Only the intended owner closes the relationship. Successfully exec'd unrelated
//! children do not inherit either endpoint.

use std::io;
use std::process::Stdio;

use crate::LocalStream;

#[cfg(unix)]
#[path = "owner/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "owner/windows.rs"]
mod platform;

/// Create a private duplex pair, without registering either endpoint on an async runtime.
pub fn owner_endpoint_pair() -> io::Result<(OwnerControllerEndpoint, OwnerBootstrap)> {
    let (controller, bootstrap) = platform::pair()?;
    Ok((
        OwnerControllerEndpoint(controller),
        OwnerBootstrap(bootstrap),
    ))
}

/// The controller's single-use, non-cloneable raw owner endpoint.
pub struct OwnerControllerEndpoint(platform::Controller);

impl OwnerControllerEndpoint {
    /// Register on the current runtime and transfer the connected stream exactly once.
    ///
    /// A cancelled activation retains any registered watcher in this endpoint. A retry must
    /// run on the same runtime; in particular a Windows file object cannot be re-registered
    /// on another completion port. Success consumes this endpoint's I/O authority. An already
    /// closed peer is reported as an error without consuming control bytes.
    pub async fn activate(&mut self) -> io::Result<LocalStream> {
        self.0.activate().await
    }

    /// Close this endpoint and any pending watcher. Repeated calls are harmless.
    /// After a successful activation the returned stream owns closure instead.
    pub fn close(&mut self) {
        self.0.close();
    }
}

/// The host's single-use, non-cloneable raw owner endpoint.
pub struct OwnerBootstrap(platform::Bootstrap);

impl OwnerBootstrap {
    /// Move the still-unregistered endpoint into explicitly selected child stdio.
    ///
    /// This transfers disconnect responsibility as well as the OS handle. Dropping the original
    /// bootstrap or the parent's post-spawn stdio copy cannot disconnect the child. Activation
    /// makes transfer invalid without disturbing a registered watcher.
    pub fn take_stdio(&mut self) -> io::Result<Stdio> {
        self.0.take_stdio()
    }

    /// Register on the host runtime and transfer the connected stream exactly once.
    ///
    /// Cancellation retains the same watcher and pending connection operation. Resume on the
    /// same runtime. The liveness probe does not consume bytes; the caller owns all reads and
    /// the protocol supervisor after success.
    pub async fn activate(&mut self) -> io::Result<LocalStream> {
        self.0.activate().await
    }

    /// Duplicate a dedicated inherited Unix owner socket without owning the original fd.
    ///
    /// Requires a connected, nonblocking AF_UNIX stream socket. The owned duplicate is
    /// close-on-exec and remains unregistered until activation. The trusted launcher is
    /// responsible for its provenance; OS checks do not authenticate the source.
    ///
    /// # Safety
    ///
    /// `fd` must remain open throughout this call and come from a dedicated owner bootstrap.
    /// It and all aliases must be unregistered with every reactor and have no concurrent I/O.
    /// After adoption the source and other aliases must not perform I/O or be registered with
    /// another reactor, including after activation. They retain their original physical owner;
    /// closing this value shuts down the connection without closing those fds.
    #[cfg(unix)]
    pub unsafe fn from_inherited_fd(fd: std::os::fd::RawFd) -> io::Result<Self> {
        // SAFETY: caller supplies the dedicated, valid, unregistered source described above.
        unsafe { platform::adopt(fd) }.map(Self)
    }

    /// Duplicate a dedicated inherited Windows owner pipe without owning the original handle.
    ///
    /// Requires the duplex, byte-mode, overlapped server end. The non-inheritable duplicate
    /// remains unregistered until activation. OS checks do not authenticate its provenance/ACL.
    ///
    /// # Safety
    ///
    /// `handle` must remain open throughout this call and come from a dedicated owner bootstrap.
    /// It and every alias must never have been registered with a reactor/IOCP and must have no
    /// concurrent I/O. The source and aliases must not perform I/O or be registered afterwards,
    /// including on another runtime after activation. Duplication does not remove an IOCP
    /// association. The source retains its physical owner; closing this value disconnects the
    /// pipe without closing the original inherited stdin handle.
    #[cfg(windows)]
    pub unsafe fn from_inherited_handle(
        handle: std::os::windows::io::RawHandle,
    ) -> io::Result<Self> {
        // SAFETY: caller supplies the dedicated, valid, unregistered source described above.
        unsafe { platform::adopt(handle) }.map(Self)
    }

    /// Disconnect and release this endpoint or its pending watcher. Idempotent.
    /// After successful activation the returned stream owns closure instead.
    pub fn close(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
#[path = "owner/tests.rs"]
mod tests;
