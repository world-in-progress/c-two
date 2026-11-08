//! One original HTTP server-stream response and its independent cancellation record.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget, PayloadBufferReservation};
use tokio::sync::{Notify, mpsc, watch};

use super::HttpError;
use crate::stream_wire::{self, HttpStreamRecordKind, HttpStreamStatus, HttpStreamTerminal};

#[derive(Default)]
struct CancellationState {
    requested: AtomicBool,
    changed: Notify,
}

/// A cancellation request can precede the POST response headers. The DELETE
/// is sent when the call token becomes available and never ends the POST read.
#[derive(Clone, Default)]
pub struct HttpStreamCancellation(Arc<CancellationState>);

impl HttpStreamCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.0.requested.swap(true, Ordering::AcqRel) {
            self.0.changed.notify_waiters();
        }
    }

    fn requested(&self) -> bool {
        self.0.requested.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.requested() {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Clone)]
enum ObservedOutcome {
    Terminal(HttpStreamTerminal),
    Failure(c2_error::C2Error),
    Transport(String),
}

impl ObservedOutcome {
    fn completion(self) -> Result<HttpStreamTerminal, HttpError> {
        match self {
            Self::Terminal(terminal) => Ok(terminal),
            Self::Failure(error) => Err(HttpError::LocalCallRejected(error)),
            Self::Transport(error) => Err(HttpError::Transport(error)),
        }
    }

    fn receive(self) -> Result<(), HttpError> {
        match self {
            Self::Terminal(HttpStreamTerminal {
                status: HttpStreamStatus::Success,
                ..
            }) => Ok(()),
            Self::Terminal(terminal) => Err(HttpError::LocalCallRejected(
                terminal.error.expect("error CALL_END"),
            )),
            Self::Failure(error) => Err(HttpError::LocalCallRejected(error)),
            Self::Transport(error) => Err(HttpError::Transport(error)),
        }
    }
}

/// The actor owns the POST body and operation permit through the real terminal.
/// One delivered item and one actor-authorized item can coexist.
pub struct HttpResponseStream {
    items: mpsc::Receiver<BudgetedBuffer>,
    outcome: watch::Receiver<Option<ObservedOutcome>>,
    local_error: watch::Receiver<Option<c2_error::C2Error>>,
    cancellation: HttpStreamCancellation,
}

