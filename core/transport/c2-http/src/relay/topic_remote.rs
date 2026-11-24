//! Cross-relay topic upstream and source confirmation.
//!
//! An anchor relay whose route table holds a peer registration opens exactly
//! one HTTP topic subscription toward the source relay. The request carries
//! the single-hop loop-prevention header `x-c2-relay-origin-only: 1`; a relay
//! receiving that header admits only its own local routes and never forwards
//! again. Each confirmation binds to the origin-only subscriber's token.
//! Compatible late joiners share a confirmation when the Host has no
//! admission hook, otherwise each joiner confirms with its own metadata.

use bytes::{Buf, Bytes};
use c2_config::CallMetadata;
use c2_contract::ExpectedRouteContract;
use c2_error::{C2Error, ErrorCode};
use c2_mem::{WindowChunk, WindowSlice};
use c2_wire::rpc::CancelCause;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use tokio::sync::oneshot;

use crate::client::add_expected_contract_headers;
use crate::http_wire::{
    MAX_CONTROL_ERROR_BYTES, add_metadata, read_control_body, single_header, topic_open_headers,
};
use crate::topic_wire::{self, RecordKind};
use crate::{TOPIC_CONFIRM_PATH, TOPIC_SUBSCRIBE_PATH, decode_control_error};

pub(crate) const ORIGIN_ONLY_HEADER: &str = "x-c2-relay-origin-only";
pub(crate) const REGISTRATION_HEADER: &str = "x-c2-topic-registration";
pub(crate) const EXPECTED_REGISTRATION_HEADER: &str = "x-c2-expected-registration";
pub(crate) const UPSTREAM_TOKEN_HEADER: &str = "x-c2-topic-upstream-token";

pub(crate) fn valid_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub(crate) fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn transport(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ResourceUnavailable, message)
}

fn protocol(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ProtocolViolation, message).with_details(BTreeMap::from([(
        "dispatch_phase".into(),
        "dispatch_uncertain".into(),
    )]))
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
pub(crate) fn registration_fingerprint(server_instance_id: &str, route_uid: &str) -> String {
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
    /// Partial-read progress for the record `next_record` is currently inside.
    /// The relay shutdown select drops `next_record` futures between reads;
    /// keeping the already-copied bytes here lets a resumed read continue at
    /// the exact byte it stopped on instead of misaligning the stream.
    partial: Option<PartialRecord>,
    header: [u8; topic_wire::RECORD_HEADER_LEN],
    small: [u8; 16],
}

/// The resumable stages of one upstream record: the fixed-length header
/// itself, or the body of one of the records that carries one.
enum PartialRecord {
    Header { filled: usize },
    Ready { filled: usize },
    ItemBegin { filled: usize },
    Terminal { filled: usize, bytes: Vec<u8> },
}

impl HttpTopicUpstream {
    pub(crate) fn cancel_handle(&self) -> std::sync::Arc<HttpUpstreamCancel> {
        self.cancel.clone()
    }

    /// Copy bytes into `dst` starting at `*filled`, advancing `filled` as
    /// each byte arrives so a dropped future resumes with the copied prefix
    /// intact. `Ok(false)` reports a clean end of stream while nothing of
    /// this read unit has arrived yet; an end inside a partially filled unit
    /// is an error. Free of `&mut self` so `next_record` can resume partial
    /// reads kept in this struct's own fields.
    async fn read_unit(
        response: &mut reqwest::Response,
        pending: &mut Bytes,
        dst: &mut [u8],
        filled: &mut usize,
    ) -> Result<bool, C2Error> {
        while *filled < dst.len() {
            if pending.is_empty() {
                match response
                    .chunk()
                    .await
                    .map_err(|error| transport(format!("topic upstream read failed: {error}")))?
                {
                    Some(chunk) if chunk.is_empty() => continue,
                    Some(chunk) => *pending = chunk,
                    None => {
                        if *filled == 0 {
                            return Ok(false);
                        }
                        return Err(transport("topic upstream ended inside a record"));
                    }
                }
            }
            let n = (dst.len() - *filled).min(pending.len());
            dst[*filled..*filled + n].copy_from_slice(&pending[..n]);
            pending.advance(n);
            *filled += n;
        }
        Ok(true)
    }

