//! Single-hop forwarding to the relay that owns a route.

use std::{convert::Infallible, io, sync::Arc, time::Duration};

use axum::{
    body::{Body, BodyDataStream, Bytes, to_bytes},
    response::{IntoResponse, Response},
};
use bytes::Buf;
use c2_config::CallMetadata;
use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use c2_mem::{DomainBudget, MemoryReservation, OperationPermit, ReusableWindow};
use c2_wire::rpc::{CancelCause, WireCallOutcome};
use futures::StreamExt;
use http::{HeaderMap, Method, Request, StatusCode, header};
use tokio::sync::oneshot;
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};

use crate::{
    OUTCOME_PATH,
    http_wire::{MAX_CONTROL_ERROR_BYTES, single_header},
    outcome_wire::{CALL_ID_HEADER, MAX_OUTCOME_JSON_BYTES, OutcomeJson},
    stream_wire::{self, HttpStreamRecordKind, HttpStreamStatus},
};

use super::{
    forwarding::StreamCancellation,
    iroh_transport::MeshLane,
    peer_client::{CONTROL_TIMEOUT, PeerHttpClient, PeerRequestError},
    router::{
        OUTCOME_QUERY_TIMEOUT, content_length_from_headers, control_error_response,
        forwarding_capacity_response, phased_resource_unavailable_response,
        read_unknown_length_body, relay_shutdown_refusal,
    },
    state::RelayState,
    topic_remote::valid_token,
    types::{PeerAddress, RouteEntry},
};

pub(super) const ORIGIN_ONLY_HEADER: &str = "x-c2-relay-origin-only";
const CALL_TOKEN_HEADER: &str = "x-c2-call-token";
const COPY_CAP: u64 = if c2_config::REMOTE_PAYLOAD_CHUNK_SIZE < c2_config::CHUNK_SIZE {
    c2_config::REMOTE_PAYLOAD_CHUNK_SIZE
} else {
    c2_config::CHUNK_SIZE
};
const CONTRACT_HEADERS: [&str; 5] = [
    "x-c2-expected-crm-ns",
    "x-c2-expected-crm-name",
    "x-c2-expected-crm-ver",
    "x-c2-expected-abi-hash",
    "x-c2-expected-signature-hash",
];

pub(super) fn origin_request(
    method: Method,
    path: &str,
    query: &[(&str, &str)],
) -> http::request::Builder {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(query)
        .finish();
    Request::builder()
        .method(method)
        .uri(format!("{path}?{query}"))
        .header(ORIGIN_ONLY_HEADER, "1")
}

pub(super) fn contract_headers(
    mut request: http::request::Builder,
    expected: &ExpectedRouteContract,
    metadata: &CallMetadata,
) -> http::request::Builder {
    let values = [
        &expected.crm_ns,
        &expected.crm_name,
        &expected.crm_ver,
        &expected.abi_hash,
        &expected.signature_hash,
    ];
    for (name, value) in CONTRACT_HEADERS.into_iter().zip(values) {
        request = request.header(name, value);
    }
    for (key, value) in metadata.iter() {
        request = request.header(format!("x-c2-meta-{key}"), value);
    }
    request
}

fn forward_headers(
    mut request: http::request::Builder,
    headers: &HeaderMap,
) -> http::request::Builder {
    for (name, value) in headers {
        if matches!(name.as_str(), "accept" | "content-type" | CALL_ID_HEADER)
            || CONTRACT_HEADERS.contains(&name.as_str())
            || name.as_str().starts_with("x-c2-meta-")
        {
            request = request.header(name, value);
        }
    }
    request
}

fn response_headers(response: &HeaderMap) -> HeaderMap {
    [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CACHE_CONTROL,
    ]
    .into_iter()
    .filter_map(|name| response.get(&name).cloned().map(|value| (name, value)))
    .collect()
}

pub(super) fn peer_error(error: PeerRequestError) -> C2Error {
    let phase = if error.is_pre_dispatch() {
        "pre_dispatch"
    } else {
        "dispatch_uncertain"
    };
    C2Error::new(ErrorCode::ResourceUnavailable, error.to_string())
        .with_details([("dispatch_phase".into(), phase.into())].into())
}

fn forwarding_error(route: &str, error: PeerRequestError) -> Response {
    let error = peer_error(error);
    phased_resource_unavailable_response(route, error.message, &error.details["dispatch_phase"])
}

