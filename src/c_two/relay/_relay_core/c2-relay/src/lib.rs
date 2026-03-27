//! C-Two HTTP relay server — bridges HTTP requests to IPC v3.
//!
//! ```text
//! HTTP Client → POST /{route}/{method} → axum → c2-ipc → UDS+SHM → Python ServerV2
//! ```
//!
//! # URL Mapping
//!
//! ```text
//! POST /{route_name}/{method_name}  →  IpcClient.call(route, method, body)
//! GET  /health                      →  {"status":"ok","routes":[...]}
//! GET  /routes                      →  {"routes":[{"name":"grid","methods":["hello",...]}]}
//! ```

pub mod router;
pub mod state;
pub mod config;

pub use config::RelayConfig;
pub use state::RelayState;
