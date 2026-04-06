//! C-Two HTTP transport layer.
//!
//! - `client`: HTTP client for connecting to relay servers (always available)
//! - `relay`: HTTP relay server bridging HTTP→IPC (requires `relay` feature)

pub mod client;

#[cfg(feature = "relay")]
pub mod relay;
