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

/// Build a relay client builder with an explicit proxy policy.
pub fn relay_client_builder_with_proxy(use_proxy: bool) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().pool_idle_timeout(c2_config::IDLE_CONNECTION_TIMEOUT);
    if use_proxy {
        builder
    } else {
        builder.no_proxy()
    }
}

/// Validate a relay base URL before opening a business or control path.
pub fn valid_relay_url(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.username().is_empty()
                && parsed.password().is_none()
        }
        Err(_) => false,
    }
}
