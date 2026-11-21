//! One complete-value receiver per IPC 17 topic subscription.
//!
//! Mirrors `stream.rs`: the socket reader never blocks on `DomainBudget`
//! memory reservation or SHM copy. Each subscription awaits `READY(start_seq)`
//! before returning its handle, computes `dropped` locally from `(expected_seq,
//! frame.seq)`, copies SHM blocks into a local `BudgetedBuffer` before
//! releasing the server loan, and keeps its operation-slot `completion_owner`
//! until the real terminal `CALL_END` or transport end.

use super::*;
use c2_mem::{DomainBudget, MemoryReservation};
use std::sync::Weak;
use tokio::sync::{Notify, mpsc, oneshot};

/// Shared cancellation intent for an opening subscriber whose caller stops
/// waiting before `READY` arrives.
#[derive(Clone, Default)]
pub struct IpcSubscribeCancellation {
    state: Arc<StdMutex<SubscribeCancellationIntent>>,
    changed: Arc<Notify>,
}

#[derive(Default)]
struct SubscribeCancellationIntent {
    requested: bool,
    call: Weak<SubscribeReceive>,
}

impl IpcSubscribeCancellation {
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

    fn attach(&self, call: &Arc<SubscribeReceive>) {
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

/// One immutable topic delivery received over IPC 17.
pub struct IpcTopicDelivery {
    pub bytes: BudgetedBuffer,
    pub seq: u64,
    pub dropped: u64,
}

struct ActiveItemBuffer {
    bytes: Vec<u8>,
    reservation: MemoryReservation,
    seq: u64,
    dropped: u64,
}

#[derive(Default)]
struct SubscribeReceiveState {
    id: Option<u64>,
    start_seq: Option<u64>,
    next_expected: u64,
    ready_tx: Option<oneshot::Sender<Result<u64, crate::IpcCallError>>>,
    item: Option<ActiveItemBuffer>,
    copying: bool,
    deferred_terminal: Option<(stream::StreamTerminal, PendingResponse)>,
    deferred_failure: Option<IpcError>,
    discarded: bool,
    ended: bool,
    terminal: Option<stream::StreamTerminal>,
    local_error: Option<C2Error>,
    io_error: Option<IpcError>,
    pending_storage: u64,
    pending_demand: u64,
    pending_cancel: bool,
    deliveries: usize,
}

pub(crate) struct SubscribeReceive {
    state: StdMutex<SubscribeReceiveState>,
    options: rpc::WireSubscribeOptions,
    cancel_requested: AtomicBool,
    completion_owner: StdMutex<Option<Box<dyn Send>>>,
    budget: DomainBudget,
    conn: Arc<ClientConnection>,
    output: mpsc::Sender<IpcTopicDelivery>,
    changed: Notify,
    max_item_bytes: u64,
}

struct SubscribeDeliveryOwner(Arc<SubscribeReceive>);
impl Drop for SubscribeDeliveryOwner {
    fn drop(&mut self) {
        self.0.state.lock().deliveries -= 1;
        self.0.changed.notify_waiters();
    }
}

/// Transport receive handle for an active IPC 17 topic subscription.
pub struct IpcTopicSubscription {
    call: Arc<SubscribeReceive>,
    input: mpsc::Receiver<IpcTopicDelivery>,
    start_seq: u64,
    topic_name: String,
}

impl IpcTopicSubscription {
    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn topic_name(&self) -> &str {
        &self.topic_name
    }

    pub async fn recv(&mut self) -> Result<IpcTopicDelivery, crate::IpcCallError> {
        loop {
            let changed = self.call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Ok(item) = self.input.try_recv() {
                return Ok(item);
            }
            if let Some(result) =
                receive_after_empty(&mut self.call.state.lock(), &mut self.input)
            {
                return result;
            }
            tokio::select! {
                biased;
                item = self.input.recv() => {
                    if let Some(item) = item {
                        return Ok(item);
                    }
                }
                _ = &mut changed => {}
            }
        }
    }

    pub fn cancel(&self) -> Result<(), IpcError> {
        self.call.cancel()
    }

    pub async fn completion(&self) -> Result<stream::StreamTerminal, crate::IpcCallError> {
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

    pub fn try_completion(&self) -> Option<Result<stream::StreamTerminal, crate::IpcCallError>> {
        self.call
            .state
            .try_lock()
            .and_then(|state| completion_from_state(&state))
    }
}

fn completion_from_state(
    state: &SubscribeReceiveState,
) -> Option<Result<stream::StreamTerminal, crate::IpcCallError>> {
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
            .map_or(IpcError::Closed, stream::clone_observation_error),
    )))
}

