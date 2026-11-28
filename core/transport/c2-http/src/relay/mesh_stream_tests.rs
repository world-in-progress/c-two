//! Live server streams and topics over the authenticated relay mesh.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use c2_config::{BridgeConfig, LocalEndpointContext, RelayConfig};
use c2_contract::{ExpectedRouteContract, ValidatedContractDescriptor};
use c2_error::ErrorCode;
use c2_server::{
    AdmissionContext, AdmissionDecision, AdmissionOperation, AdmissionTransport, ConcurrencyMode,
    CrmCallback, CrmError, CrmStream, CrmStreamCallback, CrmStreamItem, ResponseMeta, ResponsePool,
    RouteBuildSpec, SchedulerLimits, Server, ServerIdentity, ServerIpcConfig,
    topic::{RouteTopics, TopicConfig},
};
use c2_wire::rpc::WireLagPolicy;
use tokio::task::JoinHandle;

use super::RelayServer;
use crate::{
    bridge::BridgeServer,
    client::{HttpError, HttpResponseStream, HttpStreamCancellation, HttpTopicSubscription},
    stream_wire::{self, HttpStreamStatus},
    topic_wire,
};

const WAIT: Duration = Duration::from_secs(30);
const ROUTE: &str = "portable";
const TOPIC: &str = "state";
const PREFIX_ITEMS: u64 = 3;
const ITEM_BYTES: usize = 32 * 1024;
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

async fn within<T>(label: &str, work: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, work)
        .await
        .unwrap_or_else(|_| panic!("{label} timed out"))
}

#[derive(Default)]
struct Observed {
    stream_finishes: AtomicUsize,
    last_produced: AtomicUsize,
    topic_admissions: parking_lot::Mutex<Vec<String>>,
}

struct Resource(Arc<Observed>);

impl CrmCallback for Resource {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::Inline(Vec::new()))
    }
}

impl CrmStreamCallback for Resource {
    fn open_stream(
        &self,
        _: &str,
        method: u16,
        input: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        assert_eq!(method, 1);
        let finite = match input.as_slice() {
            b"finite" => true,
            b"open" => false,
            _ => return Err(CrmError::InternalError("unexpected stream input".into())),
        };
        Ok(Box::new(Sequence {
            next: 0,
            finite,
            observed: self.0.clone(),
        }))
    }
}

struct Sequence {
    next: u64,
    finite: bool,
    observed: Arc<Observed>,
}

