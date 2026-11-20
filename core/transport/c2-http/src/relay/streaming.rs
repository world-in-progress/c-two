//! HTTP response pulls own the only reads from an upstream response cursor.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use c2_error::{C2Error, ErrorCode};
use c2_ipc::{ForwardRecord, IpcForwardStream, ResponseData};
use c2_mem::{OperationPermit, ReusableWindow};
use c2_wire::rpc::{CallStatus, CancelCause};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::task_tracker::TaskTrackerToken;

use super::{conn_pool::UpstreamLease, forwarding::StreamCancellation, state::RelayState};
use crate::stream_wire::{self, HttpStreamRecordKind, HttpStreamStatus, HttpStreamTerminal};

type PullReply = Result<(Bytes, bool), std::io::Error>;

struct PullBody {
    window: Option<Arc<ReusableWindow>>,
    requests: Option<mpsc::Sender<oneshot::Sender<PullReply>>>,
    cancellation: Arc<StreamCancellation>,
    finished: bool,
    hold: Arc<ResponseHold>,
}

impl Drop for PullBody {
    fn drop(&mut self) {
        // Release the body's window handle and close `requests` before
        // recording consumer cancellation, so `drive_stream` observes
        // `requests.is_closed()` and drops its own window before cancelling
        // upstream while still recording `Consumer` as the first cause.
        self.window.take();
        self.requests.take();
        if !self.finished {
            self.cancellation.cancel(CancelCause::Consumer);
        }
    }
}

/// Body, outbound frames and upstream work share the forwarding token.
struct ResponseHold {
    _permit: OperationPermit,
    _lease: UpstreamLease,
    _tracked: TaskTrackerToken,
}

struct ResponseChunk<T> {
    bytes: T,
    _hold: Arc<ResponseHold>,
}

impl<T: AsRef<[u8]>> AsRef<[u8]> for ResponseChunk<T> {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

fn owned_response_bytes<T: AsRef<[u8]> + Send + 'static>(
    bytes: T,
    hold: &Arc<ResponseHold>,
) -> Bytes {
    Bytes::from_owner(ResponseChunk {
        bytes,
        _hold: hold.clone(),
    })
}

