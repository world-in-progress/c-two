//! Cross-relay topic upstream and merged source confirmation.
//!
//! An anchor relay whose route table holds a peer registration opens exactly
//! one HTTP topic subscription toward the source relay. The request carries
//! the single-hop loop-prevention header `x-c2-relay-origin-only: 1`; a relay
//! receiving that header admits only its own local routes and never forwards
//! again. Late joiners on an already active remote hub share one lightweight
//! confirmation request (no payload bytes) that returns the source head
//! sequence for the same registration.

use bytes::{Buf, Bytes};
use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use c2_mem::{DomainBudget, WindowChunk, WindowSlice};
use c2_wire::rpc::CancelCause;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::client::{add_expected_contract_headers, encode_segment};
use crate::topic_wire::{self, RecordKind};

pub(crate) const ORIGIN_ONLY_HEADER: &str = "x-c2-relay-origin-only";
pub(crate) const REGISTRATION_HEADER: &str = "x-c2-topic-registration";
pub(crate) const EXPECTED_REGISTRATION_HEADER: &str = "x-c2-expected-registration";
pub(crate) const CONFIRM_SEGMENT: &str = "_confirm";

fn transport(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ResourceUnavailable, message)
}

fn protocol(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ProtocolViolation, message)
}

/// Whether a confirmation refusal proves the confirmed registration no longer
/// serves the route, so the requesting hub must be isolated immediately.
pub(crate) fn isolates_hub(error: &C2Error) -> bool {
    matches!(
        error.code,
        ErrorCode::ResourceNotFound
            | ErrorCode::ResourceClosed
            | ErrorCode::ContractMismatch
            | ErrorCode::CallCancelled
    )
}

