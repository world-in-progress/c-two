//! Host hook admission and control operations on an established IPC connection.

use super::*;
use crate::connection::FlightGuard;
use crate::hook::{AdmissionContext, AdmissionTransport, evaluate_admission_hook};
use crate::topic::{RemoteTopicAttach, TopicChannel};
use c2_wire::connection::calls::CallRecord;
use std::any::Any;
use std::sync::atomic::AtomicUsize;
use tokio::time::Instant;

type Extension = Arc<dyn Any + Send + Sync>;

#[derive(Default)]
pub(super) struct ActiveAdmissions {
    count: AtomicUsize,
    changed: Notify,
}

impl ActiveAdmissions {
    pub(super) async fn wait_idle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

struct ActiveAdmissionGuard(Arc<Server>);

impl ActiveAdmissionGuard {
    fn new(server: &Arc<Server>) -> Self {
        server
            .active_admissions
            .count
            .fetch_add(1, Ordering::AcqRel);
        Self(server.clone())
    }
}

impl Drop for ActiveAdmissionGuard {
    fn drop(&mut self) {
        let active = &self.0.active_admissions;
        if active.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            active.changed.notify_waiters();
        }
    }
}

struct HookRequest {
    route: Arc<CrmRoute>,
    method: Option<u16>,
    channel: Option<Arc<TopicChannel>>,
    operation: AdmissionOperation,
    call_id: Option<String>,
    metadata: CallMetadata,
}

impl HookRequest {
    async fn evaluate(
        self: Arc<Self>,
        guard: ActiveAdmissionGuard,
        permit: OwnedSemaphorePermit,
    ) -> Result<Option<Extension>, C2Error> {
        let hook = guard.0.admission_hook().expect("configured admission hook");
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let _permit = permit;
            let route = &self.route;
            let context = AdmissionContext::new(
                &route.name,
                &route.crm_ns,
                &route.crm_name,
                &route.crm_ver,
                self.method
                    .map(|index| route.method_names[index as usize].as_str())
                    .or_else(|| self.channel.as_ref().map(|channel| channel.topic_name())),
                self.method.and_then(|index| route.method_access(index)),
                self.operation,
                AdmissionTransport::Ipc,
                self.call_id.as_deref(),
                &self.metadata,
            );
            evaluate_admission_hook(hook.as_ref(), &context)
        })
        .await
        .unwrap_or_else(|_| {
            Err(
                C2Error::new(ErrorCode::AdmissionDenied, "admission hook failed")
                    .with_details([("reason".into(), "hook_error".into())].into()),
            )
        })
    }
}

pub(super) enum OpenOperation {
    Call {
        method: u16,
        mode: CallMode,
        call_id: Option<String>,
    },
    Recover {
        method: u16,
        call_id: String,
    },
    Subscribe {
        topic: String,
        options: rpc::WireSubscribeOptions,
    },
}

pub(super) struct PendingAdmission {
    request: Arc<HookRequest>,
    generation: u64,
    input: Option<StagedInput>,
    subscription_options: Option<rpc::WireSubscribeOptions>,
    // The receiver wakes as soon as the record retires before admission.
    retired: watch::Sender<()>,
    pub(super) flight: FlightGuard,
}

impl PendingAdmission {
    pub(super) fn expired(&self, now: Instant) -> bool {
        matches!(
            &self.input,
            Some(StagedInput::Chunked { deadline: Some(deadline), .. }) if *deadline <= now
        )
    }
}

enum StagedInput {
    Whole(OwnedRequestInput),
    Chunked {
        receive: ValueReceive,
        count: usize,
        frames: VecDeque<PendingChunkFrame>,
        deadline: Option<Instant>,
    },
}

// Queue nodes contain only IDs. Retirement unlinks a node in constant time
// so a blocked hook cannot accumulate tombstones as the peer opens and cancels.
#[derive(Default)]
pub(crate) struct AdmissionQueue {
    waiting: HashMap<u64, AdmissionLinks>,
    first: Option<u64>,
    last: Option<u64>,
    running: bool,
}

