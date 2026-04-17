//! Chunked payload reassembly backed by [`MemHandle`].
//!
//! Replaces the Python `ChunkAssembler` and `_ReplyChunkAssembler` with a
//! single Rust implementation that writes chunks directly into a unified
//! [`MemHandle`] (buddy SHM, dedicated SHM, or file-backed mmap).

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;

/// Reassembles chunked payloads into a contiguous [`MemHandle`].
///
/// # Lifecycle
/// ```text
/// new() → feed_chunk() × N → is_complete() → finish() → MemHandle
/// ```
///
/// On error or timeout, call `abort()` to release any partial MemHandle.
pub struct ChunkAssembler {
    total_chunks: usize,
    chunk_size: usize,
    received: usize,
    /// High-water mark: one past the last byte written.
    written_end: usize,
    handle: Option<MemHandle>,
    received_flags: Vec<bool>,
    /// Route name extracted from the first chunk (server-side).
    pub route_name: Option<String>,
    /// Method index extracted from the first chunk (server-side).
    pub method_idx: Option<u16>,
}

impl std::fmt::Debug for ChunkAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkAssembler")
            .field("total_chunks", &self.total_chunks)
            .field("received", &self.received)
            .field("written_end", &self.written_end)
            .finish()
    }
}

impl ChunkAssembler {
    /// Create a new assembler.
    ///
    /// `pool` is used to allocate the reassembly buffer via `alloc_handle`.
    /// `total_chunks` and `chunk_size` come from the first chunk's header.
    pub fn new(
        pool: &mut MemPool,
        total_chunks: usize,
        chunk_size: usize,
        max_total_chunks: usize,
        max_reassembly_bytes: usize,
    ) -> Result<Self, String> {
        if total_chunks == 0 {
            return Err("total_chunks must be > 0".into());
        }
        if total_chunks > max_total_chunks {
            return Err(format!(
                "total_chunks {total_chunks} exceeds limit {max_total_chunks}"
            ));
        }
        let alloc_size = total_chunks.checked_mul(chunk_size)
            .ok_or_else(|| "chunk allocation overflow".to_string())?;
        if alloc_size > max_reassembly_bytes {
            return Err(format!(
                "reassembly size {alloc_size} exceeds limit {max_reassembly_bytes}"
            ));
        }
        let handle = pool.alloc_handle(alloc_size)?;
        Ok(Self {
            total_chunks,
            chunk_size,
            received: 0,
            written_end: 0,
            handle: Some(handle),
            received_flags: vec![false; total_chunks],
            route_name: None,
            method_idx: None,
        })
    }

