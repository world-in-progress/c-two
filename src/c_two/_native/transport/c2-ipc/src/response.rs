//! Unified response data — either inline bytes, SHM coordinates, or a reassembled handle.

use c2_mem::MemHandle;

/// Response data from a CRM call.
#[derive(Debug)]
pub enum ResponseData {
    /// UDS inline data (already in Rust heap).
    Inline(Vec<u8>),
    /// SHM buddy/dedicated data (coordinates only — no copy yet).
    Shm {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Reassembled chunked response (owned by client's reassembly pool).
    Handle(MemHandle),
}

impl ResponseData {
    pub fn len(&self) -> usize {
        match self {
            ResponseData::Inline(v) => v.len(),
            ResponseData::Shm { data_size, .. } => *data_size as usize,
            ResponseData::Handle(h) => h.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Extract inline bytes directly. Panics on SHM/Handle variants.
    ///
    /// Prefer `into_bytes_with_pool()` when SHM responses are possible.
    pub fn into_inline_bytes(self) -> Vec<u8> {
        match self {
            ResponseData::Inline(v) => v,
            ResponseData::Shm { .. } => {
                panic!("into_inline_bytes called on SHM response — use into_bytes_with_pool()")
            }
            ResponseData::Handle(_) => {
                panic!("into_inline_bytes called on Handle response — use into_bytes_with_pool()")
            }
        }
    }

    /// Materialize response into owned bytes, reading from SHM if needed.
    ///
    /// Used by the relay which must copy data before forwarding over HTTP.
    pub fn into_bytes_with_pool(
        self,
        server_pool: &std::sync::Arc<parking_lot::Mutex<Option<super::client::ServerPoolState>>>,
        reassembly_pool: &std::sync::Arc<parking_lot::RwLock<c2_mem::MemPool>>,
    ) -> Result<Vec<u8>, String> {
        match self {
            ResponseData::Inline(v) => Ok(v),
            ResponseData::Shm { seg_idx, offset, data_size, is_dedicated } => {
                let mut guard = server_pool.lock();
                let state = guard.as_mut().ok_or("server pool not initialised")?;
                let (data, _) = state.read_and_free(seg_idx, offset, data_size, is_dedicated)?;
                Ok(data)
            }
            ResponseData::Handle(handle) => {
                let pool = reassembly_pool.read();
                let slice = pool.handle_slice(&handle);
                let data = slice.to_vec();
                drop(pool);
                reassembly_pool.write().release_handle(handle);
                Ok(data)
            }
        }
    }
}
