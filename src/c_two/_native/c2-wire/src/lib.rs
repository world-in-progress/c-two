//! C-Two IPC wire protocol codec.
//!
//! Implements the binary frame format used by IPC transports:
//!
//! - **Frame header** (16 bytes): `[4B total_len LE][8B request_id LE][4B flags LE]`
//! - **Buddy payload** (11 bytes): `[2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]`
//! - **V2 call control**: `[1B name_len][route_name UTF-8][2B method_idx LE]`
//! - **V2 reply control**: `[1B status][optional: 4B error_len LE + error_bytes]`
//! - **Chunk header** (4 bytes): `[2B chunk_idx LE][2B total_chunks LE]`
//! - **Control messages**: Segment announce, consumed, buddy announce
//! - **Handshake**: Segments + capabilities + routes + method tables
//! - **MsgType**: Signal/message type enum
//!
//! All integers are little-endian.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};

pub mod flags;
pub mod frame;
pub mod buddy;
pub mod control;
pub mod chunk;
pub mod ctrl;
pub mod msg_type;
pub mod handshake;
#[cfg(feature = "std")]
pub mod assembler;

#[cfg(test)]
mod tests;
