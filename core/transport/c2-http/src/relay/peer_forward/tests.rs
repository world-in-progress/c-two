use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use axum::{
    Router,
    extract::Query,
    routing::{delete, post},
};
use c2_config::RelayConfig;
use futures::FutureExt;
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    sync::{Notify, mpsc},
};
use tokio_util::task::AbortOnDropHandle;

use crate::{
    CALL_PATH,
    relay::{disseminator::FullBroadcast, forwarding::ForwardingWaiter, types::Locality},
    stream_wire::HttpStreamTerminal,
};

use super::*;

const WAIT: Duration = Duration::from_secs(10);
const SOURCE_TOKEN: &str = "00000000000000000000000000000001";

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("timed out waiting for relay or origin")
}

async fn listen() -> TcpListener {
    TcpListener::bind(("127.0.0.1", 0)).await.unwrap()
}

fn serve(listener: TcpListener, app: Router) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }))
}

fn fixture(listener: &TcpListener) -> (Arc<RelayState>, RouteEntry) {
    let config = Arc::new(RelayConfig {
        use_proxy: false,
        ..Default::default()
    });
    let state = Arc::new(RelayState::new(
        config,
        Arc::new(FullBroadcast::with_proxy_policy(false)),
    ));
    let route = RouteEntry {
        name: "grid".into(),
        relay_id: "origin".into(),
        address: PeerAddress::http(format!("http://{}", listener.local_addr().unwrap())),
        server_id: None,
        server_instance_id: None,
        ipc_address: None,
        crm_ns: "test".into(),
        crm_name: "Grid".into(),
        crm_ver: "1".into(),
        abi_hash: String::new(),
        signature_hash: String::new(),
        max_payload_size: 64,
        route_uid: None,
        locality: Locality::Peer,
        registered_at: 0.0,
    };
    (state, route)
}

fn open(
    state: &Arc<RelayState>,
    route: RouteEntry,
    body: Body,
    length: u64,
    stream: bool,
) -> ForwardingWaiter<Response> {
    let work = state.clone();
    state
        .forwarding
        .spawn(move |permit, cancelled| async move {
            let mut headers = HeaderMap::new();
            if stream {
                headers.insert(
                    header::ACCEPT,
                    http::HeaderValue::from_static(stream_wire::MEDIA_TYPE),
                );
            }
            forward_call(
                work,
                Call {
                    route,
                    path: CALL_PATH,
                    method: "sample",
                    headers: &headers,
                    content_length: Some(length),
                    body,
                    stream,
                },
                permit,
                cancelled,
            )
            .await
        })
        .unwrap()
}

async fn phase(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), MAX_CONTROL_ERROR_BYTES)
        .await
        .unwrap();
    C2Error::from_json_bytes(&bytes).unwrap().details["dispatch_phase"].clone()
}

#[tokio::test]
async fn downstream_drop_stops_transport_preparation_for_unary_and_stream() {
    for stream in [false, true] {
        let listener = listen().await;
        let (state, route) = fixture(&listener);
        let (entered, mut resume) = state.peer_client().pause_before_dispatch();
        let waiter = open(&state, route, Body::empty(), 0, stream);
        bounded(entered).await.unwrap();
        drop(waiter);
        bounded(resume.closed()).await;
        state.forwarding.close();
        bounded(state.forwarding.drain()).await;
        assert!(listener.accept().now_or_never().is_none());
    }
}

#[tokio::test]
async fn relay_shutdown_stops_transport_preparation_for_unary_and_stream() {
    for stream in [false, true] {
        let listener = listen().await;
        let (state, route) = fixture(&listener);
        let (entered, resume) = state.peer_client().pause_before_dispatch();
        let waiter = open(&state, route, Body::empty(), 0, stream);
        bounded(entered).await.unwrap();
        state.forwarding.close();
        assert_eq!(phase(bounded(waiter).await.unwrap()).await, "pre_dispatch");
        bounded(state.forwarding.drain()).await;
        assert!(resume.send(()).is_err());
        assert!(listener.accept().now_or_never().is_none());
    }
}

