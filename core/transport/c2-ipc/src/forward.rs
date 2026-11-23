//! Pull-driven IPC response forwarding. The physical reader only admits
//! records; the consumer supplies a charged copy window before each read.

use super::*;
use c2_mem::{MemoryReservation, WindowChunk, WindowSlice};
use tokio::sync::{Notify, oneshot};

#[derive(Debug, Clone)]
pub enum ForwardRecord {
    ItemBegin { total_bytes: u64 },
    CallEnd(stream::StreamTerminal),
}

/// Records from a pull-driven topic subscription. Payload slices are read with
/// the same caller-owned window as a forwarded server stream.
#[derive(Debug, Clone)]
pub enum ForwardTopicRecord {
    ItemBegin { seq: u64, total_bytes: u64 },
    CallEnd(stream::StreamTerminal),
}

struct PendingRead {
    bytes: Vec<u8>,
    slice: WindowSlice,
    window: usize,
    tx: oneshot::Sender<ReadyRead>,
}

enum ReadyRead {
    Inline(WindowChunk),
    Shm(WindowSlice, usize),
    Terminal,
}

// The consumer owns both the real read carrier and its granted copy window.
// Preparing this step never borrows the mapping; its synchronous copy runs
// after the call-state lock has been released.
struct ShmCopyStep {
    carrier: ServerShmResponse,
    offset: usize,
    slice: WindowSlice,
    window: usize,
}

impl ShmCopyStep {
    fn copy(self) -> (ServerShmResponse, Result<WindowChunk, IpcError>) {
        let Self {
            carrier,
            offset,
            slice,
            window,
        } = self;
        let result = (|| {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(window).map_err(|e| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, e))
            })?;
            if bytes.capacity() as u64 > slice.bytes() {
                return Err(IpcError::Config(
                    "response copy allocation exceeds window slice".into(),
                ));
            }
            bytes.resize(window, 0);
            carrier
                .copy_range_into(offset, &mut bytes)
                .map_err(IpcError::Shm)?;
            Ok(slice.into_buffer(bytes))
        })();
        (carrier, result)
    }
}

fn cancelled_read() -> IpcError {
    IpcError::LocalCallRejected(C2Error::new(
        ErrorCode::CallCancelled,
        "forwarded read cancelled",
    ))
}

fn topic_opening_terminal_error(terminal: &stream::StreamTerminal) -> crate::IpcCallError {
    if terminal.status == CallStatus::Success {
        dispatch_uncertain_call_error(IpcError::Protocol(
            "subscription succeeded before READY".into(),
        ))
    } else {
        stream::terminal_failure(terminal)
    }
}

#[derive(Default)]
struct ForwardState {
    id: Option<u64>,
    begin: Option<(Option<u64>, u64)>,
    ready: Option<u64>,
    next_seq: Option<u64>,
    sequence_exhausted: bool,
    total: Option<u64>,
    copied: u64,
    carrier: Option<ServerShmResponse>,
    copying: bool,
    deferred_failure: Option<IpcError>,
    pending: Option<PendingRead>,
    pending_storage: u64,
    pending_demand: u64,
    sync_pending: Vec<oneshot::Sender<u64>>,
    sync_inflight: Vec<oneshot::Sender<u64>>,
    demand_outstanding: bool,
    cancel: Option<rpc::CancelCause>,
    cancel_wire: Option<rpc::CancelCause>,
    next_cancel_wire: Option<rpc::CancelCause>,
    terminal: Option<stream::StreamTerminal>,
    interrupted_item: bool,
    deferred_terminal: Option<(stream::StreamTerminal, PendingResponse)>,
    local_error: Option<IpcError>,
    io_error: Option<IpcError>,
    ended: bool,
}

pub(crate) struct ForwardReceive {
    state: StdMutex<ForwardState>,
    conn: Arc<ClientConnection>,
    changed: Notify,
    upload_stop: StdMutex<Option<watch::Sender<bool>>>,
    max_item_bytes: u64,
    subscription: bool,
}

#[derive(Clone)]
pub struct IpcForwardCancellation(Arc<ForwardReceive>);

impl IpcForwardCancellation {
    pub fn cancel(&self, cause: rpc::CancelCause) -> Result<(), IpcError> {
        self.0.cancel(cause)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn wait_deferred_terminal_for_test(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.state.lock().deferred_terminal.is_some() {
                return;
            }
            changed.await;
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn wait_queued_begin_for_test(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.state.lock().begin.is_some() {
                return;
            }
            changed.await;
        }
    }
}

pub struct IpcForwardStream {
    call: Arc<ForwardReceive>,
    opening: Option<oneshot::Receiver<Result<(), crate::IpcCallError>>>,
    uploaded: bool,
}

impl Drop for IpcForwardStream {
    fn drop(&mut self) {
        let _ = self.call.cancel(rpc::CancelCause::Consumer);
    }
}

impl IpcForwardStream {
    pub fn cancellation(&self) -> IpcForwardCancellation {
        IpcForwardCancellation(self.call.clone())
    }

    pub async fn wait_uploaded(&mut self) -> Result<(), crate::IpcCallError> {
        if self.uploaded {
            return Ok(());
        }
        let opening = self
            .opening
            .take()
            .expect("upload observation consumed once");
        opening
            .await
            .map_err(|_| dispatch_uncertain_call_error(IpcError::Closed))??;
        self.uploaded = true;
        Ok(())
    }

    pub async fn next_record(&mut self) -> Result<ForwardRecord, crate::IpcCallError> {
        loop {
            let notified = self.call.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.call.state.lock();
                if let Some((_, total_bytes)) = state.begin.take() {
                    return Ok(ForwardRecord::ItemBegin { total_bytes });
                }
                if let Some(error) = &state.local_error {
                    return Err(dispatch_uncertain_call_error(
                        stream::clone_observation_error(error),
                    ));
                }
                if let Some(terminal) = &state.terminal {
                    if state.total.is_none() || state.cancel == Some(rpc::CancelCause::Consumer) {
                        return Ok(ForwardRecord::CallEnd(terminal.clone()));
                    }
                }
                if state.ended {
                    return Err(dispatch_uncertain_call_error(
                        state
                            .io_error
                            .as_ref()
                            .map_or(IpcError::Closed, stream::clone_observation_error),
                    ));
                }
                if state.total.is_none() && !state.demand_outstanding && state.cancel.is_none() {
                    let id = state.id.expect("published forwarding stream");
                    self.call
                        .conn
                        .core
                        .stream_credit(id, 0, 1)
                        .map_err(|e| dispatch_uncertain_call_error(core_error(e)))?;
                    state.pending_demand = 1;
                    state.demand_outstanding = true;
                    self.call.conn.wake.notify_one();
                }
            }
            notified.await;
        }
    }