impl HttpResponseStream {
    pub(crate) fn start(
        response: reqwest::Response,
        client: reqwest::Client,
        base_url: String,
        token: String,
        budget: PayloadBufferBudget,
        max_item_size: u64,
        completion_owner: Box<dyn Send>,
        cancellation: HttpStreamCancellation,
    ) -> Self {
        let (items_tx, items) = mpsc::channel(1);
        let (outcome_tx, outcome) = watch::channel(None);
        let (local_error_tx, local_error) = watch::channel(None);
        let cancel_for_delete = cancellation.clone();
        let mut delete_stop = outcome.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_for_delete.wait() => {},
                _ = delete_stop.changed() => return,
            }
            // ACK only submits intent. The POST actor remains authoritative.
            let _ = client
                .delete(format!("{base_url}/_calls/{token}"))
                .send()
                .await;
        });
        let actor_cancel = cancellation.clone();
        tokio::spawn(async move {
            let result = drive(
                response,
                budget,
                max_item_size,
                items_tx,
                local_error_tx,
                actor_cancel,
            )
            .await;
            // The operation slot is no longer occupied when close observes the
            // original terminal, matching IPC's publication order.
            drop(completion_owner);
            let _ = outcome_tx.send(Some(result));
        });
        Self {
            items,
            outcome,
            local_error,
            cancellation,
        }
    }

    pub async fn recv(&mut self) -> Result<Option<BudgetedBuffer>, HttpError> {
        let mut local_error_open = true;
        loop {
            if let Ok(item) = self.items.try_recv() {
                return Ok(Some(item));
            }
            if let Some(result) = self.receive_after_empty() {
                return result;
            }
            tokio::select! {
                item = self.items.recv() => match item {
                    Some(item) => return Ok(Some(item)),
                    None => {
                        if self.outcome.changed().await.is_err() {
                            return Err(HttpError::Transport("HTTP stream actor ended without an outcome".into()));
                        }
                    }
                },
                changed = self.local_error.changed(), if local_error_open => {
                    local_error_open = changed.is_ok();
                },
                changed = self.outcome.changed() => {
                    if changed.is_err() {
                        return Err(HttpError::Transport("HTTP stream actor ended without an outcome".into()));
                    }
                },
            }
        }
    }

    fn receive_after_empty(&mut self) -> Option<Result<Option<BudgetedBuffer>, HttpError>> {
        let local_error = self.local_error.borrow().clone();
        let outcome = self.outcome.borrow().clone();
        if local_error.is_some() || outcome.is_some() {
            // Publication follows all complete item sends. Recheck after
            // observing it: a final send can follow the caller's empty read.
            if let Ok(item) = self.items.try_recv() {
                return Some(Ok(Some(item)));
            }
        }
        if let Some(error) = local_error {
            return Some(Err(HttpError::LocalCallRejected(error)));
        }
        outcome.map(|outcome| {
            outcome.receive()?;
            Ok(None)
        })
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn completion(&self) -> Result<HttpStreamTerminal, HttpError> {
        let mut outcome = self.outcome.clone();
        loop {
            if let Some(value) = outcome.borrow().clone() {
                return value.completion();
            }
            if outcome.changed().await.is_err() {
                return Err(HttpError::Transport(
                    "HTTP stream actor ended without a terminal".into(),
                ));
            }
        }
    }

    /// Close drains the original response after cancellation. Undelivered items
    /// are discarded while delivered holds keep their own local owners.
    #[doc(hidden)]
    pub async fn completion_after_cancel(&mut self) -> Result<HttpStreamTerminal, HttpError> {
        let mut outcome = self.outcome.clone();
        loop {
            while self.items.try_recv().is_ok() {}
            if let Some(value) = outcome.borrow().clone() {
                while self.items.try_recv().is_ok() {}
                return value.completion();
            }
            tokio::select! {
                item = self.items.recv() => match item {
                    Some(item) => drop(item),
                    None => {
                        if outcome.changed().await.is_err() {
                            return Err(HttpError::Transport("HTTP stream actor ended without a terminal".into()));
                        }
                    }
                },
                changed = outcome.changed() => {
                    if changed.is_err() {
                        return Err(HttpError::Transport("HTTP stream actor ended without a terminal".into()));
                    }
                },
            }
        }
    }

    #[doc(hidden)]
    pub fn discard_buffered(&mut self) {
        while self.items.try_recv().is_ok() {}
    }

    pub fn try_completion(&self) -> Option<Result<HttpStreamTerminal, HttpError>> {
        self.outcome
            .borrow()
            .clone()
            .map(ObservedOutcome::completion)
    }
}

impl Drop for HttpResponseStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct BodyReader {
    response: reqwest::Response,
    pending: Bytes,
}

impl BodyReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            pending: Bytes::new(),
        }
    }

    async fn read_exact(&mut self, target: &mut [u8]) -> Result<bool, String> {
        let mut copied = 0;
        while copied < target.len() {
            if self.pending.is_empty() {
                self.pending = match self.response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => return Ok(false),
                    Err(error) => return Err(format!("HTTP stream body read failed: {error}")),
                };
                if self.pending.is_empty() {
                    continue;
                }
            }
            let take = (target.len() - copied).min(self.pending.len());
            target[copied..copied + take].copy_from_slice(&self.pending[..take]);
            self.pending.advance(take);
            copied += take;
        }
        Ok(true)
    }
}

use bytes::Buf;

struct ReceivingItem<'a> {
    total: usize,
    received: usize,
    bytes: Vec<u8>,
    reservation: Option<PayloadBufferReservation>,
    slot: Option<mpsc::Permit<'a, BudgetedBuffer>>,
    discarding: bool,
}

impl ReceivingItem<'_> {
    fn discard(&mut self) {
        self.slot.take();
        drop(std::mem::take(&mut self.bytes));
        self.reservation.take();
        self.discarding = true;
    }
}

fn protocol_failure(message: impl Into<String>) -> ObservedOutcome {
    ObservedOutcome::Failure(
        c2_error::C2Error::new(c2_error::ErrorCode::ProtocolViolation, message).with_details(
            std::collections::BTreeMap::from([
                ("transport".into(), "http".into()),
                ("transport_phase".into(), "dispatch_uncertain".into()),
                ("dispatch_phase".into(), "dispatch_uncertain".into()),
            ]),
        ),
    )
}