pub(super) async fn read_control_body(response: Response, limit: usize) -> io::Result<Bytes> {
    let declared = content_length_from_headers(response.headers()).map_err(io::Error::other)?;
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(io::Error::other(
            "relay control response exceeds its size limit",
        ));
    }
    let bytes = to_bytes(response.into_body(), limit)
        .await
        .map_err(io::Error::other)?;
    if declared.is_some_and(|len| len != bytes.len() as u64) {
        return Err(io::Error::other(
            "relay control response length does not match Content-Length",
        ));
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
pub(super) enum Control {
    Probe,
    Outcome,
}

pub(super) async fn forward_control(
    state: &RelayState,
    route: &RouteEntry,
    headers: &HeaderMap,
    control: Control,
) -> Response {
    let (path, timeout) = match control {
        Control::Probe => ("/_probe", CONTROL_TIMEOUT),
        Control::Outcome => (OUTCOME_PATH, OUTCOME_QUERY_TIMEOUT),
    };
    let request = forward_headers(
        origin_request(Method::GET, path, &[("route", &route.name)]),
        headers,
    )
    .body(Body::empty())
    .expect("validated relay control request");
    // The handler owns this future. Dropping an HTTP stream while this is
    // dialing or reading a probe drops the peer request as well.
    let mut dispatched = false;
    let reply = tokio::time::timeout(timeout, async {
        let request = state
            .peer_client()
            .prepare(MeshLane::Control, &route.address, request)
            .await?;
        dispatched = true;
        let response = request.dispatch().await?;
        let status = response.status();
        let headers = response_headers(response.headers());
        let limit = match (control, status) {
            (Control::Probe, StatusCode::OK) => 0,
            (Control::Outcome, StatusCode::OK) => MAX_OUTCOME_JSON_BYTES,
            _ => MAX_CONTROL_ERROR_BYTES,
        };
        let body = read_control_body(response, limit)
            .await
            .map_err(PeerRequestError::after_dispatch)?;
        if matches!((control, status), (Control::Outcome, StatusCode::OK)) {
            let outcome: OutcomeJson<'_> =
                serde_json::from_slice(&body).map_err(PeerRequestError::after_dispatch)?;
            WireCallOutcome::try_from(outcome).map_err(PeerRequestError::after_dispatch)?;
        } else if !status.is_success() {
            let error = C2Error::from_wire_bytes(&body)
                .ok()
                .flatten()
                .or_else(|| C2Error::from_json_bytes(&body).ok());
            if error.is_none() {
                return Err(PeerRequestError::after_dispatch(
                    "origin returned an invalid relay control error",
                ));
            }
        }
        let mut response = (status, Body::from(body)).into_response();
        *response.headers_mut() = headers;
        Ok(response)
    })
    .await;
    match reply {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => forwarding_error(&route.name, error),
        Err(_) => {
            let message = "origin relay control request timed out";
            let error = if dispatched {
                PeerRequestError::after_dispatch(message)
            } else {
                PeerRequestError::before_dispatch(message)
            };
            forwarding_error(&route.name, error)
        }
    }
}

pub(super) async fn cancel_peer_call(
    client: &PeerHttpClient,
    address: &PeerAddress,
    token: &str,
    cause: CancelCause,
) -> bool {
    let cause = match cause {
        CancelCause::Consumer => "consumer",
        CancelCause::Lifecycle => "lifecycle",
    };
    let request = origin_request(
        Method::DELETE,
        &format!("/_calls/{token}"),
        &[("cause", cause)],
    )
    .header(header::CONTENT_LENGTH, "0")
    .body(Body::empty())
    .expect("a validated relay token and cause form a cancellation request");
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let response = client
            .send(MeshLane::Control, address, request)
            .await
            .ok()?;
        // An empty 404 from the source means the token has already retired.
        // The original response still determines how the stream ended.
        if !matches!(
            response.status(),
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND
        ) {
            return None;
        }
        read_control_body(response, 0).await.ok()
    })
    .await
    .is_ok_and(|result| result.is_some())
}

pub(super) async fn retry_peer_cancellation(
    client: &PeerHttpClient,
    address: &PeerAddress,
    token: &str,
    cause: CancelCause,
) {
    let mut retry_delay = Duration::from_millis(100);
    while !cancel_peer_call(client, address, token, cause).await {
        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(CONTROL_TIMEOUT);
    }
}

