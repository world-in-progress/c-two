//! Synchronous control state for one physical IPC connection.
//!
//! Drivers retain business bytes, budgets and actual streams. This module
//! returns control bytes and allocation-reclamation actions; it performs no
//! transport I/O and invokes the injected notification only outside its lock.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use c2_config::IPC_RELEASE_BATCH_CAPACITY;
use c2_mem::{MemHandle, MemPool};
use parking_lot::{Mutex, RwLock};

use crate::frame::{FrameHeader, HEADER_SIZE};
use crate::release::{append_release_frame, decode_release_frame};

static LOAN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSide {
    Client,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    ProtocolViolation(String),
    BeforeHandshake,
    Deactivated,
    PeerEnded,
    Closed,
    WriterBusy,
    WriterBroken,
    LoanIdExhausted,
    Capacity(&'static str),
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProtocolViolation(message) => write!(f, "ProtocolViolation: {message}"),
            Self::BeforeHandshake => f.write_str("frame before handshake"),
            Self::Deactivated => f.write_str("physical connection is deactivated"),
            Self::PeerEnded => f.write_str("peer has ended"),
            Self::Closed => f.write_str("physical connection is closed"),
            Self::WriterBusy => f.write_str("a write submission is already in progress"),
            Self::WriterBroken => f.write_str("write framing can no longer be continued"),
            Self::LoanIdExhausted => f.write_str("process loan ID space exhausted"),
            Self::Capacity(scope) => write!(f, "connection storage exhausted: {scope}"),
        }
    }
}

impl std::error::Error for ConnectionError {}

struct Allocation {
    pool: Arc<RwLock<MemPool>>,
    handle: Option<MemHandle>,
    published: bool,
}

impl Allocation {
    fn release(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.pool.write().release_handle(handle);
        }
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        // A prepared allocation was never exposed. Published allocations may
        // only be reused after a release record or peer end. Final connection
        // Drop therefore leaves any unconfirmed allocation to pool teardown.
        if !self.published {
            self.release();
        }
    }
}

#[derive(Default, PartialEq, Eq)]
enum WriterState {
    #[default]
    Idle,
    Reserved,
    InFrame,
    AwaitingFlush,
    Broken,
}

#[derive(Default)]
struct State {
    handshake_done: bool,
    deactivated: bool,
    peer_ended: bool,
    closed: bool,
    // Both maps are one loan book partitioned by publication state. Each
    // allocation occurs exactly once; publication only moves its owner.
    prepared: HashMap<u64, Allocation>,
    published: HashMap<u64, Allocation>,
    settling: usize,
    remote_reads: usize,
    releases: VecDeque<u64>,
    inflight_releases: usize,
    pending_controls: usize,
    writer: WriterState,
}

struct Shared {
    side: ConnectionSide,
    state: Mutex<State>,
    wake: Box<dyn Fn() + Send + Sync>,
}

/// Cloneable access to the same physical connection's control state.
#[derive(Clone)]
pub struct ConnectionCore {
    shared: Arc<Shared>,
}