/// Stream one admitted unary response through its reusable download window.
///
/// The forwarding `OperationPermit` and the upstream connection lease live in
/// the response body's own state: they are released only after the entire
/// body has been written or the downstream disconnect drops the body, never
/// when the forwarding handler returns. Each pulled slice reserves from and
/// refunds the same [`ReusableWindow`], so a response far larger than the
/// window still never re-applies to the relay's shared memory budget; the
/// window returns its whole reservation to the domain when the body ends.
pub(crate) fn unary_response(
    data: ResponseData,
    status: StatusCode,
    download: ReusableWindow,
    permit: OperationPermit,
    lease: UpstreamLease,
    tracked: TaskTrackerToken,
    width: usize,
) -> Response {
    let length = data.len();
    let hold = Arc::new(ResponseHold {
        _permit: permit,
        _lease: lease,
        _tracked: tracked,
    });
    let body = Body::from_stream(futures::stream::unfold(
        (data.into_cursor(), download, hold),
        move |(mut cursor, window, hold)| async move {
            if cursor.remaining() == 0 {
                return None;
            }
            let want = (cursor.remaining().min(width)) as u64;
            let result = async {
                let slice = window.reserve(want).await.map_err(std::io::Error::other)?;
                cursor
                    .read_chunk(slice, width)
                    .map_err(std::io::Error::other)?
                    .map(|chunk| owned_response_bytes(chunk, &hold))
                    .ok_or_else(|| {
                        std::io::Error::other("unary response cursor ended before its length")
                    })
            }
            .await;
            Some((result, (cursor, window, hold)))
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
    permit: OperationPermit,
    download: ReusableWindow,
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
    let hold = Arc::new(ResponseHold {
        _permit: permit,
        _lease: lease,
        _tracked: state.forwarding.track_body(),
    });
    let download = Arc::new(download);
    let body_state = PullBody {
        window: Some(download.clone()),
        requests: Some(requests),
        cancellation: cancellation.clone(),
        finished: false,
        hold: hold.clone(),
    };
    let body = Body::from_stream(futures::stream::unfold(body_state, |mut body| async move {
        if body.finished {
            return None;
        }
        let (reply, response) = oneshot::channel();
        let Some(requests) = body.requests.as_ref() else {
            return None;
        };
        if requests.send(reply).await.is_err() {
            return None;
        }
        let result = match response.await {
            Ok(Ok((bytes, finished))) => {
                body.finished = finished;
                Ok(owned_response_bytes(bytes, &body.hold))
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
        drive_stream(stream, cancellation, receiver, download, width).await;
        work_state.forwarding.remove_stream(&work_token);
        drop(hold);
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
    window: Arc<ReusableWindow>,
    width: usize,
) {
    let upstream_cancel = stream.cancellation();
    let mut observed_cancel = false;
    let mut observed_consumer = false;
    let mut window = Some(window);
    let mut remaining = 0;
    let mut pending_data = None;
    let mut terminal = false;
    'body: loop {
        let request = tokio::select! {
            biased;
            cause = cancellation.requested(), if !observed_cancel => {
                if requests.is_closed() {
                    break;
                }
                observed_cancel = true;
                observed_consumer = cause == CancelCause::Consumer;
                let _ = upstream_cancel.cancel(cause);
                continue;
            }
            _ = cancellation.wait_consumer_requested(), if observed_cancel && !observed_consumer => {
                if requests.is_closed() {
                    break;
                }
                observed_consumer = true;
                let _ = upstream_cancel.cancel(CancelCause::Consumer);
                continue;
            }
            request = requests.recv() => match request {
                Some(request) if request.is_closed() => {
                    // The body poll future dropped its oneshot before PullBody
                    // finished dropping `_window` and `requests`. Wait for the
                    // channel to close so `_window` is gone before cancelling upstream.
                    while requests.recv().await.is_some() {}
                    break;
                }
                Some(request) => request,
                None => break,
            },
        };
        // A single pull can be interrupted to submit cancellation, without a
        // second reader or an eager loop requesting the next payload chunk.
        let data_open = pending_data.is_some();
        let result = {
            let mut pull = Box::pin(pull_frame(
                &mut stream,
                &mut remaining,
                &mut pending_data,
                &mut window,
                width,
                &cancellation,
            ));
            loop {
                tokio::select! {
                    biased;
                    next = requests.recv() => {
                        // Body polls are serial: no second pull can be issued
                        // until this reply is sent. Closing its sender is a real
                        // abandonment even when token cancellation was already
                        // observed or a library-owned Bytes still fills B.
                        // Break first so `pull`, `pending_data`, and `window` all
                        // drop before sending the upstream consumer cancel.
                        debug_assert!(next.is_none());
                        break 'body;
                    }
                    cause = cancellation.requested(), if !observed_cancel => {
                        observed_cancel = true;
                        observed_consumer = cause == CancelCause::Consumer;
                        let _ = upstream_cancel.cancel(cause);
                        if cause == CancelCause::Consumer && !data_open {
                            drop(pull);
                            window.take();
                            break match stream.completion().await {
                                Ok(terminal) => terminal_frame(terminal, &mut window),
                                Err(error) => failure_frame(error, &mut window),
                            };
                        }
                    }
                    _ = cancellation.wait_consumer_requested(), if observed_cancel && !observed_consumer => {
                        observed_consumer = true;
                        let _ = upstream_cancel.cancel(CancelCause::Consumer);
                        if !data_open {
                            drop(pull);
                            window.take();
                            break match stream.completion().await {
                                Ok(terminal) => terminal_frame(terminal, &mut window),
                                Err(error) => failure_frame(error, &mut window),
                            };
                        }
                    }
                    result = &mut pull => break result,
                }
            }
        };
        terminal = result.as_ref().is_ok_and(|(_, ended)| *ended);
        let failed = result.is_err();
        if let Err(unsent) = request.send(result) {
            drop(unsent);
            drop(pending_data.take());
            drop(window.take());
            if !terminal {
                // `response` dropped before `PullBody` finished dropping `_window`
                // and `requests`. Wait for `requests` to close so `_window` is
                // released before cancelling upstream.
                while requests.recv().await.is_some() {}
            }
            break;
        }
        if failed || terminal {
            break;
        }
    }
    // No downstream reader remains for these local bytes. Their lifetime
    // ends before waiting for the resource's independent cleanup.
    drop(pending_data.take());
    drop(window.take());
    if !terminal {
        if let Some(cause) = (!observed_cancel).then(|| cancellation.cause()).flatten() {
            let _ = upstream_cancel.cancel(cause);
        }
        // Consumer abandonment can follow lifecycle cancellation. That second
        // intent discards this call's data while retaining the original cause.
        let _ = upstream_cancel.cancel(CancelCause::Consumer);
        let _ = stream.completion().await;
    }
}

fn terminal_frame(
    terminal: c2_ipc::StreamTerminal,
    window: &mut Option<Arc<ReusableWindow>>,
) -> PullReply {
    window.take();
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
    window: &mut Option<Arc<ReusableWindow>>,
) -> PullReply {
    window.take();
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
    window: &mut Option<Arc<ReusableWindow>>,
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
            Ok(terminal) => terminal_frame(terminal, window),
            Err(error) => failure_frame(error, window),
        };
    }
    if *remaining != 0 {
        // Every slice of this forwarded response loops through the entry
        // reservation: capacity returns to the window when the previous
        // chunk's Bytes drop, and the domain sees the whole window back only
        // when the response ends.
        let want = (*remaining).min(width as u64);
        let slice = match window.as_mut() {
            Some(window) => window.reserve(want).await.map_err(std::io::Error::other)?,
            None => {
                return failure_frame("forwarding window already released", window);
            }
        };
        match stream.read_chunk(slice, width).await {
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
                        Ok(terminal) => terminal_frame(terminal, window),
                        Err(error) => failure_frame(error, window),
                    };
                }
                return failure_frame(error, window);
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
        Ok(ForwardRecord::CallEnd(terminal)) => terminal_frame(terminal, window),
        Err(error) => failure_frame(error, window),
    }
}
