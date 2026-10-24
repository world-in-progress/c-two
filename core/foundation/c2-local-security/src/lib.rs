//! Windows TokenUser identity and ACLs shared by local pipes and memory mappings.
//!
//! This crate owns token queries, security descriptors and pipe-owner checks.
//! Transport connection identity, process liveness and runtime policy stay in
//! their owners. Same-user access does not imply equal trust or prevent DoS.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    LocalSecurityAttributes, current_scope_id, current_session_id, current_user_sid,
    grant_server_identity_query_access, verify_current_user_pipe_owner,
    verify_server_process_identity,
};
