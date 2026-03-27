//! C-Two HTTP relay server — bridges HTTP requests to IPC v3.
//!
//! Supports multi-upstream dynamic registration: CRM processes register
//! themselves at runtime via `POST /_register`.
//!
//! # URL Mapping
//!
//! ```text
//! POST /_register                   →  register upstream {name, address}
//! POST /_unregister                 →  remove upstream {name}
//! GET  /_routes                     →  list registered routes
//! POST /{route_name}/{method_name}  →  relay to upstream IpcClient
//! GET  /health                      →  {"status":"ok","routes":[...]}
//! ```

pub mod config;
pub mod router;
pub mod server;
pub mod state;

pub use config::RelayConfig;
pub use server::RelayServer;
pub use state::RelayState;
