//! HTTP response pulls own the only reads from an upstream response cursor.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use c2_error::{C2Error, ErrorCode};
use c2_ipc::{ForwardRecord, IpcForwardStream, ResponseData};
use c2_mem::retention::{PayloadBufferBudget, PayloadBufferReservation};
use c2_wire::rpc::{CallStatus, CancelCause};
use tokio::sync::{mpsc, oneshot};

use super::{conn_pool::UpstreamLease, forwarding::StreamCancellation, state::RelayState};
use crate::stream_wire::{self, HttpStreamRecordKind, HttpStreamStatus, HttpStreamTerminal};

type PullReply = Result<(Bytes, bool), std::io::Error>;

struct PullBody {
    requests: mpsc::Sender<oneshot::Sender<PullReply>>,
    cancellation: Arc<StreamCancellation>,
    finished: bool,
}

impl Drop for PullBody {
    fn drop(&mut self) {
        if !self.finished {
            self.cancellation.cancel(CancelCause::Consumer);
        }
    }
}

pub(crate) fn unary_response(
    data: ResponseData,
    status: StatusCode,
    first: PayloadBufferReservation,
    budget: PayloadBufferBudget,
    width: usize,
) -> Response {
    let length = data.len();
    let body = Body::from_stream(futures::stream::unfold(
        (data.into_cursor(), Some(first), budget),
        move |(mut cursor, mut first, budget)| async move {
            if cursor.remaining() == 0 {
                return None;
            }
            let reservation = match first.take() {
                Some(reservation) => Ok(reservation),
                None => budget.reserve((cursor.remaining().min(width)) as u64).await,
            };
            let chunk = match reservation {
                Ok(reservation) => cursor
                    .read_chunk(reservation, width)
                    .map(|chunk| Bytes::from_owner(chunk.expect("nonempty response cursor")))
                    .map_err(std::io::Error::other),
                Err(error) => Err(std::io::Error::other(error)),
            };
            Some((chunk, (cursor, first, budget)))
        },
    ));
    (
        status,
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        body,
    )
        .into_response()
}

pub(crate) fn stream_response(
    state: Arc<RelayState>,
    stream: IpcForwardStream,
    lease: UpstreamLease,
    permit: c2_mem::RetentionPermit,
    first: PayloadBufferReservation,
    width: usize,
) -> Result<Response, getrandom::Error> {
    let (token, cancellation) = match state.forwarding.register_stream() {
        Ok(registered) => registered,
        Err(error) => {
            state.forwarding.spawn_driver(async move {
                let _ = stream.cancellation().cancel(CancelCause::Consumer);
                let _ = stream.completion().await;
                drop(lease);
                drop(permit);
            });
            return Err(error);
        }
    };
    let (requests, receiver) = mpsc::channel(1);
    let body_state = PullBody {
        requests,
        cancellation: cancellation.clone(),
        finished: false,
    };
    let body = Body::from_stream(futures::stream::unfold(body_state, |mut body| async move {
        if body.finished {
            return None;
        }
        let (reply, response) = oneshot::channel();
        if body.requests.send(reply).await.is_err() {
            return None;
        }
        let result = match response.await {
            Ok(Ok((bytes, finished))) => {
                body.finished = finished;
                Ok(bytes)
            }
            Ok(Err(error)) => {
                body.finished = true;
                Err(error)
            }
            Err(_) => {
                body.finished = true;
                Err(std::io::Error::other("relay stream driver ended"))
            }
        };
        Some((result, body))
    }));
    let work_state = state.clone();
    let work_token = token.clone();
    state.forwarding.spawn_driver(async move {
        drive_stream(
            stream,
            cancellation,
            receiver,
            first,
            work_state.forwarding.payload_budget(),
            width,
        )
        .await;
        work_state.forwarding.remove_stream(&work_token);
        drop(lease);
        drop(permit);
    });
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, stream_wire::MEDIA_TYPE.to_string()),
            (
                axum::http::HeaderName::from_static("x-c2-call-token"),
                token,
            ),
        ],
        body,
    )
        .into_response())
}