impl CrmStream for Sequence {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        if self.next >= PREFIX_ITEMS {
            if self.finite {
                return Ok(None);
            }
            // Keep the stream open while putting sparse work on the data lane.
            std::thread::sleep(Duration::from_millis(5));
        }
        let item = vec![self.next as u8; ITEM_BYTES];
        self.next += 1;
        Ok(Some(Box::new(item)))
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        self.observed
            .last_produced
            .store(self.next as usize, Ordering::SeqCst);
        self.observed.stream_finishes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Host {
    server: Arc<Server>,
    task: JoinHandle<Result<(), c2_server::ServerError>>,
    expected: ExpectedRouteContract,
    topics: Arc<RouteTopics>,
    observed: Arc<Observed>,
}

impl Host {
    async fn start(context: &LocalEndpointContext, id: &str, address: &str) -> Self {
        let mut json: serde_json::Value = serde_json::from_slice(DESCRIPTOR).unwrap();
        let binding = json["methods"][1]["bindings"]["output"].clone();
        json["topics"] = serde_json::json!([{
            "name": TOPIC,
            "output": {"kind": "payload"},
            "binding": binding
        }]);
        let hashes =
            c2_contract::derive_contract_fingerprints_json(json.to_string().as_bytes()).unwrap();
        json["fingerprints"]["abi_hash"] = hashes.abi_hash().into();
        json["fingerprints"]["signature_hash"] = hashes.signature_hash().into();
        let descriptor =
            ValidatedContractDescriptor::from_json(json.to_string().as_bytes()).unwrap();
        let expected = ExpectedRouteContract {
            route_name: ROUTE.into(),
            crm_ns: descriptor.crm_namespace().into(),
            crm_name: descriptor.crm_name().into(),
            crm_ver: descriptor.crm_version().into(),
            abi_hash: descriptor.abi_hash().into(),
            signature_hash: descriptor.signature_hash().into(),
        };
        let observed = Arc::new(Observed::default());
        let hook = {
            let observed = observed.clone();
            Arc::new(move |context: &AdmissionContext<'_>| {
                if context.operation() == AdmissionOperation::Subscribe {
                    assert_eq!(context.target(), Some(TOPIC));
                    assert_eq!(context.transport(), AdmissionTransport::Ipc);
                    let name = context.metadata().get("subscriber").unwrap();
                    observed.topic_admissions.lock().push(name.into());
                    if name == "blocked" {
                        return AdmissionDecision::deny("subscriber refused");
                    }
                }
                AdmissionDecision::allow()
            })
        };
        let server = Arc::new(
            Server::new_with_identity_and_endpoint(
                context.endpoint(address).unwrap(),
                ServerIpcConfig::default(),
                ServerIdentity {
                    server_id: id.into(),
                    server_instance_id: format!("{id}-instance"),
                },
            )
            .unwrap()
            .with_admission_hook(hook),
        );
        let topics = server.create_route_topics(
            expected.clone(),
            &BTreeMap::from([(TOPIC.into(), TopicConfig { capacity: 2 })]),
        );
        let route = server
            .build_stream_route(
                RouteBuildSpec {
                    name: expected.route_name.clone(),
                    crm_ns: expected.crm_ns.clone(),
                    crm_name: expected.crm_name.clone(),
                    crm_ver: expected.crm_ver.clone(),
                    abi_hash: expected.abi_hash.clone(),
                    signature_hash: expected.signature_hash.clone(),
                    method_names: vec!["ping".into(), "echo".into()],
                    access_map: [(0, c2_server::AccessLevel::Read)].into(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Resource(observed.clone())),
                &descriptor,
            )
            .unwrap()
            .with_topics(topics.clone());
        let reservation = server.reserve_route(route).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        server.begin_start_attempt().unwrap();
        let task = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });
        server.wait_until_responsive(WAIT).await.unwrap();
        Self {
            server,
            task,
            expected,
            topics,
            observed,
        }
    }

    async fn stop(self) {
        self.server.shutdown_and_wait(WAIT).await.unwrap();
        within("Host join", self.task).await.unwrap().unwrap();
    }
}

struct AnchorClient {
    http: reqwest::Client,
    url: String,
    expected: ExpectedRouteContract,
}

impl AnchorClient {
    fn with_contract(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header("x-c2-expected-crm-ns", &self.expected.crm_ns)
            .header("x-c2-expected-crm-name", &self.expected.crm_name)
            .header("x-c2-expected-crm-ver", &self.expected.crm_ver)
            .header("x-c2-expected-abi-hash", &self.expected.abi_hash)
            .header(
                "x-c2-expected-signature-hash",
                &self.expected.signature_hash,
            )
    }