async fn drive(
    response: reqwest::Response,
    budget: PayloadBufferBudget,
    max_item_size: u64,
    items: mpsc::Sender<BudgetedBuffer>,
    local_error: watch::Sender<Option<c2_error::C2Error>>,
    cancellation: HttpStreamCancellation,
) -> ObservedOutcome {
    let mut reader = BodyReader::new(response);
    let mut seen_items = 0u64;
    let mut active: Option<ReceivingItem<'_>> = None;
    loop {
        let mut header = [0u8; stream_wire::RECORD_HEADER_LEN];
        match reader.read_exact(&mut header).await {
            Ok(true) => {}
            Ok(false) => {
                return ObservedOutcome::Transport("HTTP stream ended before CALL_END".into());
            }
            Err(error) => return ObservedOutcome::Transport(error),
        }
        let (kind, length) = match stream_wire::decode_header(header) {
            Ok(value) => value,
            Err(error) => return protocol_failure(error.to_string()),
        };
        let Ok(len) = usize::try_from(length) else {
            return protocol_failure("HTTP stream record length overflows usize");
        };
        match kind {
            HttpStreamRecordKind::ItemBegin => {
                if active.is_some() {
                    return protocol_failure("HTTP ITEM_BEGIN interrupted an incomplete item");
                }
                let mut total = [0u8; 8];
                match reader.read_exact(&mut total).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return ObservedOutcome::Transport(
                            "HTTP stream ended in ITEM_BEGIN".into(),
                        );
                    }
                    Err(error) => return ObservedOutcome::Transport(error),
                }
                let total = stream_wire::decode_item_begin(total);
                if total > max_item_size {
                    return protocol_failure(format!(
                        "HTTP item exceeds route payload limit: {total}"
                    ));
                }
                let Ok(total) = usize::try_from(total) else {
                    return protocol_failure("HTTP item length overflows usize");
                };
                let slot = if cancellation.requested() {
                    None
                } else {
                    tokio::select! {
                        slot = items.reserve() => slot.ok(),
                        _ = cancellation.wait() => None,
                    }
                };
                if slot.is_none() {
                    cancellation.cancel();
                }
                if total == 0 {
                    seen_items = seen_items.saturating_add(1);
                    if let Some(slot) = slot {
                        slot.send(BudgetedBuffer::new(
                            Vec::new(),
                            budget.try_reserve(0).expect("zero-byte item"),
                        ));
                    }
                    continue;
                }
                let discarding = slot.is_none();
                active = Some(ReceivingItem {
                    total,
                    received: 0,
                    bytes: Vec::new(),
                    reservation: None,
                    slot,
                    discarding,
                });
            }
            HttpStreamRecordKind::ItemData => {
                let Some(item) = active.as_mut() else {
                    return protocol_failure("HTTP ITEM_DATA has no ITEM_BEGIN");
                };
                if len > item.total - item.received {
                    return protocol_failure("HTTP ITEM_DATA exceeds the announced item length");
                }
                if cancellation.requested() {
                    item.discard();
                }
                if !item.discarding && item.reservation.is_none() {
                    // ITEM_BEGIN is fixed metadata. Admit the whole local item
                    // before copying its first data byte; a terminal after the
                    // begin can therefore be observed without waiting on B.
                    let reservation = tokio::select! {
                        reservation = budget.reserve(item.total as u64) => Some(reservation),
                        _ = cancellation.wait() => None,
                    };
                    match reservation {
                        Some(Ok(reservation)) => {
                            if let Err(error) = item.bytes.try_reserve_exact(item.total) {
                                let _ =
                                    local_error.send(Some(local_capacity_error(error.to_string())));
                                cancellation.cancel();
                                item.discard();
                            } else if item.bytes.capacity() as u64 > reservation.bytes() {
                                return protocol_failure(
                                    "HTTP item allocation exceeded its reservation",
                                );
                            } else {
                                item.bytes.resize(item.total, 0);
                                item.reservation = Some(reservation);
                            }
                        }
                        Some(Err(error)) => {
                            let _ = local_error.send(Some(local_capacity_error(error.to_string())));
                            cancellation.cancel();
                            item.discard();
                        }
                        None => item.discard(),
                    }
                }
                if item.discarding {
                    if let Err(error) = discard(&mut reader, len).await {
                        return ObservedOutcome::Transport(error);
                    }
                } else {
                    let end = item.received + len;
                    match reader.read_exact(&mut item.bytes[item.received..end]).await {
                        Ok(true) => {}
                        Ok(false) => {
                            return ObservedOutcome::Transport(
                                "HTTP stream ended mid ITEM_DATA".into(),
                            );
                        }
                        Err(error) => return ObservedOutcome::Transport(error),
                    }
                }
                item.received += len;
                if item.received == item.total {
                    let mut item = active.take().expect("complete active item");
                    seen_items = seen_items.saturating_add(1);
                    if cancellation.requested() {
                        item.discard();
                    }
                    if let Some(slot) = item.slot {
                        slot.send(BudgetedBuffer::new(
                            item.bytes,
                            item.reservation.expect("admitted complete item"),
                        ));
                    }
                }
            }
            HttpStreamRecordKind::CallEnd | HttpStreamRecordKind::Failure => {
                let mut payload = Vec::new();
                if let Err(error) = payload.try_reserve_exact(len) {
                    return ObservedOutcome::Transport(format!(
                        "HTTP terminal allocation failed: {error}"
                    ));
                }
                payload.resize(len, 0);
                match reader.read_exact(&mut payload).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return ObservedOutcome::Transport("HTTP stream ended mid terminal".into());
                    }
                    Err(error) => return ObservedOutcome::Transport(error),
                }
                if kind == HttpStreamRecordKind::Failure {
                    drop(active);
                    return match stream_wire::decode_failure(&payload) {
                        Ok(error) => ObservedOutcome::Failure(error),
                        Err(error) => protocol_failure(error.to_string()),
                    };
                }
                let terminal = match stream_wire::decode_terminal(&payload) {
                    Ok(terminal) => terminal,
                    Err(error) => return protocol_failure(error.to_string()),
                };
                if active.is_some() && terminal.status != HttpStreamStatus::Failed {
                    return protocol_failure(
                        "HTTP CALL_END has an incomplete item without Failed status",
                    );
                }
                // A real failed terminal owns this outcome. The unfinished
                // item is discarded; previously delivered item owners survive.
                drop(active);
                return checked_terminal(terminal, seen_items, cancellation.requested());
            }
        }
    }
}