    /// Copy one granted slice. `None` means a real terminal interrupted this
    /// item; `next_record` returns that CALL_END without losing its outcome.
    ///
    /// A relay loops one [`c2_mem::ReusableWindow`] for a whole forwarded
    /// response: reserve a slice, read it, drop the returned chunk, then
    /// re-reserve the same capacity for the next slice:
    ///
    /// ```text
    /// let window = reservation.into_reusable_window();
    /// loop {
    ///     let slice = window.reserve(window.capacity()).await?;
    ///     match stream.read_chunk(slice, max_bytes).await? {
    ///         Some(chunk) => forward(chunk.as_ref()),
    ///         None => break,
    ///     }
    /// }
    /// ```
    pub async fn read_chunk(
        &mut self,
        slice: WindowSlice,
        max_bytes: usize,
    ) -> Result<Option<WindowChunk>, crate::IpcCallError> {
        let (copy, receiver) = {
            let mut state = self.call.state.lock();
            if state.cancel == Some(rpc::CancelCause::Consumer) {
                return Err(dispatch_uncertain_call_error(cancelled_read()));
            }
            if state.interrupted_item {
                state.interrupted_item = false;
                return Ok(None);
            }
            let total = state.total.ok_or_else(|| {
                dispatch_uncertain_call_error(IpcError::Protocol("no active forwarded item".into()))
            })?;
            let remaining = total - state.copied;
            let window = remaining.min(slice.bytes()).min(max_bytes as u64) as usize;
            if window == 0 {
                return Err(dispatch_uncertain_call_error(IpcError::Config(
                    "positive response copy window required".into(),
                )));
            }
            if state.carrier.is_some() {
                (
                    Some(
                        self.call
                            .prepare_copy_locked(&mut state, slice, window)
                            .map_err(dispatch_uncertain_call_error)?,
                    ),
                    None,
                )
            } else {
                if state.pending.is_some() || state.copying {
                    return Err(dispatch_uncertain_call_error(IpcError::Protocol(
                        "concurrent forwarded reads".into(),
                    )));
                }
                let (tx, rx) = oneshot::channel();
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(window).map_err(|e| {
                    dispatch_uncertain_call_error(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::OutOfMemory,
                        e,
                    )))
                })?;
                if bytes.capacity() as u64 > slice.bytes() {
                    return Err(dispatch_uncertain_call_error(IpcError::Config(
                        "response copy allocation exceeds window slice".into(),
                    )));
                }
                state.pending = Some(PendingRead {
                    slice,
                    bytes,
                    window,
                    tx,
                });
                let id = state.id.expect("published forwarding stream");
                if let Err(error) = self.call.conn.core.stream_credit(id, window as u64, 0) {
                    state.pending.take();
                    return Err(dispatch_uncertain_call_error(core_error(error)));
                }
                state.pending_storage += window as u64;
                self.call.conn.wake.notify_one();
                (None, Some(rx))
            }
        };
        if let Some(copy) = copy {
            return self
                .call
                .copy_carrier(copy)
                .map(Some)
                .map_err(dispatch_uncertain_call_error);
        }
        match receiver
            .expect("pending inline or SHM read")
            .await
            .map_err(|_| dispatch_uncertain_call_error(IpcError::Closed))?
        {
            ReadyRead::Inline(bytes) => Ok(Some(bytes)),
            ReadyRead::Shm(slice, window) => {
                let copy = {
                    let mut state = self.call.state.lock();
                    self.call
                        .prepare_copy_locked(&mut state, slice, window)
                        .map_err(dispatch_uncertain_call_error)?
                };
                self.call
                    .copy_carrier(copy)
                    .map(Some)
                    .map_err(dispatch_uncertain_call_error)
            }
            ReadyRead::Terminal => Ok(None),
        }
    }

    pub async fn completion(&self) -> Result<stream::StreamTerminal, crate::IpcCallError> {
        loop {
            let notified = self.call.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let observed = {
                // Contention is not a pending outcome: the last notification
                // may have been sent while this state lock was still held.
                let state = self.call.state.lock();
                if let Some(terminal) = &state.terminal {
                    Some(Ok(terminal.clone()))
                } else if state.ended {
                    Some(Err(dispatch_uncertain_call_error(
                        state
                            .io_error
                            .as_ref()
                            .map_or(IpcError::Closed, stream::clone_observation_error),
                    )))
                } else {
                    None
                }
            };
            if let Some(result) = observed {
                return result;
            }
            notified.await;
        }
    }
}

/// A relay-owned subscription whose payloads are copied through charged,
/// independently releasable windows instead of a whole-item allocation.
pub struct IpcForwardTopic {
    stream: IpcForwardStream,
    start_seq: u64,
}

/// Independent control handle so source confirmation can run while the
/// forwarding task is waiting for or copying a topic item.
#[derive(Clone)]
pub struct IpcTopicSynchronizer(Arc<ForwardReceive>);

impl IpcForwardTopic {
    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn cancellation(&self) -> IpcForwardCancellation {
        self.stream.cancellation()
    }

    pub fn synchronizer(&self) -> IpcTopicSynchronizer {
        IpcTopicSynchronizer(self.stream.call.clone())
    }

    /// Confirm the source's current head against this exact subscription.
    /// Waiters queued before a SYNC is written share its acknowledgement;
    /// later waiters require a new SYNC to preserve causal ordering.
    pub async fn sync_head_seq(&self) -> Result<u64, crate::IpcCallError> {
        self.synchronizer().sync_head_seq().await
    }

