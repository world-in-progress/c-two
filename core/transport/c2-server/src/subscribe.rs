//! Per-subscriber IPC 18 topic driver.
//!
//! One async task owns one remote subscription without holding route
//! concurrency permits or execution worker threads. It sends `READY(start_seq, head_seq)`
//! immediately after attach, gates each new frame on both item demand (`CREDIT`)
//! and settlement of its prior SHM loan (at most one published SHM loan per
//! subscriber), delivers SHM frames via 1-to-N `SharedShmLoan` or falls back to
//! chunked inline transfer directly from the sealed `TopicFrame`, and writes a
//! terminal `CALL_END` carrying `complete_items` and the canonical error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use c2_error::{C2Error, ErrorCode};
use c2_mem::DomainBudget;
use c2_wire::connection::ConnectionError;
use c2_wire::rpc::{self, CallMode, CallStatus};
use tokio::sync::Notify;

use super::*;
use crate::topic::{RemoteSelectOutcome, RemoteTopicAttach, TopicCloseCause, TopicFrame};

struct LoanSettledSignal {
    settled: AtomicBool,
    notify: Notify,
}

impl LoanSettledSignal {
    fn new() -> Self {
        Self {
            settled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn is_settled(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }

    fn mark_settled(&self) {
        self.settled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum SubStop {
    PeerEnded,
    Terminal(C2Error),
}

enum FrameSendError {
    PeerEnded,
    Cancelled,
    Capacity(String),
    Protocol(String),
    Transport(String),
}

fn stream_cancel_to_topic_error(cause: StreamCancelCause) -> C2Error {
    match cause {
        StreamCancelCause::Consumer => TopicCloseCause::ConsumerCancelled.into_error(),
        StreamCancelCause::Unregister => TopicCloseCause::RouteUnregistered.into_error(),
        StreamCancelCause::Shutdown => TopicCloseCause::HostShutdown.into_error(),
        StreamCancelCause::RelayShutdown => TopicCloseCause::RelayShutdown.into_error(),
    }
}

fn subscription_cancellation(conn: &Connection, id: u64) -> Option<C2Error> {
    conn.calls.lock().get(id).and_then(|call| {
        call.data.stream.lock().as_ref().and_then(|stream| {
            if stream.sync_reply_failed {
                Some(C2Error::new(
                    ErrorCode::CallCapacityExceeded,
                    "subscription SYNC_ACK could not be queued",
                ))
            } else {
                stream.cancellation.map(stream_cancel_to_topic_error)
            }
        })
    })
}

pub(super) fn fail_sync_reply(conn: &Connection, id: u64) {
    // Keep retirement from removing the stream between recording the failure
    // and cancelling any storage wait. The driver sends the terminal reply
    // through the business writer queue, independent of the control backlog.
    let calls = conn.calls.lock();
    if let Some(call) = calls.get(id)
        && let Some(stream) = call.data.stream.lock().as_mut()
    {
        stream.sync_reply_failed = true;
        let _ = conn.core.stream_cancel(id);
    }
}

fn check_stop_before_select(
    conn: &Connection,
    id: u64,
    attach: &RemoteTopicAttach,
) -> Option<SubStop> {
    if conn.is_work_cancelled() {
        return Some(SubStop::PeerEnded);
    }
    let Some(state) = conn.core.stream_state(id) else {
        return Some(SubStop::PeerEnded);
    };
    if let Some(error) = subscription_cancellation(conn, id) {
        return Some(SubStop::Terminal(error));
    }
    if state.cancelled {
        return Some(SubStop::Terminal(
            TopicCloseCause::ConsumerCancelled.into_error(),
        ));
    }
    if let Some(cause) = attach.channel.close_cause() {
        return Some(SubStop::Terminal(cause.into_error()));
    }
    None
}

async fn wait_ready_for_select(
    conn: &Connection,
    id: u64,
    attach: &RemoteTopicAttach,
    last_loan: Option<&Arc<LoanSettledSignal>>,
) -> Result<(), SubStop> {
    loop {
        let writer_notified = conn.writer_wake.notified();
        let chan_notified = attach.channel.notified();
        tokio::pin!(writer_notified);
        tokio::pin!(chan_notified);
        writer_notified.as_mut().enable();
        chan_notified.as_mut().enable();

        if let Some(stop) = check_stop_before_select(conn, id, attach) {
            return Err(stop);
        }
        let has_demand = conn
            .core
            .stream_state(id)
            .is_some_and(|state| state.demand > 0);
        let loan_ready = last_loan.is_none_or(|sig| sig.is_settled());
        if has_demand && loan_ready {
            return Ok(());
        }

        if let Some(sig) = last_loan {
            let loan_notified = sig.notify.notified();
            tokio::pin!(loan_notified);
            loan_notified.as_mut().enable();
            if sig.is_settled() && has_demand {
                return Ok(());
            }
            tokio::select! {
                _ = writer_notified => {}
                _ = chan_notified => {}
                _ = loan_notified => {}
            }
        } else {
            tokio::select! {
                _ = writer_notified => {}
                _ = chan_notified => {}
            }
        }
    }
}

async fn wait_storage(conn: &Connection, id: u64) -> Result<u64, FrameSendError> {
    loop {
        let notified = conn.writer_wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if conn.is_work_cancelled() {
            return Err(FrameSendError::PeerEnded);
        }
        let Some(state) = conn.core.stream_state(id) else {
            return Err(FrameSendError::PeerEnded);
        };
        if state.cancelled {
            return Err(FrameSendError::Cancelled);
        }
        if state.storage_bytes > 0 {
            return Ok(state.storage_bytes);
        }
        notified.await;
    }
}

async fn send_topic_frame(
    conn: &Connection,
    writer: &Arc<ConnectionWriter>,
    id: u64,
    frame: &Arc<TopicFrame>,
    last_loan: &mut Option<Arc<LoanSettledSignal>>,
    budget: &DomainBudget,
) -> Result<(), FrameSendError> {
    let len = frame.len() as u64;
    if conn.is_work_cancelled() {
        return Err(FrameSendError::PeerEnded);
    }
    conn.core
        .stream_begin_item(id, len)
        .map_err(|error| match error {
            ConnectionError::SubscriptionCancelled => FrameSendError::Cancelled,
            other => FrameSendError::Protocol(other.to_string()),
        })?;
    let begin = rpc::encode_subscribe_item_begin_frame(id, frame.seq(), len)
        .map_err(|error| FrameSendError::Protocol(error.to_string()))?;
    writer
        .write_call_end(&begin)
        .await
        .map_err(|error| FrameSendError::Transport(error.to_string()))?;
    if len == 0 {
        return Ok(());
    }

    // Wait for receiver-backed storage CREDIT before publishing SHM or sending
    // inline chunks. Only peer EOF or consumer CANCEL interrupts a frame whose
    // ITEM_BEGIN has already been announced.
    wait_storage(conn, id).await?;

    if conn.shm_response_capable()
        && !conn.is_deactivated()
        && let Some(block) = frame.shm_block()
    {
        let signal = Arc::new(LoanSettledSignal::new());
        let on_settle = Arc::clone(&signal);
        let loan = block.mint_loan().with_on_settle(move || {
            on_settle.mark_settled();
        });
        match writer
            .write_item_shared_shm(id, loan)
            .await
            .map_err(|error| FrameSendError::Transport(error.to_string()))?
        {
            None => {
                *last_loan = Some(signal);
                conn.core
                    .stream_shm_data(id, len)
                    .map_err(|error| FrameSendError::Protocol(error.to_string()))?;
                conn.core
                    .stream_finish_item(id)
                    .map_err(|error| FrameSendError::Protocol(error.to_string()))?;
                return Ok(());
            }
            Some(unpublished) => {
                // Connection deactivated before publication: settle unused loan
                // and deliver the exact same sealed frame over inline chunks.
                unpublished.settle();
            }
        }
    }

    let frame_limit =
        usize::try_from(c2_config::MAX_RPC_FRAME_BYTES.saturating_sub(13)).unwrap_or(usize::MAX);
    let chunk_size = (c2_config::CHUNK_SIZE as usize).min(frame_limit).max(1);
    let mut offset = 0usize;
    while offset < frame.len() {
        let available = wait_storage(conn, id).await?;
        let width = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(chunk_size)
            .min(frame.len() - offset);
        let reservation = budget
            .try_reserve_memory(width as u64)
            .map_err(|error| FrameSendError::Capacity(error.to_string()))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(width)
            .map_err(|error| FrameSendError::Capacity(error.to_string()))?;
        bytes.resize(width, 0);
        frame
            .copy_range(offset, &mut bytes)
            .map_err(FrameSendError::Protocol)?;
        conn.core
            .stream_data(id, width as u64)
            .map_err(|error| FrameSendError::Protocol(error.to_string()))?;
        writer
            .write_item(id, bytes, Some(reservation))
            .await
            .map_err(|error| FrameSendError::Transport(error.to_string()))?;
        offset += width;
    }
    conn.core
        .stream_finish_item(id)
        .map_err(|error| FrameSendError::Protocol(error.to_string()))?;
    Ok(())
}

async fn write_subscribe_terminal(
    writer: &Arc<ConnectionWriter>,
    id: u64,
    complete_items: u64,
    error: &C2Error,
) {
    let Some(_claim) = claim_terminal(writer, id, CallStatus::Failed) else {
        return;
    };
    let frame = match rpc::encode_call_end_error_frame(
        id,
        CallMode::Subscribe,
        CallStatus::Failed,
        complete_items,
        error,
    ) {
        Ok(frame) => frame,
        Err(encode_error) => {
            warn!(request_id=id, %encode_error, "invalid subscription terminal payload");
            return;
        }
    };
    if let Err(write_error) = writer.write_call_end(&frame).await {
        warn!(request_id=id, %write_error, "subscription CALL_END write failed");
    }
}

pub(super) async fn drive_subscription(
    conn: Arc<Connection>,
    writer: Arc<ConnectionWriter>,
    id: u64,
    attach: RemoteTopicAttach,
    _flight: crate::connection::FlightGuard,
    budget: DomainBudget,
) {
    let ready = match rpc::encode_ready_frame(id, attach.start_seq, attach.head_seq) {
        Ok(frame) => frame,
        Err(error) => {
            warn!(request_id=id, %error, "invalid READY frame");
            claim_terminal(&writer, id, CallStatus::Failed);
            return;
        }
    };
    if let Err(error) = writer.write_call_end(&ready).await {
        warn!(request_id=id, %error, "subscription READY write failed");
        claim_terminal(&writer, id, CallStatus::Failed);
        return;
    }

    let mut next_expected = attach.start_seq;
    let mut sequence_exhausted = false;
    let mut complete_items = 0u64;
    let mut last_loan: Option<Arc<LoanSettledSignal>> = None;

    let terminal: Option<C2Error> = 'subscription: loop {
        if sequence_exhausted {
            loop {
                let writer_notified = conn.writer_wake.notified();
                let chan_notified = attach.channel.notified();
                tokio::pin!(writer_notified);
                tokio::pin!(chan_notified);
                writer_notified.as_mut().enable();
                chan_notified.as_mut().enable();
                if let Some(stop) = check_stop_before_select(&conn, id, &attach) {
                    break 'subscription match stop {
                        SubStop::PeerEnded => None,
                        SubStop::Terminal(error) => Some(error),
                    };
                }
                tokio::select! { _ = writer_notified => {}, _ = chan_notified => {} }
            }
        }
        if let Err(stop) = wait_ready_for_select(&conn, id, &attach, last_loan.as_ref()).await {
            break match stop {
                SubStop::PeerEnded => None,
                SubStop::Terminal(error) => Some(error),
            };
        }

        // Select the next frame from the bounded history ring or wait for the
        // next publish / close / cancel / peer-end event.
        let frame = loop {
            let chan_notified = attach.channel.notified();
            let writer_notified = conn.writer_wake.notified();
            tokio::pin!(chan_notified);
            tokio::pin!(writer_notified);
            writer_notified.as_mut().enable();

            if let Some(stop) = check_stop_before_select(&conn, id, &attach) {
                match stop {
                    SubStop::PeerEnded => {
                        claim_terminal(&writer, id, CallStatus::Failed);
                        return;
                    }
                    SubStop::Terminal(error) => {
                        write_subscribe_terminal(&writer, id, complete_items, &error).await;
                        return;
                    }
                }
            }

            match attach.channel.select_next_for_remote(
                &mut next_expected,
                attach.options.on_lag,
                chan_notified.as_mut(),
            ) {
                RemoteSelectOutcome::Frame(frame) => break frame,
                RemoteSelectOutcome::Terminal(error) => {
                    write_subscribe_terminal(&writer, id, complete_items, &error).await;
                    return;
                }
                RemoteSelectOutcome::Wait => {
                    tokio::select! {
                        _ = chan_notified => {}
                        _ = writer_notified => {}
                    }
                }
            }
        };

        match send_topic_frame(&conn, &writer, id, &frame, &mut last_loan, &budget).await {
            Ok(()) => {
                complete_items += 1;
                sequence_exhausted = frame.seq() == u64::MAX;
            }
            Err(FrameSendError::PeerEnded) => break None,
            Err(FrameSendError::Cancelled) => {
                break Some(
                    subscription_cancellation(&conn, id)
                        .unwrap_or_else(|| TopicCloseCause::ConsumerCancelled.into_error()),
                );
            }
            Err(FrameSendError::Capacity(message)) => {
                break Some(C2Error::new(ErrorCode::CallCapacityExceeded, message));
            }
            Err(FrameSendError::Protocol(message)) => {
                break Some(C2Error::new(ErrorCode::ProtocolViolation, message));
            }
            Err(FrameSendError::Transport(message)) => {
                warn!(request_id=id, %message, "subscription frame write failed");
                break None;
            }
        }
    };

    if conn.is_work_cancelled() || terminal.is_none() {
        claim_terminal(&writer, id, CallStatus::Failed);
        return;
    }
    let error = terminal.expect("deliverable terminal");
    write_subscribe_terminal(&writer, id, complete_items, &error).await;
}
