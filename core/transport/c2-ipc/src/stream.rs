//! One complete-value receiver per original server-stream call.
//!
//! The socket reader never waits for the payload budget or the local consumer.
//! A complete item enters the capacity-one consumer interface before granting
//! the next demand. A second item can then be receiving or awaiting delivery.

use super::*;
use c2_mem::retention::PayloadBufferReservation;
use std::sync::Weak;
use tokio::sync::{Notify, mpsc};

/// Shared intent for an opening caller that expires before the stream handle
/// can be returned. The original call accepts the intent once it is published.
#[derive(Clone, Default)]
pub struct IpcStreamCancellation {
    state: Arc<StdMutex<CancellationIntent>>,
    changed: Arc<Notify>,
}

#[derive(Default)]
struct CancellationIntent {
    requested: bool,
    call: Weak<StreamReceive>,
}

impl IpcStreamCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_requested(&self) -> bool {
        self.state.lock().requested
    }

    pub async fn wait_requested(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_requested() {
                return;
            }
            changed.await;
        }
    }

    pub fn cancel(&self) {
        let call = {
            let mut state = self.state.lock();
            state.requested = true;
            state.call.upgrade()
        };
        if let Some(call) = call {
            let _ = call.cancel();
        }
        self.changed.notify_waiters();
    }

    fn attach(&self, call: &Arc<StreamReceive>) {
        let requested = {
            let mut state = self.state.lock();
            state.call = Arc::downgrade(call);
            state.requested
        };
        if requested {
            let _ = call.cancel();
        }
    }
}

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
    copying: bool,
    deferred_terminal: Option<(StreamTerminal, PendingResponse)>,
    deferred_failure: Option<IpcError>,
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
    cancel_requested: AtomicBool,
    // The caller's operation slot stays with the original receive record.
    // It is released before the terminal is made observable.
    completion_owner: StdMutex<Option<Box<dyn Send>>>,
    budget: PayloadBufferBudget,
    conn: Arc<ClientConnection>,
    output: mpsc::Sender<BudgetedBuffer>,
    changed: Notify,
    max_item_bytes: u64,
    #[cfg(test)]
    shm_copy_gate: StdMutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
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
}

impl IpcResponseStream {
    #[cfg(test)]
    pub(crate) fn receive_call_for_test(&self) -> Arc<StreamReceive> {
        self.call.clone()
    }