struct AdmissionLinks {
    previous: Option<u64>,
    next: Option<u64>,
}

impl AdmissionQueue {
    fn push(&mut self, id: u64) -> bool {
        let links = AdmissionLinks {
            previous: self.last,
            next: None,
        };
        if let Some(last) = self.last {
            self.waiting.get_mut(&last).expect("queue tail").next = Some(id);
        } else {
            self.first = Some(id);
        }
        self.last = Some(id);
        self.waiting.insert(id, links);
        !std::mem::replace(&mut self.running, true)
    }

    fn pop(&mut self) -> Option<u64> {
        let id = self.first?;
        self.remove(id);
        Some(id)
    }

    pub(super) fn remove(&mut self, id: u64) {
        let Some(links) = self.waiting.remove(&id) else {
            return;
        };
        if let Some(previous) = links.previous {
            self.waiting
                .get_mut(&previous)
                .expect("previous queue node")
                .next = links.next;
        } else {
            self.first = links.next;
        }
        if let Some(next) = links.next {
            self.waiting
                .get_mut(&next)
                .expect("next queue node")
                .previous = links.previous;
        } else {
            self.last = links.previous;
        }
    }

    pub(super) fn clear(&mut self) {
        self.waiting.clear();
        self.first = None;
        self.last = None;
    }
}

fn closed() -> C2Error {
    C2Error::new(ErrorCode::ResourceClosed, "route or subscription closed")
}

fn protocol_error(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ProtocolViolation, message)
}

fn validate_generation(server: &Server, generation: u64) -> Result<(), C2Error> {
    server
        .admission
        .lock()
        .check_accepted(generation)
        .map_err(|error| route_admission_error(RouteAdmissionError::Acquire(error)))
}

fn validate_registration(dispatcher: &Dispatcher, route: &Arc<CrmRoute>) -> Result<(), C2Error> {
    if !dispatcher
        .resolve(&route.name)
        .is_some_and(|current| Arc::ptr_eq(&current, route))
    {
        return Err(closed());
    }
    validate_live_route(route).map_err(route_admission_error)
}

fn stage_input(
    server: &Server,
    conn: &Arc<Connection>,
    value: ValuePart<'_>,
) -> Result<StagedInput, C2Error> {
    match value.chunk {
        ChunkPosition::Whole => {
            let bytes = value.storage.len();
            let memory = server
                .domain_budget
                .try_reserve_memory(bytes)
                .map_err(|error| {
                    release_request_storage(conn, value.storage);
                    input_admission_error("call input admission", bytes, &error)
                })?;
            let data = request_storage(conn, value.storage).map_err(protocol_error)?;
            Ok(StagedInput::Whole(OwnedRequestInput {
                data,
                memory: Some(memory),
            }))
        }
        ChunkPosition::First { total_bytes } => {
            let (receive, count, width) = (|| {
                let mut receive = ValueReceive::first(total_bytes, value.storage.len())?;
                let (count, width) = receive.geometry()?;
                receive.admit(value.storage.len())?;
                Ok((receive, count, width))
            })()
            .map_err(|error: &'static str| {
                release_request_storage(conn, value.storage);
                protocol_error(error)
            })?;
            let frame = stage_chunk(
                server,
                conn,
                value.storage,
                0,
                Some((total_bytes, count, width)),
            )?;
            Ok(StagedInput::Chunked {
                receive,
                count,
                frames: VecDeque::from([frame]),
                deadline: Some(Instant::now() + server.chunk_registry.config().assembler_timeout),
            })
        }
        ChunkPosition::Continuation => unreachable!("OPEN first record"),
    }
}

