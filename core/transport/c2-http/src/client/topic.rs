//! One HTTP topic response and its independently cancellable receive actor.
use bytes::{Buf, Bytes};
use c2_error::{C2Error, ErrorCode};
use c2_mem::{BudgetedBuffer, DomainBudget, MemoryReservation};
use c2_wire::rpc::WireLagPolicy;
use tokio::sync::{mpsc, oneshot, watch};

use super::{HttpError, HttpStreamCancellation};
use crate::topic_wire::{self, RecordKind, Status, Terminal};

pub struct HttpTopicDelivery {
    pub bytes: BudgetedBuffer,
    pub seq: u64,
    pub dropped: u64,
}

#[derive(Clone)]
enum Outcome {
    Terminal(Terminal),
    Failure(C2Error),
    Transport(String),
}
impl Outcome {
    fn completion(self) -> Result<Terminal, HttpError> {
        match self {
            Self::Terminal(value) => Ok(value),
            Self::Failure(error) => Err(HttpError::LocalCallRejected(error)),
            Self::Transport(error) => Err(HttpError::Transport(error)),
        }
    }
    fn receive(self) -> Result<HttpTopicDelivery, HttpError> {
        match self {
            Self::Terminal(value) => Err(HttpError::LocalCallRejected(value.error)),
            Self::Failure(error) => Err(HttpError::LocalCallRejected(error)),
            Self::Transport(error) => Err(HttpError::Transport(error)),
        }
    }
}

pub struct HttpTopicSubscription {
    items: mpsc::Receiver<HttpTopicDelivery>,
    outcome: watch::Receiver<Option<Outcome>>,
    local_error: watch::Receiver<Option<C2Error>>,
    cancellation: HttpStreamCancellation,
    start_seq: u64,
}

impl HttpTopicSubscription {
    pub(crate) async fn start(
        response: reqwest::Response,
        client: reqwest::Client,
        base_url: String,
        token: String,
        budget: DomainBudget,
        lag: WireLagPolicy,
        completion_owner: Box<dyn Send>,
        cancellation: HttpStreamCancellation,
    ) -> Result<Self, HttpError> {
        let (items_tx, items) = mpsc::channel(1);
        let (outcome_tx, outcome) = watch::channel(None);
        let (local_tx, local_error) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        let cancel_for_delete = cancellation.clone();
        let mut delete_stop = outcome.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_for_delete.wait() => {},
                _ = delete_stop.changed() => return,
            }
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
                lag,
                items_tx,
                local_tx,
                actor_cancel,
                ready_tx,
            )
            .await;
            drop(completion_owner);
            let _ = outcome_tx.send(Some(result));
        });
        let mut sub = Self {
            items,
            outcome,
            local_error,
            cancellation,
            start_seq: 0,
        };
        match ready_rx.await {
            Ok(Ok(start_seq)) => {
                sub.start_seq = start_seq;
                Ok(sub)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(HttpError::Transport("HTTP topic ended before READY".into())),
        }
    }

    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn recv(&mut self) -> Result<HttpTopicDelivery, HttpError> {
        loop {
            if let Ok(item) = self.items.try_recv() {
                return Ok(item);
            }
            let outcome = self.outcome.borrow().clone();
            let local = self.local_error.borrow().clone();
            if outcome.is_some() || local.is_some() {
                if let Ok(item) = self.items.try_recv() {
                    return Ok(item);
                }
            }
            if let Some(error) = local {
                return Err(HttpError::LocalCallRejected(error));
            }
            if let Some(outcome) = outcome {
                return outcome.receive();
            }
            tokio::select! {
                item = self.items.recv() => if let Some(item) = item { return Ok(item); },
                _ = self.outcome.changed() => {},
                _ = self.local_error.changed() => {},
            }
        }
    }

    pub async fn completion_after_cancel(&mut self) -> Result<Terminal, HttpError> {
        loop {
            while self.items.try_recv().is_ok() {}
            if let Some(outcome) = self.outcome.borrow().clone() {
                return outcome.completion();
            }
            tokio::select! {
                item = self.items.recv() => { drop(item); },
                changed = self.outcome.changed() => if changed.is_err() { return Err(HttpError::Transport("HTTP topic actor ended without terminal".into())); },
            }
        }
    }

    pub fn try_completion(&self) -> Option<Result<Terminal, HttpError>> {
        self.outcome.borrow().clone().map(Outcome::completion)
    }
}
impl Drop for HttpTopicSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Reader {
    response: reqwest::Response,
    pending: Bytes,
}
impl Reader {
    async fn read(&mut self, dst: &mut [u8]) -> Result<(), String> {
        let mut at = 0;
        while at < dst.len() {
            if self.pending.is_empty() {
                self.pending = self
                    .response
                    .chunk()
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "HTTP topic ended before terminal".to_string())?;
                if self.pending.is_empty() {
                    continue;
                }
            }
            let n = (dst.len() - at).min(self.pending.len());
            dst[at..at + n].copy_from_slice(&self.pending[..n]);
            self.pending.advance(n);
            at += n;
        }
        Ok(())
    }
    async fn discard(&mut self, mut len: usize) -> Result<(), String> {
        let mut scratch = [0; 4096];
        while len > 0 {
            let n = len.min(scratch.len());
            self.read(&mut scratch[..n]).await?;
            len -= n;
        }
        Ok(())
    }
}