impl ConnectionCore {
    /// `wake` must only notify the unique writer; it must not allocate, block
    /// on network work, or perform I/O. Reentrant observation is safe.
    pub fn new(side: ConnectionSide, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                side,
                state: Mutex::new(State::default()),
                wake: Box::new(wake),
            }),
        }
    }

    /// The driver supplies this fact only after the final metadata ACK.
    pub fn finish_handshake(&self) -> Result<(), ConnectionError> {
        let mut state = self.shared.state.lock();
        if state.handshake_done {
            drop(state);
            drop(self.deactivate());
            return Err(ConnectionError::ProtocolViolation(
                "duplicate handshake".into(),
            ));
        }
        check_live(&state)?;
        state.handshake_done = true;
        Ok(())
    }

    /// Register the real owner before its descriptor can reach the writer.
    /// Dropping the returned token before publication cancels the allocation.
    pub fn prepare_loan(
        &self,
        pool: Arc<RwLock<MemPool>>,
        handle: MemHandle,
    ) -> Result<PreparedLoan, ConnectionError> {
        self.prepare_loan_from(pool, handle, &LOAN_SEQUENCE)
    }

    fn prepare_loan_from(
        &self,
        pool: Arc<RwLock<MemPool>>,
        handle: MemHandle,
        sequence: &AtomicU64,
    ) -> Result<PreparedLoan, ConnectionError> {
        // Declared before any connection guard, so rejection releases the
        // local allocation only after that guard has been dropped.
        let allocation = Allocation {
            pool,
            handle: Some(handle),
            published: false,
        };
        let result = (|| {
            let mut state = self.shared.state.lock();
            check_business(&state)?;
            state
                .prepared
                .try_reserve(1)
                .map_err(|_| ConnectionError::Capacity("loan book"))?;
            let pending = state.prepared.len() + 1;
            state
                .published
                .try_reserve(pending)
                .map_err(|_| ConnectionError::Capacity("published loan book"))?;
            let id = sequence
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map(|n| n + 1)
                .map_err(|_| ConnectionError::LoanIdExhausted)?;
            state.prepared.insert(id, allocation);
            Ok(PreparedLoan {
                shared: self.shared.clone(),
                id,
            })
        })();
        if matches!(
            result,
            Err(ConnectionError::Capacity(_) | ConnectionError::LoanIdExhausted)
        ) {
            drop(self.deactivate());
        }
        result
    }

    /// Admit an already decoded SHM loan. The opaque guard must retain the
    /// original OS stream and mapping until the last remote read has ended.
    /// Late loans on a deactivated but readable connection are still admitted
    /// so the driver can discard/consume them and send their release records.
    pub fn admit_remote_loan(
        &self,
        loan_id: u64,
        guard: Box<dyn Send>,
    ) -> Result<RemoteLoan, ConnectionError> {
        let result = (|| {
            let mut state = self.shared.state.lock();
            if state.closed {
                return Err(ConnectionError::Closed);
            }
            if state.peer_ended {
                return Err(ConnectionError::PeerEnded);
            }
            if !state.handshake_done {
                return Err(ConnectionError::BeforeHandshake);
            }
            if loan_id == 0 {
                return Err(ConnectionError::ProtocolViolation(
                    "zero remote loan ID".into(),
                ));
            }
            // Capacity covers every live node, queued record and record held
            // by a writer. Taking a prefix must not spend another node's slot.
            let additional = state
                .remote_reads
                .checked_add(state.inflight_releases)
                .and_then(|n| n.checked_add(1))
                .ok_or(ConnectionError::Capacity("release queue"))?;
            state
                .releases
                .try_reserve(additional)
                .map_err(|_| ConnectionError::Capacity("release queue"))?;
            state.remote_reads += 1;
            Ok(RemoteLoan {
                shared: self.shared.clone(),
                loan_id,
                guard: Some(guard),
            })
        })();
        if matches!(
            result,
            Err(ConnectionError::Capacity(_)
                | ConnectionError::ProtocolViolation(_)
                | ConnectionError::BeforeHandshake)
        ) {
            drop(self.deactivate());
        }
        result
    }

    /// Reserve a control frame before handing its bytes to the driver's queue.
    /// The move-only token keeps a deactivated connection open until that
    /// submission is flushed or discarded. Controls carry no new loans and
    /// remain admissible after deactivation, including standalone negotiation
    /// signals before the RPC handshake. Closed/ended/broken streams reject it.
    pub fn prepare_control_frame(&self) -> Result<ControlFrame, ConnectionError> {
        let mut state = self.shared.state.lock();
        check_writable(&state)?;
        state.pending_controls += 1;
        Ok(ControlFrame {
            shared: self.shared.clone(),
        })
    }

    /// Reserve the unique writer for queued releases and at most one supplied
    /// frame (one chunk or one control frame). Frame bytes and budget guards
    /// stay with the driver. Business writes require a live completed handshake;
    /// control writes can finish negotiation or run after deactivation.
    /// Rejected requests consume/cancel their original preparation tokens.
    pub fn begin_write(
        &self,
        request: WriteRequest,
    ) -> Result<Option<WriteBatch>, ConnectionError> {
        let (loans, control, business) = match request {
            WriteRequest::Business(loans) => (loans, None, true),
            WriteRequest::Control(control) => (Vec::new(), Some(control), false),
            WriteRequest::Releases => (Vec::new(), None, false),
        };
        let has_frame = business || control.is_some();
        let result = (|| {
            let mut state = self.shared.state.lock();
            check_writable(&state)?;
            if !state.handshake_done && control.is_none() {
                return Err(ConnectionError::BeforeHandshake);
            }
            if business && state.deactivated {
                return Err(ConnectionError::Deactivated);
            }
            if state.writer != WriterState::Idle {
                return Err(ConnectionError::WriterBusy);
            }
            if control
                .as_ref()
                .is_some_and(|control| !Arc::ptr_eq(&control.shared, &self.shared))
                || loans
                    .iter()
                    .any(|loan| !Arc::ptr_eq(&loan.shared, &self.shared))
            {
                return Err(ConnectionError::ProtocolViolation(
                    "preparation token belongs to another connection".into(),
                ));
            }
            if !has_frame && state.releases.is_empty() {
                return Ok(None);
            }
            let count = state.releases.len();
            let frames = count.div_ceil(IPC_RELEASE_BATCH_CAPACITY);
            let size = count
                .checked_mul(8)
                .and_then(|n| frames.checked_mul(HEADER_SIZE + 2)?.checked_add(n))
                .ok_or(ConnectionError::Capacity("release frames"))?;
            let mut prefix = Vec::new();
            prefix
                .try_reserve_exact(size)
                .map_err(|_| ConnectionError::Capacity("release frames"))?;
            while !state.releases.is_empty() {
                let mut ids = [0; IPC_RELEASE_BATCH_CAPACITY];
                let n = state.releases.len().min(IPC_RELEASE_BATCH_CAPACITY);
                for id in &mut ids[..n] {
                    *id = state.releases.pop_front().unwrap();
                }
                append_release_frame(
                    &mut prefix,
                    &ids[..n],
                    self.shared.side == ConnectionSide::Server,
                );
            }
            state.inflight_releases += count;
            state.writer = WriterState::Reserved;
            Ok(Some(WriteBatch {
                shared: self.shared.clone(),
                loans,
                prefix,
                release_count: count,
                frames_remaining: frames + usize::from(has_frame),
                business,
                control,
                attempted: false,
                finished: false,
            }))
        })();
        if matches!(
            result,
            Err(ConnectionError::Capacity(_)
                | ConnectionError::ProtocolViolation(_)
                | ConnectionError::BeforeHandshake)
        ) {
            drop(self.deactivate());
        }
        result
    }

    /// Validate one release boundary, then settle IDs in wire order. On error,
    /// already removed owners remain in `reclaims`; they are never rolled back.
    pub fn receive_release(&self, header: &FrameHeader, payload: &[u8]) -> ReleaseOutcome {
        let mut reclaims = Reclaims::new(self.shared.clone());
        let mut state = self.shared.state.lock();
        let batch = decode_release_frame(
            header,
            payload,
            self.shared.side == ConnectionSide::Client,
            state.handshake_done,
        );
        let error = match batch {
            Err(error) => Some(ConnectionError::ProtocolViolation(error.to_string())),
            Ok(batch) => {
                let mut error = None;
                for (slot, id) in reclaims.records.iter_mut().zip(batch.ids()) {
                    match state.published.remove(id) {
                        Some(allocation) => {
                            *slot = Some(allocation);
                            state.settling += 1;
                        }
                        None => {
                            error = Some(ConnectionError::ProtocolViolation(format!(
                                "unknown or unpublished loan ID {id}"
                            )));
                            break;
                        }
                    }
                }
                error
            }
        };
        if error.is_some() {
            stop_locked(&mut state, &mut reclaims);
        }
        drop(state);
        if error.is_some() {
            (self.shared.wake)();
        }
        ReleaseOutcome { reclaims, error }
    }

    /// Stop admission and revoke allocations that have not reached any I/O
    /// attempt. Published allocations and incoming release processing remain.
    pub fn deactivate(&self) -> Reclaims {
        let mut reclaims = Reclaims::new(self.shared.clone());
        stop_locked(&mut self.shared.state.lock(), &mut reclaims);
        (self.shared.wake)();
        reclaims
    }

    /// Only a read-side peer-end fact permits clearing the remaining loans.
    /// Write errors, deadlines, DISCONNECT and local cancellation use deactivate.
    pub fn peer_end(&self) -> Reclaims {
        let mut reclaims = Reclaims::new(self.shared.clone());
        let mut state = self.shared.state.lock();
        stop_locked(&mut state, &mut reclaims);
        state.peer_ended = true;
        reclaims.published = std::mem::take(&mut state.published);
        state.settling += reclaims.published.len();
        state.releases.clear();
        drop(state);
        (self.shared.wake)();
        reclaims
    }

    pub fn is_deactivated(&self) -> bool {
        self.shared.state.lock().deactivated
    }

    /// Atomically seal incoming reads and new writes when B=L=0. Outstanding
    /// release records may be discarded: the peer will settle them on EOF.
    /// Queued controls and the unique writer reservation (including its flush)
    /// must finish before this seal can close the physical stream.
    pub fn try_close(&self) -> bool {
        let mut state = self.shared.state.lock();
        if state.deactivated
            && matches!(state.writer, WriterState::Idle | WriterState::Broken)
            && state.pending_controls == 0
            && state.remote_reads == 0
            && state.prepared.is_empty()
            && state.published.is_empty()
            && state.settling == 0
        {
            state.closed = true;
            state.releases.clear();
        }
        state.closed
    }
}

