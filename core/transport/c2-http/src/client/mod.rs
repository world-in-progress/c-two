//! HTTP client for C-Two relay transport.
//!
//! Provides [`HttpClient`] for making CRM calls through an HTTP relay
//! server, and [`HttpClientPool`] for reference-counted connection pooling.

mod call_control;
mod connect_deadline;
mod control;
mod http_client;
mod pool;
mod relay_aware;

pub use call_control::HttpCallControl;
#[cfg(feature = "relay")]
pub(crate) use control::validate_local_endpoint_namespace;
pub use control::{
    LOCAL_ENDPOINT_NAMESPACE_HEADER, RelayControlClient, RelayRegistration, RelayRegistrationScope,
    RelayResolvedRoutes, RelayRouteInfo,
};
pub use http_client::{HttpClient, HttpError, HttpInputOwner};
pub use pool::HttpClientPool;
pub use relay_aware::{
    HttpCallError, HttpCallPhase, RelayAwareClientConfig, RelayAwareHttpClient,
    RelayLocalIpcCandidate, RelayResolvedTarget,
};

#[cfg(all(test, feature = "relay"))]
mod controlled_tests;