fn checked_terminal(
    mut terminal: HttpStreamTerminal,
    seen_items: u64,
    cancelled: bool,
) -> ObservedOutcome {
    if terminal.status == HttpStreamStatus::NotExecuted {
        if seen_items != 0 {
            return ObservedOutcome::Transport("HTTP stream NotExecuted followed an item".into());
        }
        let error = terminal.error.as_mut().expect("checked NotExecuted error");
        error
            .details
            .insert("transport_phase".into(), "pre_dispatch".into());
        error
            .details
            .insert("dispatch_phase".into(), "pre_dispatch".into());
    } else if terminal
        .complete_items
        .is_none_or(|count| count < seen_items || (!cancelled && count != seen_items))
    {
        return ObservedOutcome::Transport(
            "HTTP stream CALL_END complete item count mismatch".into(),
        );
    }
    ObservedOutcome::Terminal(terminal)
}

fn local_capacity_error(message: String) -> c2_error::C2Error {
    c2_error::C2Error::new(c2_error::ErrorCode::CallCapacityExceeded, message).with_details(
        std::collections::BTreeMap::from([
            ("transport".into(), "http".into()),
            ("transport_phase".into(), "dispatch_uncertain".into()),
            ("stage".into(), "payload_buffer_admission".into()),
        ]),
    )
}