    pub async fn next_record(&mut self) -> Result<ForwardTopicRecord, crate::IpcCallError> {
        loop {
            let notified = self.stream.call.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.stream.call.state.lock();
                if let Some((Some(seq), total_bytes)) = state.begin.take() {
                    return Ok(ForwardTopicRecord::ItemBegin { seq, total_bytes });
                }
                if let Some(error) = &state.local_error {
                    return Err(dispatch_uncertain_call_error(
                        stream::clone_observation_error(error),
                    ));
                }
                if let Some(terminal) = &state.terminal {
                    if state.total.is_none() || state.cancel == Some(rpc::CancelCause::Consumer) {
                        return Ok(ForwardTopicRecord::CallEnd(terminal.clone()));
                    }
                }
                if state.ended {
                    return Err(dispatch_uncertain_call_error(
                        state
                            .io_error
                            .as_ref()
                            .map_or(IpcError::Closed, stream::clone_observation_error),
                    ));
                }
                if state.total.is_none() && !state.demand_outstanding && state.cancel.is_none() {
                    let id = state.id.expect("published forwarding subscription");
                    self.stream
                        .call
                        .conn
                        .core
                        .stream_credit(id, 0, 1)
                        .map_err(|e| dispatch_uncertain_call_error(core_error(e)))?;
                    state.pending_demand = 1;
                    state.demand_outstanding = true;
                    self.stream.call.conn.wake.notify_one();
                }
            }
            notified.await;
        }
    }

    pub async fn read_chunk(
        &mut self,
        slice: WindowSlice,
        max_bytes: usize,
    ) -> Result<Option<WindowChunk>, crate::IpcCallError> {
        self.stream.read_chunk(slice, max_bytes).await
    }

    pub async fn completion(&self) -> Result<stream::StreamTerminal, crate::IpcCallError> {
        self.stream.completion().await
    }
}

impl IpcTopicSynchronizer {
    pub async fn sync_head_seq(&self) -> Result<u64, crate::IpcCallError> {
        let (tx, rx) = oneshot::channel();
        let queued = {
            let mut state = self.0.state.lock();
            if state.ended || state.cancel.is_some() {
                false
            } else {
                state.sync_pending.push(tx);
                true
            }
        };
        if !queued {
            return self.sync_terminal_error().await;
        }
        self.0.conn.wake.notify_one();
        match rx.await {
            Ok(head_seq) => Ok(head_seq),
            Err(_) => self.sync_terminal_error().await,
        }
    }

    async fn sync_terminal_error(&self) -> Result<u64, crate::IpcCallError> {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let outcome = {
                let state = self.0.state.lock();
                if let Some(terminal) = &state.terminal {
                    Some(if terminal.status != CallStatus::Success {
                        stream::terminal_failure(terminal)
                    } else {
                        dispatch_uncertain_call_error(IpcError::Protocol(
                            "subscription ended without an error".into(),
                        ))
                    })
                } else if state.ended {
                    Some(dispatch_uncertain_call_error(
                        state
                            .io_error
                            .as_ref()
                            .map_or(IpcError::Closed, stream::clone_observation_error),
                    ))
                } else {
                    None
                }
            };
            if let Some(error) = outcome {
                return Err(error);
            }
            notified.await;
        }
    }
}

impl ForwardReceive {
    fn new(
        conn: Arc<ClientConnection>,
        max_item_bytes: u64,
        subscription: bool,
    ) -> (Arc<Self>, IpcForwardStream) {
        let call = Arc::new(Self {
            state: StdMutex::new(ForwardState {
                demand_outstanding: true,
                ..ForwardState::default()
            }),
            conn,
            changed: Notify::new(),
            upload_stop: StdMutex::new(None),
            max_item_bytes,
            subscription,
        });
        let stream = IpcForwardStream {
            call: call.clone(),
            opening: None,
            uploaded: false,
        };
        (call, stream)
    }

    pub(crate) fn publish(&self, id: u64) -> Result<(), IpcError> {
        if self.subscription {
            self.conn.core.subscribe_open(id).map_err(core_error)?;
        } else {
            self.conn.core.stream_open(id).map_err(core_error)?;
        }
        let mut state = self.state.lock();
        state.id = Some(id);
        if let Some(cause) = state.cancel {
            if cause == rpc::CancelCause::Lifecycle
                || self.conn.core.stream_cancel(id).map_err(core_error)?
            {
                state.cancel_wire = Some(cause);
            }
        }
        self.conn.wake.notify_one();
        Ok(())
    }

