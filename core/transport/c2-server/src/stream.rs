//! Server-stream resource driver. One async task owns the stream and its call
//! slot; only individual resource steps and cleanup enter blocking workers.

use super::*;
use crate::dispatcher::CrmStreamItem;
use crate::scheduler::{SchedulerCallSlot, SchedulerGuard};

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
    Advanced(Result<Option<Box<dyn CrmStreamItem>>, CrmError>),
    Cancelled,
    Acquire(SchedulerAcquireError),
}

enum ItemSendError {
    Cancelled,
    PeerEnded,
    Output(String),
    Capacity(String),
    Crm(CrmError),
    Transport(String),
}

fn cancelled(conn: &Connection, id: u64) -> StepStop {
    if conn.is_work_cancelled() {
        StepStop::PeerEnded
    } else {
        match conn.core.stream_state(id) {
            Some(state) if state.cancelled || cancellation_cause(conn, id).is_some() => {
                StepStop::Cancelled
            }
            Some(_) => StepStop::Acquire(SchedulerAcquireError::Unavailable),
            None => StepStop::PeerEnded,
        }
    }
}

fn cancellation_cause(conn: &Connection, id: u64) -> Option<StreamCancelCause> {
    conn.calls.lock().get(id).and_then(|call| {
        call.data
            .stream
            .lock()
            .as_ref()
            .and_then(|stream| stream.cancellation)
    })
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

fn cancellation_bytes(conn: &Connection, id: u64) -> Vec<u8> {
    error_wire(
        ErrorCode::CallCancelled,
        cancellation_cause(conn, id)
            .unwrap_or(StreamCancelCause::Consumer)
            .message(),
    )
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
        if state.cancelled || cancellation_cause(conn, id).is_some() {
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

async fn send_item(
    server: &Server,
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    item: Box<dyn CrmStreamItem>,
) -> Result<(), ItemSendError> {
    let len = item.len() as u64;
    if len > server.config.max_payload_size {
        return Err(ItemSendError::Output(format!(
            "stream item size {len} exceeds max_payload_size {}",
            server.config.max_payload_size,
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
    // Announce only a length. Until CREDIT, only the domain item is retained;
    // neither a C-Two encoding copy nor a backing allocation exists.
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
    wait_storage(conn, id).await?;
    let use_shm = conn.shm_response_capable() && !conn.is_deactivated();
    let pool = server.response_pool.clone();
    let threshold = server.config.shm_threshold;
    let (item, prepared) = if use_shm {
        tokio::task::spawn_blocking(move || {
            let mut fill_error = None;
            let prepared = try_prepare_shm_response(&pool, threshold, item.len(), |dst| {
                item.write_range(0, dst).map_err(|error| {
                    fill_error = Some(error);
                    "stream item encoding failed".to_string()
                })
            })
            .map_err(|error| {
                ItemSendError::Crm(fill_error.unwrap_or(CrmError::InternalError(error)))
            });
            (item, prepared)
        })
        .await
        .expect("stream encoding worker panicked")
    } else {
        (item, Ok(None))
    };
    if let Some(prepared) = prepared? {
        match writer
            .write_item_prepared(id, prepared)
            .await
            .map_err(|error| ItemSendError::Transport(error.to_string()))?
        {
            None => {
                conn.core
                    .stream_shm_data(id, len)
                    .map_err(|error| ItemSendError::Output(error.to_string()))?;
                conn.core
                    .stream_finish_item(id)
                    .map_err(|error| ItemSendError::Output(error.to_string()))?;
                return Ok(());
            }
            Some(unpublished) => {
                // A deactivation raced SHM publication. End that allocation
                // before encoding the independently owned item into bytes.
                drop(unpublished);
            }
        }
    }
    // Inline owns only the current transport frame. In particular, a Runtime
    // connected to itself must not reserve a second whole item from B while
    // its receiving side already holds the complete-value reservation.
    let mut item = item;
    let frame_limit =
        usize::try_from(server.config.max_frame_size.saturating_sub(13)).unwrap_or(usize::MAX);
    let chunk_size = (server.config.chunk_size as usize).min(frame_limit).max(1);
    let mut offset = 0;
    while offset < item.len() {
        let available = wait_storage(conn, id).await?;
        let width = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(chunk_size)
            .min(item.len() - offset);
        let (returned, bytes) = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(width)
                .map_err(|error| ItemSendError::Capacity(error.to_string()))?;
            bytes.resize(width, 0);
            item.write_range(offset, &mut bytes)
                .map_err(ItemSendError::Crm)?;
            Ok((item, bytes))
        })
        .await
        .expect("stream encoding worker panicked")?;
        item = returned;
        conn.core
            .stream_data(id, width as u64)
            .map_err(|error| ItemSendError::Output(error.to_string()))?;
        writer
            .write_item(id, bytes)
            .await
            .map_err(|error| ItemSendError::Transport(error.to_string()))?;
        offset += width;
    }
    conn.core
        .stream_finish_item(id)
        .map_err(|error| ItemSendError::Output(error.to_string()))?;
    Ok(())
}

async fn finish_stream(
    server: &Server,
    conn: &Connection,
    id: u64,
    slot: &SchedulerCallSlot,
    stream: Box<dyn crate::dispatcher::CrmStream>,
) -> Result<(), CrmError> {
    let pending = slot
        .reserve_step()
        .map_err(|error| CrmError::InternalError(error.to_string()))?;
    let (route_guard, execution_guard) = acquire_step(server, conn, id, pending, Some(slot), false)
        .await
        .map_err(|error| match error {
            StepStop::Acquire(error) => CrmError::InternalError(error.to_string()),
            _ => unreachable!("cleanup is not interruptible"),
        })?;
    tokio::task::spawn_blocking(move || {
        let mut route_guard = route_guard;
        let _execution_guard = execution_guard;
        route_guard
            .commit()
            .map_err(|error| CrmError::InternalError(error.to_string()))?;
        stream.finish()
    })
    .await
    .expect("stream cleanup worker panicked")
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
    let callback = route
        .stream_callback
        .as_ref()
        .expect("validated stream route")
        .clone();
    match conn.core.stream_start_step(id) {
        Ok(true) => {}
        Ok(false) => {
            queue_rejection(writer, id, &cancellation_bytes(conn, id)).await;
            return;
        }
        Err(_) => return,
    }
    let (route_guard, execution_guard) =
        match acquire_step(server, conn, id, pending_permit, None, true).await {
            Ok(guards) => guards,
            Err(StepStop::Cancelled) => {
                queue_rejection(writer, id, &cancellation_bytes(conn, id)).await;
                return;
            }
            Err(StepStop::PeerEnded) => return,
            Err(StepStop::Acquire(error)) => {
                queue_rejection(writer, id, &acquire_error_bytes(error)).await;
                return;
            }
        };
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
            || cancellation_cause(&execution_conn, id).is_some()
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
    let (stream, mut step) = match first {
        Ok(value) => value,
        Err(FirstError::Cancelled) => {
            if !conn.is_work_cancelled() {
                queue_rejection(writer, id, &cancellation_bytes(conn, id)).await;
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
    let mut stream = Some(stream);
    let mut cleanup = None;
    let mut complete_items = 0;
    let terminal = loop {
        if conn.is_work_cancelled() {
            break None;
        }
        // Consumer cancellation relinquishes the item. Host cancellation only
        // stops future resource steps, retaining the current item's delivery.
        if step.is_ok()
            && conn
                .core
                .stream_state(id)
                .is_some_and(|state| state.cancelled)
        {
            break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
        }
        match step {
            Ok(Some(item)) => {
                let sending = send_item(server, conn, writer, id, item);
                tokio::pin!(sending);
                let sent = tokio::select! {
                    result = &mut sending => result,
                    _ = wait_cancelled(conn, id) => {
                        // Cleanup uses neither demand nor output storage. Keep
                        // polling the original send while cleanup runs, so no
                        // accepted frame or owned item is abandoned.
                        let finishing = finish_stream(server, conn, id, &slot, stream.take().expect("live stream"));
                        let (result, finished) = tokio::join!(&mut sending, finishing);
                        cleanup = Some(finished);
                        result
                    }
                };
                match sent {
                    Ok(()) => complete_items += 1,
                    Err(ItemSendError::Cancelled) => {
                        break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
                    }
                    Err(ItemSendError::PeerEnded) => break None,
                    Err(ItemSendError::Crm(error)) => {
                        break Some((CallStatus::Failed, crm_error_bytes(error)));
                    }
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
                }
                if cleanup.is_some() || cancellation_cause(conn, id).is_some() {
                    break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
                }
            }
            Ok(None) => {
                // This read fixes the natural-end decision. Cancellation
                // recorded while the step ran wins; a later intent during
                // cleanup must not rewrite an already natural end.
                break Some(if cancellation_cause(conn, id).is_some() {
                    (CallStatus::Failed, cancellation_bytes(conn, id))
                } else {
                    (CallStatus::Success, Vec::new())
                });
            }
            Err(error) => break Some((CallStatus::Failed, crm_error_bytes(error))),
        }
        match wait_demand(conn, id).await {
            Ok(()) => {}
            Err(StepStop::Cancelled) => {
                break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
            }
            Err(StepStop::PeerEnded) => break None,
            Err(StepStop::Acquire(_)) => unreachable!(),
        }
        match conn.core.stream_start_step(id) {
            Ok(true) => {}
            Ok(false) => break Some((CallStatus::Failed, cancellation_bytes(conn, id))),
            Err(_) => break None,
        }
        let pending = match slot.reserve_step() {
            Ok(pending) => pending,
            Err(error) => break Some((CallStatus::Failed, acquire_error_bytes(error))),
        };
        let (route_guard, execution_guard) =
            match acquire_step(server, conn, id, pending, Some(&slot), true).await {
                Ok(guards) => guards,
                Err(StepStop::Cancelled) => {
                    break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
                }
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
        let mut advancing = stream.take().expect("live stream");
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let mut route_guard = route_guard;
            let _execution_guard = execution_guard;
            if let Err(error) = route_guard.commit() {
                return (advancing, StepResult::Acquire(error));
            }
            let _start = record.data.start_gate.lock();
            if execution_conn.is_work_cancelled()
                || core.stream_state(id).is_none_or(|state| state.cancelled)
                || cancellation_cause(&execution_conn, id).is_some()
            {
                return (advancing, StepResult::Cancelled);
            }
            drop(_start);
            let result = advancing.step();
            (advancing, StepResult::Advanced(result))
        })
        .await
        .expect("stream step worker panicked");
        stream = Some(returned);
        step = match result {
            StepResult::Advanced(result) => result,
            StepResult::Cancelled => {
                break Some((CallStatus::Failed, cancellation_bytes(conn, id)));
            }
            StepResult::Acquire(error) => {
                break Some((CallStatus::Failed, acquire_error_bytes(error)));
            }
        };
    };
    let cleanup = match cleanup {
        Some(result) => result,
        None => finish_stream(server, conn, id, &slot, stream.take().expect("live stream")).await,
    };
    let terminal = terminal.map(|outcome| match cleanup {
        Err(error) => (CallStatus::Failed, crm_error_bytes(error)),
        Ok(()) => outcome,
    });
    if conn.is_work_cancelled() || terminal.is_none() {
        claim_terminal(writer, id, CallStatus::Failed);
        return;
    }
    let (status, error) = terminal.expect("deliverable terminal");
    write_terminal(writer, id, status, complete_items, &error).await;
}
