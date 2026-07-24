//! Async IPC client for C-Two.
//!
//! Connects to a C-Two IPC server via Unix Domain Socket, performs
//! handshake, and forwards requests using buddy SHM.
//!
//! # Architecture
//!
//! ```text
//! HTTP handler
//!     -> IpcClient::acquire_route_token(route contract, route token)
//!     -> IpcClient::call_bound(binding, method, payload)
//!         -> select inline / buddy SHM / chunked request transport
//!         -> send_task:  serialize frame -> write UDS
//!         -> recv_task:  read UDS -> match request_id -> oneshot -> caller
//! ```

pub mod client;
pub mod control;
pub mod pool;
pub mod response;
pub mod shm;
pub mod sync_client;

#[cfg(test)]
mod tests;

pub use c2_wire::shutdown_control::{DirectShutdownAck, ShutdownControlRouteOutcome};
pub use client::{
    ClientIpcConfig, IpcClient, IpcError, MethodTable, RouteBinding, ServerPoolState,
};
pub use control::{ping, shutdown, socket_path_from_ipc_address};
pub use pool::ClientPool;
pub use response::{ResponseData, ResponseLease};
pub use shm::{MappedSegment, SegmentCache, ShmError};
pub use sync_client::SyncClient;