fn check_writable(state: &State) -> Result<(), ConnectionError> {
    if state.closed {
        Err(ConnectionError::Closed)
    } else if state.peer_ended {
        Err(ConnectionError::PeerEnded)
    } else if state.writer == WriterState::Broken {
        Err(ConnectionError::WriterBroken)
    } else {
        Ok(())
    }
}

/// One driver submission. Only business frames can carry prepared loans;
/// queued controls retain their original connection's close reservation.
pub enum WriteRequest {
    Business(Vec<PreparedLoan>),
    Control(ControlFrame),
    Releases,
}

/// An outstanding control frame, bound to its original physical connection.
/// Drop cancels a queued frame without allocating or waiting for network I/O.
pub struct ControlFrame {
    shared: Arc<Shared>,
}

impl Drop for ControlFrame {
    fn drop(&mut self) {
        self.shared.state.lock().pending_controls -= 1;
        (self.shared.wake)();
    }
}

fn check_live(state: &State) -> Result<(), ConnectionError> {
    if state.closed {
        Err(ConnectionError::Closed)
    } else if state.peer_ended {
        Err(ConnectionError::PeerEnded)
    } else if state.deactivated {
        Err(ConnectionError::Deactivated)
    } else {
        Ok(())
    }
}

fn check_business(state: &State) -> Result<(), ConnectionError> {
    check_live(state)?;
    if !state.handshake_done {
        return Err(ConnectionError::BeforeHandshake);
    }
    Ok(())
}