pub(super) struct Call<'a> {
    pub route: RouteEntry,
    pub path: &'static str,
    pub method: &'a str,
    pub headers: &'a HeaderMap,
    pub content_length: Option<u64>,
    pub body: Body,
    pub stream: bool,
}

struct Upload {
    body: Body,
    length: u64,
    download: MemoryReservation,
}

impl Upload {
    async fn prepare(
        call: &mut Call<'_>,
        budget: &DomainBudget,
        cancelled: &CancellationToken,
    ) -> Result<Option<Self>, Response> {
        let body = std::mem::take(&mut call.body);
        let (length, body, download) = if let Some(length) = call.content_length {
            let body = if length == 0 {
                if to_bytes(body, 0).await.is_err() {
                    return Err(control_error_response(
                        StatusCode::BAD_REQUEST,
                        C2Error::new(
                            ErrorCode::ProtocolViolation,
                            "request body exceeds Content-Length",
                        )
                        .with_details([("dispatch_phase".into(), "pre_dispatch".into())].into()),
                    ));
                }
                Body::empty()
            } else {
                body
            };
            let limit = budget.snapshot().memory.limit_bytes;
            let upload = length.min(COPY_CAP).min((limit / 2).max(1));
            let download = COPY_CAP.min(limit.saturating_sub(upload));
            if download == 0 {
                return Err(forwarding_capacity_response(
                    &call.route.name,
                    "no peer forwarding window",
                ));
            }
            let mut reservation =
                budget
                    .reserve_memory(upload + download)
                    .await
                    .map_err(|error| {
                        forwarding_capacity_response(&call.route.name, error.to_string())
                    })?;
            let upload = reservation
                .split_off(upload)
                .expect("atomic peer forwarding reservation");
            let body = if length == 0 {
                body
            } else {
                windowed_body(body, Some(length), upload.into_reusable_window(), None)
            };
            (length, body, reservation)
        } else {
            let Some(prepared) = read_unknown_length_body(
                &call.route.name,
                body,
                call.route.max_payload_size,
                budget,
                cancelled,
                COPY_CAP,
            )
            .await
            .map_err(IntoResponse::into_response)?
            else {
                return Ok(None);
            };
            let length = prepared.payload.len() as u64;
            let body = if length == 0 {
                Body::empty()
            } else {
                Body::from_stream(futures::stream::iter(
                    prepared
                        .payload
                        .into_segments()
                        .into_iter()
                        .map(|part| Ok::<_, Infallible>(Bytes::from_owner(part))),
                ))
            };
            (length, body, prepared.download)
        };
        Ok(Some(Self {
            body,
            length,
            download,
        }))
    }
}

fn peer_call_shutdown_refusal(call: &Call<'_>) -> Response {
    if call.stream {
        return relay_shutdown_refusal(&call.route.name);
    }
    control_error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        C2Error::new(
            ErrorCode::CallCancelled,
            "relay shut down before forwarding the call",
        )
        .with_details([("dispatch_phase".into(), "pre_dispatch".into())].into()),
    )
}

