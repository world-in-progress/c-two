//! Server-stream resource driver. One async task owns the stream and its call
//! slot; only individual resource steps and cleanup enter blocking workers.

use super::*;
use crate::scheduler::{SchedulerCallSlot, SchedulerGuard};
use c2_mem::retention::{BudgetedBuffer, PayloadBufferReservation};

enum StepStop {
    Cancelled,
    PeerEnded,
    Acquire(SchedulerAcquireError),
}

enum FirstError {
    Cancelled,
    Input(String),
    Acquire(SchedulerAcquireError),
    Crm(CrmError),
}

enum StepResult {
    Advanced(Result<Option<ResponseMeta>, CrmError>),
    Cancelled,
    Acquire(SchedulerAcquireError),
}

enum ItemSendError {
    Cancelled,
    PeerEnded,
    Output(String),
    Capacity(String),
    Transport(String),
}

async fn reserve_item_copy(
    server: &Server,
    conn: &Connection,
    id: u64,
    bytes: u64,
) -> Result<c2_mem::retention::PayloadBufferReservation, ItemSendError> {
    tokio::select! {
        biased;
        stop = wait_cancelled(conn, id) => match stop {
            StepStop::Cancelled => Err(ItemSendError::Cancelled),
            StepStop::PeerEnded => Err(ItemSendError::PeerEnded),
            StepStop::Acquire(_) => unreachable!(),
        },
        result = server.stream_payload_budget.reserve(bytes) => result.map_err(|error| ItemSendError::Capacity(error.to_string())),
    }
}

fn cancelled(conn: &Connection, id: u64) -> StepStop {
    if conn.is_work_cancelled() {
        StepStop::PeerEnded
    } else {
        match conn.core.stream_state(id) {
            Some(state) if state.cancelled => StepStop::Cancelled,
            Some(_) => StepStop::Acquire(SchedulerAcquireError::Unavailable),
            None => StepStop::PeerEnded,
        }
    }
}

async fn wait_cancelled(conn: &Connection, id: u64) -> StepStop {
    loop {
        let notified = conn.writer_wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        match cancelled(conn, id) {
            StepStop::Acquire(_) => notified.await,
            reason => return reason,
        }
    }
}

async fn acquire_step(
    server: &Server,
    conn: &Connection,
    id: u64,
    pending: SchedulerPendingPermit,
    slot: Option<&SchedulerCallSlot>,
    interruptible: bool,
) -> Result<(SchedulerGuard, SchedulerGuard), StepStop> {
    let route_guard = if interruptible {
        tokio::select! {
            result = pending.async_activate() => result.map_err(StepStop::Acquire)?,
            stop = wait_cancelled(conn, id) => return Err(stop),
        }
    } else {
        pending.async_activate().await.map_err(StepStop::Acquire)?
    };
    let global = async {
        match slot {
            Some(slot) => {
                server
                    .execution_scheduler
                    .async_acquire_continuation(0, slot)
                    .await
            }
            None => {
                server
                    .execution_scheduler
                    .async_acquire_accepted(0, route_guard.admission_generation())
                    .await
            }
        }
    };
    let execution_guard = if interruptible {
        tokio::select! {
            result = global => result.map_err(StepStop::Acquire)?,
            error = route_guard.wait_rejected() => return Err(StepStop::Acquire(error)),
            stop = wait_cancelled(conn, id) => return Err(stop),
        }
    } else {
        global.await.map_err(StepStop::Acquire)?
    };
    Ok((route_guard, execution_guard))
}

fn crm_error_bytes(error: CrmError) -> Vec<u8> {
    match error {
        CrmError::UserError(bytes) => {
            if matches!(C2Error::from_wire_bytes(&bytes), Ok(Some(_))) {
                bytes
            } else {
                error_wire(
                    ErrorCode::ResourceFunctionExecuting,
                    "invalid CRM error payload",
                )
            }
        }
        CrmError::InternalError(message) => {
            error_wire(ErrorCode::ResourceFunctionExecuting, message)
        }
    }
}

