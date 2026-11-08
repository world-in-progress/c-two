//! C-Two IPC wire protocol codec.
//!
//! Implements the binary frame format used by IPC transports:
//!
//! - **Frame header** (16 bytes): `[4B total_len LE][8B request_id LE][4B flags LE]`
//! - **Buddy payload** (23 bytes): `[2B slot LE][4B generation LE][4B offset LE][4B data_size LE][1B kind][8B loan_id LE]`
//! - **RPC records**: OPEN, ITEM_BEGIN, ITEM_DATA, CREDIT, CANCEL, CALL_END and RELEASE
//! - **Control messages**: independent route and lifecycle payloads
//! - **Handshake**: fixed version preamble, capabilities, server identity, routes, and method tables
//! - **Route catalog control**: list, lookup, and NACK payloads
//! - **MsgType**: Signal/message type enum
//!
//! All integers are little-endian.

pub mod assembler;
pub mod buddy;
pub mod chunk;
pub mod connection;
pub mod flags;
pub mod frame;
pub mod handshake;
pub mod msg_type;
pub mod registration_control;
pub mod release;
pub mod route_catalog_control;
pub mod rpc;
pub mod shutdown_control;

#[cfg(test)]
mod rpc_encoding_tests;
#[cfg(test)]
mod tests;