fn stage_chunk(
    server: &Server,
    conn: &Arc<Connection>,
    storage: ValueStorage<'_>,
    chunk_idx: usize,
    first_geometry: Option<(u64, usize, usize)>,
) -> Result<PendingChunkFrame, C2Error> {
    let inline_len = match storage {
        ValueStorage::Inline(bytes) => bytes.len() as u64,
        ValueStorage::Shm(_) => 0,
    };
    let charge = inline_len + PENDING_FRAME_OVERHEAD as u64;
    let memory = server
        .domain_budget
        .try_reserve_memory(charge)
        .map_err(|error| {
            release_request_storage(conn, storage);
            input_admission_error("frame admission", charge, &error)
        })?;
    let data = request_storage(conn, storage).map_err(protocol_error)?;
    Ok(PendingChunkFrame {
        chunk_idx,
        first_geometry,
        data: Ok(data),
        _memory: memory,
    })
}

/// Called under conn.calls, which also serializes admission commit and retirement.
pub(super) fn stage_continuation(
    pending: &mut PendingAdmission,
    server: &Server,
    conn: &Arc<Connection>,
    storage: ValueStorage<'_>,
) -> Result<(), C2Error> {
    let Some(StagedInput::Chunked {
        receive,
        count,
        frames,
        deadline,
    }) = &mut pending.input
    else {
        release_request_storage(conn, storage);
        return Err(protocol_error("OPEN value is already complete"));
    };
    let index = receive.admit(storage.len()).map_err(|error| {
        release_request_storage(conn, storage);
        protocol_error(error)
    })?;
    frames.push_back(stage_chunk(server, conn, storage, index, None)?);
    *deadline = (frames.len() < *count)
        .then(|| Instant::now() + server.chunk_registry.config().assembler_timeout);
    Ok(())
}

