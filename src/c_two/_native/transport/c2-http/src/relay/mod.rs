//! C-Two HTTP relay server — bridges HTTP requests to IPC.
//!
//! This module is behind the `relay` feature gate. Enable it with:
//! ```toml
//! c2-http = { path = "...", features = ["relay"] }
//! ```

pub mod config;
pub mod route_table;
pub mod router;
pub mod server;
pub mod state;
pub mod types;

pub use config::RelayConfig;
pub use route_table::RouteTable;
pub use server::RelayServer;
pub use state::RelayState;
pub use types::*;
