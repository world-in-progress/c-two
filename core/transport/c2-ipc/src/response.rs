//! Move-only response carriers. Every public call materializes these before
//! returning, so SDK hold owns only local bytes.

use std::sync::Arc;

use c2_mem::retention::{BudgetedBuffer, PayloadBufferReservation};
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

    pub(crate) fn copy_into(self, destination: &mut [u8]) -> Result<(), String> {
        // SAFETY: the admitted carrier pins this immutable peer span until
        // copy returns; dropping it then ends the read and queues RELEASE.
        unsafe { self.read.copy_into(destination) }
    }

    pub(crate) fn copy_range_into(
        &self,
        offset: usize,
        destination: &mut [u8],
    ) -> Result<(), String> {
        // SAFETY: this move-only carrier pins the mapping and remote loan for
        // the entire synchronous copy. The borrow ends before any await.
        unsafe { self.read.copy_range_into(offset, destination) }
    }
}

/// Sequential local reads from one admitted response carrier. A SHM loan is
/// released immediately after its final range has been copied.
pub struct ResponseCursor {
    data: Option<ResponseData>,
    offset: usize,
    len: usize,
}

impl ResponseCursor {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn remaining(&self) -> usize {
        self.len - self.offset
    }

    pub fn read_chunk(
        &mut self,
        mut reservation: PayloadBufferReservation,
        max_bytes: usize,
    ) -> Result<Option<BudgetedBuffer>, crate::client::IpcError> {
        let take = self
            .remaining()
            .min(max_bytes)
            .min(usize::try_from(reservation.bytes()).unwrap_or(usize::MAX));
        if take == 0 {
            if self.remaining() == 0 {
                return Ok(None);
            }
            return Err(crate::client::IpcError::Config(
                "positive response copy window required".into(),
            ));
        }
        let mut bytes = vec![0; take];
        if bytes.capacity() as u64 > reservation.bytes() {
            return Err(crate::client::IpcError::Config(
                "response copy allocation exceeds reservation".into(),
            ));
        }
        match self.data.as_ref().expect("live response cursor") {
            ResponseData::Inline(source) => {
                bytes.copy_from_slice(&source[self.offset..self.offset + take])
            }
            ResponseData::Shm(carrier) => carrier
                .copy_range_into(self.offset, &mut bytes)
                .map_err(crate::client::IpcError::Shm)?,
            ResponseData::Handle(backing) => backing
                .with_slice(|source| {
                    bytes.copy_from_slice(&source[self.offset..self.offset + take])
                })
                .map_err(crate::client::IpcError::Shm)?,
        }
        self.offset += take;
        if self.remaining() == 0 {
            self.data.take();
        }
        let charge = reservation
            .split_off(bytes.capacity() as u64)
            .expect("bounded by reservation");
        Ok(Some(BudgetedBuffer::new(bytes, charge)))
    }
}

impl ResponseData {
    pub fn into_cursor(self) -> ResponseCursor {
        let len = self.len();
        ResponseCursor {
            data: Some(self),
            offset: 0,
            len,
        }
    }
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
