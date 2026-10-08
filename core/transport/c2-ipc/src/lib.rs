//! Async IPC client for C-Two.
//!
//! Connects to a C-Two IPC server through its local OS stream, performs
//! handshake, and forwards requests using buddy SHM.
//!
//! # Architecture
//!
//! ```text
//! HTTP handler
//!     -> IpcClient::acquire_route_token(route contract, route token)
//!     -> IpcClient::call_bound(binding, method, payload)
//!         -> select inline / buddy SHM / chunked request transport
//!         -> send_task:  serialize frame -> write local stream
//!         -> recv_task:  read local stream -> match request_id -> oneshot -> caller
//! ```

pub mod client;
pub mod control;
pub mod pool;
pub mod response;
pub mod sync_client;

#[cfg(test)]
mod tests;

pub use c2_wire::shutdown_control::{DirectShutdownAck, ShutdownControlRouteOutcome};
pub use client::{
    ClientIpcConfig, IpcClient, IpcError, MethodTable, RequestBlock, RouteBinding, ServerPoolState,
};
pub use control::{
    local_endpoint_from_ipc_address, ping, ping_with_context, ping_with_endpoint, shutdown,
    shutdown_with_context, shutdown_with_endpoint,
};
pub use pool::{ClientCacheMemorySnapshot, ClientPool};
pub use response::{ResponseData, ResponseLease};
pub use sync_client::{IpcCallError, SyncClient, TransportPhase};
