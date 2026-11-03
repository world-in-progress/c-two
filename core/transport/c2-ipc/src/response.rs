//! Move-only response carriers. Every public call materializes these before
//! returning, so SDK hold owns only local bytes.

use std::sync::Arc;

use c2_mem::{PeerMappingError, PeerRead};
use c2_wire::{buddy::BuddyPayload, chunk::ReassemblyBacking, connection::RemoteLoan};
use parking_lot::RwLock;

use crate::client::ServerPoolState;

#[derive(Debug)]
pub enum ResponseData {
    Inline(Vec<u8>),
    Shm(ServerShmResponse),
    Handle(ReassemblyBacking),
}

/// An admitted remote read, bound to its original mapping and physical stream.
/// Dropping an unread carrier also queues exactly one release record.
pub struct ServerShmResponse {
    // Fields drop in order: end the mapped read before queueing its release.
    read: PeerRead,
    payload: BuddyPayload,
    _loan: RemoteLoan,
}

impl std::fmt::Debug for ServerShmResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerShmResponse")
            .field("payload", &self.payload)
            .finish()
    }
}

impl ServerShmResponse {
    pub(crate) fn admit(
        pool: Arc<RwLock<ServerPoolState>>,
        payload: BuddyPayload,
        loan: RemoteLoan,
    ) -> Result<Self, PeerMappingError> {
        // The loan is already admitted: mapping/validation failure ends its
        // read interval and queues release via Drop, just like a copy failure.
        let read = pool.write().admit(&payload)?;
        Ok(Self {
            read,
            payload,
            _loan: loan,
        })
    }

    pub fn is_dedicated(&self) -> bool {
        self.payload.is_dedicated
    }
    pub fn seg_idx(&self) -> u16 {
        self.payload.seg_idx
    }
    pub fn data_size(&self) -> u32 {
        self.payload.data_size
    }

    fn into_owned_bytes(self) -> Result<Vec<u8>, String> {
        // SAFETY: the admitted move-only read pins the original mapping, and
        // its remote loan keeps the peer's span immutable through this copy.
        let result = unsafe { self.read.copy() };
        drop(self);
        result
    }
}

impl ResponseData {
    pub fn len(&self) -> usize {
        match self {
            Self::Inline(bytes) => bytes.len(),
            Self::Shm(carrier) => carrier.data_size() as usize,
            Self::Handle(backing) => backing.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn into_owned_bytes(self) -> Result<Vec<u8>, String> {
        match self {
            Self::Inline(bytes) => Ok(bytes),
            Self::Shm(carrier) => carrier.into_owned_bytes(),
            Self::Handle(mut backing) => {
                let copied = backing
                    .copy_bytes()
                    .map_err(|error| format!("response handle copy failed: {error}"));
                backing.release();
                copied
            }
        }
    }
}