pub(super) async fn forward_call(
    state: Arc<RelayState>,
    mut call: Call<'_>,
    permit: OperationPermit,
    cancelled: CancellationToken,
) -> Response {
    let shutdown = state.forwarding.shutdown_token();
    let upload = tokio::select! {
        biased;
        _ = cancelled.cancelled() => return StatusCode::NO_CONTENT.into_response(),
        _ = shutdown.cancelled() => return peer_call_shutdown_refusal(&call),
        result = Upload::prepare(&mut call, state.forwarding.budget(), &cancelled) => result,
    };
    let Upload {
        body,
        length,
        download,
    } = match upload {
        Ok(Some(upload)) => upload,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(response) => return response,
    };
    let stream = if call.stream {
        match StreamRegistration::new(state.clone()) {
            Ok(stream) => Some(stream),
            Err(error) => {
                return phased_resource_unavailable_response(
                    &call.route.name,
                    format!("cannot create relay call cancellation token: {error}"),
                    "pre_dispatch",
                );
            }
        }
    } else {
        None
    };
    let request = forward_headers(
        origin_request(
            Method::POST,
            call.path,
            &[("route", &call.route.name), ("method", call.method)],
        ),
        call.headers,
    )
    .header(header::CONTENT_LENGTH, length)
    .body(body)
    .expect("validated relay forwarding request");
    let client = state.peer_client().clone();
    let prepared = tokio::select! {
        biased;
        _ = cancelled.cancelled() => return StatusCode::NO_CONTENT.into_response(),
        _ = shutdown.cancelled() => return peer_call_shutdown_refusal(&call),
        result = client.prepare(MeshLane::Data, &call.route.address, request) => result,
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return forwarding_error(&call.route.name, error),
    };
    if cancelled.is_cancelled() {
        return StatusCode::NO_CONTENT.into_response();
    }
    if shutdown.is_cancelled() {
        return peer_call_shutdown_refusal(&call);
    }
    // Submission is synchronous. From this point the driver waits for the
    // original reply, even if its downstream waiter has disappeared.
    let response = prepared.dispatch();
    let response = {
        tokio::pin!(response);
        if let Some(stream) = &stream {
            // Wait for the origin token once dispatch starts. Shutdown records
            // Lifecycle on the same control even before the headers arrive.
            tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    stream.control.cancel(CancelCause::Consumer);
                    response.await
                }
                result = &mut response => result,
            }
        } else {
            response.await
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => return forwarding_error(&call.route.name, error),
    };
    let hold = Arc::new(ForwardHold {
        _permit: permit,
        _tracked: state.forwarding.track_body(),
    });
    let download = download.into_reusable_window();
    if let Some(stream) = stream
        && response.status() == StatusCode::OK
    {
        return stream_response(client, call.route, stream, response, download, hold);
    }
    let status = response.status();
    let length = match content_length_from_headers(response.headers()) {
        Ok(length) => length,
        Err(error) => {
            return phased_resource_unavailable_response(
                &call.route.name,
                error,
                "dispatch_uncertain",
            );
        }
    };
    let headers = response_headers(response.headers());
    let body = windowed_body(response.into_body(), length, download, Some(hold));
    let mut response = (status, body).into_response();
    *response.headers_mut() = headers;
    response
}

struct ForwardHold {
    _permit: OperationPermit,
    _tracked: TaskTrackerToken,
}

struct ForwardChunk<T> {
    chunk: T,
    _hold: Option<Arc<ForwardHold>>,
}

impl<T: AsRef<[u8]>> AsRef<[u8]> for ForwardChunk<T> {
    fn as_ref(&self) -> &[u8] {
        self.chunk.as_ref()
    }
}

#[derive(Debug)]
struct Reader {
    body: BodyDataStream,
    pending: Bytes,
}

impl Reader {
    fn new(body: Body) -> Self {
        Self {
            body: body.into_data_stream(),
            pending: Bytes::new(),
        }
    }

    async fn fill(&mut self) -> io::Result<bool> {
        while self.pending.is_empty() {
            match self.body.next().await {
                Some(Ok(chunk)) => self.pending = chunk,
                Some(Err(error)) => return Err(io::Error::other(error)),
                None => return Ok(false),
            }
        }
        Ok(true)
    }

    async fn read_exact(&mut self, mut output: &mut [u8]) -> io::Result<()> {
        while !output.is_empty() {
            if !self.fill().await? {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "origin ended within an HTTP stream record",
                ));
            }
            let (prefix, remainder) = output.split_at_mut(output.len().min(self.pending.len()));
            prefix.copy_from_slice(&self.pending[..prefix.len()]);
            self.pending.advance(prefix.len());
            output = remainder;
        }
        Ok(())
    }
}

fn windowed_body(
    body: Body,
    remaining: Option<u64>,
    window: ReusableWindow,
    hold: Option<Arc<ForwardHold>>,
) -> Body {
    Body::from_stream(futures::stream::try_unfold(
        (Reader::new(body), remaining, window, hold),
        |(mut source, mut remaining, window, hold)| async move {
            if !source.fill().await? {
                if remaining.is_some_and(|len| len != 0) {
                    return Err(io::Error::other("relay body ended before Content-Length"));
                }
                return Ok(None);
            }
            if remaining.is_some_and(|len| (source.pending.len() as u64) > len) {
                return Err(io::Error::other("relay body exceeds Content-Length"));
            }
            let size = (source.pending.len() as u64).min(window.capacity());
            let slice = window.reserve(size).await.map_err(io::Error::other)?;
            let chunk = slice.into_buffer(source.pending[..size as usize].to_vec());
            source.pending.advance(size as usize);
            if let Some(remaining) = &mut remaining {
                *remaining -= size;
            }
            let bytes = Bytes::from_owner(ForwardChunk {
                chunk,
                _hold: hold.clone(),
            });
            Ok(Some((bytes, (source, remaining, window, hold))))
        },
    ))
}