fn receive_after_empty(
    state: &mut SubscribeReceiveState,
    input: &mut mpsc::Receiver<IpcTopicDelivery>,
) -> Option<Result<IpcTopicDelivery, crate::IpcCallError>> {
    if let Ok(item) = input.try_recv() {
        return Some(Ok(item));
    }
    if let Some(error) = &state.local_error
        && state.deliveries == 0
    {
        Some(Err(local_failure(error.clone())))
    } else if state.ended && state.deliveries == 0 {
        match &state.terminal {
            Some(terminal) => Some(Err(stream::terminal_failure(terminal))),
            None => Some(Err(dispatch_uncertain_call_error(
                state
                    .io_error
                    .as_ref()
                    .map_or(IpcError::Closed, stream::clone_observation_error),
            ))),
        }
    } else {
        None
    }
}

fn local_failure(error: C2Error) -> crate::IpcCallError {
    dispatch_uncertain_call_error(IpcError::ResponseFailure(Box::new(
        IpcError::LocalCallRejected(error),
    )))
}

impl SubscribeReceive {
    fn new(
        conn: Arc<ClientConnection>,
        budget: DomainBudget,
        max_item_bytes: u64,
        options: rpc::WireSubscribeOptions,
        completion_owner: Box<dyn Send>,
        opening_cancellation: Option<IpcSubscribeCancellation>,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<IpcTopicDelivery>,
        oneshot::Receiver<Result<u64, crate::IpcCallError>>,
    ) {
        let (output, input) = mpsc::channel(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        let call = Arc::new(Self {
            state: StdMutex::new(SubscribeReceiveState {
                ready_tx: Some(ready_tx),
                ..SubscribeReceiveState::default()
            }),
            options,
            cancel_requested: AtomicBool::new(false),
            completion_owner: StdMutex::new(Some(completion_owner)),
            budget,
            conn,
            output,
            changed: Notify::new(),
            max_item_bytes,
        });
        if let Some(cancellation) = &opening_cancellation {
            cancellation.attach(&call);
        }
        (call, input, ready_rx)
    }

    pub(crate) fn publish(&self, id: u64) -> Result<(), IpcError> {
        self.conn.core.subscribe_open(id).map_err(core_error)?;
        self.state.lock().id = Some(id);
        if self.cancel_requested.load(Ordering::Acquire) {
            self.conn.wake.notify_one();
        }
        Ok(())
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

    pub(crate) fn take_control(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        if state.ended {
            return None;
        }
        let id = state.id?;
        self.apply_cancel_locked(&mut state, id);
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

    fn cancel(&self) -> Result<(), IpcError> {
        self.cancel_requested.store(true, Ordering::Release);
        self.changed.notify_waiters();
        self.conn.wake.notify_waiters();
        self.conn.wake.notify_one();
        Ok(())
    }

    fn apply_cancel_locked(&self, state: &mut SubscribeReceiveState, id: u64) {
        if !self.cancel_requested.swap(false, Ordering::AcqRel) || state.ended {
            return;
        }
        if self
            .conn
            .core
            .stream_cancel(id)
            .expect("published subscription remains in connection core")
        {
            state.discarded = true;
            state.item.take();
            state.pending_demand = 0;
            state.pending_cancel = true;
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn fail_local(&self, error: C2Error) {
        let ready = {
            let mut state = self.state.lock();
            if state.ended {
                return;
            }
            state.local_error.get_or_insert(error.clone());
            state.ready_tx.take()
        };
        if let Some(tx) = ready {
            let _ = tx.send(Err(local_failure(error)));
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

    pub(crate) fn ready(&self, start_seq: u64) -> Result<(), IpcError> {
        if start_seq == 0 {
            return Err(IpcError::Protocol("zero READY start_seq".into()));
        }
        let tx = {
            let mut state = self.state.lock();
            if state.start_seq.is_some() {
                return Err(IpcError::Protocol("duplicate READY for subscription".into()));
            }
            state.start_seq = Some(start_seq);
            state.next_expected = start_seq;
            state.ready_tx.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(Ok(start_seq));
        }
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn begin(self: &Arc<Self>, seq: u64, total: u64) -> Result<(), IpcError> {
        let (id, dropped) = {
            let mut state = self.state.lock();
            let id = state.id.ok_or(IpcError::Closed)?;
            if state.start_seq.is_none() {
                return Err(IpcError::Protocol(
                    "subscription ITEM_BEGIN before READY".into(),
                ));
            }
            if seq < state.next_expected {
                return Err(IpcError::Protocol(format!(
                    "non-monotonic subscription seq {seq} < expected {}",
                    state.next_expected
                )));
            }
            let dropped = seq - state.next_expected;
            if self.options.on_lag == rpc::WireLagPolicy::Error && dropped > 0 {
                return Err(IpcError::Protocol(format!(
                    "subscription skipped {dropped} frames under on_lag=error"
                )));
            }
            state.next_expected = seq
                .checked_add(1)
                .ok_or_else(|| IpcError::Protocol("subscription seq overflow".into()))?;
            (id, dropped)
        };

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
                    "topic item length {total} exceeds payload or address-space limit {}",
                    self.max_item_bytes
                ),
            ));
            return Ok(());
        }
        if total == 0 {
            let reservation = self
                .budget
                .try_reserve_memory(0)
                .expect("zero-byte reservation");
            self.start_delivery(IpcTopicDelivery {
                bytes: BudgetedBuffer::new(Vec::new(), reservation),
                seq,
                dropped,
            });
            return Ok(());
        }

        let call = self.clone();
        tokio::spawn(async move {
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
                    reservation = call.budget.reserve_memory(total) => break reservation,
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
                        "topic item is not addressable",
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
            state.item = Some(ActiveItemBuffer {
                bytes,
                reservation,
                seq,
                dropped,
            });
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
                        "SHM ITEM_DATA must carry one whole topic item".into(),
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
                .expect("original subscription flow");
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
                    let call = self.clone();
                    let mut item = item.take().expect("admitted item buffer");
                    let delivery = SubscribeDeliveryOwner(self.clone());
                    tokio::task::spawn_blocking(move || {
                        let copied = carrier
                            .copy_into(&mut item.bytes[start..end])
                            .map_err(IpcError::Shm);
                        // `carrier` has already ended its PeerRead and queued RELEASE.
                        call.settle_data(id, complete, Some(item), copied);
                        drop(delivery);
                    });
                    return Ok(());
                }
                _ => unreachable!("admitted subscription carrier"),
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
        mut item: Option<ActiveItemBuffer>,
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
            self.spawn_delivery(IpcTopicDelivery {
                bytes: BudgetedBuffer::new(item.bytes, item.reservation),
                seq: item.seq,
                dropped: item.dropped,
            });
        }
        if let Some((terminal, entry)) = deferred {
            self.finish(terminal, entry);
        } else if let Some(error) = failed {
            self.transport_failed(error);
        }
    }

    fn start_delivery(self: &Arc<Self>, delivery: IpcTopicDelivery) {
        self.state.lock().deliveries += 1;
        self.spawn_delivery(delivery);
    }

    fn spawn_delivery(self: &Arc<Self>, delivery: IpcTopicDelivery) {
        let owner = SubscribeDeliveryOwner(self.clone());
        let call = self.clone();
        tokio::spawn(async move {
            call.deliver(delivery).await;
            drop(owner);
        });
    }

    async fn deliver(self: &Arc<Self>, delivery: IpcTopicDelivery) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.cancel_requested.load(Ordering::Acquire) || self.state.lock().discarded {
                return;
            }
            let permit = tokio::select! {
                permit = self.output.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
                _ = &mut changed => continue,
            };
            let mut state = self.state.lock();
            if self.cancel_requested.load(Ordering::Acquire) || state.discarded {
                return;
            }
            let id = state.id.expect("published subscription");
            permit.send(delivery);
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

    pub(crate) fn finish(&self, terminal: stream::StreamTerminal, entry: PendingResponse) {
        let mut state = self.state.lock();
        if state.copying {
            state.deferred_terminal = Some((terminal, entry));
            return;
        }
        let ready_tx = state.ready_tx.take();
        let result = self.finish_locked(&mut state, &terminal);
        drop(state);
        if let Some(tx) = ready_tx {
            let err = match &result {
                Ok(()) => stream::terminal_failure(&terminal),
                Err(error) => {
                    dispatch_uncertain_call_error(stream::clone_observation_error(error))
                }
            };
            let _ = tx.send(Err(err));
        }
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
        state: &mut SubscribeReceiveState,
        terminal: &stream::StreamTerminal,
    ) -> Result<(), IpcError> {
        let id = state.id.ok_or(IpcError::Closed)?;
        let flow = self.conn.core.stream_state(id).ok_or(IpcError::Closed)?;
        if state.local_error.is_none()
            && (terminal.complete_items != flow.complete_items
                || (terminal.status == CallStatus::NotExecuted && flow.total_bytes.is_some()))
        {
            return Err(IpcError::Protocol(
                "CALL_END complete item count does not match received subscription items".into(),
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
            if state.deferred_terminal.is_none() {
                state.deferred_failure.get_or_insert(error);
            }
            return;
        }
        let ready_tx = state.ready_tx.take();
        let ready_error = stream::clone_observation_error(&error);
        state.item.take();
        state.io_error = Some(error);
        drop(self.completion_owner.lock().take());
        state.ended = true;
        if let Some(id) = state.id {
            self.conn.core.stream_retire(id);
        }
        drop(state);
        if let Some(tx) = ready_tx {
            let _ = tx.send(Err(dispatch_uncertain_call_error(ready_error)));
        }
        self.changed.notify_waiters();
    }
}

impl IpcClient {
    pub async fn subscribe_bound(
        &self,
        binding: &RouteBinding,
        topic_name: &str,
        options: rpc::WireSubscribeOptions,
        budget: DomainBudget,
    ) -> Result<IpcTopicSubscription, crate::IpcCallError> {
        self.subscribe_bound_inner(binding, topic_name, options, budget, Box::new(()), None, None)
            .await
    }

    pub async fn subscribe_bound_controlled_phased(
        &self,
        binding: &RouteBinding,
        topic_name: &str,
        options: rpc::WireSubscribeOptions,
        budget: DomainBudget,
        completion_owner: Box<dyn Send>,
        cancellation: IpcSubscribeCancellation,
        control: &IpcCallControl,
    ) -> Result<IpcTopicSubscription, crate::IpcCallError> {
        self.subscribe_bound_inner(
            binding,
            topic_name,
            options,
            budget,
            completion_owner,
            Some(cancellation),
            Some(control),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn subscribe_bound_inner(
        &self,
        binding: &RouteBinding,
        topic_name: &str,
        options: rpc::WireSubscribeOptions,
        budget: DomainBudget,
        completion_owner: Box<dyn Send>,
        cancellation: Option<IpcSubscribeCancellation>,
        control: Option<&IpcCallControl>,
    ) -> Result<IpcTopicSubscription, crate::IpcCallError> {
        c2_contract::validate_contract_text_field("topic_name", topic_name)
            .map_err(|error| pre_dispatch_call_error(IpcError::Protocol(error.to_string())))?;
        let (identity, limit) = binding.subscribe_target();
        IpcCallControl::check_preparation(control).map_err(pre_dispatch_call_error)?;

        let (send, mut rx) = self
            .begin_pending(PendingKind::Call(CallMode::Subscribe), limit)
            .map_err(pre_dispatch_call_error)?;
        let (call, input, mut ready_rx) = SubscribeReceive::new(
            send.conn.clone(),
            budget,
            limit,
            options,
            completion_owner,
            cancellation.clone(),
        );
        {
            let mut pending = self.pending.lock();
            let entry = pending
                .unpublished_mut(&send.state)
                .expect("original subscription draft");
            entry.subscription = Some(call.clone());
        }

        let bytes = rpc::encode_open_subscribe_draft_frame(&identity, topic_name, options)
            .map_err(|error| pre_dispatch_call_error(IpcError::Protocol(error.to_string())))?;
        let submit = send
            .conn
            .submit(
                bytes,
                WriteRequest::Business(Vec::new()),
                send.state.clone(),
                control.cloned(),
            )
            .await;

        if let Err(error) = submit {
            let err = match prefer_ready_response(error, &mut rx) {
                Ok(_) => dispatch_call_error(IpcError::Closed, send.started()),
                Err(source) => dispatch_call_error(source, send.started()),
            };
            if send.started() {
                let _ = call.cancel();
            }
            return Err(err);
        }

        // Wait for READY(start_seq), early refusal CALL_END(NotExecuted), or
        // opening cancellation.
        let ready = if let Some(cancel) = &cancellation {
            tokio::select! {
                biased;
                res = &mut ready_rx => res,
                _ = cancel.wait_requested() => {
                    let _ = call.cancel();
                    return Err(dispatch_uncertain_call_error(IpcError::CallRejected(
                        C2Error::new(
                            c2_error::ErrorCode::CallCancelled,
                            "consumer cancelled subscription while awaiting READY",
                        ),
                    )));
                }
            }
        } else {
            ready_rx.await
        };

        match ready {
            Ok(Ok(start_seq)) => Ok(IpcTopicSubscription {
                call,
                input,
                start_seq,
                topic_name: topic_name.to_string(),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                if send.started() {
                    let _ = call.cancel();
                }
                Err(dispatch_call_error(IpcError::Closed, send.started()))
            }
        }
    }
}