pub(super) async fn open(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    identity: RouteCallIdentity,
    operation: OpenOperation,
    metadata: CallMetadata,
    value: Option<ValuePart<'_>>,
) {
    let result = (|| {
        if conn.is_deactivated() || conn.shutdown_requested() || conn.is_work_cancelled() {
            return Err(closed());
        }
        let dispatcher = server.dispatcher.read();
        let route =
            resolve_route_contract(&dispatcher, &identity).map_err(route_admission_error)?;
        validate_live_route(&route).map_err(route_admission_error)?;
        let generation = {
            let gate = server.admission.lock();
            gate.check(gate.generation)
                .map_err(|error| route_admission_error(RouteAdmissionError::Acquire(error)))?;
            gate.generation
        };
        let (method, channel, operation, call_id, subscription_options) = match operation {
            OpenOperation::Call {
                method,
                mode,
                call_id,
            } => {
                let contract_mode = if mode == CallMode::KeyedUnary {
                    CallMode::Unary
                } else {
                    mode
                };
                validate_route_method(&route, method, contract_mode)
                    .map_err(route_admission_error)?;
                let operation = if mode == CallMode::ServerStream {
                    AdmissionOperation::ServerStream
                } else {
                    AdmissionOperation::Unary
                };
                (Some(method), None, operation, call_id, None)
            }
            OpenOperation::Recover { method, call_id } => {
                validate_route_method(&route, method, CallMode::Unary)
                    .map_err(route_admission_error)?;
                (
                    Some(method),
                    None,
                    AdmissionOperation::Recover,
                    Some(call_id),
                    None,
                )
            }
            OpenOperation::Subscribe { topic, options } => {
                let channel = route
                    .topics
                    .as_ref()
                    .ok_or_else(|| crate::topic::unknown_topic_error(&route.name, &topic))?
                    .open_channel(&topic)?
                    .clone();
                (
                    None,
                    Some(channel),
                    AdmissionOperation::Subscribe,
                    None,
                    Some(options),
                )
            }
        };
        if let Some(value) = &value {
            let size = match value.chunk {
                ChunkPosition::Whole => value.storage.len(),
                ChunkPosition::First { total_bytes } => total_bytes,
                ChunkPosition::Continuation => unreachable!("OPEN first record"),
            };
            if size > server.config.max_payload_size {
                return Err(C2Error::new(
                    ErrorCode::ResourceUnavailable,
                    format!(
                        "request payload size {size} exceeds max_payload_size {}",
                        server.config.max_payload_size
                    ),
                ));
            }
        }
        let flight = FlightGuard::new(conn);
        let operation_permit = if subscription_options.is_some() {
            server.domain_budget.reserve_subscription_operation()
        } else {
            server.domain_budget.reserve_operation()
        }
        .map_err(|error| keyed_capacity_refused(id, &error))?;
        let stream = match operation {
            AdmissionOperation::Subscribe => Some(conn.core.subscribe_open(id)),
            AdmissionOperation::ServerStream => Some(conn.core.stream_open(id)),
            _ => None,
        };
        let stream = stream
            .map(|opened| {
                opened.map(|()| StreamCallControl {
                    route_uid: route.route_uid.clone(),
                    cancellation: None,
                    topic_channel: None,
                    topic_route: None,
                    sync_reply_failed: false,
                })
            })
            .transpose()
            .map_err(|error| protocol_error(error.to_string()))?;
        let pending = PendingAdmission {
            request: Arc::new(HookRequest {
                route,
                method,
                channel,
                operation,
                call_id,
                metadata,
            }),
            generation,
            input: None,
            subscription_options,
            retired: watch::channel(()).0,
            flight,
        };
        let mut calls = conn.calls.lock();
        if conn.is_work_cancelled() {
            conn.core.stream_retire(id);
            drop(calls);
            return Err(closed());
        }
        calls
            .insert_open(
                id,
                ServerCall {
                    _operation: Some(operation_permit),
                    stream: CallMutex::new(stream),
                    pending_admission: CallMutex::new(Some(pending)),
                    ..ServerCall::default()
                },
            )
            .expect("admitted OPEN follows the advanced high-water mark");
        Ok(subscription_options.is_some())
    })();
    let subscription = match result {
        Ok(subscription) => subscription,
        Err(error) => {
            if let Some(value) = value {
                release_request_storage(conn, value.storage);
            }
            queue_open_refusal(writer, id, &error).await;
            return;
        }
    };
    // Reserve and copy the first frame outside the dispatcher fence. Only
    // this reader can receive a continuation, and the worker starts after
    // the staged input is installed below.
    let input = match value
        .map(|value| stage_input(server, conn, value))
        .transpose()
    {
        Ok(input) => input,
        Err(error) => {
            refuse(writer, id, &error);
            return;
        }
    };
    let start_worker = {
        let calls = conn.calls.lock();
        let Some(call) = calls.get(id) else { return };
        let mut pending = call.data.pending_admission.lock();
        pending.as_mut().expect("runnable pending admission").input = input;
        subscription || conn.admission_queue.lock().push(id)
    };
    if start_worker {
        let (server, conn, writer) = (server.clone(), conn.clone(), writer.clone());
        tokio::spawn(async move {
            if subscription {
                admit_one(&server, &conn, &writer, id).await;
            } else {
                drive_admissions(server, conn, writer).await;
            }
        });
    }
}

async fn drive_admissions(
    server: Arc<Server>,
    conn: Arc<Connection>,
    writer: Arc<ConnectionWriter>,
) {
    loop {
        let id = {
            let mut queue = conn.admission_queue.lock();
            let Some(id) = queue.pop() else {
                queue.running = false;
                return;
            };
            id
        };
        admit_one(&server, &conn, &writer, id).await;
    }
}

