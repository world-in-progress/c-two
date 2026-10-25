//! Native peer identity failures and Windows TokenUser identity/ACL checks.
//!
//! This crate owns token queries, security descriptors and pipe-owner checks.
//! Transport connection identity, process liveness and runtime policy stay in
//! their owners. Same-user access does not imply equal trust or prevent DoS.

use std::{fmt, io};

#[derive(Debug)]
struct PeerIdentityMismatch(&'static str);

impl fmt::Display for PeerIdentityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PeerIdentityMismatch {}

/// Preserve a native identity refusal in the existing I/O error chain.
pub fn peer_identity_mismatch(message: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        PeerIdentityMismatch(message),
    )
}

/// OS access failures and untyped permission errors are not identity mismatches.
pub fn is_peer_identity_mismatch(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<PeerIdentityMismatch>())
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    LocalSecurityAttributes, current_scope_id, current_session_id, current_user_sid,
    grant_server_identity_query_access, verify_current_user_pipe_owner,
    verify_server_process_identity,
};
