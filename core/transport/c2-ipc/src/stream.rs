//! One complete-value receiver per original server-stream call.
//!
//! The socket reader never waits for the payload budget or the local consumer.
//! A complete item enters the capacity-one consumer interface before granting
//! the next demand. A second item can then be receiving or awaiting delivery.

use super::*;
use c2_mem::retention::PayloadBufferReservation;
use tokio::sync::{Notify, mpsc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTerminal {
    pub status: CallStatus,
    pub complete_items: u64,
    pub error: Option<C2Error>,
}

struct ItemBuffer {
    bytes: Vec<u8>,
    reservation: PayloadBufferReservation,
}

#[derive(Default)]
struct ReceiveState {
    id: Option<u64>,
    item: Option<ItemBuffer>,
    discarded: bool,
    ended: bool,
    terminal: Option<StreamTerminal>,
    local_error: Option<C2Error>,
    io_error: Option<IpcError>,
    pending_storage: u64,
    pending_demand: u64,
    pending_cancel: bool,
    deliveries: usize,
}

pub(crate) struct StreamReceive {
    state: StdMutex<ReceiveState>,
    budget: PayloadBufferBudget,
    conn: Arc<ClientConnection>,
    output: mpsc::Sender<BudgetedBuffer>,
    changed: Notify,
    max_item_bytes: u64,
}

struct DeliveryOwner(Arc<StreamReceive>);
impl Drop for DeliveryOwner {
    fn drop(&mut self) {
        self.0.state.lock().deliveries -= 1;
        self.0.changed.notify_waiters();
    }
}

/// Transport receive interface. Cancellation requests intent; completion
/// observes the original CALL_END. Language-level handle cleanup belongs to SDKs.
pub struct IpcResponseStream {
    call: Arc<StreamReceive>,
    input: mpsc::Receiver<BudgetedBuffer>,
    error_delivered: bool,
}

impl IpcResponseStream {
    pub async fn recv(&mut self) -> Result<Option<BudgetedBuffer>, crate::IpcCallError> {
        loop {
            let changed = self.call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Ok(item) = self.input.try_recv() {
                return Ok(Some(item));
            }
            if self.error_delivered {
                return Ok(None);
            }
            let result = receive_after_empty(&mut self.call.state.lock(), &mut self.input);
            if let Some(result) = result {
                self.error_delivered = result.is_err();
                return result;
            }
            tokio::select! {
                biased;
                item = self.input.recv() => {
                    if let Some(item) = item { return Ok(Some(item)); }
                }
                _ = &mut changed => {}
            }
        }
    }

    pub fn cancel(&self) -> Result<(), IpcError> {
        self.call.cancel()
    }

    pub async fn completion(&self) -> Result<StreamTerminal, crate::IpcCallError> {
        loop {
            let changed = self.call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.call.state.lock();
                if state.ended {
                    if let Some(error) = &state.local_error {
                        return Err(local_failure(error.clone()));
                    }
                    if let Some(terminal) = &state.terminal {
                        return Ok(terminal.clone());
                    }
                    return Err(dispatch_uncertain_call_error(
                        state.io_error.take().unwrap_or(IpcError::Closed),
                    ));
                }
            }
            changed.await;
        }
    }
}

// This decision runs after recv's first empty queue observation. Keep the
// terminal state and the second queue observation in one receive boundary.
fn receive_after_empty(
    state: &mut ReceiveState,
    input: &mut mpsc::Receiver<BudgetedBuffer>,
) -> Option<Result<Option<BudgetedBuffer>, crate::IpcCallError>> {
    // The delivery task enqueues while holding this same state lock. An item
    // queued after recv's first empty read therefore wins over either error.
    if let Ok(item) = input.try_recv() {
        return Some(Ok(Some(item)));
    }
    if let Some(error) = &state.local_error {
        Some(Err(local_failure(error.clone())))
    } else if state.ended && state.deliveries == 0 {
        match &state.terminal {
            Some(terminal) if terminal.status == CallStatus::Success => Some(Ok(None)),
            Some(terminal) => Some(Err(terminal_failure(terminal))),
            None => Some(Err(dispatch_uncertain_call_error(
                state.io_error.take().unwrap_or(IpcError::Closed),
            ))),
        }
    } else {
        None
    }
}