#[tokio::test]
async fn dispatched_unary_upload_survives_downstream_drop_and_shutdown() {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let entered = Arc::new(Notify::new());
    let (received, mut payloads) = mpsc::unbounded_channel();
    let _origin = serve(
        listener,
        Router::new().route(
            CALL_PATH,
            post({
                let entered = entered.clone();
                move |body: Body| async move {
                    entered.notify_one();
                    let body = to_bytes(body, 2).await.unwrap();
                    received.send(body.clone()).unwrap();
                    body
                }
            }),
        ),
    );
    let send_tail = Arc::new(Notify::new());
    let body = Body::from_stream(
        futures::stream::iter([Ok::<_, Infallible>(Bytes::from_static(b"a"))]).chain(
            futures::stream::once({
                let send_tail = send_tail.clone();
                async move {
                    send_tail.notified().await;
                    Ok(Bytes::from_static(b"b"))
                }
            }),
        ),
    );
    let waiter = open(&state, route, body, 2, false);
    bounded(entered.notified()).await;
    drop(waiter);
    state.forwarding.close();
    assert_eq!(state.forwarding.snapshot().used_operations, 1);
    send_tail.notify_one();
    assert_eq!(bounded(payloads.recv()).await.unwrap(), "ab");
    bounded(state.forwarding.drain()).await;
    assert_eq!(state.forwarding.snapshot().used_operations, 0);
}