/// Deterministic opaque fingerprint of one source registration. The same Host
/// process re-registering a route keeps its server instance but issues a new
/// route UID, so the identity a relay confirms against covers both. Raw
/// identities never travel in HTTP headers; only this SHA-256 hex does.
pub(crate) fn registration_fingerprint(
    server_instance_id: &str,
    route_uid: &str,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for field in [server_instance_id, route_uid] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Relay-to-relay cancellation of one cross-relay topic upstream. The source's
/// cancel channel is exercised exactly once and promptly, even while this
/// relay's own downstream readers are stalled.
pub(crate) struct HttpUpstreamCancel {
    client: reqwest::Client,
    delete_url: String,
    handle: Option<tokio::runtime::Handle>,
    sent: std::sync::atomic::AtomicBool,
}

impl HttpUpstreamCancel {
    pub(crate) fn cancel(&self, _cause: CancelCause) {
        if self.sent.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let request = self
            .client
            .delete(&self.delete_url)
            .header(reqwest::header::CONTENT_LENGTH, "0");
        let Some(handle) = &self.handle else {
            return;
        };
        handle.spawn(async move {
            let _ = request.send().await;
        });
    }
}

/// One topic response from the source relay, parsed as the same record stream
/// an HTTP client consumes. Payload bytes are pulled through caller-charged
/// windows exactly like the IPC upstream.
pub(crate) struct HttpTopicUpstream {
    response: reqwest::Response,
    pending: Bytes,
    record_remaining: usize,
    ready: Option<u64>,
    terminal: Option<C2Error>,
    cancel: std::sync::Arc<HttpUpstreamCancel>,
}

impl HttpTopicUpstream {
    pub(crate) fn cancel_handle(&self) -> std::sync::Arc<HttpUpstreamCancel> {
        self.cancel.clone()
    }

    /// `Ok(false)` reports a clean end of stream exactly at a record boundary.
    async fn read_some(&mut self, dst: &mut [u8]) -> Result<bool, C2Error> {
        let mut at = 0;
        while at < dst.len() {
            if self.pending.is_empty() {
                match self
                    .response
                    .chunk()
                    .await
                    .map_err(|error| transport(format!("topic upstream read failed: {error}")))?
                {
                    Some(chunk) if chunk.is_empty() => continue,
                    Some(chunk) => self.pending = chunk,
                    None => {
                        if at == 0 {
                            return Ok(false);
                        }
                        return Err(transport("topic upstream ended inside a record"));
                    }
                }
            }
            let n = (dst.len() - at).min(self.pending.len());
            dst[at..at + n].copy_from_slice(&self.pending[..n]);
            self.pending.advance(n);
            at += n;
        }
        Ok(true)
    }

    pub(crate) async fn next_record(&mut self) -> Result<HubUpstreamRecord, C2Error> {
        loop {
            let mut header = [0; topic_wire::RECORD_HEADER_LEN];
            if !self.read_some(&mut header).await? {
                return Err(transport("topic upstream ended before its terminal"));
            }
            let (kind, len) = topic_wire::decode_header(header).map_err(protocol)?;
            match kind {
                RecordKind::Ready => {
                    let mut bytes = [0; 8];
                    self.read_some(&mut bytes).await?;
                    let start = u64::from_le_bytes(bytes);
                    if self.ready.replace(start).is_some() || start == 0 {
                        return Err(protocol("invalid upstream READY"));
                    }
                }
                RecordKind::ItemBegin => {
                    if self.ready.is_none() || self.record_remaining != 0 {
                        return Err(protocol("upstream ITEM_BEGIN outside an item"));
                    }
                    let mut bytes = [0; 16];
                    self.read_some(&mut bytes).await?;
                    let seq = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                    let total = u64::from_le_bytes(bytes[8..].try_into().unwrap());
                    if seq == 0 {
                        return Err(protocol("upstream item without a sequence"));
                    }
                    return Ok(HubUpstreamRecord::ItemBegin { seq, total_bytes: total });
                }
                RecordKind::ItemData => {
                    return Err(protocol("upstream ITEM_DATA outside an item"));
                }
                RecordKind::Terminal => {
                    let length = usize::try_from(len).map_err(|_| protocol("terminal too large"))?;
                    let mut bytes = vec![0; length];
                    self.read_some(&mut bytes).await?;
                    let terminal = topic_wire::decode_terminal(&bytes).map_err(protocol)?;
                    let mut error = terminal.error;
                    if terminal.status == topic_wire::Status::NotExecuted {
                        error.details.insert("transport_phase".into(), "pre_dispatch".into());
                        error
                            .details
                            .insert("dispatch_phase".into(), "pre_dispatch".into());
                    }
                    self.terminal = Some(error.clone());
                    return Ok(HubUpstreamRecord::End {
                        error: Some(error),
                    });
                }
            }
        }
    }

    pub(crate) async fn read_chunk(
        &mut self,
        slice: WindowSlice,
        max_bytes: usize,
    ) -> Result<WindowChunk, C2Error> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(max_bytes)
            .map_err(|_| transport("topic upstream copy allocation failed"))?;
        if bytes.capacity() as u64 > slice.bytes() {
            return Err(transport("topic upstream copy exceeds its window"));
        }
        bytes.resize(max_bytes, 0);
        let mut filled = 0;
        while filled < max_bytes {
            if self.record_remaining == 0 {
                let mut header = [0; topic_wire::RECORD_HEADER_LEN];
                self.read_some(&mut header).await?;
                let (kind, len) = topic_wire::decode_header(header).map_err(protocol)?;
                if kind != RecordKind::ItemData {
                    return Err(protocol("topic upstream data interrupted"));
                }
                self.record_remaining =
                    usize::try_from(len).map_err(|_| protocol("data record too large"))?;
            }
            let take = (max_bytes - filled).min(self.record_remaining);
            self.read_some(&mut bytes[filled..filled + take]).await?;
            self.record_remaining -= take;
            filled += take;
        }
        Ok(slice.into_buffer(bytes))
    }

    pub(crate) async fn completion(&mut self) -> Result<Option<C2Error>, C2Error> {
        while let Some(chunk) = self
            .response
            .chunk()
            .await
            .map_err(|error| transport(format!("topic upstream read failed: {error}")))?
        {
            drop(chunk);
        }
        Ok(self.terminal.clone())
    }
}

/// Records from either upstream kind, normalized for the hub driver.
pub(crate) enum HubUpstreamRecord {
    ItemBegin { seq: u64, total_bytes: u64 },
    End { error: Option<C2Error> },
}

pub(crate) struct OpenedRemoteUpstream {
    pub(crate) source: HttpTopicUpstream,
    pub(crate) registration: String,
}