fn protocol(message: impl Into<String>) -> C2Error {
    C2Error::new(ErrorCode::ProtocolViolation, message).with_details(
        std::collections::BTreeMap::from([
            ("transport".into(), "http".into()),
            ("transport_phase".into(), "dispatch_uncertain".into()),
            ("dispatch_phase".into(), "dispatch_uncertain".into()),
        ]),
    )
}

struct Active<'a> {
    seq: u64,
    dropped: u64,
    total: usize,
    received: usize,
    bytes: Vec<u8>,
    reservation: Option<MemoryReservation>,
    slot: Option<mpsc::Permit<'a, HttpTopicDelivery>>,
}

async fn drive(
    response: reqwest::Response,
    budget: DomainBudget,
    lag: WireLagPolicy,
    items: mpsc::Sender<HttpTopicDelivery>,
    local_error: watch::Sender<Option<C2Error>>,
    cancellation: HttpStreamCancellation,
    ready_tx: oneshot::Sender<Result<u64, HttpError>>,
) -> Outcome {
    let mut ready = Some(ready_tx);
    let result = drive_records(
        Reader {
            response,
            pending: Bytes::new(),
        },
        &budget,
        lag,
        &items,
        &local_error,
        &cancellation,
        &mut ready,
    )
    .await;
    if let Some(ready) = ready {
        let error = match &result {
            Outcome::Terminal(t) => {
                let mut error = t.error.clone();
                if t.status == Status::NotExecuted {
                    error
                        .details
                        .insert("transport_phase".into(), "pre_dispatch".into());
                    error
                        .details
                        .insert("dispatch_phase".into(), "pre_dispatch".into());
                }
                HttpError::LocalCallRejected(error)
            }
            Outcome::Failure(e) => HttpError::LocalCallRejected(e.clone()),
            Outcome::Transport(e) => HttpError::Transport(e.clone()),
        };
        let _ = ready.send(Err(error));
    }
    result
}

