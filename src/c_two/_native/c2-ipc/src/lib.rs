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
pub mod shm;

#[cfg(test)]
mod tests;

pub use client::{IpcClient, IpcError, MethodTable};
pub use shm::{MappedSegment, SegmentCache, ShmError};

