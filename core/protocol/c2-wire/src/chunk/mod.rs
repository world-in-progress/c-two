//! Chunk codec and lifecycle management.
//!
//! - `header`: chunk header encode/decode (4-byte wire format)
//! - `config`: chunk reassembly configuration
//! - `registry`: sharded lifecycle manager for in-flight chunked transfers

pub mod config;
pub mod header;
pub mod registry;

// Re-export header codec at chunk:: level for backward compatibility.
// Existing code uses c2_wire::chunk::encode_chunk_header etc.
pub use header::*;

// Re-export key types at chunk:: level.
pub use config::ChunkConfig;
pub use registry::{ChunkRegistry, FinishedChunk, GcStats};