struct StreamRegistration {
    state: Arc<RelayState>,
    token: String,
    control: Arc<StreamCancellation>,
}

impl StreamRegistration {
    fn new(state: Arc<RelayState>) -> Result<Self, getrandom::Error> {
        let (token, control) = state.forwarding.register_stream()?;
        Ok(Self {
            state,
            token,
            control,
        })
    }
}

impl Drop for StreamRegistration {
    fn drop(&mut self) {
        self.state.forwarding.remove_stream(&self.token);
    }
}

fn stream_response(
    client: Arc<PeerHttpClient>,
    route: RouteEntry,
    stream: StreamRegistration,
    response: Response,
    download: ReusableWindow,
    hold: Arc<ForwardHold>,
) -> Response {
    let headers = response.headers();
    let token = single_header(headers, CALL_TOKEN_HEADER).filter(|token| valid_token(token));
    let Some(remote_token) =
        token.filter(|_| single_header(headers, "content-type") == Some(stream_wire::MEDIA_TYPE))
    else {
        return phased_resource_unavailable_response(
            &route.name,
            "invalid origin server-stream response headers",
            "dispatch_uncertain",
        );
    };
    let remote_token = remote_token.to_owned();
    let token = stream.token.clone();
    let reader = Reader::new(response.into_body());
    let (abandon, abandoned) = oneshot::channel();
    let finished = CancellationToken::new();
    let state = stream.state.clone();
    let body = StreamBody {
        reader: Some(reader),
        abandon: Some(abandon),
        finished: finished.clone(),
        control: stream.control.clone(),
        window: download,
        hold: hold.clone(),
        pending_data: None,
        item_remaining: 0,
        record_remaining: 0,
        complete_items: 0,
    };
    state.forwarding.spawn_driver(async move {
        let cancellation = async {
            let first = stream.control.requested().await;
            // DELETE is idempotent for this exact peer and token. Keep
            // submitting cancellation if the control lane fails transiently.
            retry_peer_cancellation(&client, &route.address, &remote_token, first).await;
            if first == CancelCause::Lifecycle {
                stream.control.wait_consumer_requested().await;
                retry_peer_cancellation(
                    &client,
                    &route.address,
                    &remote_token,
                    CancelCause::Consumer,
                )
                .await;
            }
        };
        let response_ended = async {
            let drain = async {
                if let Ok(mut reader) = abandoned.await {
                    // Keep observing the original response independently of
                    // cancellation acknowledgements on the control lane.
                    while let Some(Ok(_)) = reader.body.next().await {
                        tokio::task::yield_now().await;
                    }
                }
            };
            tokio::select! {
                _ = finished.cancelled() => {}
                _ = drain => {}
            }
        };
        tokio::pin!(response_ended);
        tokio::select! {
            _ = &mut response_ended => {}
            _ = cancellation => response_ended.await,
        }
        drop(stream);
        drop(hold);
    });
    let body = Body::from_stream(futures::stream::unfold(body, |mut body| async move {
        if body.finished.is_cancelled() {
            return None;
        }
        let bytes = match body.next().await {
            Ok(bytes) => bytes,
            Err(error) => {
                body.finish();
                let error = C2Error::new(ErrorCode::ResourceUnavailable, error.to_string())
                    .with_details([("dispatch_phase".into(), "dispatch_uncertain".into())].into());
                Bytes::from(
                    stream_wire::encode_failure(&error).expect("bounded relay stream failure"),
                )
            }
        };
        let bytes = Bytes::from_owner(ForwardChunk {
            chunk: bytes,
            _hold: Some(body.hold.clone()),
        });
        Some((Ok::<_, Infallible>(bytes), body))
    }));
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE.as_str(),
                stream_wire::MEDIA_TYPE.to_string(),
            ),
            (CALL_TOKEN_HEADER, token),
        ],
        body,
    )
        .into_response()
}