fn terminal_failure(terminal: &StreamTerminal) -> crate::IpcCallError {
    let error = terminal.error.as_ref().expect("checked error CALL_END");
    if terminal.status == CallStatus::NotExecuted {
        pre_dispatch_call_error(IpcError::CallRejected(error.clone()))
    } else {
        dispatch_uncertain_call_error(IpcError::CrmError(error.to_wire_bytes()))
    }
}

fn local_failure(error: C2Error) -> crate::IpcCallError {
    dispatch_uncertain_call_error(IpcError::ResponseFailure(Box::new(
        IpcError::LocalCallRejected(error),
    )))
}

impl StreamReceive {
    fn new(
        conn: Arc<ClientConnection>,
        budget: PayloadBufferBudget,
        max_item_bytes: u64,
    ) -> (Arc<Self>, IpcResponseStream) {
        let (output, input) = mpsc::channel(1);
        let call = Arc::new(Self {
            state: StdMutex::new(ReceiveState::default()),
            budget,
            conn,
            output,
            changed: Notify::new(),
            max_item_bytes,
        });
        let stream = IpcResponseStream {
            call: call.clone(),
            input,
            error_delivered: false,
        };
        (call, stream)
    }

    pub(crate) fn publish(&self, id: u64) -> Result<(), IpcError> {
        self.conn.core.stream_open(id).map_err(core_error)?;
        self.state.lock().id = Some(id);
        Ok(())
    }

    pub(crate) fn take_control(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        if state.ended {
            return None;
        }
        let id = state.id?;
        if std::mem::take(&mut state.pending_cancel) {
            return Some(rpc::encode_cancel_frame(id).expect("nonzero stream ID"));
        }
        let storage = std::mem::take(&mut state.pending_storage);
        let demand = std::mem::take(&mut state.pending_demand);
        if storage == 0 && demand == 0 {
            return None;
        }
        Some(rpc::encode_credit_frame(id, storage, demand).expect("nonzero checked grant"))
    }

    pub(crate) fn has_control(&self) -> bool {
        let state = self.state.lock();
        !state.ended
            && state.id.is_some()
            && (state.pending_cancel || state.pending_storage > 0 || state.pending_demand > 0)
    }