async fn admit_one(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    writer: &Arc<ConnectionWriter>,
    id: u64,
) {
    let (request, mut retired) = {
        let calls = conn.calls.lock();
        let Some(call) = calls.get(id) else { return };
        let pending = call.data.pending_admission.lock();
        let Some(pending) = pending.as_ref() else {
            return;
        };
        (pending.request.clone(), pending.retired.subscribe())
    };
    let permit = tokio::select! {
        biased;
        _ = retired.changed() => return,
        permit = server.admission_permits.clone().acquire_owned() => {
            permit.expect("host admission semaphore remains open")
        }
    };
    let guard = {
        let calls = conn.calls.lock();
        if calls.get(id).is_none() {
            return;
        }
        ActiveAdmissionGuard::new(server)
    };
    let mut extension = match request.clone().evaluate(guard, permit).await {
        Ok(extension) => extension,
        Err(error) => {
            refuse(writer, id, &error);
            return;
        }
    };
    if matches!(
        request.operation,
        AdmissionOperation::Subscribe | AdmissionOperation::Recover
    ) {
        drop(extension.take());
    }
    let committed = commit(server, conn, id, &request, &mut extension);
    drop(extension);
    match committed {
        Ok(Some(Committed::Whole(input, admission))) => {
            let (server, conn, writer) = (server.clone(), conn.clone(), writer.clone());
            tokio::spawn(async move {
                dispatch_complete_call(&server, &conn, &writer, id, input, admission).await;
            });
        }
        Ok(Some(Committed::Chunks)) => {}
        Ok(Some(Committed::Recover(resolution, flight))) => {
            resolve_waiter_on_connection(conn, writer, id, resolution, Some(flight));
        }
        Ok(Some(Committed::Subscribe(attach, flight))) => {
            let (conn, writer, budget) =
                (conn.clone(), writer.clone(), server.domain_budget.clone());
            tokio::spawn(async move {
                super::subscribe::drive_subscription(conn, writer, id, attach, flight, budget)
                    .await;
            });
        }
        Ok(None) => {}
        Err(error) => refuse(writer, id, &error),
    }
}

enum Committed {
    Whole(OwnedRequestInput, ChunkRouteAdmission),
    Chunks,
    Recover(WaiterResolution, FlightGuard),
    Subscribe(RemoteTopicAttach, FlightGuard),
}

fn commit(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    id: u64,
    request: &HookRequest,
    extension: &mut Option<Extension>,
) -> Result<Option<Committed>, C2Error> {
    let dispatcher = server.dispatcher.read();
    validate_registration(&dispatcher, &request.route)?;
    let calls = conn.calls.lock();
    let Some(call) = calls.get(id) else {
        return Ok(None);
    };
    let mut pending = call.data.pending_admission.lock();
    let Some(open) = pending.as_mut() else {
        return Ok(None);
    };
    validate_generation(server, open.generation)?;
    if conn.is_work_cancelled()
        || call
            .data
            .stream
            .lock()
            .as_ref()
            .is_some_and(|stream| stream.cancellation.is_some())
    {
        return Ok(None);
    }
    if let Some(options) = open.subscription_options {
        let attach = request
            .channel
            .as_ref()
            .expect("subscription channel")
            .attach_remote(options.into())?;
        let mut stream = call.data.stream.lock();
        let stream = stream.as_mut().expect("subscription stream control");
        if stream.cancellation.is_some() {
            return Ok(None);
        }
        stream.topic_channel = Some(attach.channel.clone());
        stream.topic_route = Some(request.route.clone());
        assert!(
            call.queue() && call.enter_resource(),
            "fresh admitted subscription"
        );
        let flight = pending.take().expect("pending admission").flight;
        return Ok(Some(Committed::Subscribe(attach, flight)));
    }

    let method = request.method.expect("method admission");
    if request.operation == AdmissionOperation::Recover {
        let route_uid = Arc::from(request.route.route_uid.as_str());
        let (resolution, ()) = server.outcome_store.recover_with(
            &route_uid,
            request.call_id.as_deref().expect("recovery call ID"),
            method,
            || Ok(()),
        )?;
        call.queue();
        let flight = pending.take().expect("pending admission").flight;
        return Ok(Some(Committed::Recover(resolution, flight)));
    }

    // Prepare an empty registry entry before reserving the business ticket, so
    // failure leaves the pending frame owners available to the terminal path.
    let chunk_request = if matches!(open.input, Some(StagedInput::Chunked { .. })) {
        let request = server.chunk_registry.begin_frame(conn.conn_id(), id);
        server
            .chunk_registry
            .prepare_request(&request, ())
            .map_err(|(error, _)| protocol_error(error.to_string()))?;
        Some(request)
    } else {
        None
    };
    let admitted = admit_execution(server, request, method, open.generation);
    let (pending_permit, keyed) = match admitted {
        Ok(admitted) => admitted,
        Err(error) => {
            if let Some(request) = &chunk_request {
                server.chunk_registry.settle(request);
            }
            return Err(error);
        }
    };
    let open = pending.take().expect("pending admission");
    let context = keyed
        .as_ref()
        .is_none_or(|key| key.role == KeyedRole::Original)
        .then(|| {
            rpc_call_context(
                &request.route,
                method,
                request.call_id.clone(),
                request.metadata.clone(),
                extension.take(),
            )
        });
    let admission = ChunkRouteAdmission {
        route: request.route.clone(),
        method_idx: method,
        context,
        keyed,
        pending_permit,
        _flight: Some(open.flight),
    };
    match open.input.expect("execution has input") {
        StagedInput::Whole(input) => {
            call.queue();
            Ok(Some(Committed::Whole(input, admission)))
        }
        StagedInput::Chunked {
            receive, frames, ..
        } => {
            *call.data.admission.lock() = Some(admission);
            *call.data.input.lock() = Some(ServerInput {
                receive,
                queue_key: (conn.conn_id(), id),
                scheduler: server.copy_scheduler.clone(),
                request: chunk_request.expect("chunk request prepared"),
                registry: server.chunk_registry.clone(),
                copy: CallMutex::new(PendingCopyQueue {
                    queue: frames,
                    published: false,
                    readied: true,
                    finished: false,
                }),
            });
            server.copy_scheduler.push_back(conn, id);
            Ok(Some(Committed::Chunks))
        }
    }
}