struct StreamBody {
    reader: Option<Reader>,
    abandon: Option<oneshot::Sender<Reader>>,
    finished: CancellationToken,
    control: Arc<StreamCancellation>,
    window: ReusableWindow,
    hold: Arc<ForwardHold>,
    pending_data: Option<Bytes>,
    item_remaining: u64,
    record_remaining: u64,
    complete_items: u64,
}

impl StreamBody {
    fn finish(&mut self) {
        self.finished.cancel();
        self.reader.take();
    }

    async fn next(&mut self) -> io::Result<Bytes> {
        use HttpStreamRecordKind::*;
        if let Some(data) = self.pending_data.take() {
            return Ok(data);
        }
        let reader = self
            .reader
            .as_mut()
            .expect("active origin stream has a reader");
        if self.record_remaining == 0 {
            let mut header = [0; stream_wire::RECORD_HEADER_LEN];
            reader.read_exact(&mut header).await?;
            let (kind, length) = stream_wire::decode_header(header).map_err(io::Error::other)?;
            match kind {
                ItemBegin => {
                    if self.item_remaining != 0 {
                        return Err(io::Error::other(
                            "origin started a new item before finishing the previous item",
                        ));
                    }
                    let mut payload = [0; 8];
                    reader.read_exact(&mut payload).await?;
                    self.item_remaining = stream_wire::decode_item_begin(payload);
                    if self.item_remaining == 0 {
                        self.complete_items += 1;
                    }
                    return Ok(Bytes::from_owner(stream_wire::encode_item_begin(
                        self.item_remaining,
                    )));
                }
                ItemData => {
                    if length > self.item_remaining {
                        return Err(io::Error::other(
                            "origin item data exceeds its announced length",
                        ));
                    }
                    self.record_remaining = length;
                }
                CallEnd | Failure => {
                    // An origin's terminal uses the IPC inline error bound.
                    // Read it whole so even a mid-terminal transport failure
                    // can be represented by a complete FAILURE downstream.
                    if length > c2_config::MAX_RPC_FRAME_BYTES {
                        return Err(io::Error::other(
                            "origin stream terminal exceeds the IPC frame limit",
                        ));
                    }
                    let mut terminal = header.to_vec();
                    terminal.resize(stream_wire::RECORD_HEADER_LEN + length as usize, 0);
                    let payload = &mut terminal[stream_wire::RECORD_HEADER_LEN..];
                    reader.read_exact(payload).await?;
                    if kind == CallEnd {
                        let terminal =
                            stream_wire::decode_terminal(payload).map_err(io::Error::other)?;
                        if self.item_remaining != 0 && terminal.status != HttpStreamStatus::Failed
                            || terminal.status == HttpStreamStatus::NotExecuted
                                && self.complete_items != 0
                            || terminal.complete_items.is_some_and(|count| {
                                count < self.complete_items
                                    || (!self.control.is_consumer_requested()
                                        && count != self.complete_items)
                            })
                        {
                            return Err(io::Error::other(
                                "origin stream terminal conflicts with forwarded items",
                            ));
                        }
                    } else {
                        stream_wire::decode_failure(payload).map_err(io::Error::other)?;
                    }
                    self.finish();
                    return Ok(Bytes::from(terminal));
                }
            }
        }
        // Do not emit a DATA header before its corresponding bytes are owned.
        // A lost peer can then end the downstream at a valid record boundary.
        let size = self.record_remaining.min(self.window.capacity());
        let slice = self.window.reserve(size).await.map_err(io::Error::other)?;
        let mut data = vec![0; size as usize];
        reader.read_exact(&mut data).await?;
        self.record_remaining -= size;
        self.item_remaining -= size;
        if self.item_remaining == 0 {
            self.complete_items += 1;
        }
        self.pending_data = Some(Bytes::from_owner(slice.into_buffer(data)));
        Ok(Bytes::from_owner(stream_wire::encode_header(
            ItemData, size,
        )))
    }
}

impl Drop for StreamBody {
    fn drop(&mut self) {
        if !self.finished.is_cancelled() {
            self.control.cancel(CancelCause::Consumer);
            if let (Some(abandon), Some(reader)) = (self.abandon.take(), self.reader.take()) {
                let _ = abandon.send(reader);
            }
        }
    }
}

#[cfg(test)]
mod tests;