async fn discard(reader: &mut BodyReader, len: usize) -> Result<(), String> {
    let mut remaining = len;
    let mut scratch = [0u8; 4096];
    while remaining != 0 {
        let chunk = remaining.min(scratch.len());
        match reader.read_exact(&mut scratch[..chunk]).await {
            Ok(true) => remaining -= chunk,
            Ok(false) => return Err("HTTP stream ended mid-item".into()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "relay"))]
mod tests {
    use super::*;
    use crate::stream_wire::{encode_header, encode_terminal};
    use axum::{
        Router,
        body::Body,
        routing::{delete, post},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    fn item_prefix(total: u64) -> Vec<u8> {
        let mut bytes = stream_wire::encode_item_begin(total).to_vec();
        if total != 0 {
            bytes.extend_from_slice(&encode_header(HttpStreamRecordKind::ItemData, total));
        }
        bytes
    }

    fn terminal(status: HttpStreamStatus, count: u64) -> Vec<u8> {
        encode_terminal(&HttpStreamTerminal {
            status,
            complete_items: Some(count),
            error: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn shared_sequences_preserve_partial_failure_and_reject_malformed_items() {
        #[derive(serde::Deserialize)]
        struct Sequence {
            name: String,
            body_hex: String,
            items_hex: Vec<String>,
            error_code: Option<u16>,
            complete_items: Option<u64>,
            protocol_error: bool,
        }
        fn bytes(hex: &str) -> Vec<u8> {
            hex.as_bytes()
                .chunks_exact(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect()
        }
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/stream-format-v1.json"))
                .unwrap();
        let sequences: Vec<Sequence> =
            serde_json::from_value(fixture["sequences"].clone()).unwrap();
        for sequence in sequences {
            let body = bytes(&sequence.body_hex);
            let router = Router::new().route(
                "/call",
                post(move || {
                    let body = body.clone();
                    async move { body }
                }),
            );
            let (base, server) = serve(router).await;
            let client = reqwest::Client::new();
            let response = client.post(format!("{base}/call")).send().await.unwrap();
            let budget = PayloadBufferBudget::new(32);
            let mut stream = HttpResponseStream::start(
                response,
                client,
                base,
                "0123456789abcdef0123456789abcdef".into(),
                budget.clone(),
                32,
                Box::new(()),
                HttpStreamCancellation::new(),
            );
            let mut held = Vec::new();
            for expected in &sequence.items_hex {
                let item = stream.recv().await.unwrap().unwrap();
                assert_eq!(item.as_ref(), bytes(expected), "{}", sequence.name);
                held.push(item);
            }
            let result = stream.recv().await;
            if sequence.protocol_error {
                assert!(
                    matches!(result, Err(HttpError::LocalCallRejected(error))
                    if error.code == c2_error::ErrorCode::ProtocolViolation),
                    "{}",
                    sequence.name
                );
                assert!(stream.completion().await.is_err());
            } else {
                match sequence.error_code {
                    Some(code) => assert!(
                        matches!(result, Err(HttpError::LocalCallRejected(error))
                        if error.code as u16 == code),
                        "{}",
                        sequence.name
                    ),
                    None => assert!(matches!(result, Ok(None)), "{}", sequence.name),
                }
                assert_eq!(
                    stream.completion().await.unwrap().complete_items,
                    sequence.complete_items,
                    "{}",
                    sequence.name
                );
            }
            assert_eq!(
                budget.snapshot().used_bytes,
                held.iter()
                    .map(|item| item.as_ref().len() as u64)
                    .sum::<u64>(),
                "{}",
                sequence.name
            );
            drop(held);
            assert_eq!(budget.snapshot().used_bytes, 0);
            server.abort();
        }
    }

    #[test]
    fn item_published_after_empty_is_delivered_before_its_terminal() {
        use std::sync::Barrier;
        for status in [HttpStreamStatus::Success, HttpStreamStatus::Failed] {
            let budget = PayloadBufferBudget::new(8);
            let (items_tx, items) = mpsc::channel(1);
            let (outcome_tx, outcome) = watch::channel(None);
            let (_local_error_tx, local_error) = watch::channel(None);
            let mut stream = HttpResponseStream {
                items,
                outcome,
                local_error,
                cancellation: HttpStreamCancellation::new(),
            };
            let empty = Arc::new(Barrier::new(2));
            let published = Arc::new(Barrier::new(2));
            let worker = std::thread::spawn({
                let empty = empty.clone();
                let published = published.clone();
                let budget = budget.clone();
                move || {
                    empty.wait();
                    items_tx
                        .blocking_send(BudgetedBuffer::new(
                            vec![42; 8],
                            budget.try_reserve(8).unwrap(),
                        ))
                        .unwrap();
                    outcome_tx
                        .send(Some(ObservedOutcome::Terminal(HttpStreamTerminal {
                            status,
                            complete_items: Some(1),
                            error: (status == HttpStreamStatus::Failed).then(|| {
                                c2_error::C2Error::unknown("resource failed after its item")
                            }),
                        })))
                        .unwrap();
                    published.wait();
                }
            });
            assert!(matches!(
                stream.items.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            empty.wait();
            published.wait();
            let result = stream
                .receive_after_empty()
                .expect("item or terminal available");
            let item = match result {
                Ok(Some(item)) => item,
                _ => panic!("queued final item must precede its published terminal"),
            };
            assert_eq!(item.as_ref(), &[42; 8]);
            drop(item);
            let terminal = stream.receive_after_empty().unwrap();
            match status {
                HttpStreamStatus::Success => assert!(matches!(terminal, Ok(None))),
                HttpStreamStatus::Failed => {
                    assert!(matches!(terminal, Err(HttpError::LocalCallRejected(_))))
                }
                _ => unreachable!(),
            }
            worker.join().unwrap();
            assert_eq!(budget.snapshot().used_bytes, 0);
        }
    }

    #[tokio::test]
    async fn oversized_terminal_header_is_rejected_before_allocation_or_body_read() {
        for kind in [HttpStreamRecordKind::CallEnd, HttpStreamRecordKind::Failure] {
            // An HTTP payload includes status/count, whereas IPC also has its
            // kind byte and the twelve RID/flags bytes in total_len.
            let length = u64::from(u32::MAX) - 12;
            let header = encode_header(kind, length).to_vec();
            let router = Router::new().route(
                "/call",
                post(move || {
                    let header = header.clone();
                    async move { header }
                }),
            );
            let (base, server) = serve(router).await;
            let client = reqwest::Client::new();
            let response = client.post(format!("{base}/call")).send().await.unwrap();
            let budget = PayloadBufferBudget::new(0);
            let stream = HttpResponseStream::start(
                response,
                client,
                base,
                "0123456789abcdef0123456789abcdef".into(),
                budget.clone(),
                1,
                Box::new(()),
                HttpStreamCancellation::new(),
            );
            let error = match stream.completion().await {
                Err(HttpError::LocalCallRejected(error)) => error,
                _ => panic!("expected a structured protocol failure"),
            };
            assert_eq!(error.code, c2_error::ErrorCode::ProtocolViolation);
            assert!(
                error
                    .message
                    .contains("terminal payload exceeds IPC CALL_END limit")
            );
            assert_eq!(error.details["transport_phase"], "dispatch_uncertain");
            assert_eq!(error.details["dispatch_phase"], "dispatch_uncertain");
            assert_eq!(budget.snapshot().rejected_allocations, 0);
            server.abort();
        }
    }

    #[tokio::test]
    async fn terminal_count_mismatch_keeps_previous_item_but_refuses_success_or_failure() {
        for status in [HttpStreamStatus::Success, HttpStreamStatus::Failed] {
            let mut body = item_prefix(1);
            body.push(42);
            body.extend_from_slice(
                &encode_terminal(&HttpStreamTerminal {
                    status,
                    complete_items: Some(0),
                    error: (status == HttpStreamStatus::Failed)
                        .then(|| c2_error::C2Error::unknown("resource failed")),
                })
                .unwrap(),
            );
            let router = Router::new().route(
                "/call",
                post(move || {
                    let body = body.clone();
                    async move { body }
                }),
            );
            let (base, server) = serve(router).await;
            let client = reqwest::Client::new();
            let response = client.post(format!("{base}/call")).send().await.unwrap();
            let mut stream = HttpResponseStream::start(
                response,
                client,
                base,
                "0123456789abcdef0123456789abcdef".into(),
                PayloadBufferBudget::new(4096),
                4096,
                Box::new(()),
                HttpStreamCancellation::new(),
            );
            let item = stream.recv().await.unwrap().unwrap();
            assert_eq!(item.as_ref(), &[42]);
            assert!(
                matches!(stream.recv().await, Err(HttpError::Transport(message)) if message.contains("count mismatch"))
            );
            assert!(
                matches!(stream.completion().await, Err(HttpError::Transport(message)) if message.contains("count mismatch"))
            );
            assert_eq!(item.as_ref(), &[42]);
            server.abort();
        }
    }

    #[tokio::test]
    async fn not_executed_terminal_projects_pre_dispatch_for_receive_and_receipt() {
        let body = encode_terminal(&HttpStreamTerminal {
            status: HttpStreamStatus::NotExecuted,
            complete_items: None,
            error: Some(c2_error::C2Error::new(
                c2_error::ErrorCode::CallCancelled,
                "cancelled before resource entry",
            )),
        })
        .unwrap();
        let router = Router::new().route(
            "/call",
            post(move || {
                let body = body.clone();
                async move { body }
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            PayloadBufferBudget::new(4096),
            4096,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        let error = match stream.recv().await {
            Err(HttpError::LocalCallRejected(error)) => error,
            _ => panic!("expected structured refusal"),
        };
        assert_eq!(error.details["transport_phase"], "pre_dispatch");
        assert_eq!(error.details["dispatch_phase"], "pre_dispatch");
        let terminal = stream.completion().await.unwrap();
        assert_eq!(terminal.status, HttpStreamStatus::NotExecuted);
        assert_eq!(
            terminal.error.unwrap().details["transport_phase"],
            "pre_dispatch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn complete_items_hold_budget_until_each_owner_drops() {
        let mut body = Vec::new();
        body.extend_from_slice(&item_prefix(3));
        body.extend_from_slice(b"one");
        body.extend_from_slice(&item_prefix(3));
        body.extend_from_slice(b"two");
        body.extend_from_slice(&terminal(HttpStreamStatus::Success, 2));
        let router = Router::new().route(
            "/call",
            post(move || async move {
                (
                    [
                        ("content-type", crate::stream_wire::MEDIA_TYPE),
                        ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                    ],
                    body.clone(),
                )
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let budget = PayloadBufferBudget::new(12);
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            budget.clone(),
            128,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        let first = stream.recv().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"one");
        let second = stream.recv().await.unwrap().unwrap();
        assert_eq!(second.as_ref(), b"two");
        assert_eq!(budget.snapshot().used_bytes, 6);
        drop(first);
        drop(second);
        assert!(stream.recv().await.unwrap().is_none());
        assert_eq!(stream.completion().await.unwrap().complete_items, Some(2));
        assert_eq!(budget.snapshot().used_bytes, 0);
        server.abort();
    }

    #[tokio::test]
    async fn empty_stream_finishes_with_one_byte_payload_budget() {
        let body = terminal(HttpStreamStatus::Success, 0);
        let router = Router::new().route(
            "/call",
            post(move || async move {
                (
                    [
                        ("content-type", crate::stream_wire::MEDIA_TYPE),
                        ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                    ],
                    body.clone(),
                )
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let budget = PayloadBufferBudget::new(1);
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            budget.clone(),
            128,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        assert!(stream.recv().await.unwrap().is_none());
        assert_eq!(stream.completion().await.unwrap().complete_items, Some(0));
        assert_eq!(budget.snapshot().used_bytes, 0);
        server.abort();
    }

    #[tokio::test]
    async fn incomplete_item_is_not_published_and_earlier_item_remains_owned() {
        let mut body = Vec::new();
        body.extend_from_slice(&item_prefix(3));
        body.extend_from_slice(b"one");
        body.extend_from_slice(&item_prefix(4));
        body.extend_from_slice(b"tw");
        let router = Router::new().route(
            "/call",
            post(move || async move {
                (
                    [
                        ("content-type", crate::stream_wire::MEDIA_TYPE),
                        ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                    ],
                    body.clone(),
                )
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let budget = PayloadBufferBudget::new(16);
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            budget.clone(),
            128,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        let first = stream.recv().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"one");
        assert!(matches!(stream.recv().await, Err(HttpError::Transport(_))));
        assert_eq!(first.as_ref(), b"one");
        assert_eq!(budget.snapshot().used_bytes, 3);
        drop(first);
        assert_eq!(budget.snapshot().used_bytes, 0);
        server.abort();
    }

    #[tokio::test]
    async fn oversized_item_refuses_local_delivery_but_keeps_post_terminal() {
        let mut body = Vec::new();
        body.extend_from_slice(&item_prefix(2));
        body.extend_from_slice(b"ab");
        body.extend_from_slice(&terminal(HttpStreamStatus::Success, 1));
        let router = Router::new().route(
            "/call",
            post(move || async move {
                (
                    [
                        ("content-type", crate::stream_wire::MEDIA_TYPE),
                        ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                    ],
                    body.clone(),
                )
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            PayloadBufferBudget::new(1),
            128,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        let error = stream.recv().await.expect_err("item exceeds B");
        assert!(matches!(error, HttpError::LocalCallRejected(error)
            if error.code == c2_error::ErrorCode::CallCapacityExceeded));
        assert_eq!(stream.completion().await.unwrap().complete_items, Some(1));
        server.abort();
    }

    #[tokio::test]
    async fn delete_submits_intent_and_original_post_reports_terminal() {
        let changed = Arc::new(Notify::new());
        let deletes = Arc::new(AtomicUsize::new(0));
        let post_changed = changed.clone();
        let delete_changed = changed.clone();
        let delete_count = deletes.clone();
        let router = Router::new()
            .route(
                "/call",
                post(move || {
                    let changed = post_changed.clone();
                    async move {
                        let body = Body::from_stream(futures::stream::unfold(
                            Some(changed),
                            |state| async move {
                                let changed = state?;
                                changed.notified().await;
                                Some((
                                    Ok::<_, std::io::Error>(bytes::Bytes::from(terminal(
                                        HttpStreamStatus::Success,
                                        0,
                                    ))),
                                    None,
                                ))
                            },
                        ));
                        (
                            [
                                ("content-type", crate::stream_wire::MEDIA_TYPE),
                                ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                            ],
                            body,
                        )
                    }
                }),
            )
            .route(
                "/_calls/{token}",
                delete(move || {
                    let changed = delete_changed.clone();
                    let count = delete_count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        changed.notify_one();
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let cancellation = HttpStreamCancellation::new();
        let stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            PayloadBufferBudget::new(128),
            128,
            Box::new(()),
            cancellation.clone(),
        );
        cancellation.cancel();
        cancellation.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), stream.completion())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.complete_items, Some(0));
        assert_eq!(deletes.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_mid_item_discards_remainder_then_reads_call_end() {
        let first_sent = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let post_first = first_sent.clone();
        let post_resume = resume.clone();
        let delete_resume = resume.clone();
        let router = Router::new()
            .route(
                "/call",
                post(move || {
                    let first_sent = post_first.clone();
                    let resume = post_resume.clone();
                    async move {
                        let body = Body::from_stream(futures::stream::unfold(
                            (0u8, first_sent, resume),
                            |(stage, first_sent, resume)| async move {
                                match stage {
                                    0 => {
                                        let mut bytes = item_prefix(2);
                                        bytes.push(b'a');
                                        first_sent.notify_one();
                                        Some((
                                            Ok::<_, std::io::Error>(Bytes::from(bytes)),
                                            (1, first_sent, resume),
                                        ))
                                    }
                                    1 => {
                                        resume.notified().await;
                                        let mut bytes = vec![b'b'];
                                        bytes.extend_from_slice(&terminal(
                                            HttpStreamStatus::Success,
                                            1,
                                        ));
                                        Some((
                                            Ok::<_, std::io::Error>(Bytes::from(bytes)),
                                            (2, first_sent, resume),
                                        ))
                                    }
                                    _ => None,
                                }
                            },
                        ));
                        (
                            [
                                ("content-type", crate::stream_wire::MEDIA_TYPE),
                                ("x-c2-call-token", "0123456789abcdef0123456789abcdef"),
                            ],
                            body,
                        )
                    }
                }),
            )
            .route(
                "/_calls/{token}",
                delete(move || {
                    let resume = delete_resume.clone();
                    async move {
                        resume.notify_one();
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let cancellation = HttpStreamCancellation::new();
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            PayloadBufferBudget::new(8),
            128,
            Box::new(()),
            cancellation.clone(),
        );
        first_sent.notified().await;
        cancellation.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), stream.completion())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.complete_items, Some(1));
        assert!(stream.recv().await.unwrap().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn close_discards_queued_items_while_delivered_holds_remain_charged() {
        let mut body = Vec::new();
        for byte in [7, 8] {
            body.extend_from_slice(&item_prefix(8));
            body.extend_from_slice(&[byte; 8]);
        }
        body.extend_from_slice(
            &encode_terminal(&HttpStreamTerminal {
                status: HttpStreamStatus::Failed,
                complete_items: Some(2),
                error: Some(c2_error::C2Error::new(
                    c2_error::ErrorCode::CallCancelled,
                    "consumer cancelled the stream",
                )),
            })
            .unwrap(),
        );
        let router = Router::new().route(
            "/call",
            post(move || {
                let body = body.clone();
                async move { body }
            }),
        );
        let (base, server) = serve(router).await;
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/call")).send().await.unwrap();
        let budget = PayloadBufferBudget::new(16);
        let mut stream = HttpResponseStream::start(
            response,
            client,
            base,
            "0123456789abcdef0123456789abcdef".into(),
            budget.clone(),
            8,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        let held = stream.recv().await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stream.items.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(budget.snapshot().used_bytes, 16);
        stream.cancel();
        let terminal = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.completion_after_cancel(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(terminal.complete_items, Some(2));
        assert_eq!(
            terminal.error.unwrap().code,
            c2_error::ErrorCode::CallCancelled
        );
        assert_eq!(held.as_ref(), &[7; 8]);
        assert_eq!(budget.snapshot().used_bytes, 8);
        drop(held);
        assert_eq!(budget.snapshot().used_bytes, 0);
        server.abort();
    }
}