    fn cancel(&self, cause: rpc::CancelCause) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if state.ended {
            return Ok(());
        }
        let first = state.cancel.is_none();
        let escalate = state.cancel == Some(rpc::CancelCause::Lifecycle)
            && cause == rpc::CancelCause::Consumer;
        if state.cancel.is_none() || cause == rpc::CancelCause::Consumer {
            state.cancel = Some(cause);
        }
        state.sync_pending.clear();
        if cause == rpc::CancelCause::Consumer {
            state.begin.take();
            state.pending.take();
            state.carrier.take();
            state.pending_demand = 0;
            if !state.copying {
                if let Some((terminal, entry)) = state.deferred_terminal.take() {
                    self.finish_locked(&mut state, terminal, entry);
                    return Ok(());
                }
            }
        }
        if first || escalate {
            if let Some(id) = state.id {
                if cause == rpc::CancelCause::Lifecycle
                    || self.conn.core.stream_cancel(id).map_err(core_error)?
                    || escalate
                {
                    if escalate && state.cancel_wire.is_some() {
                        state.next_cancel_wire = Some(cause);
                    } else {
                        state.cancel_wire = Some(cause);
                    }
                }
            }
        }
        drop(state);
        if let Some(stop) = self.upload_stop.lock().as_ref() {
            stop.send_replace(true);
        }
        self.changed.notify_waiters();
        self.conn.wake.notify_one();
        Ok(())
    }

    pub(crate) fn has_control(&self) -> bool {
        let state = self.state.lock();
        !state.ended
            && state.id.is_some()
            && (state.pending_storage > 0
                || state.pending_demand > 0
                || (state.cancel.is_none()
                    && !state.sync_pending.is_empty()
                    && state.sync_inflight.is_empty())
                || state.cancel_wire.is_some())
    }

    pub(crate) fn take_control(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        if state.ended {
            return None;
        }
        let id = state.id?;
        let storage = std::mem::take(&mut state.pending_storage);
        let demand = std::mem::take(&mut state.pending_demand);
        if storage > 0 || demand > 0 {
            return Some(rpc::encode_credit_frame(id, storage, demand).expect("checked grant"));
        }
        if state.sync_inflight.is_empty()
            && !state.sync_pending.is_empty()
            && state.cancel.is_none()
        {
            state.sync_inflight = std::mem::take(&mut state.sync_pending);
            return Some(rpc::encode_sync_frame(id).expect("published request ID"));
        }
        let cause = state.cancel_wire.take();
        state.cancel_wire = state.next_cancel_wire.take();
        cause.map(|cause| rpc::encode_cancel_frame(id, cause).expect("published request ID"))
    }

    pub(crate) fn ready(&self, start_seq: u64) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if !self.subscription || start_seq == 0 || state.ready.is_some() {
            return Err(IpcError::Protocol("invalid subscription READY".into()));
        }
        state.ready = Some(start_seq);
        state.next_seq = Some(start_seq);
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn sync_ack(&self, head_seq: u64) -> Result<(), IpcError> {
        let waiters = {
            let mut state = self.state.lock();
            if !self.subscription || state.sync_inflight.is_empty() {
                return Err(IpcError::Protocol(
                    "unexpected subscription SYNC_ACK".into(),
                ));
            }
            if state.cancel.is_some() || state.local_error.is_some() {
                state.sync_inflight.clear();
                return Ok(());
            }
            std::mem::take(&mut state.sync_inflight)
        };
        for waiter in waiters {
            let _ = waiter.send(head_seq);
        }
        self.conn.wake.notify_one();
        Ok(())
    }

    pub(crate) fn begin(&self, seq: Option<u64>, total: u64) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if self.subscription {
            if state.sequence_exhausted {
                return Err(IpcError::Protocol("subscription sequence exhausted".into()));
            }
            let expected = state
                .next_seq
                .ok_or_else(|| IpcError::Protocol("subscription ITEM_BEGIN before READY".into()))?;
            let actual = seq
                .ok_or_else(|| IpcError::Protocol("subscription ITEM_BEGIN without seq".into()))?;
            if actual < expected {
                return Err(IpcError::Protocol("non-monotonic subscription seq".into()));
            }
            state.next_seq = Some(actual.saturating_add(1));
            state.sequence_exhausted = actual == u64::MAX;
        } else if seq.is_some() {
            return Err(IpcError::Protocol(
                "server stream ITEM_BEGIN with seq".into(),
            ));
        }
        let id = state.id.ok_or(IpcError::Closed)?;
        self.conn
            .core
            .stream_begin_item(id, total)
            .map_err(core_error)?;
        if total > self.max_item_bytes {
            return Err(IpcError::Config(format!(
                "stream item {total} exceeds route limit {}",
                self.max_item_bytes
            )));
        }
        state.begin = (state.cancel != Some(rpc::CancelCause::Consumer)).then_some((seq, total));
        state.demand_outstanding = false;
        state.copied = 0;
        state.total = (total > 0).then_some(total);
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn data(
        &self,
        storage: ValueStorage<'_>,
        pool: &Arc<RwLock<ServerPoolState>>,
        shm_allowed: bool,
        retirement: Arc<PhysicalRetirement>,
    ) -> Result<(), IpcError> {
        let carrier = if let ValueStorage::Shm(desc) = storage {
            Some(admit_response_storage(
                ValueStorage::Shm(desc),
                pool,
                shm_allowed,
                &self.conn,
                retirement,
            )?)
        } else {
            None
        };
        let mut state = self.state.lock();
        let id = state.id.ok_or(IpcError::Closed)?;
        if carrier.is_some() {
            self.conn
                .core
                .stream_shm_data(id, storage.len())
                .map_err(core_error)?;
            if state.cancel == Some(rpc::CancelCause::Consumer) {
                drop(carrier);
                return Ok(());
            }
            state.carrier = match carrier {
                Some(ResponseData::Shm(c)) => Some(c),
                _ => unreachable!(),
            };
            if let Some(pending) = state.pending.take() {
                let PendingRead {
                    slice,
                    bytes,
                    window,
                    tx,
                } = pending;
                // End the old allocation before waking a consumer that can
                // allocate a new copy under this same window slice.
                drop(bytes);
                if tx.send(ReadyRead::Shm(slice, window)).is_err() {
                    state.carrier.take();
                }
            }
        } else if let ValueStorage::Inline(bytes) = storage {
            self.conn
                .core
                .stream_data(id, bytes.len() as u64)
                .map_err(core_error)?;
            if state.cancel == Some(rpc::CancelCause::Consumer) {
                return Ok(());
            }
            let pending = state.pending.as_mut().ok_or_else(|| {
                IpcError::Protocol("inline ITEM_DATA without charged copy window".into())
            })?;
            if pending.bytes.len() + bytes.len() > pending.window {
                return Err(IpcError::Protocol(
                    "inline ITEM_DATA exceeds charged window".into(),
                ));
            }
            pending.bytes.extend_from_slice(bytes);
            let complete = self
                .conn
                .core
                .stream_state(id)
                .is_some_and(|flow| flow.admitted_bytes == flow.total_bytes.unwrap_or(0));
            if pending.bytes.len() == pending.window || complete {
                let pending = state.pending.take().expect("charged read");
                state.copied += pending.bytes.len() as u64;
                if complete {
                    self.conn.core.stream_finish_item(id).map_err(core_error)?;
                    state.total = None;
                }
                let _ = pending
                    .tx
                    .send(ReadyRead::Inline(pending.slice.into_buffer(pending.bytes)));
            }
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn prepare_copy_locked(
        &self,
        state: &mut ForwardState,
        slice: WindowSlice,
        window: usize,
    ) -> Result<ShmCopyStep, IpcError> {
        if state.cancel == Some(rpc::CancelCause::Consumer) {
            return Err(cancelled_read());
        }
        if state.copying {
            return Err(IpcError::Protocol("concurrent forwarded reads".into()));
        }
        let carrier = state.carrier.take().ok_or_else(|| {
            state
                .io_error
                .as_ref()
                .map_or(IpcError::Closed, stream::clone_observation_error)
        })?;
        state.copying = true;
        Ok(ShmCopyStep {
            carrier,
            offset: state.copied as usize,
            slice,
            window,
        })
    }

    fn copy_carrier(&self, step: ShmCopyStep) -> Result<WindowChunk, IpcError> {
        let window = step.window;
        // No state lock or await spans this bounded mapping borrow and copy.
        let (carrier, mut result) = step.copy();
        let mut state = self.state.lock();
        let discard = state.cancel == Some(rpc::CancelCause::Consumer)
            || state.deferred_failure.is_some()
            || result.is_err();
        if discard {
            drop(carrier);
            if result.is_ok() {
                result = Err(state
                    .deferred_failure
                    .as_ref()
                    .map_or_else(cancelled_read, stream::clone_observation_error));
            }
        } else {
            state.copied += window as u64;
            if state.total == Some(state.copied) {
                drop(carrier); // End the read and queue RELEASE before completion/output.
                if let Err(error) = self
                    .conn
                    .core
                    .stream_finish_item(state.id.expect("published stream"))
                {
                    result = Err(core_error(error));
                }
                state.total = None;
            } else {
                state.carrier = Some(carrier);
            }
        }
        state.copying = false;
        if state.carrier.is_none() {
            if let Some((terminal, entry)) = state.deferred_terminal.take() {
                self.finish_locked(&mut state, terminal, entry);
            } else if let Some(error) = state.deferred_failure.take() {
                self.transport_failed_locked(&mut state, error);
            }
        }
        let failure = result.as_ref().err().map(stream::clone_observation_error);
        drop(state);
        if let Some(error) = failure {
            self.fail_response(error);
        }
        self.changed.notify_waiters();
        result
    }

    pub(crate) fn finish(&self, terminal: stream::StreamTerminal, entry: PendingResponse) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        if state.copying
            || (state.carrier.is_some() && state.cancel != Some(rpc::CancelCause::Consumer))
        {
            state.deferred_terminal = Some((terminal, entry));
            self.changed.notify_waiters();
            return;
        }
        self.finish_locked(&mut state, terminal, entry);
    }

    fn finish_locked(
        &self,
        state: &mut ForwardState,
        terminal: stream::StreamTerminal,
        entry: PendingResponse,
    ) {
        let id = state.id.expect("published stream");
        let flow = self.conn.core.stream_state(id).expect("live stream flow");
        if state.cancel != Some(rpc::CancelCause::Consumer)
            && (terminal.complete_items != flow.complete_items
                || (matches!(
                    terminal.status,
                    CallStatus::Success | CallStatus::NotExecuted
                ) && flow.total_bytes.is_some()))
        {
            state.io_error = Some(IpcError::Protocol(
                "CALL_END complete item count mismatch".into(),
            ));
        } else {
            state.terminal = Some(terminal.clone());
            if state.total.is_some() {
                // A failed CALL_END can retire a partly encoded item. Wake its
                // charged reader without inventing a transport failure; the
                // caller observes the real terminal through next_record.
                state.total = None;
                state.copied = 0;
                if let Some(pending) = state.pending.take() {
                    let PendingRead {
                        bytes, slice, tx, ..
                    } = pending;
                    drop(bytes);
                    drop(slice);
                    state.interrupted_item = tx.send(ReadyRead::Terminal).is_err();
                } else {
                    state.interrupted_item = true;
                }
            }
        }
        state.pending.take();
        state.carrier.take();
        state.sync_pending.clear();
        state.sync_inflight.clear();
        state.ended = true;
        self.conn.core.stream_retire(id);
        entry.finish_stream_upload(match terminal.status {
            CallStatus::NotExecuted => Err(IpcError::CallRejected(
                terminal.error.expect("checked C2E1 refusal"),
            )),
            _ => Ok(ResponseData::Inline(Vec::new())),
        });
        self.changed.notify_waiters();
    }

    pub(crate) fn fail_response(&self, error: IpcError) {
        {
            let mut state = self.state.lock();
            if state.ended {
                return;
            }
            state.local_error.get_or_insert(error);
        }
        let _ = self.cancel(rpc::CancelCause::Consumer);
        self.changed.notify_waiters();
    }

    pub(crate) fn transport_failed(&self, error: IpcError) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        if state.copying {
            if state.deferred_terminal.is_none() {
                state.deferred_failure.get_or_insert(error);
            }
            return;
        }
        // An already received CALL_END remains authoritative while its SHM
        // item is still being consumed, even if the peer then closes.
        if state.deferred_terminal.is_some() {
            return;
        }
        self.transport_failed_locked(&mut state, error);
        drop(state);
        self.changed.notify_waiters();
    }

    fn transport_failed_locked(&self, state: &mut ForwardState, error: IpcError) {
        state.io_error = Some(error);
        state.pending.take();
        state.carrier.take();
        state.sync_pending.clear();
        state.sync_inflight.clear();
        state.ended = true;
        if let Some(id) = state.id {
            self.conn.core.stream_retire(id);
        }
    }
}