async fn drive_records(
    mut reader: Reader,
    budget: &DomainBudget,
    lag: WireLagPolicy,
    items: &mpsc::Sender<HttpTopicDelivery>,
    local_error: &watch::Sender<Option<C2Error>>,
    cancellation: &HttpStreamCancellation,
    ready: &mut Option<oneshot::Sender<Result<u64, HttpError>>>,
) -> Outcome {
    let mut expected = 0u64;
    let mut exhausted = false;
    let mut complete = 0u64;
    let mut active: Option<Active<'_>> = None;
    loop {
        let mut hdr = [0; topic_wire::RECORD_HEADER_LEN];
        if let Err(e) = reader.read(&mut hdr).await {
            return Outcome::Transport(e);
        }
        let (kind, len) = match topic_wire::decode_header(hdr) {
            Ok(x) => x,
            Err(e) => return Outcome::Failure(protocol(e)),
        };
        let Ok(len) = usize::try_from(len) else {
            return Outcome::Failure(protocol("topic record too large"));
        };
        match kind {
            RecordKind::Ready => {
                if expected != 0 {
                    return Outcome::Failure(protocol("duplicate READY"));
                }
                let mut bytes = [0; 8];
                if let Err(e) = reader.read(&mut bytes).await {
                    return Outcome::Transport(e);
                }
                expected = u64::from_le_bytes(bytes);
                if expected == 0 {
                    return Outcome::Failure(protocol("zero READY sequence"));
                }
                if let Some(ready) = ready.take() {
                    if ready.send(Ok(expected)).is_err() {
                        cancellation.cancel();
                    }
                }
            }
            RecordKind::ItemBegin => {
                if expected == 0 || active.is_some() {
                    return Outcome::Failure(protocol("unexpected ITEM_BEGIN"));
                }
                let mut bytes = [0; 16];
                if let Err(e) = reader.read(&mut bytes).await {
                    return Outcome::Transport(e);
                }
                let seq = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                let total = u64::from_le_bytes(bytes[8..].try_into().unwrap());
                if exhausted || seq < expected || (lag == WireLagPolicy::Error && seq != expected) {
                    return Outcome::Failure(protocol("invalid topic sequence"));
                }
                let dropped = seq - expected;
                expected = seq.saturating_add(1);
                exhausted = seq == u64::MAX;
                let Ok(total) = usize::try_from(total) else {
                    return Outcome::Failure(protocol("topic item too large"));
                };
                let slot = if cancellation.requested() {
                    None
                } else {
                    tokio::select! { slot = items.reserve() => slot.ok(), _ = cancellation.wait() => None }
                };
                if slot.is_none() {
                    cancellation.cancel();
                }
                if total == 0 {
                    complete = complete.saturating_add(1);
                    if let Some(slot) = slot {
                        slot.send(HttpTopicDelivery {
                            bytes: BudgetedBuffer::new(
                                Vec::new(),
                                budget.try_reserve_memory(0).unwrap(),
                            ),
                            seq,
                            dropped,
                        });
                    }
                } else {
                    active = Some(Active {
                        seq,
                        dropped,
                        total,
                        received: 0,
                        bytes: Vec::new(),
                        reservation: None,
                        slot,
                    });
                }
            }
            RecordKind::ItemData => {
                let Some(item) = active.as_mut() else {
                    return Outcome::Failure(protocol("ITEM_DATA without ITEM_BEGIN"));
                };
                if len > item.total - item.received {
                    return Outcome::Failure(protocol("ITEM_DATA exceeds announced size"));
                }
                if cancellation.requested() {
                    item.slot.take();
                    drop(std::mem::take(&mut item.bytes));
                    item.reservation.take();
                }
                if item.slot.is_some() && item.reservation.is_none() {
                    let reservation = tokio::select! { result = budget.reserve_memory(item.total as u64) => Some(result), _ = cancellation.wait() => None };
                    match reservation {
                        Some(Ok(reservation)) => {
                            if item.bytes.try_reserve_exact(item.total).is_err()
                                || item.bytes.capacity() as u64 > reservation.bytes()
                            {
                                let _ = local_error.send(Some(C2Error::new(
                                    ErrorCode::CallCapacityExceeded,
                                    "topic receive allocation failed",
                                )));
                                item.slot.take();
                                cancellation.cancel();
                            } else {
                                item.bytes.resize(item.total, 0);
                                item.reservation = Some(reservation);
                            }
                        }
                        Some(Err(e)) => {
                            let _ = local_error.send(Some(C2Error::new(
                                ErrorCode::CallCapacityExceeded,
                                e.to_string(),
                            )));
                            item.slot.take();
                            cancellation.cancel();
                        }
                        None => {
                            item.slot.take();
                        }
                    }
                }
                if item.slot.is_some() {
                    if let Err(e) = reader
                        .read(&mut item.bytes[item.received..item.received + len])
                        .await
                    {
                        return Outcome::Transport(e);
                    }
                } else if let Err(e) = reader.discard(len).await {
                    return Outcome::Transport(e);
                }
                item.received += len;
                if item.received == item.total {
                    let item = active.take().unwrap();
                    complete = complete.saturating_add(1);
                    if let Some(slot) = item.slot {
                        slot.send(HttpTopicDelivery {
                            bytes: BudgetedBuffer::new(item.bytes, item.reservation.unwrap()),
                            seq: item.seq,
                            dropped: item.dropped,
                        });
                    }
                }
            }
            RecordKind::Terminal => {
                let mut bytes = vec![0; len];
                if let Err(e) = reader.read(&mut bytes).await {
                    return Outcome::Transport(e);
                }
                let terminal = match topic_wire::decode_terminal(&bytes) {
                    Ok(x) => x,
                    Err(e) => return Outcome::Failure(protocol(e)),
                };
                if (active.is_some() && terminal.status != Status::Failed)
                    || terminal.complete_items < complete
                    || (!cancellation.requested() && terminal.complete_items != complete)
                    || (expected == 0 && terminal.status != Status::NotExecuted)
                    || (expected != 0 && terminal.status == Status::NotExecuted)
                {
                    return Outcome::Failure(protocol("invalid topic terminal ordering or count"));
                }
                return Outcome::Terminal(terminal);
            }
        }
    }
}