    /// Feed a chunk into the assembler.
    ///
    /// `pool` is needed to get a mutable slice into the MemHandle.
    /// Returns `true` when all chunks have been received.
    pub fn feed_chunk(
        &mut self,
        pool: &MemPool,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String> {
        if chunk_idx >= self.total_chunks {
            return Err(format!(
                "chunk_idx {chunk_idx} >= total_chunks {}",
                self.total_chunks
            ));
        }
        if self.received_flags[chunk_idx] {
            return Err(format!("duplicate chunk_idx {chunk_idx}"));
        }
        if data.len() > self.chunk_size {
            return Err(format!(
                "data length {} exceeds chunk_size {}",
                data.len(), self.chunk_size
            ));
        }
        let handle = self.handle.as_mut()
            .ok_or("assembler handle already taken")?;
        let offset = chunk_idx * self.chunk_size;
        let slice = pool.handle_slice_mut(handle);
        slice[offset..offset + data.len()].copy_from_slice(data);
        self.received_flags[chunk_idx] = true;
        self.received += 1;
        let end = offset + data.len();
        if end > self.written_end {
            self.written_end = end;
        }
        Ok(self.received == self.total_chunks)
    }

    /// Check if all chunks have been received.
    pub fn is_complete(&self) -> bool {
        self.received == self.total_chunks
    }

    /// Number of chunks received so far.
    pub fn received(&self) -> usize {
        self.received
    }

    /// Consume the assembler and return the completed [`MemHandle`].
    ///
    /// The returned handle's logical length is trimmed to `written_end`
    /// (one past the last byte written), which may be less than
    /// `total_chunks × chunk_size` if the last chunk was short.
    pub fn finish(mut self) -> Result<MemHandle, String> {
        if !self.is_complete() {
            return Err(format!(
                "incomplete: {}/{} chunks",
                self.received, self.total_chunks
            ));
        }
        let mut handle = self.handle.take()
            .ok_or("assembler handle already taken")?;
        handle.set_len(self.written_end);
        Ok(handle)
    }

    /// Abort reassembly, releasing the underlying MemHandle.
    ///
    /// `pool` is needed to free buddy/dedicated handles.
    pub fn abort(mut self, pool: &mut MemPool) {
        if let Some(handle) = self.handle.take() {
            pool.release_handle(handle);
        }
    }

    /// One past the last byte written (actual data length).
    pub fn written_end(&self) -> usize {
        self.written_end
    }

    /// Extract the underlying MemHandle without consuming self.
    ///
    /// Returns `None` if the handle was already taken (by a previous
    /// `take_handle`, `finish`, or `abort` call).
    /// Used by `ChunkRegistry` for safe error-path cleanup.
    pub fn take_handle(&mut self) -> Option<MemHandle> {
        self.handle.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;

    fn test_pool() -> MemPool {
        MemPool::new(PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_asm_test"),
        })
    }

    #[test]
    fn test_single_chunk() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 1, 4096, 512, 8 * (1 << 30)).unwrap();
        let data = b"hello world";
        let complete = asm.feed_chunk(&pool, 0, data).unwrap();
        assert!(complete);
        let handle = asm.finish().unwrap();
        assert_eq!(handle.len(), data.len());
        let slice = pool.handle_slice(&handle);
        assert_eq!(&slice[..data.len()], data);
        pool.release_handle(handle);
    }

    #[test]
    fn test_multi_chunk_in_order() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 3, 8, 512, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(&pool, 0, b"aaaaaaaa").unwrap()); // full
        assert!(!asm.feed_chunk(&pool, 1, b"bbbbbbbb").unwrap()); // full
        assert!(asm.feed_chunk(&pool, 2, b"cc").unwrap());        // last, short
        let handle = asm.finish().unwrap();
        assert_eq!(handle.len(), 18); // 8 + 8 + 2
        let slice = pool.handle_slice(&handle);
        assert_eq!(&slice[0..8], b"aaaaaaaa");
        assert_eq!(&slice[8..16], b"bbbbbbbb");
        assert_eq!(&slice[16..18], b"cc");
        pool.release_handle(handle);
    }

    #[test]
    fn test_out_of_order() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 2, 8, 512, 8 * (1 << 30)).unwrap();
        assert!(!asm.feed_chunk(&pool, 1, b"second").unwrap()); // last, short
        assert!(asm.feed_chunk(&pool, 0, b"firstttt").unwrap()); // full
        let handle = asm.finish().unwrap();
        assert_eq!(handle.len(), 14); // written_end = max(8+6, 0+8) = 14
        let slice = pool.handle_slice(&handle);
        assert_eq!(&slice[0..8], b"firstttt");
        assert_eq!(&slice[8..14], b"second");
        pool.release_handle(handle);
    }

    #[test]
    fn test_duplicate_chunk_rejected() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 2, 8, 512, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(&pool, 0, b"data").unwrap();
        let err = asm.feed_chunk(&pool, 0, b"dup").unwrap_err();
        assert!(err.contains("duplicate"));
        asm.abort(&mut pool);
    }

    #[test]
    fn test_abort_releases_handle() {
        let mut pool = test_pool();
        let asm = ChunkAssembler::new(&mut pool, 4, 4096, 512, 8 * (1 << 30)).unwrap();
        asm.abort(&mut pool);
        // Pool should still work after abort.
        let h = pool.alloc_handle(4096).unwrap();
        pool.release_handle(h);
    }

    #[test]
    fn test_finish_incomplete_fails() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 3, 8, 512, 8 * (1 << 30)).unwrap();
        asm.feed_chunk(&pool, 0, b"data").unwrap();
        let err = asm.finish().unwrap_err();
        assert!(err.contains("incomplete"));
    }

    #[test]
    fn test_zero_chunks_rejected() {
        let mut pool = test_pool();
        let err = ChunkAssembler::new(&mut pool, 0, 4096, 512, 8 * (1 << 30)).unwrap_err();
        assert!(err.contains("total_chunks must be > 0"));
    }

    #[test]
    fn test_oversized_chunk_data() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 2, 8, 512, 8 * (1 << 30)).unwrap();
        let err = asm.feed_chunk(&pool, 0, &[0u8; 16]).unwrap_err();
        assert!(err.contains("exceeds chunk_size"));
        asm.abort(&mut pool);
    }

    #[test]
    fn test_zero_chunk_size_rejected() {
        let mut pool = test_pool();
        let err = ChunkAssembler::new(&mut pool, 1, 0, 512, 8 * (1 << 30)).unwrap_err();
        assert!(err.contains("allocate 0 bytes") || err.contains("chunk_size"));
    }

    #[test]
    fn take_handle_extracts_and_clears() {
        let mut pool = test_pool();
        let mut asm = ChunkAssembler::new(&mut pool, 1, 4096, 512, 8 * (1 << 30)).unwrap();
        let h = asm.take_handle();
        assert!(h.is_some());
        let h2 = asm.take_handle();
        assert!(h2.is_none()); // second call returns None
        pool.release_handle(h.unwrap());
    }
}
