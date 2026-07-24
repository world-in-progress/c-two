//! HTTP client for C-Two relay transport.
//!
//! Provides [`HttpClient`] for making CRM calls through an HTTP relay
//! server, and [`HttpClientPool`] for reference-counted connection pooling.

mod control;
mod http_client;
mod pool;
mod relay_aware;

pub use control::{RelayControlClient, RelayRegistration, RelayRouteInfo};
pub use http_client::{HttpClient, HttpError};
pub use pool::HttpClientPool;
pub use relay_aware::{
    RelayAwareClientConfig, RelayAwareHttpClient, RelayLocalIpcCandidate, RelayResolvedTarget,
};