/// Open the single cross-relay upstream subscription toward the source relay.
/// The request always carries the one-hop loop-prevention header; the source
/// relays only local routes for it and never forwards again.
pub(crate) async fn open_remote_upstream(
    client: reqwest::Client,
    source_relay_url: &str,
    expected: &ExpectedRouteContract,
    topic_name: &str,
) -> Result<OpenedRemoteUpstream, C2Error> {
    let base = source_relay_url.trim_end_matches('/');
    let url = format!(
        "{}/{}/_topics/{}",
        base,
        encode_segment(&expected.route_name),
        encode_segment(topic_name)
    );
    let request = add_expected_contract_headers(
        client
            .post(&url)
            .header(reqwest::header::ACCEPT, crate::topic_wire::MEDIA_TYPE)
            .header("x-c2-topic-start", "latest")
            .header("x-c2-topic-on-lag", "skip")
            .header(ORIGIN_ONLY_HEADER, "1")
            .header(reqwest::header::CONTENT_LENGTH, "0"),
        expected,
    )
    .build()
    .map_err(|error| transport(format!("topic upstream request build failed: {error}")))?;
    let response = client
        .execute(request)
        .await
        .map_err(|error| transport(format!("topic upstream request failed: {error}")))?;
    if response.status().as_u16() != 200 {
        return Err(upstream_refusal(response).await);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(crate::topic_wire::MEDIA_TYPE)
    {
        return Err(protocol("topic upstream response has an invalid content type"));
    }
    let token = response
        .headers()
        .get("x-c2-call-token")
        .and_then(|value| value.to_str().ok())
        .filter(|token| token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| protocol("topic upstream response has an invalid call token"))?
        .to_owned();
    let registration = response
        .headers()
        .get(REGISTRATION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| protocol("topic upstream response has no registration identity"))?
        .to_owned();
    let handle = tokio::runtime::Handle::try_current().ok();
    Ok(OpenedRemoteUpstream {
        source: HttpTopicUpstream {
            response,
            pending: Bytes::new(),
            record_remaining: 0,
            ready: None,
            terminal: None,
            cancel: std::sync::Arc::new(HttpUpstreamCancel {
                client,
                delete_url: format!("{base}/_calls/{token}"),
                handle,
                sent: std::sync::atomic::AtomicBool::new(false),
            }),
        },
        registration,
    })
}

/// Read a bounded canonical refusal body from the source relay.
async fn upstream_refusal(mut response: reqwest::Response) -> C2Error {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = match response.chunk().await {
        Ok(value) => value,
        Err(error) => return transport(format!("topic upstream refusal read failed: {error}")),
    } {
        if body.len() as u64 + chunk.len() as u64 > topic_wire::MAX_TERMINAL_PAYLOAD_BYTES {
            return transport("topic upstream refusal exceeds the bounded error size");
        }
        body.extend_from_slice(&chunk);
    }
    C2Error::from_json_bytes(&body)
        .unwrap_or_else(|_| transport(format!("topic upstream refused with status {status}")))
}

struct ConfirmWaiters(Vec<oneshot::Sender<Result<u64, C2Error>>>);

/// Merged lightweight confirmation toward the source relay. Concurrent late
/// joiners on the same remote hub share one in-flight request; a joiner that
/// arrives while no request is outstanding starts the next one.
pub(crate) struct RemoteConfirm {
    client: reqwest::Client,
    confirm_url: String,
    expected: ExpectedRouteContract,
    registration: String,
    budget: DomainBudget,
    inflight: Mutex<Option<ConfirmWaiters>>,
}

impl RemoteConfirm {
    pub(crate) fn new(
        client: reqwest::Client,
        source_relay_url: &str,
        expected: &ExpectedRouteContract,
        topic_name: &str,
        registration: &str,
        budget: DomainBudget,
    ) -> Self {
        let base = source_relay_url.trim_end_matches('/');
        Self {
            client,
            confirm_url: format!(
                "{}/{}/_topics/{}/{}",
                base,
                encode_segment(&expected.route_name),
                encode_segment(topic_name),
                CONFIRM_SEGMENT
            ),
            expected: expected.clone(),
            registration: registration.to_string(),
            budget,
            inflight: Mutex::new(None),
        }
    }

    /// Confirm the source head sequence for this exact registration. The
    /// acknowledgement the source returns is the atomic acceptance moment for
    /// every late joiner sharing this request.
    pub(crate) async fn head_seq(self: &std::sync::Arc<Self>) -> Result<u64, C2Error> {
        enum Join {
            Leader,
            Follower(oneshot::Receiver<Result<u64, C2Error>>),
        }
        let joined = {
            let mut inflight = self.inflight.lock();
            match inflight.as_mut() {
                Some(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.0.push(tx);
                    Join::Follower(rx)
                }
                None => {
                    *inflight = Some(ConfirmWaiters(Vec::new()));
                    Join::Leader
                }
            }
        };
        match joined {
            Join::Follower(rx) => rx
                .await
                .map_err(|_| transport("merged topic confirmation leader ended"))?,
            Join::Leader => {
                let guard = ConfirmLead(self.clone());
                let result = self.request().await;
                let waiters = guard.release();
                for waiter in waiters.0 {
                    let _ = waiter.send(result.clone());
                }
                result
            }
        }
    }

    async fn request(&self) -> Result<u64, C2Error> {
        let request = add_expected_contract_headers(
            self.client
                .post(&self.confirm_url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(ORIGIN_ONLY_HEADER, "1")
                .header(EXPECTED_REGISTRATION_HEADER, &self.registration)
                .header(reqwest::header::CONTENT_LENGTH, "0"),
            &self.expected,
        )
        .build()
        .map_err(|error| transport(format!("topic confirmation build failed: {error}")))?;
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|error| transport(format!("topic confirmation failed: {error}")))?;
        if response.status().as_u16() != 200 {
            return Err(upstream_refusal(response).await);
        }
        let body = crate::payload::read_http_response_body(
            response,
            crate::topic_wire::MAX_TERMINAL_PAYLOAD_BYTES,
            &self.budget,
            None,
            "pre_dispatch",
        )
        .await
        .map_err(|error| transport(format!("topic confirmation body read failed: {error}")))?;
        let value: serde_json::Value =
            serde_json::from_slice(body.as_ref()).map_err(|_| protocol("invalid JSON"))?;
        value
            .get("head_seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| protocol("topic confirmation has no head sequence"))
    }
}

/// Clears the merged batch when a dropped leader cannot finish it, so waiters
/// fail fast instead of blocking a later confirmation.
struct ConfirmLead(std::sync::Arc<RemoteConfirm>);

impl ConfirmLead {
    fn release(self) -> ConfirmWaiters {
        let waiters = self.0.inflight.lock().take();
        std::mem::forget(self);
        waiters.unwrap_or_else(|| ConfirmWaiters(Vec::new()))
    }
}

impl Drop for ConfirmLead {
    fn drop(&mut self) {
        let _ = self.0.inflight.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merged_confirmation_recovers_from_a_dropped_leader() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_first = std::sync::Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            "/grid/_topics/state/_confirm",
            axum::routing::post({
                let requests = requests.clone();
                let first_started = first_started.clone();
                let release_first = release_first.clone();
                move || {
                    let seen = requests.clone();
                    let started = first_started.clone();
                    let release = release_first.clone();
                    async move {
                        if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                            started.notify_waiters();
                            release.notified().await;
                        }
                        axum::Json(serde_json::json!({ "head_seq": 7 }))
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let confirm = std::sync::Arc::new(RemoteConfirm::new(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &c2_contract::ExpectedRouteContract {
                route_name: "grid".into(),
                crm_ns: "test.echo".into(),
                crm_name: "Echo".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: "a".repeat(64),
                signature_hash: "b".repeat(64),
            },
            "state",
            "instance-1",
            c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits::default()),
        ));
        // A dropped leader (its subscriber vanished) must not wedge the batch.
        let dropped = confirm.clone();
        let started = first_started.notified();
        tokio::pin!(started);
        let leader = tokio::spawn(async move {
            let _ = dropped.head_seq().await;
        });
        started.await;
        leader.abort();
        let _ = leader.await;
        release_first.notify_waiters();
        let first = confirm.head_seq().await.unwrap();
        let second = confirm.head_seq().await.unwrap();
        assert_eq!((first, second), (7, 7));
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "dropped leader plus two sequential confirms"
        );
        server.abort();
    }
}