    pub async fn recv(&mut self) -> Result<Option<BudgetedBuffer>, crate::IpcCallError> {
        loop {
            let changed = self.call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Ok(item) = self.input.try_recv() {
                return Ok(Some(item));
            }
            if let Some(result) = receive_after_empty(&mut self.call.state.lock(), &mut self.input)
            {
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
            let observed = {
                let state = self.call.state.lock();
                completion_from_state(&state)
            };
            match observed {
                Some(result) => return result,
                None => changed.await,
            }
        }
    }

    /// Observe without waiting. A cleanup terminal wins over an earlier local
    /// receive failure so close can report the original server outcome. A
    /// contended state lock returns None for a zero-budget observation.
    pub fn try_completion(&self) -> Option<Result<StreamTerminal, crate::IpcCallError>> {
        self.call
            .state
            .try_lock()
            .and_then(|state| completion_from_state(&state))
    }
}

fn completion_from_state(
    state: &ReceiveState,
) -> Option<Result<StreamTerminal, crate::IpcCallError>> {
    if !state.ended {
        return None;
    }
    if let Some(terminal) = &state.terminal {
        return Some(Ok(terminal.clone()));
    }
    if let Some(error) = &state.local_error {
        return Some(Err(local_failure(error.clone())));
    }
    Some(Err(dispatch_uncertain_call_error(
        state
            .io_error
            .as_ref()
            .map_or(IpcError::Closed, clone_observation_error),
    )))
}

// Completion and recv can both inspect the same physical failure. Preserve
// the typed IPC variant and OS error kind when building each owned result.
pub(super) fn clone_observation_error(error: &IpcError) -> IpcError {
    match error {
        IpcError::LocalCallRejected(error) => IpcError::LocalCallRejected(error.clone()),
        IpcError::CallRejected(error) => IpcError::CallRejected(error.clone()),
        IpcError::Io(error) => IpcError::Io(std::io::Error::new(error.kind(), error.to_string())),
        IpcError::Connect(error) => {
            IpcError::Connect(std::io::Error::new(error.kind(), error.to_string()))
        }
        IpcError::Config(message) => IpcError::Config(message.clone()),
        IpcError::Decode(error) => IpcError::Decode(error.clone()),
        IpcError::Protocol(message) => IpcError::Protocol(message.clone()),
        IpcError::ProtocolVersionMismatch { local, peer } => IpcError::ProtocolVersionMismatch {
            local: *local,
            peer: *peer,
        },
        IpcError::IdentityMismatch {
            expected_server_id,
            expected_server_instance_id,
            actual_server_id,
            actual_server_instance_id,
        } => IpcError::IdentityMismatch {
            expected_server_id: expected_server_id.clone(),
            expected_server_instance_id: expected_server_instance_id.clone(),
            actual_server_id: actual_server_id.clone(),
            actual_server_instance_id: actual_server_instance_id.clone(),
        },
        IpcError::ContractMismatch(message) => IpcError::ContractMismatch(message.clone()),
        IpcError::RouteNotFound(name) => IpcError::RouteNotFound(name.clone()),
        IpcError::RouteRemoved {
            route_name,
            route_uid,
        } => IpcError::RouteRemoved {
            route_name: route_name.clone(),
            route_uid: route_uid.clone(),
        },
        IpcError::RouteClosed {
            route_name,
            route_uid,
            reason,
        } => IpcError::RouteClosed {
            route_name: route_name.clone(),
            route_uid: route_uid.clone(),
            reason: reason.clone(),
        },
        IpcError::RouteStale {
            route_name,
            current_route_uid,
        } => IpcError::RouteStale {
            route_name: route_name.clone(),
            current_route_uid: current_route_uid.clone(),
        },
        IpcError::MethodNotFound {
            route_name,
            method_name,
        } => IpcError::MethodNotFound {
            route_name: route_name.clone(),
            method_name: method_name.clone(),
        },
        IpcError::Shm(message) => IpcError::Shm(message.clone()),
        IpcError::ResponseFailure(source) => {
            IpcError::ResponseFailure(Box::new(clone_observation_error(source)))
        }
        IpcError::Chunk(ChunkError::Protocol(message)) => {
            IpcError::Chunk(ChunkError::Protocol(message.clone()))
        }
        IpcError::Chunk(ChunkError::Capacity(message)) => {
            IpcError::Chunk(ChunkError::Capacity(message.clone()))
        }
        IpcError::CrmError(bytes) => IpcError::CrmError(bytes.clone()),
        IpcError::Closed => IpcError::Closed,
        IpcError::Pool(message) => IpcError::Pool(message.clone()),
        IpcError::PoolRetired => IpcError::PoolRetired,
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
    if let Some(error) = &state.local_error
        && state.deliveries == 0
    {
        Some(Err(local_failure(error.clone())))
    } else if state.ended && state.deliveries == 0 {
        match &state.terminal {
            Some(terminal) if terminal.status == CallStatus::Success => Some(Ok(None)),
            Some(terminal) => Some(Err(terminal_failure(terminal))),
            None => Some(Err(dispatch_uncertain_call_error(
                state
                    .io_error
                    .as_ref()
                    .map_or(IpcError::Closed, clone_observation_error),
            ))),
        }
    } else {
        None
    }
}

pub(super) fn terminal_failure(terminal: &StreamTerminal) -> crate::IpcCallError {
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
    #[cfg(test)]
    pub(crate) fn hold_state_for_test(&self) -> impl Drop + '_ {
        self.state.lock()
    }

    #[cfg(test)]
    pub(crate) fn set_shm_copy_gate_for_test(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        *self.shm_copy_gate.lock() = Some((entered, release));
    }

    fn new(
        conn: Arc<ClientConnection>,
        budget: PayloadBufferBudget,
        max_item_bytes: u64,
        completion_owner: Box<dyn Send>,
        opening_cancellation: Option<IpcStreamCancellation>,
    ) -> (Arc<Self>, IpcResponseStream) {
        let (output, input) = mpsc::channel(1);
        let call = Arc::new(Self {
            state: StdMutex::new(ReceiveState::default()),
            cancel_requested: AtomicBool::new(false),
            completion_owner: StdMutex::new(Some(completion_owner)),
            budget,
            conn,
            output,
            changed: Notify::new(),
            max_item_bytes,
            #[cfg(test)]
            shm_copy_gate: StdMutex::new(None),
        });
        let stream = IpcResponseStream {
            call: call.clone(),
            input,
        };
        if let Some(cancellation) = &opening_cancellation {
            cancellation.attach(&call);
        }
        (call, stream)
    }

    pub(crate) fn publish(&self, id: u64) -> Result<(), IpcError> {
        self.conn.core.stream_open(id).map_err(core_error)?;
        self.state.lock().id = Some(id);
        if self.cancel_requested.load(Ordering::Acquire) {
            self.conn.wake.notify_one();
        }
        Ok(())
    }

    pub(crate) fn take_control(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        if state.ended {
            return None;
        }
        let id = state.id?;
        self.apply_cancel_locked(&mut state, id);
        // An already backed grant for the current item must precede CANCEL;
        // the peer may still need it to settle that carrier.
        let storage = std::mem::take(&mut state.pending_storage);
        let demand = std::mem::take(&mut state.pending_demand);
        if storage > 0 || demand > 0 {
            return Some(
                rpc::encode_credit_frame(id, storage, demand).expect("nonzero checked grant"),
            );
        }
        std::mem::take(&mut state.pending_cancel).then(|| {
            rpc::encode_cancel_frame(id, rpc::CancelCause::Consumer).expect("nonzero stream ID")
        })
    }

    pub(crate) fn has_control(&self) -> bool {
        let state = self.state.lock();
        !state.ended
            && state.id.is_some()
            && (self.cancel_requested.load(Ordering::Acquire)
                || state.pending_cancel
                || state.pending_storage > 0
                || state.pending_demand > 0)
    }

    fn cancel(&self) -> Result<(), IpcError> {
        // Facade Drop and close(0) submit intent without waiting for any
        // in-progress receive work.
        self.cancel_requested.store(true, Ordering::Release);
        self.changed.notify_waiters();
        self.conn.wake.notify_waiters();
        self.conn.wake.notify_one();
        Ok(())
    }

    fn apply_cancel_locked(&self, state: &mut ReceiveState, id: u64) {
        if !self.cancel_requested.swap(false, Ordering::AcqRel) || state.ended {
            return;
        }
        if self
            .conn
            .core
            .stream_cancel(id)
            .expect("published stream remains in connection core")
        {
            state.discarded = true;
            state.item.take();
            state.pending_demand = 0;
            state.pending_cancel = true;
        }
        self.changed.notify_waiters();
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
                    s.ended || s.discarded || call.cancel_requested.load(Ordering::Acquire)
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
            if state.ended || state.discarded || call.cancel_requested.load(Ordering::Acquire) {
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
        let (id, offset, complete, mut item) = {
            let mut state = self.state.lock();
            let id = state.id.ok_or(IpcError::Closed)?;
            if carrier.is_some() {
                let flow = self.conn.core.stream_state(id).ok_or(IpcError::Closed)?;
                if flow.admitted_bytes != 0 || flow.total_bytes != Some(storage.len()) {
                    return Err(IpcError::Protocol(
                        "SHM ITEM_DATA must carry one whole item".into(),
                    ));
                }
            }
            let offset = if carrier.is_some() {
                self.conn.core.stream_shm_data(id, storage.len())
            } else {
                self.conn.core.stream_data(id, storage.len())
            }
            .map_err(core_error)?;
            let flow = self
                .conn
                .core
                .stream_state(id)
                .expect("original stream flow");
            let complete = flow.total_bytes == Some(flow.admitted_bytes);
            let item = state.item.take();
            if carrier.is_some() && item.is_some() {
                state.copying = true;
                state.deliveries += 1;
            }
            (id, offset, complete, item)
        };
        if let Some(buffer) = item.as_mut() {
            let start = usize::try_from(offset).expect("admitted item offset");
            let end = start + usize::try_from(storage.len()).expect("admitted item bytes");
            match (storage, carrier) {
                (ValueStorage::Inline(bytes), None) => {
                    buffer.bytes[start..end].copy_from_slice(bytes);
                }
                (_, Some(ResponseData::Shm(carrier))) => {
                    // One SHM descriptor carries the whole item. Move its
                    // owned buffer and read guard to one blocking job so the
                    // socket reader can process other calls immediately.
                    let call = self.clone();
                    let mut item = item.take().expect("admitted item buffer");
                    let delivery = DeliveryOwner(self.clone());
                    #[cfg(test)]
                    let gate = self.shm_copy_gate.lock().take();
                    tokio::task::spawn_blocking(move || {
                        #[cfg(test)]
                        if let Some((entered, release)) = gate {
                            let _ = entered.send(());
                            let _ = release.recv();
                        }
                        let copied = carrier
                            .copy_into(&mut item.bytes[start..end])
                            .map_err(IpcError::Shm);
                        call.settle_data(id, complete, Some(item), copied);
                        drop(delivery);
                    });
                    return Ok(());
                }
                _ => unreachable!("admitted stream carrier"),
            }
        } else {
            drop(carrier);
        }
        self.settle_data(id, complete, item, Ok(()));
        Ok(())
    }

    fn settle_data(
        self: &Arc<Self>,
        id: u64,
        complete: bool,
        mut item: Option<ItemBuffer>,
        copied: Result<(), IpcError>,
    ) {
        let mut state = self.state.lock();
        state.copying = false;
        if state.ended {
            drop(state);
            self.changed.notify_waiters();
            return;
        }
        let mut failure = copied.err();
        if failure.is_none() && complete {
            failure = self.conn.core.stream_finish_item(id).err().map(core_error);
        }
        if let Some(error) = &failure {
            state.local_error.get_or_insert_with(|| {
                C2Error::new(c2_error::ErrorCode::ProtocolViolation, error.to_string())
            });
            self.cancel_requested.store(true, Ordering::Release);
        }
        if state.discarded || self.cancel_requested.load(Ordering::Acquire) || failure.is_some() {
            item.take();
        } else if !complete {
            state.item = item.take();
        } else if item.is_some() {
            // CALL_END may already be in the reader. Account for delivery
            // before exposing an empty queue beside that terminal.
            state.deliveries += 1;
        }
        let deferred = state.deferred_terminal.take();
        let failed = state.deferred_failure.take();
        drop(state);
        self.changed.notify_waiters();
        if failure.is_some() {
            self.conn.wake.notify_one();
        }
        if complete && let Some(item) = item {
            self.spawn_delivery(BudgetedBuffer::new(item.bytes, item.reservation));
        }
        if let Some((terminal, entry)) = deferred {
            self.finish(terminal, entry);
        } else if let Some(error) = failed {
            self.transport_failed(error);
        }
    }

    fn start_delivery(self: &Arc<Self>, bytes: BudgetedBuffer) {
        self.state.lock().deliveries += 1;
        self.spawn_delivery(bytes);
    }

    fn spawn_delivery(self: &Arc<Self>, bytes: BudgetedBuffer) {
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
            if self.cancel_requested.load(Ordering::Acquire) || self.state.lock().discarded {
                return;
            }
            let permit = tokio::select! {
                permit = self.output.reserve() => match permit { Ok(permit) => permit, Err(_) => return },
                _ = &mut changed => continue,
            };
            let mut state = self.state.lock();
            if self.cancel_requested.load(Ordering::Acquire) || state.discarded {
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

    pub(crate) fn finish(&self, terminal: StreamTerminal, entry: PendingResponse) {
        let mut state = self.state.lock();
        if state.copying {
            state.deferred_terminal = Some((terminal, entry));
            return;
        }
        let result = self.finish_locked(&mut state, &terminal);
        drop(state);
        match result {
            Ok(()) => {
                let upload_response = match terminal.status {
                    CallStatus::Success | CallStatus::Failed => {
                        Ok(ResponseData::Inline(Vec::new()))
                    }
                    CallStatus::NotExecuted => Err(IpcError::CallRejected(
                        terminal.error.expect("checked C2E1 refusal"),
                    )),
                };
                entry.finish_stream_upload(upload_response);
            }
            Err(error) => entry.complete(Err(error.response_failure())),
        }
        self.changed.notify_waiters();
        self.conn.wake.notify_one();
    }

    fn finish_locked(
        &self,
        state: &mut ReceiveState,
        terminal: &StreamTerminal,
    ) -> Result<(), IpcError> {
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
        state.terminal = Some(terminal.clone());
        drop(self.completion_owner.lock().take());
        state.ended = true;
        self.conn.core.stream_retire(id);
        Ok(())
    }

    pub(crate) fn transport_failed(&self, error: IpcError) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        if state.copying {
            // EOF can reach the reader while an admitted SHM carrier is still
            // being copied. Keep the completed item and the actual failure in
            // their original order; the read guard remains live until then.
            if state.deferred_terminal.is_none() {
                state.deferred_failure.get_or_insert(error);
            }
            return;
        }
        state.item.take();
        state.io_error = Some(error);
        drop(self.completion_owner.lock().take());
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
    fn close_observer_prefers_actual_cleanup_terminal_over_local_receive_failure() {
        let state = ReceiveState {
            ended: true,
            local_error: Some(C2Error::new(
                ErrorCode::CallCapacityExceeded,
                "local B limit",
            )),
            terminal: Some(StreamTerminal {
                status: CallStatus::Failed,
                complete_items: 2,
                error: Some(C2Error::new(ErrorCode::CallCancelled, "cleanup completed")),
            }),
            ..ReceiveState::default()
        };
        let terminal = completion_from_state(&state).unwrap().unwrap();
        assert_eq!(terminal.complete_items, 2);
        assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    }

    #[test]
    fn repeated_transport_observation_preserves_typed_capacity_failure() {
        let state = ReceiveState {
            ended: true,
            io_error: Some(IpcError::ResponseFailure(Box::new(IpcError::Chunk(
                ChunkError::Capacity("reassembly limit".into()),
            )))),
            ..ReceiveState::default()
        };
        for _ in 0..2 {
            let error = completion_from_state(&state).unwrap().unwrap_err();
            assert!(matches!(
                error.source_error(),
                IpcError::ResponseFailure(source)
                    if matches!(source.as_ref(), IpcError::Chunk(ChunkError::Capacity(_)))
            ));
        }
    }

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
        self.stream_bound_inner(binding, method_name, data, budget, Box::new(()), None, None)
            .await
    }

    /// Transfer an owned request and an operation-slot owner to the original
    /// stream record. The owner survives a caller deadline or dropped facade.
    pub async fn stream_bound_owned_controlled_phased<B>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: B,
        budget: PayloadBufferBudget,
        completion_owner: Box<dyn Send>,
        cancellation: IpcStreamCancellation,
        control: &IpcCallControl,
    ) -> Result<IpcResponseStream, crate::IpcCallError>
    where
        B: AsRef<[u8]> + Send + 'static,
    {
        self.stream_bound_inner(
            binding,
            method_name,
            data,
            budget,
            completion_owner,
            Some(cancellation),
            Some(control),
        )
        .await
    }

    async fn stream_bound_inner<B>(
        &self,
        binding: &RouteBinding,
        method_name: &str,
        data: B,
        budget: PayloadBufferBudget,
        completion_owner: Box<dyn Send>,
        cancellation: Option<IpcStreamCancellation>,
        control: Option<&IpcCallControl>,
    ) -> Result<IpcResponseStream, crate::IpcCallError>
    where
        B: AsRef<[u8]> + Send,
    {
        let (index, identity, limit) = binding
            .call_target_for(method_name)
            .map_err(pre_dispatch_call_error)?;
        if data.as_ref().len() as u64 > limit {
            return Err(pre_dispatch_call_error(IpcError::Config(
                "request exceeds route payload limit".into(),
            )));
        }
        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;
        let (send, mut rx) = self
            .begin_pending(PendingKind::Call(CallMode::ServerStream))
            .map_err(pre_dispatch_call_error)?;
        let (call, stream) = StreamReceive::new(
            send.conn.clone(),
            budget,
            limit.min(self.config.max_reassembly_bytes),
            completion_owner,
            cancellation,
        );
        let cancellation = call.clone();
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
            if choose_request_transport(
                &self.config,
                self.shm_request_available(),
                data.as_ref().len(),
            ) == RequestTransportKind::Buddy
            {
                if let Some(block) = self.try_alloc_request_block(data.as_ref().len())? {
                    block.write_at(0, data.as_ref())?;
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
                            control.cloned(),
                        )
                        .await;
                }
            }
            let size = self.config.chunk_size as usize;
            if data.as_ref().len() > size {
                let metadata = rpc::encode_open_metadata(&identity, index)
                    .map_err(|error| IpcError::Protocol(error.to_string()))?;
                for (part, bytes) in data.as_ref().chunks(size).enumerate() {
                    if *stop_upload.borrow() {
                        return Ok(());
                    }
                    Self::write_chunk_frame(
                        &send.conn,
                        &send.state,
                        0,
                        part,
                        data.as_ref().len(),
                        (part == 0).then_some(metadata.as_slice()),
                        bytes,
                        if part == 0 { control.cloned() } else { None },
                    )
                    .await?;
                }
                Ok(())
            } else {
                let metadata = rpc::encode_open_metadata(&identity, index)
                    .map_err(|error| IpcError::Protocol(error.to_string()))?;
                send.conn
                    .submit(
                        frame::encode_frame_parts(
                            0,
                            flags::FLAG_RPC_RECORD,
                            &[&metadata, data.as_ref()],
                        ),
                        WriteRequest::Business(Vec::new()),
                        send.state.clone(),
                        control.cloned(),
                    )
                    .await
            }
        }
        .await;
        let opening = match result {
            Err(error) => prefer_ready_response(error, &mut rx)
                .map(|_| stream)
                .map_err(|source| dispatch_call_error(source, send.started())),
            Ok(()) => match rx.try_recv() {
                Ok(Ok(_)) | Ok(Err(IpcError::Closed)) => Ok(stream),
                Ok(Err(source)) => Err(classified_call_error(source)),
                Err(oneshot::error::TryRecvError::Empty) => Ok(stream),
                Err(oneshot::error::TryRecvError::Closed) => Ok(stream),
            },
        };
        if opening.is_err() && send.started() {
            // Opening failed after publication. The caller has no handle to
            // drop, so submit cancellation while the original record and its
            // slot continue to the real terminal.
            let _ = cancellation.cancel();
        }
        opening
    }
}