fn admit_execution(
    server: &Server,
    request: &HookRequest,
    method: u16,
    generation: u64,
) -> Result<(Option<SchedulerPendingPermit>, Option<KeyedCall>), C2Error> {
    let reserve = || {
        request
            .route
            .scheduler
            .reserve_pending_in_generation(method, generation)
    };
    let admission_error = |error| route_admission_error(RouteAdmissionError::Acquire(error));
    let Some(call_id) = &request.call_id else {
        return Ok((Some(reserve().map_err(admission_error)?), None));
    };
    let route_uid: Arc<str> = Arc::from(request.route.route_uid.as_str());
    let call_id: Arc<str> = Arc::from(call_id.as_str());
    let (outcome, pending_permit) = server
        .outcome_store
        .admit_with(&route_uid, &call_id, method, reserve)
        .map_err(admission_error)?;
    let (generation, role) = match outcome {
        AdmitOutcome::MethodConflict => return Err(conflict_error()),
        AdmitOutcome::Hit { generation } => (generation, KeyedRole::Duplicate),
        AdmitOutcome::Miss { generation } => (generation, KeyedRole::Original),
    };
    Ok((
        pending_permit,
        Some(KeyedCall {
            store: server.outcome_store.clone(),
            route_uid,
            call_id,
            generation,
            role,
        }),
    ))
}

fn refuse(writer: &Arc<ConnectionWriter>, id: u64, error: &C2Error) {
    if let Some(claim) = claim_terminal(writer, id, CallStatus::NotExecuted) {
        write_refusal(writer, id, claim, error);
    }
}

pub(super) fn write_refusal(
    writer: &Arc<ConnectionWriter>,
    id: u64,
    claim: TerminalClaim,
    error: &C2Error,
) {
    let bytes = rpc::bounded_call_end_error_bytes(error);
    let writer = Arc::clone(writer);
    // Keep the accepted operation's slot and flight through its terminal
    // write. A stalled peer cannot hold the admission FIFO or accumulate
    // more refusal tasks than the admitted operation budget.
    tokio::spawn(async move {
        let _claim = claim;
        write_error_record(&writer, id, CallStatus::NotExecuted, &bytes, false).await;
    });
}