#[cfg(all(test, feature = "relay"))]
mod tests {
    use super::*;
    use axum::{Router, body::Body, response::IntoResponse, routing::get};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct Completion(Arc<AtomicBool>);
    impl Drop for Completion {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn ready_sequence_payload_and_terminal_are_observed_in_order() {
        let mut body = topic_wire::ready(1).unwrap();
        body.extend(topic_wire::item_begin(3, 3).unwrap());
        body.extend(topic_wire::header(RecordKind::ItemData, 3));
        body.extend(b"abc");
        body.extend(topic_wire::terminal(&Terminal {
            status: Status::Failed,
            complete_items: 1,
            error: C2Error::new(ErrorCode::CallCancelled, "route unregistered"),
        }));
        let app = Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { Body::from(body).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let limits = c2_config::DomainLimits {
            memory_budget_bytes: 1024,
            ..Default::default()
        };
        let mut sub = HttpTopicSubscription::start(
            response,
            reqwest::Client::new(),
            format!("http://{address}"),
            "00000000000000000000000000000000".into(),
            DomainBudget::from_limits(&limits),
            WireLagPolicy::Skip,
            Box::new(Completion(completed.clone())),
            HttpStreamCancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(sub.start_seq(), 1);
        let delivery = tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (delivery.seq, delivery.dropped, delivery.bytes.as_ref()),
            (3, 2, b"abc".as_slice())
        );
        let terminal = tokio::time::timeout(Duration::from_secs(2), sub.completion_after_cancel())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.error.code, ErrorCode::CallCancelled);
        assert!(completed.load(Ordering::SeqCst));
        server.abort();
    }

    #[tokio::test]
    async fn interrupted_http_body_reports_transport_failure_without_partial_delivery() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut body = topic_wire::ready(1).unwrap();
        body.extend(topic_wire::item_begin(1, 3).unwrap());
        body.extend(topic_wire::header(RecordKind::ItemData, 3));
        body.extend(b"one");
        body.extend(topic_wire::item_begin(2, 6).unwrap());
        body.extend(topic_wire::header(RecordKind::ItemData, 6));
        body.extend(b"two");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            socket
                .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.flush().await.unwrap();
            let _ = released.await;
            // Drop without the terminating HTTP chunk, just as a relay closes
            // the blocked connection while an item is incomplete.
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        let budget = DomainBudget::from_limits(&c2_config::DomainLimits::default());
        let mut sub = HttpTopicSubscription::start(
            response,
            client,
            format!("http://{address}"),
            "00000000000000000000000000000000".into(),
            budget.clone(),
            WireLagPolicy::Skip,
            Box::new(Completion(completed.clone())),
            HttpStreamCancellation::new(),
        )
        .await
        .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((first.seq, first.bytes.as_ref()), (1, &b"one"[..]));
        drop(first);
        let mut next = Box::pin(sub.recv());
        assert!(futures::poll!(next.as_mut()).is_pending());
        assert!(!completed.load(Ordering::SeqCst));
        release.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .unwrap();
        assert!(matches!(error, Err(HttpError::Transport(_))));
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(budget.snapshot().memory.used_bytes, 0);
        assert!(matches!(
            sub.completion_after_cancel().await,
            Err(HttpError::Transport(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn final_u64_sequence_is_delivered_before_terminal() {
        let mut body = topic_wire::ready(u64::MAX).unwrap();
        body.extend(topic_wire::item_begin(u64::MAX, 0).unwrap());
        body.extend(topic_wire::terminal(&Terminal {
            status: Status::Failed,
            complete_items: 1,
            error: C2Error::new(ErrorCode::CallCancelled, "closed"),
        }));
        let app = Router::new().route(
            "/",
            get(move || {
                let body = body.clone();
                async move { Body::from(body).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let mut sub = HttpTopicSubscription::start(
            response,
            reqwest::Client::new(),
            format!("http://{address}"),
            "00000000000000000000000000000000".into(),
            DomainBudget::from_limits(&c2_config::DomainLimits::default()),
            WireLagPolicy::Skip,
            Box::new(()),
            HttpStreamCancellation::new(),
        )
        .await
        .unwrap();
        assert_eq!(sub.recv().await.unwrap().seq, u64::MAX);
        assert_eq!(
            sub.completion_after_cancel().await.unwrap().complete_items,
            1
        );
        server.abort();
    }
}
