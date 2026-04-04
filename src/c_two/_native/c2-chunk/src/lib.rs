//! Chunk reassembly lifecycle management.
//!
//! Provides [`ChunkRegistry`] — a sharded, thread-safe manager for in-flight
//! chunked transfers. Used identically by both `c2-server` and `c2-ipc`.

pub mod config;
pub mod promote;
pub mod registry;

pub use config::ChunkConfig;
pub use promote::promote_to_shm;
pub use registry::{ChunkRegistry, GcStats};