    async fn wait_for_route(&self) {
        within("iroh mesh route convergence", async {
            loop {
                let response = self
                    .http
                    .get(format!("{}/_resolve", self.url))
                    .query(&[
                        ("route", self.expected.route_name.as_str()),
                        ("crm_ns", &self.expected.crm_ns),
                        ("crm_name", &self.expected.crm_name),
                        ("crm_ver", &self.expected.crm_ver),
                        ("abi_hash", &self.expected.abi_hash),
                        ("signature_hash", &self.expected.signature_hash),
                    ])
                    .send()
                    .await
                    .unwrap();
                if response.status().is_success() {
                    let routes: serde_json::Value = response.json().await.unwrap();
                    assert_eq!(routes[0]["proxy_via_anchor"], true);
                    assert!(routes[0]["relay_url"].is_null());
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
    }

    async fn stream(&self, input: &'static str) -> (String, HttpResponseStream) {
        let response = within(
            "server-stream headers",
            self.with_contract(self.http.post(format!(
                "{}/_c2/calls/call?route={ROUTE}&method=echo",
                self.url
            )))
            .header("accept", stream_wire::MEDIA_TYPE)
            .body(input)
            .send(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], stream_wire::MEDIA_TYPE);
        let token = response.headers()["x-c2-call-token"]
            .to_str()
            .unwrap()
            .to_owned();
        let stream = HttpResponseStream::start(
            response,
            self.http.clone(),
            self.url.clone(),
            token.clone(),
            crate::payload::memory_only_budget(1024 * 1024),
            ITEM_BYTES as u64,
            Box::new(()),
            HttpStreamCancellation::new(),
        );
        (token, stream)
    }

    async fn cancel(&self, token: &str, cause: &str) {
        let response = within(
            "stream DELETE",
            self.http
                .delete(format!("{}/_calls/{token}", self.url))
                .query(&[("cause", cause)])
                .send(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 204);
    }

    async fn subscribe(&self, name: &str, head_seq: u64) -> HttpTopicSubscription {
        let response = within(
            "topic response headers",
            self.with_contract(self.http.post(format!(
                "{}/_c2/topics/subscribe?route={ROUTE}&topic={TOPIC}",
                self.url
            )))
            .header("accept", topic_wire::MEDIA_TYPE)
            .header("x-c2-topic-start", "next")
            .header("x-c2-topic-on-lag", "skip")
            .header("x-c2-meta-subscriber", name)
            .body("")
            .send(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], topic_wire::MEDIA_TYPE);
        assert_eq!(response.headers()["x-c2-topic-admission-required"], "1");
        assert_eq!(
            response.headers()["x-c2-topic-head-seq"],
            head_seq.to_string()
        );
        assert!(!response.headers().contains_key("x-c2-topic-registration"));
        let token = response.headers()["x-c2-call-token"]
            .to_str()
            .unwrap()
            .to_owned();
        let subscription = within(
            "topic READY",
            HttpTopicSubscription::start(
                response,
                self.http.clone(),
                self.url.clone(),
                token,
                crate::payload::memory_only_budget(1024 * 1024),
                WireLagPolicy::Skip,
                Box::new(()),
                HttpStreamCancellation::new(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(subscription.start_seq(), head_seq + 1);
        subscription
    }
}

fn check_stream_item(item: &[u8], index: u64) {
    assert_eq!(item.len(), ITEM_BYTES);
    assert!(item.iter().all(|&value| value == index as u8));
}

async fn exercise_streams(client: &AnchorClient, observed: &Observed) {
    let (_, mut finite) = client.stream("finite").await;
    within("finite server stream", async {
        for index in 0..PREFIX_ITEMS {
            let item = finite.recv().await.unwrap().unwrap();
            check_stream_item(item.as_ref(), index);
        }
        assert!(finite.recv().await.unwrap().is_none());
    })
    .await;
    let terminal = within("finite stream terminal", finite.completion())
        .await
        .unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::Success);
    assert_eq!(terminal.complete_items, Some(PREFIX_ITEMS));
    assert_eq!(observed.stream_finishes.load(Ordering::SeqCst), 1);

    let (_, mut consumer) = client.stream("open").await;
    let (token, mut lifecycle) = client.stream("open").await;
    for stream in [&mut consumer, &mut lifecycle] {
        within("server-stream prefix", async {
            for index in 0..PREFIX_ITEMS {
                let item = stream.recv().await.unwrap().unwrap();
                check_stream_item(item.as_ref(), index);
            }
        })
        .await;
    }

    consumer.cancel();
    let terminal = within(
        "consumer stream terminal",
        consumer.completion_after_cancel(),
    )
    .await
    .unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::Failed);
    let error = terminal.error.unwrap();
    assert_eq!(error.code, ErrorCode::CallCancelled);
    assert_eq!(error.message, "server stream cancelled by consumer");
    let complete = terminal.complete_items.unwrap();
    let produced = observed.last_produced.load(Ordering::SeqCst) as u64;
    // A consumer may relinquish an item already produced by the Host.
    assert!((PREFIX_ITEMS..=produced).contains(&complete));
    assert_eq!(observed.stream_finishes.load(Ordering::SeqCst), 2);

    client.cancel(&token, "lifecycle").await;
    let received = within("remaining lifecycle stream items", async {
        let mut received = PREFIX_ITEMS;
        loop {
            match lifecycle.recv().await {
                Ok(Some(item)) => {
                    check_stream_item(item.as_ref(), received);
                    received += 1;
                }
                Err(HttpError::LocalCallRejected(error))
                    if error.code == ErrorCode::CallCancelled =>
                {
                    return received;
                }
                _ => panic!("lifecycle stream did not deliver its cancellation"),
            }
        }
    })
    .await;
    let terminal = within("lifecycle stream terminal", lifecycle.completion())
        .await
        .unwrap();
    assert_eq!(terminal.status, HttpStreamStatus::Failed);
    assert_eq!(terminal.complete_items, Some(received));
    let error = terminal.error.unwrap();
    assert_eq!(error.code, ErrorCode::CallCancelled);
    assert_eq!(error.message, "server stream cancelled by relay shutdown");
    assert_eq!(
        observed.last_produced.load(Ordering::SeqCst) as u64,
        received
    );
    assert_eq!(observed.stream_finishes.load(Ordering::SeqCst), 3);
}

async fn expect_topic(sub: &mut HttpTopicSubscription, seq: u64, value: &[u8]) {
    let item = within("topic delivery", sub.recv()).await.unwrap();
    assert_eq!(
        (item.seq, item.dropped, item.bytes.as_ref()),
        (seq, 0, value)
    );
}

async fn expect_topic_cancel(sub: &mut HttpTopicSubscription, cause: &str, count: u64) {
    let terminal = within("topic cancellation", sub.completion_after_cancel())
        .await
        .unwrap();
    assert_eq!(terminal.status, topic_wire::Status::Failed);
    assert_eq!(terminal.complete_items, count);
    assert_eq!(terminal.error.code, ErrorCode::CallCancelled);
    assert_eq!(terminal.error.details["cause"], cause);
}

async fn exercise_topics(
    client: &AnchorClient,
    host: &Host,
    origin: &RelayServer,
    anchor: &mut RelayServer,
) {
    within("server-stream slot retirement", async {
        while origin.call_execution_snapshot().used_operations != 0
            || anchor.call_execution_snapshot().used_operations != 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let publisher = host.topics.publisher(TOPIC).unwrap();
    let mut first = client.subscribe("first", 0).await;
    assert_eq!(publisher.publish(b"one".to_vec()).unwrap(), 1);
    expect_topic(&mut first, 1, b"one").await;

    let refused = within(
        "refused subscriber",
        client
            .with_contract(client.http.post(format!(
                "{}/_c2/topics/subscribe?route={ROUTE}&topic={TOPIC}",
                client.url
            )))
            .header("accept", topic_wire::MEDIA_TYPE)
            .header("x-c2-topic-start", "next")
            .header("x-c2-topic-on-lag", "skip")
            .header("x-c2-meta-subscriber", "blocked")
            .body("")
            .send(),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), 403);
    let error: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(error["code"], 724);
    assert_eq!(host.topics.active_subscribers(), 1);

    // The late subscriber's Host admission must observe the head advanced
    // after the shared origin subscription opened.
    let mut second = client.subscribe("second", 1).await;
    assert_eq!(
        &*host.observed.topic_admissions.lock(),
        &["first", "blocked", "second"]
    );
    assert_eq!(host.topics.active_subscribers(), 1);
    assert_eq!(origin.call_execution_snapshot().used_operations, 1);
    assert_eq!(anchor.call_execution_snapshot().used_operations, 2);
    assert_eq!(publisher.publish(b"two".to_vec()).unwrap(), 2);
    expect_topic(&mut first, 2, b"two").await;
    expect_topic(&mut second, 2, b"two").await;

    first.cancel();
    expect_topic_cancel(&mut first, "consumer_cancelled", 2).await;
    assert_eq!(host.topics.active_subscribers(), 1);
    assert_eq!(origin.call_execution_snapshot().used_operations, 1);
    assert_eq!(publisher.publish(b"three".to_vec()).unwrap(), 3);
    expect_topic(&mut second, 3, b"three").await;

    tokio::task::block_in_place(|| anchor.shutdown_and_wait(Duration::ZERO)).unwrap();
    expect_topic_cancel(&mut second, "relay_shutdown", 2).await;
    within("shared upstream retirement", async {
        while host.topics.active_subscribers() != 0
            || origin.call_execution_snapshot().used_operations != 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

async fn run_mesh_streams(relay_only: bool) {
    let files = tempfile::tempdir().unwrap();
    let bridge = within(
        "private bridge startup",
        BridgeServer::start(BridgeConfig {
            bind: "127.0.0.1:0".into(),
            quic_bind: Some("127.0.0.1:0".into()),
            state_dir: Some(files.path().join("bridge")),
            ..BridgeConfig::default()
        }),
    )
    .await
    .unwrap();
    #[cfg(unix)]
    let local_root = super::test_support::EndpointTestRoot::new();
    #[cfg(unix)]
    let context = local_root.context();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();

    let id = format!(
        "mesh-stream-{}-{}",
        std::process::id(),
        u8::from(relay_only)
    );
    let address = format!("ipc://{id}");
    let host = Host::start(&context, &id, &address).await;
    let base = RelayConfig {
        bind: "127.0.0.1:0".into(),
        bridge: Some(bridge.bridge_url_template()),
        bridge_token: Some(bridge.token().to_owned()),
        cluster_secret: Some("mesh-stream-and-topic".into()),
        seed_retry_interval: Duration::from_millis(200),
        heartbeat_interval: Duration::from_secs(1),
        ..RelayConfig::default()
    };
    let start = |config: RelayConfig| {
        tokio::task::block_in_place(|| {
            if relay_only {
                RelayServer::start_relay_only_for_test(config, context.clone())
            } else {
                RelayServer::start_resolved(config, context.clone())
            }
        })
        .unwrap()
    };
    let mut origin = start(RelayConfig {
        state_dir: Some(files.path().join("origin")),
        ..base.clone()
    });
    tokio::task::block_in_place(|| origin.register_upstream(ROUTE, &id, &address)).unwrap();
    let origin_id = hex::encode(origin.iroh_endpoint_id().unwrap().as_bytes());
    let mut anchor = start(RelayConfig {
        state_dir: Some(files.path().join("anchor")),
        seeds: vec![format!("iroh://{origin_id}")],
        ..base
    });

    let client = AnchorClient {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        url: format!("http://{}", anchor.local_addr()),
        expected: host.expected.clone(),
    };
    client.wait_for_route().await;
    exercise_streams(&client, &host.observed).await;
    exercise_topics(&client, &host, &origin, &mut anchor).await;

    tokio::task::block_in_place(|| {
        assert!(anchor.shutdown_and_wait(WAIT).unwrap().completed);
        assert!(origin.shutdown_and_wait(WAIT).unwrap().completed);
    });
    host.stop().await;
    within("private bridge shutdown", bridge.stop())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_and_topics_work_with_direct_iroh_paths() {
    run_mesh_streams(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_and_topics_work_via_private_bridge_without_ip_transports() {
    run_mesh_streams(true).await;
}
