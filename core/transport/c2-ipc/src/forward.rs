//! Pull-driven IPC response forwarding. The physical reader only admits
//! records; the consumer supplies a charged copy window before each read.

use super::*;
use c2_mem::retention::PayloadBufferReservation;
use tokio::sync::{Notify, oneshot};

#[derive(Debug, Clone)]
pub enum ForwardRecord {
    ItemBegin { total_bytes: u64 },
    CallEnd(stream::StreamTerminal),
}

struct PendingRead {
    bytes: Vec<u8>,
    reservation: PayloadBufferReservation,
    window: usize,
    tx: oneshot::Sender<ReadyRead>,
}

enum ReadyRead {
    Inline(BudgetedBuffer),
    Shm(PayloadBufferReservation, usize),
    Terminal,
}

// The consumer owns both the real read carrier and its charged copy window.
// Preparing this step never borrows the mapping; its synchronous copy runs
// after the call-state lock has been released.
struct ShmCopyStep {
    carrier: ServerShmResponse,
    offset: usize,
    reservation: PayloadBufferReservation,
    window: usize,
}

impl ShmCopyStep {
    fn copy(self) -> (ServerShmResponse, Result<BudgetedBuffer, IpcError>) {
        let Self {
            carrier,
            offset,
            mut reservation,
            window,
        } = self;
        let result = (|| {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(window).map_err(|e| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, e))
            })?;
            if bytes.capacity() as u64 > reservation.bytes() {
                return Err(IpcError::Config(
                    "response copy allocation exceeds reservation".into(),
                ));
            }
            bytes.resize(window, 0);
            carrier
                .copy_range_into(offset, &mut bytes)
                .map_err(IpcError::Shm)?;
            let charge = reservation
                .split_off(bytes.capacity() as u64)
                .expect("charged window");
            Ok(BudgetedBuffer::new(bytes, charge))
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

#[derive(Default)]
struct ForwardState {
    id: Option<u64>,
    begin: Option<u64>,
    total: Option<u64>,
    copied: u64,
    carrier: Option<ServerShmResponse>,
    copying: bool,
    deferred_failure: Option<IpcError>,
    pending: Option<PendingRead>,
    pending_storage: u64,
    pending_demand: u64,
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
                if let Some(total_bytes) = state.begin.take() {
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

    /// Copy one charged slice. `None` means a real terminal interrupted this
    /// item; `next_record` returns that CALL_END without losing its outcome.
    pub async fn read_chunk(
        &mut self,
        reservation: PayloadBufferReservation,
        max_bytes: usize,
    ) -> Result<Option<BudgetedBuffer>, crate::IpcCallError> {
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
            let window = remaining.min(reservation.bytes()).min(max_bytes as u64) as usize;
            if window == 0 {
                return Err(dispatch_uncertain_call_error(IpcError::Config(
                    "positive response copy window required".into(),
                )));
            }
            if state.carrier.is_some() {
                (
                    Some(
                        self.call
                            .prepare_copy_locked(&mut state, reservation, window)
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
                if bytes.capacity() as u64 > reservation.bytes() {
                    return Err(dispatch_uncertain_call_error(IpcError::Config(
                        "response copy allocation exceeds reservation".into(),
                    )));
                }
                state.pending = Some(PendingRead {
                    reservation,
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
            ReadyRead::Shm(reservation, window) => {
                let copy = {
                    let mut state = self.call.state.lock();
                    self.call
                        .prepare_copy_locked(&mut state, reservation, window)
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

impl ForwardReceive {
    fn new(conn: Arc<ClientConnection>, max_item_bytes: u64) -> (Arc<Self>, IpcForwardStream) {
        let call = Arc::new(Self {
            state: StdMutex::new(ForwardState {
                demand_outstanding: true,
                ..ForwardState::default()
            }),
            conn,
            changed: Notify::new(),
            upload_stop: StdMutex::new(None),
            max_item_bytes,
        });
        let stream = IpcForwardStream {
            call: call.clone(),
            opening: None,
            uploaded: false,
        };
        (call, stream)
    }

    pub(crate) fn publish(&self, id: u64) -> Result<(), IpcError> {
        self.conn.core.stream_open(id).map_err(core_error)?;
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
        let cause = state.cancel_wire.take();
        state.cancel_wire = state.next_cancel_wire.take();
        cause.map(|cause| rpc::encode_cancel_frame(id, cause).expect("published request ID"))
    }

    pub(crate) fn begin(&self, total: u64) -> Result<(), IpcError> {
        let mut state = self.state.lock();
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
        state.begin = (state.cancel != Some(rpc::CancelCause::Consumer)).then_some(total);
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
                    reservation,
                    bytes,
                    window,
                    tx,
                } = pending;
                // End the old allocation before waking a consumer that can
                // allocate a new copy under this same reservation.
                drop(bytes);
                if tx.send(ReadyRead::Shm(reservation, window)).is_err() {
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
                let mut pending = state.pending.take().expect("charged read");
                state.copied += pending.bytes.len() as u64;
                if complete {
                    self.conn.core.stream_finish_item(id).map_err(core_error)?;
                    state.total = None;
                }
                let charge = pending
                    .reservation
                    .split_off(pending.bytes.capacity() as u64)
                    .expect("charged window");
                let _ = pending.tx.send(ReadyRead::Inline(BudgetedBuffer::new(
                    pending.bytes,
                    charge,
                )));
            }
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn prepare_copy_locked(
        &self,
        state: &mut ForwardState,
        reservation: PayloadBufferReservation,
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
            reservation,
            window,
        })
    }

    fn copy_carrier(&self, step: ShmCopyStep) -> Result<BudgetedBuffer, IpcError> {
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
                        bytes,
                        reservation,
                        tx,
                        ..
                    } = pending;
                    drop(bytes);
                    drop(reservation);
                    state.interrupted_item = tx.send(ReadyRead::Terminal).is_err();
                } else {
                    state.interrupted_item = true;
                }
            }
        }
        state.pending.take();
        state.carrier.take();
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
        state.ended = true;
        if let Some(id) = state.id {
            self.conn.core.stream_retire(id);
        }
    }
}

impl IpcClient {
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
        send_reservation: PayloadBufferReservation,
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
            .begin_pending(PendingKind::Call(CallMode::ServerStream))
            .map_err(pre_dispatch_call_error)?;
        let (call, mut stream) = ForwardReceive::new(
            send.conn.clone(),
            limit.min(self.config.max_reassembly_bytes),
        );
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
        let metadata = rpc::encode_open_metadata(&identity, index)
            .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        let chunk_size = self
            .forward_chunk_size(data_size, send_reservation.bytes(), metadata.len())
            .map_err(pre_dispatch_call_error)?;
        let (opening_tx, opening_rx) = oneshot::channel();
        let driver = send.conn.clone();
        driver.spawn_send(async move {
            let conn = send.conn.clone();
            let state = send.state.clone();
            let result = Self::send_forward_frames(
                conn.clone(),
                state.clone(),
                metadata,
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
            let _ = opening_tx.send(opening);
            drop(send);
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
        send_reservation: PayloadBufferReservation,
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
        let request_block = self
            .forward_request_block(data_size)
            .map_err(pre_dispatch_call_error)?;
        let (send, rx) = self
            .begin_pending(PendingKind::Call(CallMode::Unary))
            .map_err(pre_dispatch_call_error)?;
        let metadata = rpc::encode_open_metadata(&identity, index)
            .map_err(|e| pre_dispatch_call_error(IpcError::Protocol(e.to_string())))?;
        let chunk_size = self
            .forward_chunk_size(data_size, send_reservation.bytes(), metadata.len())
            .map_err(pre_dispatch_call_error)?;
        let (upload_stop, stop_upload) = watch::channel(false);
        self.pending
            .lock()
            .unpublished_mut(&send.state)
            .expect("original unary draft")
            .upload_stop = Some(upload_stop);
        let (result_tx, result_rx) = oneshot::channel();
        let driver = send.conn.clone();
        driver.spawn_send(async move {
            let sent = Self::send_forward_frames(
                send.conn.clone(),
                send.state.clone(),
                metadata,
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
            let _ = result_tx.send(result);
            drop(send);
        });
        result_rx
            .await
            .map_err(|_| dispatch_uncertain_call_error(IpcError::Closed))?
    }

    fn forward_request_block(&self, data_size: usize) -> Result<Option<RequestBlock>, IpcError> {
        if choose_request_transport(&self.config, self.shm_request_available(), data_size)
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
        let frame_room = (u32::MAX as usize).saturating_sub(12 + metadata_len + 8);
        let size = (self.config.chunk_size as usize)
            .min(frame_room)
            .min(charged.min(usize::MAX as u64) as usize);
        if size == 0 {
            return Err(IpcError::Config(
                "positive request send reservation required".into(),
            ));
        }
        Ok(size)
    }

    async fn send_forward_frames<S, B, E>(
        conn: Arc<ClientConnection>,
        state: Arc<StdMutex<SendState>>,
        metadata: Vec<u8>,
        data_size: usize,
        chunk_size: usize,
        request_block: Option<RequestBlock>,
        chunks: S,
        mut stop_upload: watch::Receiver<bool>,
        send_reservation: PayloadBufferReservation,
    ) -> Result<(), IpcError>
    where
        S: Stream<Item = Result<B, E>> + Send,
        B: AsRef<[u8]> + Send,
        E: Display + Send,
    {
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
            let descriptor = encode_buddy_payload(&BuddyPayload {
                seg_idx: alloc.seg_idx as u16,
                generation: alloc.generation,
                offset: alloc.offset,
                data_size: data_size as u32,
                is_dedicated: alloc.is_dedicated,
                loan_id: loan.id(),
            });
            let frame = frame::encode_frame_parts(
                0,
                flags::FLAG_RPC_RECORD | flags::FLAG_BUDDY,
                &[&metadata, &descriptor],
            );
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
            let frame = frame::encode_frame_parts(0, flags::FLAG_RPC_RECORD, &[&metadata]);
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
            let mut frame = frame::encode_frame_parts(0, flags::FLAG_RPC_RECORD, &[&metadata]);
            frame.reserve_exact(data_size);
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
                if frame.len() - 16 + source.as_ref().len() > metadata.len() + data_size {
                    return Err(IpcError::Config(
                        "request stream exceeded declared content length".into(),
                    ));
                }
                frame.extend_from_slice(source.as_ref());
            }
            if frame.len() != 16 + metadata.len() + data_size {
                return Err(IpcError::Config(
                    "request stream ended before declared content length".into(),
                ));
            }
            Self::finish_forward_frame(&mut frame);
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
        let mut frame = Self::forward_frame_prefix(&metadata, data_size, part);
        frame.reserve_exact(chunk_size);
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
                    Self::finish_forward_frame(&mut frame);
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
                    frame = Self::forward_frame_prefix(&metadata, data_size, part);
                    frame.reserve_exact(chunk_size);
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
            Self::finish_forward_frame(&mut frame);
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

    fn forward_frame_prefix(metadata: &[u8], data_size: usize, part: usize) -> Vec<u8> {
        let first = part == 0;
        let flags = flags::FLAG_RPC_RECORD
            | flags::FLAG_CHUNKED
            | if first { flags::FLAG_FIRST_CHUNK } else { 0 };
        if first {
            frame::encode_frame_parts(0, flags, &[metadata, &(data_size as u64).to_le_bytes()])
        } else {
            frame::encode_frame_parts(0, flags, &[&[RpcRecordKind::Open as u8]])
        }
    }

    fn finish_forward_frame(frame: &mut [u8]) {
        let total = u32::try_from(frame.len() - 4).expect("frame bounded by IPC format");
        frame[..4].copy_from_slice(&total.to_le_bytes());
    }
}

#[cfg(test)]
#[path = "forward_copy_tests.rs"]
mod copy_tests;