pub(super) fn expired(call: &CallRecord<ServerCall>, now: Instant) -> bool {
    call.data
        .pending_admission
        .lock()
        .as_ref()
        .is_some_and(|pending| pending.expired(now))
}

pub(super) enum ControlOperation {
    Query {
        identity: RouteCallIdentity,
        call_id: String,
    },
    AdmitTopic {
        subscription_id: u64,
    },
}

impl ControlOperation {
    fn encode_error(&self, id: u64, error: C2Error) -> Result<Vec<u8>, rpc::EncodeError> {
        match self {
            Self::Query { .. } => {
                rpc::encode_query_call_result_frame(id, &rpc::WireQueryCallResult::Error(error))
            }
            Self::AdmitTopic { .. } => {
                rpc::encode_admit_topic_result_frame(id, &rpc::WireAdmitTopicResult::Error(error))
            }
        }
    }

    fn snapshot(
        &self,
        dispatcher: &Dispatcher,
        conn: &Connection,
    ) -> Result<(Arc<CrmRoute>, Option<Arc<TopicChannel>>, u64), C2Error> {
        match self {
            Self::Query { identity, .. } => {
                let route =
                    resolve_route_contract(dispatcher, identity).map_err(route_admission_error)?;
                validate_live_route(&route).map_err(route_admission_error)?;
                Ok((route, None, 0))
            }
            Self::AdmitTopic { subscription_id } => {
                let (route, channel, head) =
                    subscription_snapshot(dispatcher, conn, *subscription_id)?;
                Ok((route, Some(channel), head))
            }
        }
    }

    fn encode_result(
        &self,
        server: &Server,
        id: u64,
        route: &CrmRoute,
        head_seq: u64,
    ) -> Result<Vec<u8>, rpc::EncodeError> {
        match self {
            Self::Query { call_id, .. } => {
                let route_uid: Arc<str> = Arc::from(route.route_uid.as_str());
                let outcome = server.outcome_store.query(&route_uid, call_id, |method| {
                    route.method_names.get(method as usize).cloned()
                });
                rpc::encode_query_call_result_frame(id, &rpc::WireQueryCallResult::Outcome(outcome))
            }
            Self::AdmitTopic { .. } => rpc::encode_admit_topic_result_frame(
                id,
                &rpc::WireAdmitTopicResult::Allowed { head_seq },
            ),
        }
    }
}

// Callers always hold dispatcher.read(). Unregistration and shutdown use
// dispatcher.write() before inspecting the same connection call records.
fn subscription_snapshot(
    dispatcher: &Dispatcher,
    conn: &Connection,
    id: u64,
) -> Result<(Arc<CrmRoute>, Arc<TopicChannel>, u64), C2Error> {
    let calls = conn.calls.lock();
    if conn.is_work_cancelled() {
        return Err(closed());
    }
    let call = calls.get(id).ok_or_else(closed)?;
    let stream = call.data.stream.lock();
    let stream = stream
        .as_ref()
        .filter(|stream| stream.cancellation.is_none() && !stream.sync_reply_failed)
        .ok_or_else(closed)?;
    let route = stream.topic_route.as_ref().ok_or_else(closed)?;
    let channel = stream.topic_channel.as_ref().ok_or_else(closed)?;
    validate_registration(dispatcher, route).map_err(|_| closed())?;
    let registered = route
        .topics
        .as_ref()
        .ok_or_else(closed)?
        .open_channel(channel.topic_name())
        .map_err(|_| closed())?;
    if !Arc::ptr_eq(registered, channel) {
        return Err(closed());
    }
    let head = channel.head_seq_if_open().ok_or_else(closed)?;
    Ok((route.clone(), channel.clone(), head))
}

