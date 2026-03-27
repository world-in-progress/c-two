//! C-Two IPC wire protocol codec.
//!
//! Implements the binary frame format used by IPC v3 transports:
//!
//! - **Frame header** (16 bytes): `[4B total_len LE][8B request_id LE][4B flags LE]`
//! - **Buddy payload** (11 bytes): `[2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]`
//! - **V2 call control**: `[1B name_len][route_name UTF-8][2B method_idx LE]`
//! - **V2 reply control**: `[1B status][optional: 4B error_len LE + error_bytes]`
//! - **Handshake v5**: Segments + capabilities + routes + method tables
//!
//! All integers are little-endian. Compatible with Python `c_two.rpc_v2.wire`
//! and `c_two.rpc_v2.protocol`.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec, vec::Vec};

pub mod flags;
pub mod frame;
pub mod buddy;
pub mod control;
pub mod handshake;

#[cfg(test)]
mod tests;
