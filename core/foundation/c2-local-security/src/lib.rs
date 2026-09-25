//! The Windows logon identity and ACL shared by local pipes and memory mappings.
//!
//! This crate owns security descriptors only. It does not own transports,
//! mappings, process liveness, or runtime policy.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{LocalSecurityAttributes, current_logon_sid, current_scope_id};