fn stop_locked(state: &mut State, reclaims: &mut Reclaims) {
    state.deactivated = true;
    reclaims.prepared = std::mem::take(&mut state.prepared);
    state.settling += reclaims.prepared.len();
}

/// Move-only preparation token. Its numeric ID is metadata, not release
/// authority. The real allocation remains in this token's connection book.
pub struct PreparedLoan {
    shared: Arc<Shared>,
    id: u64,
}

impl PreparedLoan {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for PreparedLoan {
    fn drop(&mut self) {
        let mut reclaims = Reclaims::new(self.shared.clone());
        let mut state = self.shared.state.lock();
        if let Some(allocation) = state.prepared.remove(&self.id) {
            reclaims.records[0] = Some(allocation);
            state.settling += 1;
        }
        drop(state);
        drop(reclaims);
    }
}

/// Origin-bound remote-read node. Finish it after copying (including a failed
/// copy), or simply drop an unread carrier. Neither path allocates or waits for
/// the network. The injected guard must obey the same Drop contract.
pub struct RemoteLoan {
    shared: Arc<Shared>,
    loan_id: u64,
    guard: Option<Box<dyn Send>>,
}

impl RemoteLoan {
    pub fn finish(self) {
        drop(self);
    }
}

impl Drop for RemoteLoan {
    fn drop(&mut self) {
        // Keep B positive while the actual read/mapping guard is being ended.
        drop(self.guard.take());
        let mut state = self.shared.state.lock();
        if !state.peer_ended && !state.closed {
            state.releases.push_back(self.loan_id);
        }
        state.remote_reads -= 1;
        drop(state);
        (self.shared.wake)();
    }
}

/// Control prefix plus writer progress, without any business bytes or budgets.
/// Drivers may co-submit `release_bytes()` and their one business frame. Call
/// begin_io before the first write attempt, then frame_completed for each whole
/// frame, including each release frame, then consume it with flush_completed.
/// From begin_io until flush_completed or write_failed this batch alone owns
/// the writer, including every co-submitted release/control/business frame.
/// A caller deadline only deactivates admission and records its reason: it
/// never cancels this batch or permits any later frame to interleave, even at
/// a completed prefix boundary. Drivers continue the same in-progress I/O.
/// write_failed permanently breaks the writer; no frame may be sent afterward.
/// Published loans then wait for received release records or peer end.
/// No partial/uncertain submission is replayed.
pub struct WriteBatch {
    shared: Arc<Shared>,
    loans: Vec<PreparedLoan>,
    prefix: Vec<u8>,
    release_count: usize,
    frames_remaining: usize,
    business: bool,
    control: Option<ControlFrame>,
    attempted: bool,
    finished: bool,
}

impl WriteBatch {
    pub fn release_bytes(&self) -> &[u8] {
        &self.prefix
    }
    pub fn has_business_frame(&self) -> bool {
        self.business
    }
    pub fn frame_count(&self) -> usize {
        self.frames_remaining
    }

