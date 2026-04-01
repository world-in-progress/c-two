//! Async IPC client for C-Two relay.
//!
//! Connects to a Python `ServerV2` via Unix Domain Socket, performs
//! handshake, and forwards HTTP-originated requests using buddy SHM.
//!
//! # Architecture
//!
//! ```text
//! HTTP handler
//!     → IpcClient::call(route, method_idx, payload)
//!         → send_task:  serialize frame → write UDS
//!         → recv_task:  read UDS → match request_id → oneshot → caller
//! ```

pub mod client;
pub mod pool;
pub mod shm;
pub mod sync_client;

#[cfg(test)]
mod tests;

pub use client::{IpcClient, IpcConfig, IpcError, MethodTable};
pub use pool::ClientPool;
pub use shm::{MappedSegment, SegmentCache, ShmError};
pub use sync_client::SyncClient;

