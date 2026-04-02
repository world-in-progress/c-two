//! Unified response data — either inline bytes or SHM coordinates.

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
}

impl ResponseData {
    pub fn len(&self) -> usize {
        match self {
            ResponseData::Inline(v) => v.len(),
            ResponseData::Shm { data_size, .. } => *data_size as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