    /// Exact read of one record body or data slice. A clean end of stream
    /// here means the response truncated inside the record; the zeros already
    /// sitting in `dst` must never be delivered as payload.
    async fn read_exact_bytes(&mut self, dst: &mut [u8]) -> Result<(), C2Error> {
        let mut filled = 0;
        if !Self::read_unit(&mut self.response, &mut self.pending, dst, &mut filled).await? {
            return Err(transport("topic upstream ended inside a record"));
        }
        Ok(())
    }

    /// Resume one partially delivered record body in place. A clean end of
    /// stream inside a body is never accepted: its header was consumed, so
    /// the stream truncated inside the record.
    async fn read_body(
        response: &mut reqwest::Response,
        pending: &mut Bytes,
        dst: &mut [u8],
        filled: &mut usize,
    ) -> Result<(), C2Error> {
        if !Self::read_unit(response, pending, dst, filled).await? {
            return Err(transport("topic upstream ended inside a record"));
        }
        Ok(())
    }

    pub(crate) async fn next_record(&mut self) -> Result<HubUpstreamRecord, C2Error> {
        loop {
            if self.partial.is_none() {
                self.partial = Some(PartialRecord::Header { filled: 0 });
            }
            match self.partial.as_mut().expect("a partial record") {
                PartialRecord::Header { filled } => {
                    // `read_unit` reports a clean end of stream only while
                    // nothing of this header has arrived, which is exactly a
                    // record boundary; anywhere else EOF is an error.
                    if !Self::read_unit(
                        &mut self.response,
                        &mut self.pending,
                        &mut self.header,
                        filled,
                    )
                    .await?
                    {
                        return Err(transport("topic upstream ended before its terminal"));
                    }
                }
                PartialRecord::Ready { filled } => {
                    Self::read_body(
                        &mut self.response,
                        &mut self.pending,
                        &mut self.small[..8],
                        filled,
                    )
                    .await?;
                }
                PartialRecord::ItemBegin { filled } => {
                    Self::read_body(
                        &mut self.response,
                        &mut self.pending,
                        &mut self.small[..16],
                        filled,
                    )
                    .await?;
                }
                PartialRecord::Terminal { filled, bytes } => {
                    Self::read_body(&mut self.response, &mut self.pending, bytes, filled).await?;
                }
            }
            match self.partial.take().expect("a completed record") {
                PartialRecord::Header { .. } => {
                    let (kind, len) = topic_wire::decode_header(self.header).map_err(protocol)?;
                    self.partial = Some(match kind {
                        RecordKind::Ready => PartialRecord::Ready { filled: 0 },
                        RecordKind::ItemBegin => {
                            if self.ready.is_none() || self.record_remaining != 0 {
                                return Err(protocol("upstream ITEM_BEGIN outside an item"));
                            }
                            PartialRecord::ItemBegin { filled: 0 }
                        }
                        RecordKind::ItemData => {
                            return Err(protocol("upstream ITEM_DATA outside an item"));
                        }
                        RecordKind::Terminal => {
                            let length =
                                usize::try_from(len).map_err(|_| protocol("terminal too large"))?;
                            PartialRecord::Terminal {
                                filled: 0,
                                bytes: vec![0; length],
                            }
                        }
                    });
                }
                PartialRecord::Ready { .. } => {
                    let start = u64::from_le_bytes(self.small[..8].try_into().unwrap());
                    if self.ready.replace(start).is_some() || start == 0 {
                        return Err(protocol("invalid upstream READY"));
                    }
                    self.partial = Some(PartialRecord::Header { filled: 0 });
                }
                PartialRecord::ItemBegin { .. } => {
                    let seq = u64::from_le_bytes(self.small[..8].try_into().unwrap());
                    let total = u64::from_le_bytes(self.small[8..].try_into().unwrap());
                    if seq == 0 {
                        return Err(protocol("upstream item without a sequence"));
                    }
                    return Ok(HubUpstreamRecord::ItemBegin {
                        seq,
                        total_bytes: total,
                    });
                }
                PartialRecord::Terminal { bytes, .. } => {
                    let terminal = topic_wire::decode_terminal(&bytes).map_err(protocol)?;
                    let mut error = terminal.error;
                    if terminal.status == topic_wire::Status::NotExecuted {
                        error
                            .details
                            .insert("transport_phase".into(), "pre_dispatch".into());
                        error
                            .details
                            .insert("dispatch_phase".into(), "pre_dispatch".into());
                    }
                    self.terminal = Some(error.clone());
                    return Ok(HubUpstreamRecord::End { error: Some(error) });
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
                self.read_exact_bytes(&mut header).await?;
                let (kind, len) = topic_wire::decode_header(header).map_err(protocol)?;
                if kind != RecordKind::ItemData {
                    return Err(protocol("topic upstream data interrupted"));
                }
                self.record_remaining =
                    usize::try_from(len).map_err(|_| protocol("data record too large"))?;
            }
            let take = (max_bytes - filled).min(self.record_remaining);
            self.read_exact_bytes(&mut bytes[filled..filled + take])
                .await?;
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
#[derive(Debug)]
pub(crate) enum HubUpstreamRecord {
    ItemBegin { seq: u64, total_bytes: u64 },
    End { error: Option<C2Error> },
}

pub(crate) struct OpenedRemoteUpstream {
    pub(crate) source: HttpTopicUpstream,
    pub(crate) registration: String,
    pub(crate) token: String,
    pub(crate) requires_admission: bool,
    pub(crate) head_seq: u64,
}

/// Open the single cross-relay upstream subscription toward the source relay.
/// The request always carries the one-hop loop-prevention header; the source
/// relays only local routes for it and never forwards again.
pub(crate) async fn open_remote_upstream(
    client: reqwest::Client,
    source_relay_url: &str,
    expected: &ExpectedRouteContract,
    topic_name: &str,
    metadata: &CallMetadata,
) -> Result<OpenedRemoteUpstream, C2Error> {
    let base = source_relay_url.trim_end_matches('/');
    let url = format!("{base}{TOPIC_SUBSCRIBE_PATH}");
    let request = add_metadata(
        add_expected_contract_headers(
            client
                .post(&url)
                .query(&[
                    ("route", expected.route_name.as_str()),
                    ("topic", topic_name),
                ])
                .header(reqwest::header::ACCEPT, crate::topic_wire::MEDIA_TYPE)
                .header("x-c2-topic-start", "latest")
                .header("x-c2-topic-on-lag", "skip")
                .header(ORIGIN_ONLY_HEADER, "1")
                .header(reqwest::header::CONTENT_LENGTH, "0"),
            expected,
        ),
        metadata,
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
    if single_header(response.headers(), "content-type") != Some(crate::topic_wire::MEDIA_TYPE) {
        return Err(protocol(
            "topic upstream response has an invalid content type",
        ));
    }
    let (requires_admission, head_seq) =
        topic_open_headers(response.headers()).map_err(protocol)?;
    let token = single_header(response.headers(), "x-c2-call-token")
        .filter(|token| valid_token(token))
        .ok_or_else(|| protocol("topic upstream response has an invalid call token"))?
        .to_owned();
    let registration = single_header(response.headers(), REGISTRATION_HEADER)
        .filter(|value| valid_fingerprint(value))
        .ok_or_else(|| protocol("topic upstream response has an invalid registration identity"))?
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
            partial: None,
            header: [0; topic_wire::RECORD_HEADER_LEN],
            small: [0; 16],
        },
        registration,
        token,
        requires_admission,
        head_seq,
    })
}

/// Read a bounded canonical refusal body from the source relay.
async fn upstream_refusal(response: reqwest::Response) -> C2Error {
    let status = response.status();
    let body = read_control_body(response, MAX_CONTROL_ERROR_BYTES).await;
    if let Ok(body) = &body
        && let Some(error) = decode_control_error(status.as_u16(), body)
    {
        return error;
    }
    C2Error::new(
        ErrorCode::ResourceUnavailable,
        format!("topic upstream returned an unrecognized refusal with status {status}"),
    )
    .with_details(BTreeMap::from([(
        "dispatch_phase".into(),
        "dispatch_uncertain".into(),
    )]))
}

type ConfirmWaiter = oneshot::Sender<Result<u64, C2Error>>;

/// The merged lightweight confirmation's two batches, mirroring the IPC
/// synchronizer's `sync_pending` / `sync_inflight` split: waiters sealed into
/// `inflight` share the request currently on the wire, while late joiners
/// that arrive during that request queue in `pending` for the next round. A
/// late joiner must never observe a head sequence from a request issued
/// before it arrived.
#[derive(Default)]
struct ConfirmBatches {
    pending: Vec<ConfirmWaiter>,
    inflight: Option<Vec<ConfirmWaiter>>,
}

/// Merged lightweight confirmation toward the source relay. Concurrent late
/// joiners on the same remote hub share one request per causal batch; the
/// leader of a batch immediately starts the next one for everyone that
/// arrived while the previous request was outstanding.
pub(crate) struct RemoteConfirm {
    client: reqwest::Client,
    confirm_url: String,
    expected: ExpectedRouteContract,
    topic: String,
    registration: String,
    token: String,
    batches: Mutex<ConfirmBatches>,
}

impl RemoteConfirm {
    pub(crate) fn new(
        client: reqwest::Client,
        source_relay_url: &str,
        expected: &ExpectedRouteContract,
        topic_name: &str,
        registration: &str,
        token: &str,
    ) -> Self {
        let base = source_relay_url.trim_end_matches('/');
        Self {
            client,
            confirm_url: format!("{base}{TOPIC_CONFIRM_PATH}"),
            expected: expected.clone(),
            registration: registration.to_string(),
            token: token.to_string(),
            topic: topic_name.to_string(),
            batches: Mutex::new(ConfirmBatches::default()),
        }
    }

    /// Confirm the source head sequence for this exact registration. The
    /// acknowledgement the source returns is the atomic acceptance moment for
    /// every late joiner sealed into that request's batch; the leader returns
    /// with its own batch's answer while later batches continue in the
    /// background.
    pub(crate) async fn head_seq(self: &std::sync::Arc<Self>) -> Result<u64, C2Error> {
        enum Join {
            Leader,
            NextBatch(oneshot::Receiver<Result<u64, C2Error>>),
        }
        let joined = {
            let mut batches = self.batches.lock();
            if batches.inflight.is_some() {
                // The current request was issued before this joiner arrived;
                // its head sequence may be older than this joiner, so the
                // joiner waits for the next merged round.
                let (tx, rx) = oneshot::channel();
                batches.pending.push(tx);
                Join::NextBatch(rx)
            } else {
                batches.inflight = Some(Vec::new());
                Join::Leader
            }
        };
        match joined {
            Join::NextBatch(rx) => rx
                .await
                .map_err(|_| transport("merged topic confirmation leader ended"))?,
            Join::Leader => {
                let guard = ConfirmLead {
                    confirm: self.clone(),
                    active: true,
                };
                let result = self.request(&CallMetadata::default()).await;
                if self.deliver(result.clone()) {
                    let confirm = self.clone();
                    tokio::spawn(async move {
                        confirm.drive_sealed_rounds().await;
                    });
                }
                guard.release();
                result
            }
        }
    }

    /// Deliver `result` to the sealed batch and seal the late joiners that
    /// arrived during the request as the next round, reporting whether a next
    /// round was sealed. Never awaits: a leader cancelled mid-request is
    /// cleaned by the guard, and the deliver-seal handoff to the next round
    /// must stay atomic against that cancellation.
    fn deliver(self: &std::sync::Arc<Self>, result: Result<u64, C2Error>) -> bool {
        let mut batches = self.batches.lock();
        for waiter in batches.inflight.take().into_iter().flatten() {
            let _ = waiter.send(result.clone());
        }
        (!batches.pending.is_empty())
            .then(|| {
                batches.inflight = Some(std::mem::take(&mut batches.pending));
            })
            .is_some()
    }

    /// Drive the already-sealed rounds: one request per causal batch until no
    /// late joiner remains.
    async fn drive_sealed_rounds(self: std::sync::Arc<Self>) {
        let guard = ConfirmLead {
            confirm: self.clone(),
            active: true,
        };
        while self.deliver(self.request(&CallMetadata::default()).await) {}
        guard.release();
    }

    pub(crate) async fn admit_subscriber(&self, metadata: &CallMetadata) -> Result<u64, C2Error> {
        self.request(metadata).await
    }

    async fn request(&self, metadata: &CallMetadata) -> Result<u64, C2Error> {
        let request = add_metadata(
            add_expected_contract_headers(
                self.client
                    .post(&self.confirm_url)
                    .query(&[
                        ("route", self.expected.route_name.as_str()),
                        ("topic", &self.topic),
                    ])
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header(ORIGIN_ONLY_HEADER, "1")
                    .header(EXPECTED_REGISTRATION_HEADER, &self.registration)
                    .header(UPSTREAM_TOKEN_HEADER, &self.token)
                    .header(reqwest::header::CONTENT_LENGTH, "0"),
                &self.expected,
            ),
            metadata,
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
        // The answer is a tiny JSON control document; read it with the same
        // bounded control reader as refusals instead of charging the shared
        // payload budget, which this hub's own latest frame may already hold.
        let body = read_control_body(response, MAX_CONTROL_ERROR_BYTES)
            .await
            .map_err(|error| transport(format!("topic confirmation body read failed: {error}")))?;
        #[derive(serde::Deserialize)]
        struct Confirmation {
            head_seq: u64,
        }
        serde_json::from_slice::<Confirmation>(&body)
            .map(|confirmation| confirmation.head_seq)
            .map_err(|_| protocol("topic confirmation has no valid head sequence"))
    }
}

/// Clears the merged batches when a leader cannot finish them, so waiters
/// fail fast instead of blocking a later confirmation. A leader that finishes
/// normally releases the guard first; dropping it then must neither leak the
/// shared `Arc<RemoteConfirm>` (and its client) nor touch the already-drained
/// batches.
struct ConfirmLead {
    confirm: std::sync::Arc<RemoteConfirm>,
    active: bool,
}

impl ConfirmLead {
    fn release(mut self) {
        self.active = false;
    }
}

impl Drop for ConfirmLead {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut batches = self.confirm.batches.lock();
        batches.inflight = None;
        batches.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn expected_contract() -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: "grid".into(),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "a".repeat(64),
            signature_hash: "b".repeat(64),
        }
    }

    /// Drain one empty-bodied HTTP request up to its header terminator.
    async fn read_request_head(stream: &mut tokio::net::TcpStream) {
        let mut seen = Vec::new();
        let mut scratch = [0u8; 512];
        loop {
            let n = stream.read(&mut scratch).await.unwrap();
            assert!(n > 0, "the client closed before sending its request");
            seen.extend_from_slice(&scratch[..n]);
            if seen.ends_with(b"\r\n\r\n") {
                return;
            }
        }
    }

    /// A hand-written 200 response head that satisfies every check
    /// `open_remote_upstream` performs before parsing the record stream.
    fn topic_response_head() -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {}\r\nx-c2-call-token: \
             0123456789abcdef0123456789abcdef\r\nx-c2-topic-registration: {}\r\nx-c2-topic-admission-required: 0\r\nx-c2-topic-head-seq: 0\r\n\
             connection: close\r\n\r\n",
            crate::topic_wire::MEDIA_TYPE,
            "a".repeat(64),
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn remote_topic_open_validates_both_v2_headers() {
        for (from, to, message) in [
            (
                "x-c2-topic-admission-required: 0\r\n",
                "",
                "invalid_topic_admission_required",
            ),
            (
                "x-c2-topic-admission-required: 0",
                "x-c2-topic-admission-required: 2",
                "invalid_topic_admission_required",
            ),
            (
                "x-c2-topic-admission-required: 0",
                "x-c2-topic-admission-required: 0\r\nx-c2-topic-admission-required: 1",
                "invalid_topic_admission_required",
            ),
            ("x-c2-topic-head-seq: 0\r\n", "", "invalid_topic_head_seq"),
            (
                "x-c2-topic-head-seq: 0",
                "x-c2-topic-head-seq: 18446744073709551616",
                "invalid_topic_head_seq",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let response = String::from_utf8(topic_response_head())
                .unwrap()
                .replace(from, to);
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request_head(&mut stream).await;
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let error = open_remote_upstream(
                reqwest::Client::new(),
                &format!("http://{address}"),
                &expected_contract(),
                "state",
                &CallMetadata::default(),
            )
            .await
            .err()
            .expect("malformed v2 header must be refused");
            assert_eq!(error.code, ErrorCode::ProtocolViolation);
            assert_eq!(error.message, message);
            assert_eq!(error.details["dispatch_phase"], "dispatch_uncertain");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn bare_topic_404s_cannot_isolate_the_anchor_hub() {
        use axum::http::StatusCode;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let app = axum::Router::new()
            .route(
                TOPIC_SUBSCRIBE_PATH,
                axum::routing::post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                TOPIC_CONFIRM_PATH,
                axum::routing::post(|| async { StatusCode::NOT_FOUND }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let expected = expected_contract();
        let metadata = CallMetadata::default();
        let open_error = open_remote_upstream(client.clone(), &base, &expected, "state", &metadata)
            .await
            .err()
            .expect("bare 404 must fail");
        let confirm = RemoteConfirm::new(
            client,
            &base,
            &expected,
            "state",
            &"a".repeat(64),
            &"0".repeat(32),
        );
        let confirm_error = confirm.admit_subscriber(&metadata).await.unwrap_err();
        for error in [open_error, confirm_error] {
            assert_eq!(error.code, ErrorCode::ResourceUnavailable);
            assert_eq!(error.details["dispatch_phase"], "dispatch_uncertain");
            assert!(!isolates_hub(&error));
        }
        server.abort();
    }

    #[tokio::test]
    async fn merged_confirmation_recovers_from_a_dropped_leader() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_first = std::sync::Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            TOPIC_CONFIRM_PATH,
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
            &expected_contract(),
            "state",
            "instance-1",
            "0123456789abcdef0123456789abcdef",
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

    #[tokio::test]
    async fn a_late_joiner_never_observes_an_older_inflight_head_seq() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_first = std::sync::Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            TOPIC_CONFIRM_PATH,
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
                            // The first request left before the late joiner
                            // arrived: its answer is stale for that joiner.
                            axum::Json(serde_json::json!({ "head_seq": 7 }))
                        } else {
                            axum::Json(serde_json::json!({ "head_seq": 9 }))
                        }
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let confirm = std::sync::Arc::new(RemoteConfirm::new(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            "instance-1",
            "0123456789abcdef0123456789abcdef",
        ));
        let first = confirm.clone();
        let started = first_started.notified();
        tokio::pin!(started);
        let leader = tokio::spawn(async move { first.head_seq().await });
        started.await;
        let second = confirm.clone();
        let joiner = tokio::spawn(async move { second.head_seq().await });
        // Let the joiner queue into the next batch before the first round ends.
        tokio::task::yield_now().await;
        release_first.notify_waiters();
        assert_eq!(leader.await.unwrap().unwrap(), 7);
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), joiner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            joined, 9,
            "a late joiner must observe a request issued after it arrived"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one request per causal batch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_completed_leader_releases_its_confirm_arc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            TOPIC_CONFIRM_PATH,
            axum::routing::post(|| async { axum::Json(serde_json::json!({ "head_seq": 7 })) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let confirm = std::sync::Arc::new(RemoteConfirm::new(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            "instance-1",
            "0123456789abcdef0123456789abcdef",
        ));
        let weak = std::sync::Arc::downgrade(&confirm);
        assert_eq!(confirm.head_seq().await.unwrap(), 7);
        drop(confirm);
        // The finished leader (and any follow-up round driver) must drop
        // their arcs; the shared client must not leak with the hub rotation.
        let mut released = false;
        for _ in 0..1000 {
            if weak.upgrade().is_none() {
                released = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            released,
            "the confirmation leader leaked its Arc<RemoteConfirm>"
        );
        assert_eq!(weak.weak_count(), 0, "no stale leader guards remain");
        server.abort();
    }

    #[tokio::test]
    async fn confirmation_reads_its_control_body_without_the_shared_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            TOPIC_CONFIRM_PATH,
            axum::routing::post(|| async { axum::Json(serde_json::json!({ "head_seq": 7 })) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits {
            memory_budget_bytes: 4096,
            ..Default::default()
        });
        // The hub's own latest frame holds the entire shared payload budget,
        // exactly while the first subscriber is still confirming.
        let snapshot = budget.snapshot();
        let _held = budget
            .try_reserve_memory(snapshot.memory.limit_bytes - snapshot.memory.used_bytes)
            .unwrap();
        let confirm = std::sync::Arc::new(RemoteConfirm::new(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            "instance-1",
            "0123456789abcdef0123456789abcdef",
        ));
        let head = tokio::time::timeout(std::time::Duration::from_secs(5), confirm.head_seq())
            .await
            .expect("the confirmation must not wait on the shared payload budget")
            .unwrap();
        assert_eq!(head, 7);
        server.abort();
    }

    #[tokio::test]
    async fn truncated_item_data_is_an_error_never_zero_fill() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request_head(&mut stream).await;
            // READY(1), ITEM_BEGIN(1, 4), then an ITEM_DATA header promising
            // four bytes of which only two ever arrive.
            let mut body = topic_wire::ready(1).unwrap();
            body.extend_from_slice(&topic_wire::item_begin(1, 4).unwrap());
            body.extend_from_slice(&topic_wire::header(RecordKind::ItemData, 4));
            body.extend_from_slice(b"ab");
            let mut out = topic_response_head();
            out.extend_from_slice(&body);
            stream.write_all(&out).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let opened = open_remote_upstream(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            &CallMetadata::default(),
        )
        .await
        .unwrap();
        let mut upstream = opened.source;
        match upstream.next_record().await.unwrap() {
            HubUpstreamRecord::ItemBegin { seq, total_bytes } => {
                assert_eq!((seq, total_bytes), (1, 4))
            }
            record => panic!("expected ITEM_BEGIN, got {record:?}"),
        }
        let budget = c2_mem::DomainBudget::from_limits(&c2_config::DomainLimits::default());
        let window = budget.try_reserve_memory(4).unwrap().into_reusable_window();
        let slice = window.reserve(4).await.unwrap();
        let error = upstream.read_chunk(slice, 4).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceUnavailable);
        assert!(
            error.to_string().contains("inside a record"),
            "truncated data must fail loudly, not deliver zeros: {error}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_terminal_body_is_an_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request_head(&mut stream).await;
            // READY(1) then a TERMINAL header whose declared payload stops
            // halfway; the zeros prefilled in the read buffer must not parse.
            let mut body = topic_wire::ready(1).unwrap();
            body.extend_from_slice(&topic_wire::header(RecordKind::Terminal, 20));
            body.extend_from_slice(&[0u8; 10]);
            let mut out = topic_response_head();
            out.extend_from_slice(&body);
            stream.write_all(&out).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let opened = open_remote_upstream(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            &CallMetadata::default(),
        )
        .await
        .unwrap();
        let mut upstream = opened.source;
        let error = upstream.next_record().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceUnavailable);
        assert!(
            error.to_string().contains("inside a record"),
            "a truncated terminal must fail loudly: {error}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_next_record_resumes_without_losing_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (partial_sent, partial_seen) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request_head(&mut stream).await;
            let item = topic_wire::item_begin(1, 6).unwrap();
            let mut out = topic_response_head();
            out.extend_from_slice(&topic_wire::ready(1).unwrap());
            // The item header plus only its first seven body bytes.
            out.extend_from_slice(&item[..topic_wire::RECORD_HEADER_LEN + 7]);
            stream.write_all(&out).await.unwrap();
            partial_sent.send(()).unwrap();
            release_rx.await.unwrap();
            stream
                .write_all(&item[topic_wire::RECORD_HEADER_LEN + 7..])
                .await
                .unwrap();
        });
        let opened = open_remote_upstream(
            reqwest::Client::new(),
            &format!("http://{address}"),
            &expected_contract(),
            "state",
            &CallMetadata::default(),
        )
        .await
        .unwrap();
        let mut upstream = opened.source;
        partial_seen.await.unwrap();
        // Drop a half-delivered record exactly like the relay shutdown select
        // in `run_source` does, then keep reading on the same upstream.
        let mut resumed = false;
        for _ in 0..1000 {
            {
                let read = upstream.next_record();
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => panic!("the partial record cannot complete yet: {result:?}"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
                }
            }
            if matches!(
                upstream.partial.as_ref(),
                Some(PartialRecord::ItemBegin { filled: 7 })
            ) {
                resumed = true;
                break;
            }
        }
        assert!(
            resumed,
            "the cancelled read never consumed its partial record bytes"
        );
        release_tx.send(()).unwrap();
        match upstream.next_record().await.unwrap() {
            HubUpstreamRecord::ItemBegin { seq, total_bytes } => {
                assert_eq!((seq, total_bytes), (1, 6))
            }
            record => panic!("expected a clean ITEM_BEGIN after resuming, got {record:?}"),
        }
        server.abort();
        let _ = server.await;
    }
}