impl IpcClient {
    /// Open one topic subscription for relay forwarding. The returned handle
    /// observes READY before exposing any item and uses caller-supplied windows.
    pub async fn forward_subscribe_bound(
        &self,
        binding: &RouteBinding,
        topic_name: &str,
        options: rpc::WireSubscribeOptions,
    ) -> Result<IpcForwardTopic, crate::IpcCallError> {
        c2_contract::validate_contract_text_field("topic_name", topic_name)
            .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        let (identity, limit) = binding.subscribe_target();
        let (send, mut rx) = self
            .begin_pending(PendingKind::Call(CallMode::Subscribe), limit)
            .map_err(pre_dispatch_call_error)?;
        let (call, mut stream) = ForwardReceive::new(send.conn.clone(), limit, true);
        self.pending
            .lock()
            .unpublished_mut(&send.state)
            .expect("original forwarded subscription draft")
            .forward = Some(call.clone());
        let frame = rpc::encode_open_subscribe_draft_frame(&identity, topic_name, options)
            .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        if let Err(error) = send
            .conn
            .submit(
                frame,
                WriteRequest::Business(Vec::new()),
                send.state.clone(),
                None,
            )
            .await
        {
            return Err(match prefer_ready_response(error, &mut rx) {
                Ok(_) => dispatch_call_error(IpcError::Closed, send.started()),
                Err(source) => dispatch_call_error(source, send.started()),
            });
        }
        stream.uploaded = true;
        let start_seq = loop {
            let notified = call.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = call.state.lock();
                if let Some(start_seq) = state.ready {
                    break start_seq;
                }
                if let Some(error) = &state.local_error {
                    return Err(dispatch_uncertain_call_error(
                        stream::clone_observation_error(error),
                    ));
                }
                if let Some(terminal) = &state.terminal {
                    return Err(topic_opening_terminal_error(terminal));
                }
                if state.ended {
                    return Err(dispatch_uncertain_call_error(
                        state
                            .io_error
                            .as_ref()
                            .map_or(IpcError::Closed, stream::clone_observation_error),
                    ));
                }
            }
            notified.await;
        };
        Ok(IpcForwardTopic { stream, start_seq })
    }

    /// Begin one server stream while uploading a declared-size request through
    /// a bounded, caller-owned send allocation. Return after upload completion
    /// or a real early terminal; the detached driver retains the charge if
    /// the opening caller stops waiting.
    pub async fn forward_stream_bound_sized_phased<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data_len: u64,
        chunks: S,
        send_reservation: MemoryReservation,
    ) -> Result<IpcForwardStream, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let (index, identity, limit) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        if data_len > limit {
            return Err(pre_dispatch_call_error(IpcError::Config(
                "request exceeds route payload limit".into(),
            )));
        }
        let data_size = checked_payload_len_usize(data_len).map_err(pre_dispatch_call_error)?;
        let request_block = self
            .forward_request_block(data_size)
            .map_err(pre_dispatch_call_error)?;
        let (send, mut rx) = self
            .begin_pending(PendingKind::Call(CallMode::ServerStream), limit)
            .map_err(pre_dispatch_call_error)?;
        let (call, mut stream) = ForwardReceive::new(send.conn.clone(), limit, false);
        let (upload_stop, stop_upload) = watch::channel(false);
        *call.upload_stop.lock() = Some(upload_stop.clone());
        {
            let mut pending = self.pending.lock();
            let entry = pending
                .unpublished_mut(&send.state)
                .expect("original forwarded draft");
            entry.upload_stop = Some(upload_stop);
            entry.forward = Some(call.clone());
        }
        let metadata_len = rpc::encoded_open_metadata_len(&identity)
            .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        let chunk_size = self
            .forward_chunk_size(data_size, send_reservation.bytes(), metadata_len)
            .map_err(pre_dispatch_call_error)?;
        let (opening_tx, opening_rx) = oneshot::channel();
        let driver = send.conn.clone();
        driver.spawn_send(async move {
            let conn = send.conn.clone();
            let state = send.state.clone();
            let result = Self::send_forward_frames(
                conn.clone(),
                state.clone(),
                (identity, index, CallMode::ServerStream, None),
                data_size,
                chunk_size,
                request_block,
                chunks,
                stop_upload,
                send_reservation,
            )
            .await;
            let started = send.started();
            let opening = match result {
                Err(error) => {
                    if started {
                        let _ = call.cancel(rpc::CancelCause::Consumer);
                    } else {
                        call.transport_failed(stream::clone_observation_error(&error));
                    }
                    prefer_ready_response(error, &mut rx)
                        .map(|_| ())
                        .map_err(|error| dispatch_call_error(error, started))
                }
                Ok(()) => match rx.try_recv() {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => Err(classified_call_error(error)),
                    Err(
                        oneshot::error::TryRecvError::Empty | oneshot::error::TryRecvError::Closed,
                    ) => {
                        if call.state.lock().cancel.is_some() {
                            Err(dispatch_call_error(
                                IpcError::LocalCallRejected(C2Error::new(
                                    ErrorCode::CallCancelled,
                                    "forwarding upload cancelled",
                                )),
                                started,
                            ))
                        } else {
                            Ok(())
                        }
                    }
                },
            };
            // Release the pending record before reporting: a caller that
            // observes the opening outcome must also observe the guard's
            // cleanup of the unpublished draft.
            drop(send);
            let _ = opening_tx.send(opening);
        });
        stream.opening = Some(opening_rx);
        Ok(stream)
    }

    /// Unary forwarding uses the same bounded upload path and returns the
    /// original response carrier for cursor-based download.
    pub async fn forward_unary_bound_sized_phased<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data_len: u64,
        chunks: S,
        send_reservation: MemoryReservation,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        self.forward_keyed_unary_bound_sized_phased(
            binding,
            method_name,
            None,
            data_len,
            chunks,
            send_reservation,
        )
        .await
    }

    /// Keyed unary forwarding (ADR 0006): the first frame carries the
    /// length-prefixed `call_id`, and the chunk width accounts for it
    /// exactly through `encoded_keyed_open_metadata_len`.
    pub async fn forward_keyed_unary_bound_sized_phased<S, B, E>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        call_id: Option<&str>,
        data_len: u64,
        chunks: S,
        send_reservation: MemoryReservation,
    ) -> Result<ResponseData, crate::IpcCallError>
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: AsRef<[u8]> + Send + 'static,
        E: Display + Send + 'static,
    {
        let (index, identity, limit) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        if data_len > limit {
            return Err(pre_dispatch_call_error(IpcError::Config(
                "request exceeds route payload limit".into(),
            )));
        }
        let data_size = checked_payload_len_usize(data_len).map_err(pre_dispatch_call_error)?;
        let metadata_len = match call_id {
            Some(call_id) => rpc::encoded_keyed_open_metadata_len(&identity, call_id),
            None => rpc::encoded_open_metadata_len(&identity),
        }
        .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        let chunk_size = self
            .forward_chunk_size(data_size, send_reservation.bytes(), metadata_len)
            .map_err(pre_dispatch_call_error)?;
        let request_block = self
            .forward_request_block(data_size)
            .map_err(pre_dispatch_call_error)?;
        let pending_kind = match call_id {
            Some(_) => PendingKind::Call(CallMode::KeyedUnary),
            None => PendingKind::Call(CallMode::Unary),
        };
        let (send, rx) = self
            .begin_pending(pending_kind, limit)
            .map_err(pre_dispatch_call_error)?;
        let call_id = call_id.map(str::to_string);
        let (upload_stop, stop_upload) = watch::channel(false);
        self.pending
            .lock()
            .unpublished_mut(&send.state)
            .ok_or_else(|| pre_dispatch_call_error(IpcError::Closed))?
            .upload_stop = Some(upload_stop);
        let (result_tx, result_rx) = oneshot::channel();
        let driver = send.conn.clone();
        driver.spawn_send(async move {
            let sent = Self::send_forward_frames(
                send.conn.clone(),
                send.state.clone(),
                (identity, index, CallMode::Unary, call_id),
                data_size,
                chunk_size,
                request_block,
                chunks,
                stop_upload,
                send_reservation,
            )
            .await;
            let result = if let Err(error) = sent {
                Err(Self::chunk_source_failure(&send.conn, &send.state, error))
            } else {
                rx.await
                    .map_err(|_| dispatch_uncertain_call_error(IpcError::Closed))
                    .and_then(|response| response.map_err(classified_call_error))
            };
            // Release the pending record before reporting, mirroring the
            // phased stream driver: the caller must observe the guard's
            // cleanup together with the outcome it reports.
            drop(send);
            let _ = result_tx.send(result);
        });
        result_rx
            .await
            .map_err(|_| dispatch_uncertain_call_error(IpcError::Closed))?
    }

    fn forward_request_block(&self, data_size: usize) -> Result<Option<RequestBlock>, IpcError> {
        if choose_request_transport(self.shm_request_available(), data_size)
            == RequestTransportKind::Buddy
        {
            self.try_alloc_request_block(data_size)
        } else {
            Ok(None)
        }
    }

    fn forward_chunk_size(
        &self,
        data_size: usize,
        charged: u64,
        metadata_len: usize,
    ) -> Result<usize, IpcError> {
        if data_size == 0 {
            return Ok(0);
        }
        let frame_room = (MAX_RPC_FRAME_BYTES as usize).saturating_sub(12 + metadata_len + 8);
        let size = (c2_config::CHUNK_SIZE as usize)
            .min(frame_room)
            .min(charged.min(usize::MAX as u64) as usize);
        if size == 0 {
            return Err(IpcError::Config(
                "positive request send reservation required".into(),
            ));
        }
        Ok(size)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_forward_frames<S, B, E>(
        conn: Arc<ClientConnection>,
        state: Arc<StdMutex<SendState>>,
        open: (RouteCallIdentity, u16, CallMode, Option<String>),
        data_size: usize,
        chunk_size: usize,
        request_block: Option<RequestBlock>,
        chunks: S,
        mut stop_upload: watch::Receiver<bool>,
        send_reservation: MemoryReservation,
    ) -> Result<(), IpcError>
    where
        S: Stream<Item = Result<B, E>> + Send,
        B: AsRef<[u8]> + Send,
        E: Display + Send,
    {
        let (identity, method_idx, mode, call_id) = open;
        // Keyed opens carry the length-prefixed call_id after the method
        // index; the exact width keeps chunk frames at their true maximum.
        let metadata_len = match call_id.as_deref() {
            Some(call_id) => rpc::encoded_keyed_open_metadata_len(&identity, call_id),
            None => rpc::encoded_open_metadata_len(&identity),
        }
        .map_err(|error| IpcError::Protocol(error.to_string()))?;
        let send_reservation = Arc::new(send_reservation);
        if let Some(block) = request_block {
            // This allocation belongs to the original request pool. Fill it
            // directly while the source owns each charged input chunk; no
            // whole-input scratch buffer or loan exists before source EOF.
            let mut written = 0usize;
            futures_util::pin_mut!(chunks);
            loop {
                if *stop_upload.borrow() {
                    return Ok(());
                }
                let next = tokio::select! {
                    changed = stop_upload.changed() => {
                        if changed.is_err() || *stop_upload.borrow() { return Ok(()); }
                        continue;
                    }
                    next = chunks.next() => next,
                };
                let Some(next) = next else { break };
                if *stop_upload.borrow() {
                    return Ok(());
                }
                let source = next.map_err(stream_error)?;
                let data = source.as_ref();
                let next_written = written
                    .checked_add(data.len())
                    .filter(|n| *n <= data_size)
                    .ok_or_else(|| {
                        IpcError::Config("request stream exceeded declared content length".into())
                    })?;
                block.write_at(written, data)?;
                written = next_written;
            }
            if *stop_upload.borrow() {
                return Ok(());
            }
            if written != data_size {
                return Err(IpcError::Config(format!(
                    "request stream ended at {written} bytes, expected {data_size}"
                )));
            }
            let loan = block.prepare(&conn)?;
            let alloc = block.allocation();
            let descriptor = BuddyPayload {
                seg_idx: alloc.seg_idx as u16,
                generation: alloc.generation,
                offset: alloc.offset,
                data_size: data_size as u32,
                is_dedicated: alloc.is_dedicated,
                loan_id: loan.id(),
            };
            let frame = match call_id.as_deref() {
                Some(call_id) => rpc::encode_open_keyed_draft_frame(
                    &identity,
                    method_idx,
                    call_id,
                    ValuePart::whole(ValueStorage::Shm(descriptor)),
                ),
                None => rpc::encode_open_draft_frame(
                    &identity,
                    method_idx,
                    mode,
                    ValuePart::whole(ValueStorage::Shm(descriptor)),
                ),
            }
            .map_err(|error| IpcError::Protocol(error.to_string()))?;
            return conn
                .submit_owned(
                    frame,
                    WriteRequest::Business(vec![loan]),
                    state,
                    None,
                    send_reservation,
                )
                .await;
        }
        if data_size == 0 {
            let frame = match call_id.as_deref() {
                Some(call_id) => rpc::encode_open_keyed_draft_frame(
                    &identity,
                    method_idx,
                    call_id,
                    ValuePart::whole(ValueStorage::Inline(&[])),
                ),
                None => rpc::encode_open_draft_frame(
                    &identity,
                    method_idx,
                    mode,
                    ValuePart::whole(ValueStorage::Inline(&[])),
                ),
            }
            .map_err(|error| IpcError::Protocol(error.to_string()))?;
            conn.submit_owned(
                frame,
                WriteRequest::Business(Vec::new()),
                state,
                None,
                send_reservation,
            )
            .await?;
            return Ok(());
        }
        if data_size <= chunk_size {
            let mut frame = match call_id.as_deref() {
                Some(call_id) => rpc::encode_open_keyed_inline_draft_prefix(
                    &identity,
                    method_idx,
                    call_id,
                    ChunkPosition::Whole,
                    data_size,
                ),
                None => rpc::encode_open_inline_draft_prefix(
                    &identity,
                    method_idx,
                    mode,
                    ChunkPosition::Whole,
                    data_size,
                ),
            }
            .map_err(|error| IpcError::Protocol(error.to_string()))?;
            let frame_len = frame::HEADER_SIZE + metadata_len + data_size;
            futures_util::pin_mut!(chunks);
            loop {
                let next = loop {
                    if *stop_upload.borrow() {
                        return Ok(());
                    }
                    tokio::select! {
                        changed = stop_upload.changed() => {
                            if changed.is_err() || *stop_upload.borrow() { return Ok(()); }
                        }
                        next = chunks.next() => break next,
                    }
                };
                let Some(next) = next else { break };
                if *stop_upload.borrow() {
                    return Ok(());
                }
                let source = next.map_err(stream_error)?;
                if frame.len() + source.as_ref().len() > frame_len {
                    return Err(IpcError::Config(
                        "request stream exceeded declared content length".into(),
                    ));
                }
                frame.extend_from_slice(source.as_ref());
            }
            if frame.len() != frame_len {
                return Err(IpcError::Config(
                    "request stream ended before declared content length".into(),
                ));
            }
            return conn
                .submit_owned(
                    frame,
                    WriteRequest::Business(Vec::new()),
                    state,
                    None,
                    send_reservation,
                )
                .await;
        }
        let mut written = 0usize;
        let mut part = 0usize;
        let mut frame = Self::forward_frame_prefix(
            (&identity, method_idx, mode, call_id.as_deref()),
            data_size,
            part,
            chunk_size,
        )?;
        let mut filled = 0usize;
        futures_util::pin_mut!(chunks);
        loop {
            let next = loop {
                if *stop_upload.borrow() {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop_upload.changed() => {
                        if changed.is_err() || *stop_upload.borrow() { return Ok(()); }
                    }
                    next = chunks.next() => break next,
                }
            };
            let Some(next) = next else { break };
            if *stop_upload.borrow() {
                return Ok(());
            }
            let source = next.map_err(stream_error)?;
            let mut data = source.as_ref();
            written = written
                .checked_add(data.len())
                .filter(|n| *n <= data_size)
                .ok_or_else(|| {
                    IpcError::Config("request stream exceeded declared content length".into())
                })?;
            while !data.is_empty() {
                let take = (chunk_size - filled).min(data.len());
                frame.extend_from_slice(&data[..take]);
                data = &data[take..];
                filled += take;
                if filled == chunk_size {
                    let request = if part == 0 {
                        WriteRequest::Business(Vec::new())
                    } else {
                        WriteRequest::CallContinuation(
                            conn.core.prepare_call_record().map_err(core_error)?,
                        )
                    };
                    conn.submit_owned(
                        frame,
                        request,
                        state.clone(),
                        None,
                        send_reservation.clone(),
                    )
                    .await?;
                    part += 1;
                    filled = 0;
                    frame = if part * chunk_size < data_size {
                        Self::forward_frame_prefix(
                            (&identity, method_idx, mode, call_id.as_deref()),
                            data_size,
                            part,
                            chunk_size,
                        )?
                    } else {
                        Vec::new()
                    };
                }
            }
        }
        if *stop_upload.borrow() {
            return Ok(());
        }
        if written != data_size {
            return Err(IpcError::Config(format!(
                "request stream ended at {written} bytes, expected {data_size}"
            )));
        }
        if filled > 0 {
            let request = if part == 0 {
                WriteRequest::Business(Vec::new())
            } else {
                WriteRequest::CallContinuation(conn.core.prepare_call_record().map_err(core_error)?)
            };
            conn.submit_owned(frame, request, state, None, send_reservation.clone())
                .await?;
        }
        Ok(())
    }

    fn forward_frame_prefix(
        open: (&RouteCallIdentity, u16, CallMode, Option<&str>),
        data_size: usize,
        part: usize,
        chunk_size: usize,
    ) -> Result<Vec<u8>, IpcError> {
        let value_bytes = chunk_size.min(data_size - part * chunk_size);
        if part == 0 {
            match open.3 {
                Some(call_id) => rpc::encode_open_keyed_inline_draft_prefix(
                    open.0,
                    open.1,
                    call_id,
                    ChunkPosition::First {
                        total_bytes: data_size as u64,
                    },
                    value_bytes,
                ),
                None => rpc::encode_open_inline_draft_prefix(
                    open.0,
                    open.1,
                    open.2,
                    ChunkPosition::First {
                        total_bytes: data_size as u64,
                    },
                    value_bytes,
                ),
            }
        } else {
            rpc::encode_open_continuation_draft_prefix(value_bytes)
        }
        .map_err(|error| IpcError::Protocol(error.to_string()))
    }
}

#[cfg(test)]
#[path = "forward_copy_tests.rs"]
mod copy_tests;
