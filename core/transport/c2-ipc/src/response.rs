//! Unified response data — either inline bytes, SHM coordinates, or a reassembled handle.

use std::sync::Arc;

use c2_mem::{MemHandle, MemPool};
use parking_lot::{Mutex, RwLock};

use crate::client::ServerPoolState;

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
        server_pool: &Arc<Mutex<Option<ServerPoolState>>>,
        reassembly_pool: &Arc<RwLock<MemPool>>,
    ) -> Result<Vec<u8>, String> {
        ResponseLease::new(self, Arc::clone(server_pool), Arc::clone(reassembly_pool))
            .into_owned_bytes()
    }
}

/// Owns one response transport allocation independently from its copied bytes.
///
/// SDK callers obtain this through their Core client. The public constructor is
/// the low-level boundary used by transport-independent Core orchestration and
/// tests; it accepts only the exact pool owners associated with `response`.
pub struct ResponseLease {
    response: Option<ResponseData>,
    server_pool: Arc<Mutex<Option<ServerPoolState>>>,
    reassembly_pool: Arc<RwLock<MemPool>>,
}

impl ResponseLease {
    pub fn new(
        response: ResponseData,
        server_pool: Arc<Mutex<Option<ServerPoolState>>>,
        reassembly_pool: Arc<RwLock<MemPool>>,
    ) -> Self {
        Self {
            response: Some(response),
            server_pool,
            reassembly_pool,
        }
    }

    /// Validate and copy response bytes without releasing transport storage.
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String> {
        let response = self
            .response
            .as_ref()
            .ok_or_else(|| "response lease is already released".to_string())?;
        match response {
            ResponseData::Inline(bytes) => Ok(bytes.clone()),
            ResponseData::Shm {
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            } => {
                let mut server_pool = self.server_pool.lock();
                let state = server_pool
                    .as_mut()
                    .ok_or_else(|| "server pool not initialised".to_string())?;
                state.copy_response(*seg_idx, *offset, *data_size, *is_dedicated)
            }
            ResponseData::Handle(handle) => self
                .reassembly_pool
                .read()
                .copy_handle_data(handle)
                .map_err(|error| format!("response handle copy failed: {error}")),
        }
    }

    /// Release transport storage exactly once.
    ///
    /// SHM coordinates and handles are validated before any coordinate-derived
    /// free is attempted.
    pub fn release(&mut self) -> Result<(), String> {
        let Some(response) = self.response.take() else {
            return Ok(());
        };
        match response {
            ResponseData::Inline(_) => Ok(()),
            ResponseData::Shm {
                seg_idx,
                offset,
                data_size,
                is_dedicated,
            } => {
                let mut server_pool = self.server_pool.lock();
                let state = server_pool
                    .as_mut()
                    .ok_or_else(|| "server pool not initialised".to_string())?;
                state.release_response(seg_idx, offset, data_size, is_dedicated)
            }
            ResponseData::Handle(handle) => {
                let mut pool = self.reassembly_pool.write();
                pool.validate_handle(&handle).map_err(|error| {
                    format!("response handle release failed: validation failed: {error}")
                })?;
                release_response_handle(&mut pool, handle)
            }
        }
    }

    /// Materialize owned bytes and release the backing allocation.
    ///
    /// Inline bytes are moved. SHM and handle variants attempt release even
    /// when copying fails and retain both causes in the returned message.
    pub fn into_owned_bytes(mut self) -> Result<Vec<u8>, String> {
        let Some(response) = self.response.take() else {
            return Err("response lease is already released".to_string());
        };
        self.response = match response {
            ResponseData::Inline(bytes) => return Ok(bytes),
            transport_response => Some(transport_response),
        };

        let copy_result = self.copy_bytes();
        let release_result = self.release();
        combine_copy_and_release(copy_result, release_result)
    }

    pub fn is_released(&self) -> bool {
        self.response.is_none()
    }
}

impl Drop for ResponseLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn release_response_handle(pool: &mut MemPool, handle: MemHandle) -> Result<(), String> {
    match handle {
        MemHandle::Buddy {
            seg_idx,
            offset,
            len,
        } => {
            let data_size = u32::try_from(len)
                .map_err(|_| "response buddy handle length exceeds the wire address space")?;
            pool.free_at(u32::from(seg_idx), offset, data_size, false)
                .map_err(|error| format!("response handle release failed: {error}"))?;
        }
        MemHandle::Dedicated { seg_idx, len } => {
            let data_size = u32::try_from(len)
                .map_err(|_| "response dedicated handle length exceeds the wire address space")?;
            pool.free_at(u32::from(seg_idx), 0, data_size, true)
                .map_err(|error| format!("response handle release failed: {error}"))?;
        }
        MemHandle::FileSpill { .. } => {}
    }
    Ok(())
}

fn combine_copy_and_release(
    copy_result: Result<Vec<u8>, String>,
    release_result: Result<(), String>,
) -> Result<Vec<u8>, String> {
    match (copy_result, release_result) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(copy_error), Ok(())) => Err(copy_error),
        (Ok(_), Err(release_error)) => Err(release_error),
        (Err(copy_error), Err(release_error)) => Err(format!(
            "response copy failed: {copy_error}; response release also failed: {release_error}"
        )),
    }
}