pub(super) async fn reply(
    writer: &Arc<ConnectionWriter>,
    id: u64,
    frame: Result<Vec<u8>, rpc::EncodeError>,
    permit: Option<OwnedSemaphorePermit>,
) {
    let frame = match frame {
        Ok(frame) => frame,
        Err(error) => {
            warn!(request_id=id, %error, "failed to encode control reply");
            return;
        }
    };
    let queued = match permit {
        Some(permit) => writer.queue_query_reply(frame, permit),
        None => writer.queue_query_overflow(frame).await,
    };
    if let Err(error) = queued {
        warn!(request_id=id, %error, "failed to queue control reply");
    }
}

pub(super) async fn control(
    server: &Arc<Server>,
    conn: &Arc<Connection>,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    operation: ControlOperation,
    metadata: CallMetadata,
) {
    let capacity = || {
        C2Error::new(
            ErrorCode::CallCapacityExceeded,
            "control admission capacity exhausted",
        )
    };
    let Ok(control_permit) = server.query_replies.clone().try_acquire_owned() else {
        reply(writer, id, operation.encode_error(id, capacity()), None).await;
        return;
    };
    if !server.has_admission_hook() {
        let frame = {
            let dispatcher = server.dispatcher.read();
            match operation.snapshot(&dispatcher, conn) {
                Ok((route, _, head)) => operation.encode_result(server, id, &route, head),
                Err(error) => operation.encode_error(id, error),
            }
        };
        reply(writer, id, frame, Some(control_permit)).await;
        return;
    }
    let Ok(admission_permit) = server.admission_permits.clone().try_acquire_owned() else {
        reply(
            writer,
            id,
            operation.encode_error(id, capacity()),
            Some(control_permit),
        )
        .await;
        return;
    };
    let pinned = {
        let dispatcher = server.dispatcher.read();
        if conn.is_work_cancelled() || server.admission.lock().is_closed() {
            Err(closed())
        } else {
            operation
                .snapshot(&dispatcher, conn)
                .map(|(route, channel, _)| {
                    (
                        route,
                        channel,
                        FlightGuard::new(conn),
                        ActiveAdmissionGuard::new(server),
                    )
                })
        }
    };
    let (route, channel, flight, guard) = match pinned {
        Ok(pinned) => pinned,
        Err(error) => {
            drop(admission_permit);
            reply(
                writer,
                id,
                operation.encode_error(id, error),
                Some(control_permit),
            )
            .await;
            return;
        }
    };
    let (server, conn, writer) = (server.clone(), conn.clone(), writer.clone());
    tokio::spawn(async move {
        let _flight = flight;
        let (kind, call_id) = match &operation {
            ControlOperation::Query { call_id, .. } => {
                (AdmissionOperation::QueryOutcome, Some(call_id.clone()))
            }
            ControlOperation::AdmitTopic { .. } => (AdmissionOperation::Subscribe, None),
        };
        let request = Arc::new(HookRequest {
            route,
            method: None,
            channel,
            operation: kind,
            call_id,
            metadata,
        });
        let verdict = request.clone().evaluate(guard, admission_permit).await;
        let frame = match verdict {
            Err(error) => operation.encode_error(id, error),
            Ok(extension) => {
                drop(extension);
                let dispatcher = server.dispatcher.read();
                let result = validate_registration(&dispatcher, &request.route).and_then(|()| {
                    match &operation {
                        ControlOperation::Query { .. } => Ok(0),
                        ControlOperation::AdmitTopic { subscription_id } => {
                            let (route, channel, head) =
                                subscription_snapshot(&dispatcher, &conn, *subscription_id)?;
                            if Arc::ptr_eq(&route, &request.route)
                                && request
                                    .channel
                                    .as_ref()
                                    .is_some_and(|pinned| Arc::ptr_eq(pinned, &channel))
                            {
                                Ok(head)
                            } else {
                                Err(closed())
                            }
                        }
                    }
                });
                match result {
                    Ok(head) => operation.encode_result(&server, id, &request.route, head),
                    Err(error) => operation.encode_error(id, error),
                }
            }
        };
        reply(&writer, id, frame, Some(control_permit)).await;
    });
}
