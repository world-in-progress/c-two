//! Chunk codec and lifecycle management.
//!
//! - `header`: chunk header encode/decode (4-byte wire format)
//! - `backing`: owned reassembly backing carrier (pool + handle + budget charge)
//! - `config`: chunk reassembly configuration
//! - `registry`: sharded lifecycle manager for in-flight chunked transfers

pub mod backing;
pub mod config;
pub mod header;
pub mod registry;

// Re-export header codec at chunk:: level for backward compatibility.
// Existing code uses c2_wire::chunk::encode_chunk_header etc.
pub use header::*;

// Re-export key types at chunk:: level.
pub use backing::ReassemblyBacking;
pub use config::ChunkConfig;
pub use registry::{ChunkAssemblyId, ChunkRegistry, FinishedChunk, GcStats};

/// The actual reason chunk admission failed, decided before publication.
/// Diagnostic text never determines protocol versus local capacity semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkAdmissionError {
    Protocol(String),
    Duplicate { conn_id: u64, request_id: u64 },
    Capacity(String),
}

impl std::fmt::Display for ChunkAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) | Self::Capacity(message) => f.write_str(message),
            Self::Duplicate {
                conn_id,
                request_id,
            } => {
                write!(f, "duplicate assembly for ({conn_id}, {request_id})")
            }
        }
    }
}

impl std::error::Error for ChunkAdmissionError {}