fn acquire_error_bytes(error: SchedulerAcquireError) -> Vec<u8> {
    match error {
        SchedulerAcquireError::Closed => error_wire(ErrorCode::ResourceClosed, "route closed"),
        SchedulerAcquireError::Unavailable => {
            error_wire(ErrorCode::ResourceUnavailable, "host admission unavailable")
        }
        SchedulerAcquireError::Capacity { field, limit } => error_wire(
            ErrorCode::ResourceUnavailable,
            format!("route concurrency capacity exceeded: {field}={limit}"),
        ),
    }
}

fn cancellation_bytes() -> Vec<u8> {
    error_wire(ErrorCode::CallCancelled, "server stream cancelled")
}

async fn write_terminal(
    writer: &Arc<ConnectionWriter>,
    id: u64,
    status: CallStatus,
    complete_items: u64,
    error: &[u8],
) {
    if claim_terminal(writer, id, status).is_none() {
        return;
    }
    let value = if status == CallStatus::Success {
        &[][..]
    } else {
        error
    };
    let frame = match rpc::encode_call_end_frame(
        id,
        CallMode::ServerStream,
        status,
        complete_items,
        ValuePart::whole(ValueStorage::Inline(value)),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            warn!(request_id=id, %error, "invalid stream terminal payload");
            return;
        }
    };
    if let Err(error) = writer.write_call_end(&frame).await {
        warn!(request_id=id, %error, "stream CALL_END write failed");
    }
}

async fn wait_demand(conn: &Connection, id: u64) -> Result<(), StepStop> {
    loop {
        let notified = conn.writer_wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if conn.is_work_cancelled() {
            return Err(StepStop::PeerEnded);
        }
        let Some(state) = conn.core.stream_state(id) else {
            return Err(StepStop::PeerEnded);
        };
        if state.cancelled {
            return Err(StepStop::Cancelled);
        }
        if state.demand > 0 {
            return Ok(());
        }
        notified.await;
    }
}

async fn wait_storage(conn: &Connection, id: u64) -> Result<u64, ItemSendError> {
    loop {
        let notified = conn.writer_wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if conn.is_work_cancelled() {
            return Err(ItemSendError::PeerEnded);
        }
        let Some(state) = conn.core.stream_state(id) else {
            return Err(ItemSendError::PeerEnded);
        };
        if state.cancelled {
            return Err(ItemSendError::Cancelled);
        }
        if state.storage_bytes > 0 {
            return Ok(state.storage_bytes);
        }
        notified.await;
    }
}

fn item_len(meta: &ResponseMeta) -> u64 {
    match meta {
        ResponseMeta::Inline(bytes) => bytes.len() as u64,
        ResponseMeta::ShmAlloc(prepared) => u64::from(prepared.data_size()),
        ResponseMeta::Empty => 0,
    }
}

fn budgeted_owner(
    data: Vec<u8>,
    mut charge: PayloadBufferReservation,
) -> Result<Arc<BudgetedBuffer>, ItemSendError> {
    let capacity = u64::try_from(data.capacity())
        .map_err(|_| ItemSendError::Capacity("item capacity is not representable".into()))?;
    if capacity > charge.bytes() {
        charge
            .try_grow(capacity - charge.bytes())
            .map_err(|error| ItemSendError::Capacity(error.to_string()))?;
    }
    Ok(Arc::new(BudgetedBuffer::new(data, charge)))
}

