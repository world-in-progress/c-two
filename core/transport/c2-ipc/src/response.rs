//! Unified response data — either inline bytes, a bound SHM receive carrier, or a reassembled handle.

use std::sync::Arc;

use c2_wire::chunk::ReassemblyBacking;
use parking_lot::{Mutex, MutexGuard};

use crate::client::ServerPoolState;

/// Response data from a CRM call.
#[derive(Debug)]
pub enum ResponseData {
    /// Local IPC inline data (already in Rust heap).
    Inline(Vec<u8>),
    /// SHM buddy/dedicated block received from the server. The carrier binds
    /// the frame's coordinates to the client's exact server pool state; it
    /// is minted only at the reply-decode admission path and is the only
    /// local read/release authority for the block.
    Shm(ServerShmResponse),
    /// Reassembled chunked response. The carrier owns the client's
    /// reassembly pool, the handle, and the reassembly budget charge until
    /// release; trimming and holding keep the full capacity charged.
    Handle(ReassemblyBacking),
}

/// Move-only receive carrier for one admitted server SHM response block.
///
/// Constructed only where a reply frame is decoded against this client's
/// live server pool state. There is no public constructor from bare
/// coordinates: copying and releasing through this carrier are the safe
/// surface, and a [`ResponseLease`] owns the carrier.
pub struct ServerShmResponse {
    server_pool: Arc<Mutex<Option<ServerPoolState>>>,
    seg_idx: u16,
    generation: u32,
    offset: u32,
    data_size: u32,
    is_dedicated: bool,
    allocation_id: u64,
    budget: c2_mem::MemoryBudget,
    released: bool,
}

impl std::fmt::Debug for ServerShmResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ServerShmResponse(pool={:p}, seg={}, gen={}, off={}, size={}, ded={}, allocation_id={})",
            Arc::as_ptr(&self.server_pool),
            self.seg_idx,
            self.generation,
            self.offset,
            self.data_size,
            self.is_dedicated,
            self.allocation_id
        )
    }
}

impl ServerShmResponse {
    /// Admit one received SHM response block. Crate-private: only the reply
    /// decode path that actually received the frame may call this.
    pub(crate) fn admit(
        server_pool: Arc<Mutex<Option<ServerPoolState>>>,
        seg_idx: u16,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
        allocation_id: u64,
        budget: c2_mem::MemoryBudget,
    ) -> Self {
        Self {
            server_pool,
            seg_idx,
            generation,
            offset,
            data_size,
            is_dedicated,
            allocation_id,
            budget,
            released: false,
        }
    }

    /// Whether the backing is a dedicated (non-buddy) segment.
    pub fn is_dedicated(&self) -> bool {
        self.is_dedicated
    }

    /// The server's segment index (diagnostics and transport probes).
    pub fn seg_idx(&self) -> u16 {
        self.seg_idx
    }

    /// Advertised payload length.
    pub fn data_size(&self) -> u32 {
        self.data_size
    }

    /// Try to settle the backing without blocking on the pool lock.
    ///
    /// `Ok(false)` means the pool lock was contended and nothing happened.
    /// Used by the receive loop when a reply arrives whose waiter is gone.
    pub(crate) fn try_release(&mut self) -> Result<bool, String> {
        if self.released {
            return Ok(true);
        }
        let Some(server_pool) = self.server_pool.try_lock() else {
            return Ok(false);
        };
        self.release_with_guard(server_pool)?;
        self.released = true;
        Ok(true)
    }

    fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        self.release_with_guard(self.server_pool.lock())?;
        self.released = true;
        Ok(())
    }

    fn release_with_guard(
        &self,
        mut server_pool: MutexGuard<'_, Option<ServerPoolState>>,
    ) -> Result<(), String> {
        let state = server_pool
            .as_mut()
            .ok_or_else(|| "server pool not initialised".to_string())?;
        state.release_response(
            self.seg_idx,
            self.generation,
            self.offset,
            self.data_size,
            self.is_dedicated,
            self.allocation_id,
        )?;
        Ok(())
    }

    fn abandon(&mut self) {
        if !self.released {
            eprintln!("Warning: abandoning server SHM response release: {self:?}");
            self.budget.record_abandoned_release();
            self.released = true;
        }
    }
}

impl Drop for ServerShmResponse {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!("Warning: final server SHM response release failed: {error}; {self:?}");
            self.abandon();
        }
    }
}

impl ResponseData {
    pub fn len(&self) -> usize {
        match self {
            ResponseData::Inline(v) => v.len(),
            ResponseData::Shm(carrier) => carrier.data_size() as usize,
            ResponseData::Handle(h) => h.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Materialize response into owned bytes, reading from SHM if needed.
    ///
    /// The SHM carrier carries the exact server pool state it was admitted
    /// with; the caller cannot substitute another pool.
    pub fn into_bytes(self) -> Result<Vec<u8>, String> {
        ResponseLease::new(self).into_owned_bytes()
    }
}

/// Owns one response transport allocation independently from its copied bytes.
///
/// SDK callers obtain this through their Core client. The SHM carrier binds
/// its own pool state; reassembled handles carry their own pool and budget
/// charge inside the [`ReassemblyBacking`] carrier.
pub struct ResponseLease {
    response: Option<ResponseData>,
}

impl ResponseLease {
    pub fn new(response: ResponseData) -> Self {
        Self {
            response: Some(response),
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
            ResponseData::Shm(carrier) => {
                let mut server_pool = carrier.server_pool.lock();
                let state = server_pool
                    .as_mut()
                    .ok_or_else(|| "server pool not initialised".to_string())?;
                state.copy_response(
                    carrier.seg_idx,
                    carrier.generation,
                    carrier.offset,
                    carrier.data_size,
                    carrier.is_dedicated,
                )
            }
            ResponseData::Handle(backing) => backing
                .copy_bytes()
                .map_err(|error| format!("response handle copy failed: {error}")),
        }
    }

    /// Release transport storage exactly once.
    ///
    /// SHM coordinates are validated before any coordinate-derived free is
    /// attempted; reassembly backing release (storage first, then budget
    /// refund) is owned by the carrier. Failure retains the original carrier
    /// for another explicit release attempt.
    pub fn release(&mut self) -> Result<(), String> {
        let Some(response) = self.response.as_mut() else {
            return Ok(());
        };
        match response {
            ResponseData::Inline(_) => Ok(()),
            ResponseData::Shm(carrier) => carrier.release(),
            ResponseData::Handle(backing) => backing
                .release()
                .map_err(|error| format!("response handle release failed: {error}")),
        }?;
        self.response.take();
        Ok(())
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
        if release_result.is_err() {
            self.abandon();
        }
        combine_copy_and_release(copy_result, release_result)
    }

    /// Consume this obligation without another free or budget refund.
    pub fn abandon(mut self) {
        if let Some(response) = self.response.take() {
            match response {
                ResponseData::Inline(_) => {}
                ResponseData::Shm(mut carrier) => carrier.abandon(),
                ResponseData::Handle(backing) => backing.abandon(),
            }
        }
    }

    pub fn is_released(&self) -> bool {
        self.response.is_none()
    }
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
