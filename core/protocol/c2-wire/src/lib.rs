//! C-Two IPC wire protocol codec.
//!
//! Implements the binary frame format used by IPC transports:
//!
//! - **Frame header** (16 bytes): `[4B total_len LE][8B request_id LE][4B flags LE]`
//! - **Buddy payload** (11 bytes): `[2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]`
//! - **V2 call control**: route identity, expected contract, and method index
//! - **V2 reply control**: `[1B status][optional: 4B error_len LE + error_bytes]`
//! - **Chunk header** (4 bytes): `[2B chunk_idx LE][2B total_chunks LE]`
//! - **Control messages**: Segment announce, consumed, buddy announce
//! - **Handshake**: Segments + capabilities + routes + method tables
//! - **Route catalog control**: list, lookup, watch, ACK, and NACK payloads
//! - **MsgType**: Signal/message type enum
//!
//! All integers are little-endian.

pub mod assembler;
pub mod buddy;
pub mod chunk;
pub mod control;
pub mod flags;
pub mod frame;
pub mod handshake;
pub mod msg_type;
pub mod registration_control;
pub mod route_catalog_control;
pub mod shutdown_control;

#[cfg(test)]
mod tests;