async fn send_item(
    server: &Server,
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    mut meta: ResponseMeta,
) -> Result<(), ItemSendError> {
    let len = item_len(&meta);
    if len > server.config.max_payload_size {
        return Err(ItemSendError::Output(format!(
            "stream item size {len} exceeds max_payload_size {}",
            server.config.max_payload_size
        )));
    }
    if conn.is_work_cancelled() {
        return Err(ItemSendError::PeerEnded);
    }
    if conn
        .core
        .stream_state(id)
        .is_some_and(|state| state.cancelled)
    {
        return Err(ItemSendError::Cancelled);
    }
    // A callback-owned Vec becomes the Host's transport copy without another
    // allocation. Its B charge remains until the final frame or cancellation.
    let mut data_charge = if let ResponseMeta::Inline(data) = &meta {
        let capacity = u64::try_from(data.capacity())
            .map_err(|_| ItemSendError::Capacity("item capacity is not representable".into()))?;
        Some(reserve_item_copy(server, conn, id, capacity).await?)
    } else {
        None
    };
    let prepared = match &meta {
        ResponseMeta::Inline(data)
            if conn.shm_response_capable()
                && !conn.is_deactivated()
                && len > server.config.shm_threshold =>
        {
            try_prepare_shm_response(
                &server.response_pool,
                server.config.shm_threshold,
                data.len(),
                |dst| {
                    dst.copy_from_slice(data);
                    Ok(())
                },
            )
            .map_err(ItemSendError::Output)?
        }
        _ => None,
    };
    if let Some(prepared) = prepared {
        meta = ResponseMeta::ShmAlloc(prepared);
        data_charge = None;
    }
    conn.core
        .stream_begin_item(id, len)
        .map_err(|error| ItemSendError::Output(error.to_string()))?;
    let begin = rpc::encode_item_begin_frame(id, len)
        .map_err(|error| ItemSendError::Output(error.to_string()))?;
    writer
        .write_call_end(&begin)
        .await
        .map_err(|error| ItemSendError::Transport(error.to_string()))?;
    if len == 0 {
        return Ok(());
    }
    let (data, charge) = match meta {
        ResponseMeta::Inline(data) => (data, data_charge.take().expect("inline item charge")),
        ResponseMeta::Empty => unreachable!("positive item length"),
        ResponseMeta::ShmAlloc(prepared) if conn.shm_response_capable() => {
            wait_storage(conn, id).await?;
            if conn
                .core
                .stream_state(id)
                .is_some_and(|state| state.storage_bytes >= len)
            {
                match writer
                    .write_item_prepared(id, prepared)
                    .await
                    .map_err(|error| ItemSendError::Transport(error.to_string()))?
                {
                    None => {
                        conn.core
                            .stream_data(id, len)
                            .map_err(|error| ItemSendError::Output(error.to_string()))?;
                        conn.core
                            .stream_finish_item(id)
                            .map_err(|error| ItemSendError::Output(error.to_string()))?;
                        return Ok(());
                    }
                    Some(prepared) => {
                        let charge = reserve_item_copy(server, conn, id, len).await?;
                        (
                            prepared
                                .copy_logical_bytes()
                                .map_err(ItemSendError::Output)?,
                            charge,
                        )
                    }
                }
            } else {
                let charge = reserve_item_copy(server, conn, id, len).await?;
                (
                    prepared
                        .copy_logical_bytes()
                        .map_err(ItemSendError::Output)?,
                    charge,
                )
            }
        }
        ResponseMeta::ShmAlloc(prepared) => {
            let charge = reserve_item_copy(server, conn, id, len).await?;
            (
                prepared
                    .copy_logical_bytes()
                    .map_err(ItemSendError::Output)?,
                charge,
            )
        }
    };
    let owner = budgeted_owner(data, charge)?;
    let frame_limit =
        usize::try_from(server.config.max_frame_size.saturating_sub(13)).unwrap_or(usize::MAX);
    let chunk_size = (server.config.chunk_size as usize).min(frame_limit).max(1);
    let mut offset = 0;
    while offset < owner.len() {
        // A complete-value receiver grants the whole item; a forwarding hop
        // may grant a smaller rolling window. Each submission is one frame.
        let available = wait_storage(conn, id).await?;
        let width = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(chunk_size)
            .min(owner.len() - offset);
        conn.core
            .stream_data(id, width as u64)
            .map_err(|error| ItemSendError::Output(error.to_string()))?;
        writer
            .write_item_slice(id, owner.clone(), offset..offset + width)
            .await
            .map_err(|error| ItemSendError::Transport(error.to_string()))?;
        offset += width;
    }
    conn.core
        .stream_finish_item(id)
        .map_err(|error| ItemSendError::Output(error.to_string()))?;
    Ok(())
}