#[tokio::test]
async fn origin_disconnect_after_dispatch_is_uncertain_and_never_replayed() {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let waiter = open(&state, route, Body::empty(), 0, false);
    let (mut connection, _) = bounded(listener.accept()).await.unwrap();
    let mut request = Vec::new();
    loop {
        let mut bytes = [0; 1024];
        let count = bounded(connection.read(&mut bytes)).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&bytes[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    drop(connection);
    let response = bounded(waiter).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(phase(response).await, "dispatch_uncertain");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.forwarding.close();
    bounded(state.forwarding.drain()).await;
}

#[tokio::test]
async fn truncated_origin_stream_ends_with_an_uncertain_failure_record() {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let _origin = serve(
        listener,
        Router::new().route(
            CALL_PATH,
            post(|| async {
                let body = [
                    stream_wire::encode_item_begin(4).as_slice(),
                    stream_wire::encode_header(HttpStreamRecordKind::ItemData, 4).as_slice(),
                    b"ab".as_slice(),
                ]
                .concat();
                (
                    [
                        (CALL_TOKEN_HEADER, SOURCE_TOKEN),
                        ("content-type", stream_wire::MEDIA_TYPE),
                    ],
                    body,
                )
            }),
        ),
    );
    let response = bounded(open(&state, route, Body::empty(), 0, true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = bounded(to_bytes(response.into_body(), 4096)).await.unwrap();
    assert_eq!(&body[..17], &stream_wire::encode_item_begin(4));
    let (kind, length) = stream_wire::decode_header(body[17..26].try_into().unwrap()).unwrap();
    assert_eq!(kind, HttpStreamRecordKind::Failure);
    assert_eq!(length as usize, body.len() - 26);
    let error = stream_wire::decode_failure(&body[26..]).unwrap();
    assert_eq!(error.code, ErrorCode::ResourceUnavailable);
    assert_eq!(error.details["dispatch_phase"], "dispatch_uncertain");
    state.forwarding.close();
    bounded(state.forwarding.drain()).await;
}

async fn cancel_while_source_token_is_pending(cause: &'static str, fail_first_delete: bool) {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let entered = Arc::new(Notify::new());
    let reply = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let (deleted, mut deletes) = mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));
    let terminal = stream_wire::encode_terminal(&HttpStreamTerminal {
        status: HttpStreamStatus::Failed,
        complete_items: Some(1),
        error: Some(
            C2Error::new(ErrorCode::CallCancelled, "origin cancelled the stream")
                .with_details([("cause".into(), cause.into())].into()),
        ),
    })
    .unwrap();
    let send_terminal = terminal.clone();
    let app = Router::new()
        .route(
            CALL_PATH,
            post({
                let entered = entered.clone();
                let reply = reply.clone();
                let cancelled = cancelled.clone();
                move || async move {
                    entered.notify_one();
                    reply.notified().await;
                    let first = Bytes::from_owner(stream_wire::encode_item_begin(0));
                    let body = Body::from_stream(
                        futures::stream::iter([Ok::<_, Infallible>(first)]).chain(
                            futures::stream::once(async move {
                                cancelled.notified().await;
                                Ok(Bytes::from(send_terminal))
                            }),
                        ),
                    );
                    (
                        [
                            (CALL_TOKEN_HEADER, SOURCE_TOKEN),
                            ("content-type", stream_wire::MEDIA_TYPE),
                        ],
                        body,
                    )
                }
            }),
        )
        .route(
            &format!("/_calls/{SOURCE_TOKEN}"),
            delete(
                move |Query(query): Query<std::collections::HashMap<String, String>>| async move {
                    deleted.send(query["cause"].clone()).unwrap();
                    if attempts.fetch_add(1, Ordering::Relaxed) == 0 && fail_first_delete {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        cancelled.notify_one();
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
    let _origin = serve(listener, app);
    let mut waiter = Some(open(&state, route, Body::empty(), 0, true));
    bounded(entered.notified()).await;
    if cause == "lifecycle" {
        state.forwarding.close();
    } else {
        drop(waiter.take());
    }
    reply.notify_one();
    for _ in 0..=usize::from(fail_first_delete) {
        assert_eq!(bounded(deletes.recv()).await.unwrap(), cause);
    }
    if let Some(waiter) = waiter {
        let response = bounded(waiter).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[CALL_TOKEN_HEADER], SOURCE_TOKEN);
        let response = bounded(to_bytes(response.into_body(), 4096)).await.unwrap();
        assert_eq!(
            response,
            [stream_wire::encode_item_begin(0).as_slice(), &terminal].concat()
        );
    }
    state.forwarding.close();
    bounded(state.forwarding.drain()).await;
    assert_eq!(state.forwarding.snapshot().used_operations, 0);
}

#[tokio::test]
async fn consumer_drop_during_dispatched_stream_waits_for_source_token() {
    cancel_while_source_token_is_pending("consumer", false).await;
}

#[tokio::test]
async fn shutdown_during_dispatched_stream_retries_failed_cancellation_at_origin() {
    cancel_while_source_token_is_pending("lifecycle", true).await;
}

#[tokio::test]
async fn control_timeout_before_dispatch_reports_pre_dispatch() {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let (entered, resume) = state.peer_client().pause_before_dispatch();
    let task = tokio::spawn(async move {
        forward_control(&state, &route, &HeaderMap::new(), Control::Probe).await
    });
    bounded(entered).await.unwrap();
    assert_eq!(phase(bounded(task).await.unwrap()).await, "pre_dispatch");
    assert!(resume.send(()).is_err());
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
async fn control_timeout_after_dispatch_reports_dispatch_uncertain() {
    let listener = listen().await;
    let (state, route) = fixture(&listener);
    let task = tokio::spawn(async move {
        forward_control(&state, &route, &HeaderMap::new(), Control::Probe).await
    });
    let (mut connection, _) = bounded(listener.accept()).await.unwrap();
    assert!(bounded(connection.read(&mut [0; 64])).await.unwrap() > 0);
    assert_eq!(
        phase(bounded(task).await.unwrap()).await,
        "dispatch_uncertain"
    );
}