    /// Publish every owner before any byte of this co-submission can be sent.
    /// A deactivation before this point cancels it; after this point the driver
    /// keeps writing this entire co-submission through flush or an actual I/O
    /// failure even after a deadline or a business waiter has gone away.
    pub fn begin_io(&mut self) -> Result<(), ConnectionError> {
        assert!(self.frames_remaining > 0, "write batch is already complete");
        assert!(!self.attempted, "write submission must not be replayed");
        let mut state = self.shared.state.lock();
        if state.closed {
            return Err(ConnectionError::Closed);
        }
        if state.peer_ended {
            return Err(ConnectionError::PeerEnded);
        }
        if self.business && state.deactivated {
            return Err(ConnectionError::Deactivated);
        }
        if state.writer == WriterState::Broken {
            return Err(ConnectionError::WriterBroken);
        }
        for loan in &self.loans {
            let mut allocation = state
                .prepared
                .remove(&loan.id)
                .expect("prepared token has its original owner");
            allocation.published = true;
            state.published.insert(loan.id, allocation);
        }
        self.attempted = true;
        state.writer = WriterState::InFrame;
        Ok(())
    }

    pub fn frame_completed(&mut self) {
        assert!(
            self.attempted && self.frames_remaining > 0,
            "unexpected frame completion"
        );
        self.frames_remaining -= 1;
        if self.frames_remaining == 0 {
            let mut state = self.shared.state.lock();
            if state.writer != WriterState::Broken {
                state.writer = WriterState::AwaitingFlush;
            }
            state.inflight_releases -= self.release_count;
            self.release_count = 0;
            drop(state);
            (self.shared.wake)();
        }
    }

