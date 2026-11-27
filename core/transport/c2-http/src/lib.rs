//! C-Two HTTP transport layer.
//!
//! - `client`: HTTP client for connecting to relay servers (always available)
//! - `relay`: HTTP relay server bridging HTTP→IPC (requires `relay` feature)

pub mod client;
mod http_wire;
pub use http_wire::{
    CALL_PATH, KEYED_PATH, OUTCOME_PATH, RECOVER_PATH, TOPIC_CONFIRM_PATH, TOPIC_SUBSCRIBE_PATH,
    decode_control_error,
};

mod outcome_wire;
mod payload;
pub mod send_budget;
pub mod stream_wire;
pub mod topic_wire;

#[cfg(feature = "relay")]
pub mod relay;

mod tls;
pub use tls::{relay_client_builder, relay_tls_config};

/// Validate a relay base URL before opening a business or control path.
pub fn valid_relay_url(url: &str) -> bool {
    parse_relay_url(url).is_some()
}

fn parse_relay_url(url: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).ok()?;
    (matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none())
    .then_some(parsed)
}