    fn cancel(&self) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if state.ended {
            return Ok(());
        }
        let id = state.id.ok_or(IpcError::Closed)?;
        if self.conn.core.stream_cancel(id).map_err(core_error)? {
            state.discarded = true;
            state.item.take();
            state.pending_storage = 0;
            state.pending_demand = 0;
            state.pending_cancel = true;
        }
        drop(state);
        self.changed.notify_waiters();
        self.conn.wake.notify_waiters();
        self.conn.wake.notify_one();
        Ok(())
    }

    pub(crate) fn fail_local(&self, error: C2Error) {
        {
            let mut state = self.state.lock();
            if state.ended {
                return;
            }
            state.local_error.get_or_insert(error);
        }
        let _ = self.cancel();
    }

    pub(crate) fn fail_response(&self, error: IpcError) {
        let code = if matches!(&error, IpcError::Chunk(ChunkError::Capacity(_)))
            || matches!(&error, IpcError::Io(source) if source.kind() == std::io::ErrorKind::OutOfMemory)
        {
            c2_error::ErrorCode::CallCapacityExceeded
        } else {
            c2_error::ErrorCode::ProtocolViolation
        };
        self.fail_local(C2Error::new(code, error.to_string()));
    }

    pub(crate) fn begin(self: &Arc<Self>, total: u64) -> Result<(), IpcError> {
        let id = self.state.lock().id.ok_or(IpcError::Closed)?;
        self.conn
            .core
            .stream_begin_item(id, total)
            .map_err(core_error)?;
        if total > self.max_item_bytes
            || usize::try_from(total).map_or(true, |n| n > isize::MAX as usize)
        {
            self.fail_local(C2Error::new(
                c2_error::ErrorCode::CallCapacityExceeded,
                format!(
                    "stream item length {total} exceeds payload or address-space limit {}",
                    self.max_item_bytes
                ),
            ));
            return Ok(());
        }
        if total == 0 {
            let reservation = self.budget.try_reserve(0).expect("zero-byte reservation");
            self.start_delivery(BudgetedBuffer::new(Vec::new(), reservation));
            return Ok(());
        }
        let call = self.clone();
        tokio::spawn(async move {
            // One reservation future belongs to this item. No copied payload
            // or blocking callback worker is retained during budget waiting.
            let reservation = loop {
                let changed = call.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if {
                    let s = call.state.lock();
                    s.ended || s.discarded
                } {
                    return;
                }
                tokio::select! {
                    reservation = call.budget.reserve(total) => break reservation,
                    _ = &mut changed => {}
                }
            };
            let reservation = match reservation {
                Ok(reservation) => reservation,
                Err(error) => {
                    call.fail_local(
                        C2Error::new(c2_error::ErrorCode::CallCapacityExceeded, error.to_string())
                            .with_details(std::collections::BTreeMap::from([(
                                "stage".into(),
                                "payload_buffer_admission".into(),
                            )])),
                    );
                    return;
                }
            };
            let size = match usize::try_from(total) {
                Ok(size) => size,
                Err(_) => {
                    call.fail_local(C2Error::new(
                        c2_error::ErrorCode::CallCapacityExceeded,
                        "item is not addressable",
                    ));
                    return;
                }
            };
            let mut bytes = Vec::new();
            if let Err(error) = bytes.try_reserve_exact(size) {
                call.fail_local(C2Error::new(
                    c2_error::ErrorCode::CallCapacityExceeded,
                    error.to_string(),
                ));
                return;
            }
            bytes.resize(size, 0);
            let mut state = call.state.lock();
            if state.ended || state.discarded {
                return;
            }
            state.item = Some(ItemBuffer { bytes, reservation });
            if let Err(error) = call.conn.core.stream_credit(id, total, 0) {
                drop(state);
                call.fail_local(C2Error::new(
                    c2_error::ErrorCode::ProtocolViolation,
                    error.to_string(),
                ));
                return;
            }
            state.pending_storage = total;
            drop(state);
            call.conn.wake.notify_one();
        });
        Ok(())
    }

    pub(crate) fn data(
        self: &Arc<Self>,
        storage: ValueStorage<'_>,
        pool: &Arc<RwLock<ServerPoolState>>,
        shm_allowed: bool,
        retirement: Arc<PhysicalRetirement>,
    ) -> Result<(), IpcError> {
        // Admit the original carrier even when the consumer already cancelled,
        // so every late SHM descriptor still ends its read and queues RELEASE.
        let carrier = if let ValueStorage::Shm(descriptor) = storage {
            Some(admit_response_storage(
                ValueStorage::Shm(descriptor),
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
        let offset = self
            .conn
            .core
            .stream_data(id, storage.len())
            .map_err(core_error)?;
        if !state.discarded {
            let item = state
                .item
                .as_mut()
                .expect("storage CREDIT follows buffer allocation");
            let start = usize::try_from(offset).expect("admitted item offset");
            let end = start + usize::try_from(storage.len()).expect("admitted item bytes");
            match (storage, carrier) {
                (ValueStorage::Inline(bytes), None) => {
                    item.bytes[start..end].copy_from_slice(bytes)
                }
                (_, Some(ResponseData::Shm(carrier))) => carrier
                    .copy_into(&mut item.bytes[start..end])
                    .map_err(IpcError::Shm)?,
                _ => unreachable!("admitted stream carrier"),
            }
        } else {
            drop(carrier);
        }
        let flow = self
            .conn
            .core
            .stream_state(id)
            .expect("original stream flow");
        if flow.total_bytes != Some(flow.admitted_bytes) {
            return Ok(());
        }
        self.conn.core.stream_finish_item(id).map_err(core_error)?;
        let item = state.item.take();
        drop(state);
        if let Some(item) = item {
            self.start_delivery(BudgetedBuffer::new(item.bytes, item.reservation));
        }
        Ok(())
    }

    fn start_delivery(self: &Arc<Self>, bytes: BudgetedBuffer) {
        self.state.lock().deliveries += 1;
        let owner = DeliveryOwner(self.clone());
        let call = self.clone();
        tokio::spawn(async move {
            call.deliver(bytes).await;
            drop(owner);
        });
    }

    async fn deliver(self: &Arc<Self>, bytes: BudgetedBuffer) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().discarded {
                return;
            }
            let permit = tokio::select! {
                permit = self.output.reserve() => match permit { Ok(permit) => permit, Err(_) => return },
                _ = &mut changed => continue,
            };
            let mut state = self.state.lock();
            if state.discarded {
                return;
            }
            let id = state.id.expect("published stream");
            // Enqueue and grant form this call's delivery boundary. No await
            // occurs here; taking the item with recv is a separate operation.
            permit.send(bytes);
            if state.ended {
                drop(state);
                self.changed.notify_waiters();
                return;
            }
            if let Err(error) = self.conn.core.stream_credit(id, 0, 1) {
                drop(state);
                self.fail_local(C2Error::new(
                    c2_error::ErrorCode::ProtocolViolation,
                    error.to_string(),
                ));
                return;
            }
            state.pending_demand = 1;
            drop(state);
            self.changed.notify_waiters();
            self.conn.wake.notify_one();
            return;
        }
    }

    pub(crate) fn finish(&self, terminal: StreamTerminal) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        let id = state.id.ok_or(IpcError::Closed)?;
        let flow = self.conn.core.stream_state(id).ok_or(IpcError::Closed)?;
        if state.local_error.is_none()
            && (terminal.complete_items != flow.complete_items
                || (matches!(
                    terminal.status,
                    CallStatus::Success | CallStatus::NotExecuted
                ) && flow.total_bytes.is_some()))
        {
            return Err(IpcError::Protocol(
                "CALL_END complete item count does not match received items".into(),
            ));
        }
        state.item.take();
        state.terminal = Some(terminal);
        state.ended = true;
        self.conn.core.stream_retire(id);
        drop(state);
        self.changed.notify_waiters();
        self.conn.wake.notify_one();
        Ok(())
    }

    pub(crate) fn transport_failed(&self, error: IpcError) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        state.item.take();
        state.io_error = Some(error);
        state.ended = true;
        if let Some(id) = state.id {
            self.conn.core.stream_retire(id);
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod receive_boundary_tests {
    use super::*;
    use c2_error::ErrorCode;

    #[test]
    fn item_arriving_after_empty_observation_precedes_terminal() {
        for (terminal, local_error) in [
            (
                Some(StreamTerminal {
                    status: CallStatus::Success,
                    complete_items: 1,
                    error: None,
                }),
                None,
            ),
            (
                Some(StreamTerminal {
                    status: CallStatus::Failed,
                    complete_items: 1,
                    error: Some(C2Error::new(
                        ErrorCode::ResourceFunctionExecuting,
                        "cleanup failed",
                    )),
                }),
                None,
            ),
            (None, None),
            (
                None,
                Some(C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    "local budget failed",
                )),
            ),
            (
                Some(StreamTerminal {
                    status: CallStatus::Failed,
                    complete_items: 1,
                    error: Some(C2Error::new(ErrorCode::CallCancelled, "remote cleanup")),
                }),
                Some(C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    "local budget failed first",
                )),
            ),
        ] {
            let (output, mut input) = mpsc::channel(1);
            let state = Arc::new(StdMutex::new(ReceiveState::default()));
            let (allow_delivery, delivery_allowed) = std::sync::mpsc::channel();
            let (delivered, delivery_done) = std::sync::mpsc::channel();
            let producer_state = state.clone();
            let producer_terminal = terminal.clone();
            let producer_local_error = local_error.clone();
            let budget = PayloadBufferBudget::new(3);
            let producer_budget = budget.clone();
            let producer = std::thread::spawn(move || {
                delivery_allowed.recv().unwrap();
                let item =
                    BudgetedBuffer::new(b"abc".to_vec(), producer_budget.try_reserve(3).unwrap());
                output.try_send(item).unwrap();
                let mut state = producer_state.lock();
                state.ended = producer_terminal.is_some() || producer_local_error.is_none();
                state.terminal = producer_terminal;
                state.local_error = producer_local_error;
                delivered.send(()).unwrap();
            });
            // The barriers place a real producer's delivery and END/EOF
            // strictly between recv's first empty read and its terminal read.
            assert!(matches!(
                input.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            allow_delivery.send(()).unwrap();
            delivery_done.recv().unwrap();

            let item = receive_after_empty(&mut state.lock(), &mut input)
                .expect("complete item is ready")
                .expect("item precedes terminal")
                .expect("queued item");
            assert_eq!(item.as_ref(), b"abc");
            drop(item);
            assert_eq!(budget.snapshot().used_bytes, 0);

            let terminal_result = receive_after_empty(&mut state.lock(), &mut input)
                .expect("terminal follows the last item");
            match (terminal, local_error) {
                (
                    Some(StreamTerminal {
                        status: CallStatus::Success,
                        ..
                    }),
                    _,
                ) => {
                    assert!(terminal_result.unwrap().is_none());
                }
                (_, Some(local_error)) => {
                    let error = terminal_result.unwrap_err();
                    assert!(matches!(
                        error.source_error(),
                        IpcError::ResponseFailure(source)
                            if matches!(source.as_ref(), IpcError::LocalCallRejected(actual)
                                if actual.code == local_error.code)
                    ));
                }
                _ => assert_eq!(
                    terminal_result.unwrap_err().phase(),
                    crate::TransportPhase::DispatchUncertain
                ),
            }
            producer.join().unwrap();
            assert!(matches!(
                input.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }
}

impl IpcClient {
    pub async fn stream_bound(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: &[u8],
        budget: PayloadBufferBudget,
    ) -> Result<IpcResponseStream, crate::IpcCallError> {
        let (index, identity, limit) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        if data.len() as u64 > limit {
            return Err(pre_dispatch_call_error(IpcError::Config(
                "request exceeds route payload limit".into(),
            )));
        }
        let (send, mut rx) = self
            .begin_pending(PendingKind::Call(CallMode::ServerStream))
            .map_err(pre_dispatch_call_error)?;
        let (call, stream) = StreamReceive::new(
            send.conn.clone(),
            budget,
            limit.min(self.config.max_reassembly_bytes),
        );
        let (upload_stop, stop_upload) = tokio::sync::watch::channel(false);
        {
            let mut pending = self.pending.lock();
            let entry = pending
                .unpublished_mut(&send.state)
                .expect("original stream draft");
            entry.upload_stop = Some(upload_stop);
            entry.stream = Some(call);
        }
        let result = async {
            if choose_request_transport(&self.config, self.shm_request_available(), data.len())
                == RequestTransportKind::Buddy
            {
                if let Some(block) = self.try_alloc_request_block(data.len())? {
                    block.write_at(0, data)?;
                    let loan = block.prepare(&send.conn)?;
                    let alloc = block.allocation();
                    let descriptor = encode_buddy_payload(&BuddyPayload {
                        seg_idx: alloc.seg_idx as u16,
                        generation: alloc.generation,
                        offset: alloc.offset,
                        data_size: block.len() as u32,
                        is_dedicated: alloc.is_dedicated,
                        loan_id: loan.id(),
                    });
                    let metadata = rpc::encode_open_metadata(&identity, index)
                        .map_err(|error| IpcError::Protocol(error.to_string()))?;
                    let bytes = frame::encode_frame_parts(
                        0,
                        flags::FLAG_RPC_RECORD | flags::FLAG_BUDDY,
                        &[&metadata, &descriptor],
                    );
                    return send
                        .conn
                        .submit(
                            bytes,
                            WriteRequest::Business(vec![loan]),
                            send.state.clone(),
                            None,
                        )
                        .await;
                }
            }
            let size = self.config.chunk_size as usize;
            if data.len() > size {
                let metadata = rpc::encode_open_metadata(&identity, index)
                    .map_err(|error| IpcError::Protocol(error.to_string()))?;
                for (part, bytes) in data.chunks(size).enumerate() {
                    if *stop_upload.borrow() {
                        return Ok(());
                    }
                    Self::write_chunk_frame(
                        &send.conn,
                        &send.state,
                        0,
                        part,
                        data.len(),
                        (part == 0).then_some(metadata.as_slice()),
                        bytes,
                        None,
                    )
                    .await?;
                }
                Ok(())
            } else {
                let metadata = rpc::encode_open_metadata(&identity, index)
                    .map_err(|error| IpcError::Protocol(error.to_string()))?;
                send.conn
                    .submit(
                        frame::encode_frame_parts(0, flags::FLAG_RPC_RECORD, &[&metadata, data]),
                        WriteRequest::Business(Vec::new()),
                        send.state.clone(),
                        None,
                    )
                    .await
            }
        }
        .await;
        match result {
            Err(error) => prefer_ready_response(error, &mut rx)
                .map(|_| stream)
                .map_err(|source| dispatch_call_error(source, send.started())),
            Ok(()) => match rx.try_recv() {
                Ok(Ok(_)) | Ok(Err(IpcError::Closed)) => Ok(stream),
                Ok(Err(source)) => Err(classified_call_error(source)),
                Err(oneshot::error::TryRecvError::Empty) => Ok(stream),
                Err(oneshot::error::TryRecvError::Closed) => Ok(stream),
            },
        }
    }
}