    /// Consume the writer after the driver has successfully flushed this
    /// complete submission. Releasing it here prevents a stale flush outcome
    /// from racing a later batch. Dropping before this fact is uncertain I/O.
    pub fn flush_completed(mut self) {
        assert!(
            self.attempted && self.frames_remaining == 0,
            "flush before complete frames"
        );
        let mut state = self.shared.state.lock();
        assert!(
            state.writer == WriterState::AwaitingFlush,
            "flush success after a failed write"
        );
        state.writer = WriterState::Idle;
        self.finished = true;
        drop(state);
        (self.shared.wake)();
    }

    /// Includes a flush failure after all complete-frame facts were reported.
    pub fn write_failed(&mut self) {
        let mut reclaims = Reclaims::new(self.shared.clone());
        let mut state = self.shared.state.lock();
        state.writer = WriterState::Broken;
        stop_locked(&mut state, &mut reclaims);
        drop(state);
        drop(reclaims);
        // Failure ends this control submission immediately; retaining the
        // failed batch object must not retain a queued-frame close reservation.
        self.control.take();
        (self.shared.wake)();
    }
}

impl Drop for WriteBatch {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut reclaims = Reclaims::new(self.shared.clone());
        let mut state = self.shared.state.lock();
        if state.writer != WriterState::Broken {
            state.writer = WriterState::Idle;
        }
        state.inflight_releases -= self.release_count;
        if self.attempted {
            state.writer = WriterState::Broken;
            stop_locked(&mut state, &mut reclaims);
        } else if !state.closed && !state.peer_ended {
            // Prefix frames were never submitted. Restore FIFO order using
            // slots reserved even while records were held by this writer.
            for index in (0..self.release_count).rev() {
                let offset = index / IPC_RELEASE_BATCH_CAPACITY
                    * (HEADER_SIZE + 2 + IPC_RELEASE_BATCH_CAPACITY * 8)
                    + HEADER_SIZE
                    + 2
                    + index % IPC_RELEASE_BATCH_CAPACITY * 8;
                let id = u64::from_le_bytes(self.prefix[offset..offset + 8].try_into().unwrap());
                state.releases.push_front(id);
            }
        }
        drop(state);
        drop(reclaims);
        (self.shared.wake)();
        // PreparedLoan fields now cancel any still-unpublished owners, outside
        // the state lock. Published ones are left in the book.
    }
}

/// Owned local reclamation actions. They can wait for original pool locks
/// outside ConnectionCore; L remains positive until actual release completes.
/// Dropping the actions executes the same once-only local reclamation.
#[must_use]
pub struct Reclaims {
    shared: Arc<Shared>,
    prepared: HashMap<u64, Allocation>,
    published: HashMap<u64, Allocation>,
    records: [Option<Allocation>; IPC_RELEASE_BATCH_CAPACITY],
}

impl Reclaims {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            prepared: HashMap::new(),
            published: HashMap::new(),
            records: std::array::from_fn(|_| None),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.prepared.is_empty()
            && self.published.is_empty()
            && self.records.iter().all(Option::is_none)
    }
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for Reclaims {
    fn drop(&mut self) {
        let mut settled = 0;
        for (_, mut allocation) in self.prepared.drain().chain(self.published.drain()) {
            allocation.release();
            settled += 1;
        }
        for slot in &mut self.records {
            if let Some(mut allocation) = slot.take() {
                allocation.release();
                settled += 1;
            }
        }
        if settled > 0 {
            self.shared.state.lock().settling -= settled;
            (self.shared.wake)();
        }
    }
}

#[must_use]
pub struct ReleaseOutcome {
    pub reclaims: Reclaims,
    pub error: Option<ConnectionError>,
}

#[cfg(test)]
mod tests;
