//! C-Two HTTP relay server — bridges HTTP requests to IPC.
//!
//! This module is behind the `relay` feature gate. Enable it with:
//! ```toml
//! c2-http = { path = "...", features = ["relay"] }
//! ```

#[cfg(test)]
macro_rules! test_commit_registration {
    (
        $state:expr,
        $name:expr,
        $server_id:expr,
        $server_instance_id:expr,
        $address:expr,
        $crm_ns:expr,
        $crm_name:expr,
        $crm_ver:expr,
        $abi_hash:expr,
        $signature_hash:expr,
        $max_payload_size:expr,
        $route_uid:expr,
        $route_revision:expr,
        $replacement:expr $(,)?
    ) => {{
        ($state).commit_register_upstream($crate::relay::authority::LocalRegistration {
            owner: $crate::relay::authority::LocalRouteOwner {
                name: $name,
                server_id: $server_id,
                server_instance_id: $server_instance_id,
                address: $address,
            },
            contract: $crate::relay::authority::AttestedRouteContract {
                crm_ns: $crm_ns,
                crm_name: $crm_name,
                crm_ver: $crm_ver,
                abi_hash: $abi_hash,
                signature_hash: $signature_hash,
                max_payload_size: $max_payload_size,
                route_uid: $route_uid,
                route_revision: $route_revision,
            },
            replacement: $replacement,
        })
    }};
}

pub(crate) mod authority;
pub(crate) mod background;
pub(crate) mod conn_pool;
pub(crate) mod disseminator;
pub(crate) mod gossip;
pub(crate) mod peer;
pub(crate) mod peer_handlers;
pub(crate) mod route_table;
pub(crate) mod router;
pub mod server;
pub(crate) mod state;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod types;
pub(crate) mod upstream_control;
pub(crate) mod url;

pub use c2_config::RelayConfig;
pub use server::{RelayControlError, RelayServer};