async fn drive_stream(
    mut stream: IpcForwardStream,
    cancellation: Arc<StreamCancellation>,
    mut requests: mpsc::Receiver<oneshot::Sender<PullReply>>,
    first: PayloadBufferReservation,
    budget: PayloadBufferBudget,
    width: usize,
) {
    let upstream_cancel = stream.cancellation();
    let mut observed_cancel = false;
    let mut observed_consumer = false;
    let mut first = Some(first);
    let mut remaining = 0;
    let mut pending_data = None;
    let mut terminal = false;
    'body: loop {
        let request = tokio::select! {
            biased;
            cause = cancellation.requested(), if !observed_cancel => {
                observed_cancel = true;
                observed_consumer = cause == CancelCause::Consumer;
                let _ = upstream_cancel.cancel(cause);
                continue;
            }
            _ = cancellation.wait_consumer_requested(), if observed_cancel && !observed_consumer => {
                observed_consumer = true;
                let _ = upstream_cancel.cancel(CancelCause::Consumer);
                continue;
            }
            request = requests.recv() => match request { Some(request) => request, None => break },
        };
        // A single pull can be interrupted to submit cancellation, without a
        // second reader or an eager loop requesting the next payload chunk.
        let data_open = pending_data.is_some();
        let mut pull = Box::pin(pull_frame(
            &mut stream,
            &mut remaining,
            &mut pending_data,
            &mut first,
            &budget,
            width,
            &cancellation,
        ));
        let result = loop {
            tokio::select! {
                biased;
                next = requests.recv() => {
                    // Body polls are serial: no second pull can be issued
                    // until this reply is sent. Closing its sender is a real
                    // abandonment even when token cancellation was already
                    // observed or a library-owned Bytes still fills B.
                    debug_assert!(next.is_none());
                    let _ = upstream_cancel.cancel(CancelCause::Consumer);
                    break 'body;
                }
                cause = cancellation.requested(), if !observed_cancel => {
                    observed_cancel = true;
                    observed_consumer = cause == CancelCause::Consumer;
                    let _ = upstream_cancel.cancel(cause);
                    if cause == CancelCause::Consumer && !data_open {
                        drop(pull);
                        break match stream.completion().await {
                            Ok(terminal) => terminal_frame(terminal, &mut first),
                            Err(error) => failure_frame(error, &mut first),
                        };
                    }
                }
                _ = cancellation.wait_consumer_requested(), if observed_cancel && !observed_consumer => {
                    observed_consumer = true;
                    let _ = upstream_cancel.cancel(CancelCause::Consumer);
                    if !data_open {
                        drop(pull);
                        break match stream.completion().await {
                            Ok(terminal) => terminal_frame(terminal, &mut first),
                            Err(error) => failure_frame(error, &mut first),
                        };
                    }
                }
                result = &mut pull => break result,
            }
        };
        terminal = result.as_ref().is_ok_and(|(_, ended)| *ended);
        let failed = result.is_err();
        if request.send(result).is_err() || failed || terminal {
            break;
        }
    }
    // No downstream reader remains for these local bytes. Their lifetime
    // ends before waiting for the resource's independent cleanup.
    drop(pending_data.take());
    drop(first.take());
    if !terminal {
        // Consumer abandonment can follow lifecycle cancellation. That second
        // intent discards this call's data while retaining the original cause.
        let _ = upstream_cancel.cancel(CancelCause::Consumer);
        let _ = stream.completion().await;
    }
}

fn terminal_frame(
    terminal: c2_ipc::StreamTerminal,
    first: &mut Option<PayloadBufferReservation>,
) -> PullReply {
    first.take();
    let status = match terminal.status {
        CallStatus::Success => HttpStreamStatus::Success,
        CallStatus::Failed => HttpStreamStatus::Failed,
        CallStatus::NotExecuted => HttpStreamStatus::NotExecuted,
    };
    let terminal = HttpStreamTerminal {
        status,
        complete_items: (status != HttpStreamStatus::NotExecuted)
            .then_some(terminal.complete_items),
        error: terminal.error,
    };
    let bytes = stream_wire::encode_terminal(&terminal).map_err(std::io::Error::other)?;
    Ok((Bytes::from(bytes), true))
}

fn failure_frame(
    error: impl std::fmt::Display,
    first: &mut Option<PayloadBufferReservation>,
) -> PullReply {
    first.take();
    let error = C2Error::new(ErrorCode::ResourceUnavailable, error.to_string()).with_details(
        std::collections::BTreeMap::from([("dispatch_phase".into(), "dispatch_uncertain".into())]),
    );
    let bytes = stream_wire::encode_failure(&error).map_err(std::io::Error::other)?;
    Ok((Bytes::from(bytes), true))
}

async fn pull_frame(
    stream: &mut IpcForwardStream,
    remaining: &mut u64,
    pending_data: &mut Option<Bytes>,
    first: &mut Option<PayloadBufferReservation>,
    budget: &PayloadBufferBudget,
    width: usize,
    cancellation: &StreamCancellation,
) -> PullReply {
    // A DATA header and its already-owned payload are separate Body frames.
    // Once its header is sent, finish this payload before another record.
    if let Some(bytes) = pending_data.take() {
        return Ok((bytes, false));
    }
    if cancellation.is_consumer_requested() {
        return match stream.completion().await {
            Ok(terminal) => terminal_frame(terminal, first),
            Err(error) => failure_frame(error, first),
        };
    }
    if *remaining != 0 {
        let reservation = match first.take() {
            Some(reservation) => reservation,
            None => budget
                .reserve((*remaining).min(width as u64))
                .await
                .map_err(std::io::Error::other)?,
        };
        match stream.read_chunk(reservation, width).await {
            Ok(Some(bytes)) => {
                let length = bytes.as_ref().len() as u64;
                *remaining -= length;
                *pending_data = Some(Bytes::from_owner(bytes));
                return Ok((
                    Bytes::copy_from_slice(&stream_wire::encode_header(
                        HttpStreamRecordKind::ItemData,
                        length,
                    )),
                    false,
                ));
            }
            Ok(None) => *remaining = 0,
            Err(error) => {
                // Consumer cancellation can interrupt a pending read before
                // CALL_END arrives. Keep the original response until that
                // actual outcome, rather than manufacturing a transport loss.
                if cancellation.cause().is_some() {
                    return match stream.completion().await {
                        Ok(terminal) => terminal_frame(terminal, first),
                        Err(error) => failure_frame(error, first),
                    };
                }
                return failure_frame(error, first);
            }
        }
    }
    match stream.next_record().await {
        Ok(ForwardRecord::ItemBegin { total_bytes }) => {
            *remaining = total_bytes;
            Ok((
                Bytes::copy_from_slice(&stream_wire::encode_item_begin(total_bytes)),
                false,
            ))
        }
        Ok(ForwardRecord::CallEnd(terminal)) => terminal_frame(terminal, first),
        Err(error) => failure_frame(error, first),
    }
}