pub(super) async fn dispatch_stream_call(
    server: &Server,
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    request: RequestData,
    admission: ChunkRouteAdmission,
) {
    let ChunkRouteAdmission {
        route,
        method_idx,
        mut pending_permit,
        _pending_permit,
        _flight,
    } = admission;
    let _call_owners = (_pending_permit, _flight);
    let slot = pending_permit.retain_call_slot();
    if conn.is_work_cancelled() {
        return;
    }
    match conn.core.stream_start_step(id) {
        Ok(true) => {}
        Ok(false) => {
            queue_rejection(writer, id, &cancellation_bytes()).await;
            return;
        }
        Err(_) => return,
    }
    let (route_guard, execution_guard) =
        match acquire_step(server, conn, id, pending_permit, None, true).await {
            Ok(guards) => guards,
            Err(StepStop::Cancelled) => {
                queue_rejection(writer, id, &cancellation_bytes()).await;
                return;
            }
            Err(StepStop::PeerEnded) => return,
            Err(StepStop::Acquire(error)) => {
                queue_rejection(writer, id, &acquire_error_bytes(error)).await;
                return;
            }
        };
    let callback = route
        .stream_callback
        .as_ref()
        .expect("validated stream route")
        .clone();
    let route_name = route.name.clone();
    let response_pool = server.response_pool.clone();
    let Some(record) = conn.calls.lock().get(id) else {
        return;
    };
    let core = conn.core.clone();
    let execution_conn = writer.conn.clone();
    let first_slot = slot.clone();
    let first = tokio::task::spawn_blocking(move || {
        let mut route_guard = route_guard;
        let _execution_guard = execution_guard;
        route_guard.commit().map_err(FirstError::Acquire)?;
        let bytes = request.into_owned_bytes().map_err(FirstError::Input)?;
        let _start = record.data.start_gate.lock();
        if execution_conn.is_work_cancelled()
            || core.stream_state(id).is_none_or(|state| state.cancelled)
        {
            return Err(FirstError::Cancelled);
        }
        if !record.enter_resource() {
            return Err(FirstError::Cancelled);
        }
        first_slot.mark_resource_entry();
        drop(_start);
        let mut stream = callback
            .open_stream(&route_name, method_idx, bytes, response_pool)
            .map_err(FirstError::Crm)?;
        let result = stream.step();
        Ok((stream, result))
    })
    .await
    .expect("stream initial worker panicked");
    let (mut stream, mut step) = match first {
        Ok(value) => value,
        Err(FirstError::Cancelled) => {
            if !conn.is_work_cancelled() {
                queue_rejection(writer, id, &cancellation_bytes()).await;
            }
            return;
        }
        Err(FirstError::Input(message)) => {
            queue_rejection(
                writer,
                id,
                &error_wire(ErrorCode::ResourceInputDeserializing, message),
            )
            .await;
            return;
        }
        Err(FirstError::Acquire(error)) => {
            queue_rejection(writer, id, &acquire_error_bytes(error)).await;
            return;
        }
        Err(FirstError::Crm(error)) => {
            write_terminal(writer, id, CallStatus::Failed, 0, &crm_error_bytes(error)).await;
            return;
        }
    };
    let mut complete_items = 0;
    let terminal = loop {
        if conn.is_work_cancelled() {
            break None;
        }
        // Cancellation intent never replaces a real error from the running step.
        if step.is_ok()
            && conn
                .core
                .stream_state(id)
                .is_some_and(|state| state.cancelled)
        {
            break Some((CallStatus::Failed, cancellation_bytes()));
        }
        match step {
            Ok(Some(meta)) => match send_item(server, conn, writer, id, meta).await {
                Ok(()) => complete_items += 1,
                Err(ItemSendError::Cancelled) => {
                    break Some((CallStatus::Failed, cancellation_bytes()));
                }
                Err(ItemSendError::PeerEnded) => break None,
                Err(ItemSendError::Output(message)) => {
                    break Some((
                        CallStatus::Failed,
                        error_wire(ErrorCode::ResourceOutputSerializing, message),
                    ));
                }
                Err(ItemSendError::Capacity(message)) => {
                    break Some((
                        CallStatus::Failed,
                        error_wire(ErrorCode::CallCapacityExceeded, message),
                    ));
                }
                Err(ItemSendError::Transport(message)) => {
                    warn!(request_id=id, %message, "stream item write failed");
                    break None;
                }
            },
            Ok(None) => break Some((CallStatus::Success, Vec::new())),
            Err(error) => break Some((CallStatus::Failed, crm_error_bytes(error))),
        }
        match wait_demand(conn, id).await {
            Ok(()) => {}
            Err(StepStop::Cancelled) => break Some((CallStatus::Failed, cancellation_bytes())),
            Err(StepStop::PeerEnded) => break None,
            Err(StepStop::Acquire(_)) => unreachable!(),
        }
        match conn.core.stream_start_step(id) {
            Ok(true) => {}
            Ok(false) => break Some((CallStatus::Failed, cancellation_bytes())),
            Err(_) => break None,
        }
        let pending = match slot.reserve_step() {
            Ok(pending) => pending,
            Err(error) => break Some((CallStatus::Failed, acquire_error_bytes(error))),
        };
        let (route_guard, execution_guard) =
            match acquire_step(server, conn, id, pending, Some(&slot), true).await {
                Ok(guards) => guards,
                Err(StepStop::Cancelled) => break Some((CallStatus::Failed, cancellation_bytes())),
                Err(StepStop::PeerEnded) => break None,
                Err(StepStop::Acquire(error)) => {
                    break Some((CallStatus::Failed, acquire_error_bytes(error)));
                }
            };
        let core = conn.core.clone();
        let execution_conn = writer.conn.clone();
        let Some(record) = conn.calls.lock().get(id) else {
            break None;
        };
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let mut route_guard = route_guard;
            let _execution_guard = execution_guard;
            if let Err(error) = route_guard.commit() {
                return (stream, StepResult::Acquire(error));
            }
            let _start = record.data.start_gate.lock();
            if execution_conn.is_work_cancelled()
                || core.stream_state(id).is_none_or(|state| state.cancelled)
            {
                return (stream, StepResult::Cancelled);
            }
            drop(_start);
            let result = stream.step();
            (stream, StepResult::Advanced(result))
        })
        .await
        .expect("stream step worker panicked");
        stream = returned;
        step = match result {
            StepResult::Advanced(result) => result,
            StepResult::Cancelled => break Some((CallStatus::Failed, cancellation_bytes())),
            StepResult::Acquire(error) => {
                break Some((CallStatus::Failed, acquire_error_bytes(error)));
            }
        };
    };
    // Cleanup is a resource operation. It gets its own ticket at the route
    // tail, without needing demand, storage credit or another call slot.
    let cleanup = match slot.reserve_step() {
        Ok(pending) => match acquire_step(server, conn, id, pending, Some(&slot), false).await {
            Ok((route_guard, execution_guard)) => tokio::task::spawn_blocking(move || {
                let mut route_guard = route_guard;
                let _execution_guard = execution_guard;
                route_guard
                    .commit()
                    .map_err(|error| CrmError::InternalError(error.to_string()))?;
                stream.finish()
            })
            .await
            .expect("stream cleanup worker panicked"),
            Err(StepStop::Acquire(error)) => Err(CrmError::InternalError(error.to_string())),
            Err(_) => unreachable!(),
        },
        Err(error) => Err(CrmError::InternalError(error.to_string())),
    };
    let terminal = terminal.map(|outcome| {
        if let Err(error) = cleanup {
            (CallStatus::Failed, crm_error_bytes(error))
        } else {
            outcome
        }
    });
    if conn.is_work_cancelled() || terminal.is_none() {
        // A broken or peer-ended writer has no trustworthy terminal delivery.
        // The original write token continues through its physical flush.
        claim_terminal(writer, id, CallStatus::Failed);
        return;
    }
    let (status, error) = terminal.expect("deliverable terminal");
    write_terminal(writer, id, status, complete_items, &error).await;
}
