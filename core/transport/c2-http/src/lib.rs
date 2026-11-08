//! C-Two HTTP transport layer.
//!
//! - `client`: HTTP client for connecting to relay servers (always available)
//! - `relay`: HTTP relay server bridging HTTP→IPC (requires `relay` feature)

pub mod client;
mod payload;
pub mod send_budget;
pub mod stream_wire;

#[cfg(feature = "relay")]
pub mod relay;

/// Build a relay client builder with an explicit proxy policy.
pub fn relay_client_builder_with_proxy(use_proxy: bool) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if use_proxy {
        builder
    } else {
        builder.no_proxy()
    }
}
