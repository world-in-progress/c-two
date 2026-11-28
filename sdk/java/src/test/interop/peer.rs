use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use c_two::generated::ErrorCode;
use c_two::{
    AdmissionContext, AdmissionDecision, AdmissionRejection, CallOptions, CallOutcomeStatus,
    CallTimeout, Connect, Error as CTwoError, HostOptions, Publisher, Runtime, RuntimeOptions,
    SubscribeOptions,
};
use fastdb::{BuildPolicy, Builder, CompiledSpec, Payload};
use sha2::{Digest, Sha256};

#[allow(dead_code)]
#[path = "../generated-rust/rust/c_two_contract.rs"]
mod contract;

const ROUTE: &str = "java-interop";
const TIMEOUT: Duration = Duration::from_secs(20);
const ECHO_SPEC: &[u8] = include_bytes!("echo.json");
const NUMBER_SPEC: &[u8] = include_bytes!("number.json");
const ECHO_TEXT: &str = "跨语言";
const ECHO_BYTE: u8 = 231;
const ECHO_COUNT: u32 = 4_000_000_007;
const ECHO_SCALE: f64 = -2.625;

fn echo_bytes() -> Vec<u8> {
    (0..1024 * 1024).map(|index| index as u8).collect()
}

fn echo() -> Payload {
    let spec = CompiledSpec::compile(ECHO_SPEC).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder
        .entry_begin(0, 1)
        .unwrap()
        .value_str(ECHO_TEXT)
        .unwrap();
    builder
        .entry_begin(1, 1)
        .unwrap()
        .value_u8(ECHO_BYTE)
        .unwrap();
    builder
        .entry_begin(2, 1)
        .unwrap()
        .value_u32(ECHO_COUNT)
        .unwrap();
    builder
        .entry_begin(3, 1)
        .unwrap()
        .value_f64(ECHO_SCALE)
        .unwrap();
    builder
        .entry_begin(4, 1)
        .unwrap()
        .value_bytes(&echo_bytes())
        .unwrap();
    builder
        .freeze()
        .unwrap()
        .execute(BuildPolicy::AllowStaging)
        .unwrap()
        .payload
}

fn verify_echo(payload: &Payload) {
    let text = payload.entry_view(0).unwrap().at(0).unwrap();
    assert_eq!(text.acquire().unwrap().str().unwrap(), ECHO_TEXT);
    assert_eq!(
        payload
            .entry_view(1)
            .unwrap()
            .at(0)
            .unwrap()
            .get_u8()
            .unwrap(),
        ECHO_BYTE
    );
    assert_eq!(
        payload
            .entry_view(2)
            .unwrap()
            .at(0)
            .unwrap()
            .get_u32()
            .unwrap(),
        ECHO_COUNT
    );
    assert_eq!(
        payload
            .entry_view(3)
            .unwrap()
            .at(0)
            .unwrap()
            .get_f64()
            .unwrap(),
        ECHO_SCALE
    );
    let data = payload.entry_view(4).unwrap().at(0).unwrap();
    let access = data.acquire().unwrap();
    let data = access.bytes().unwrap();
    let expected = echo_bytes();
    assert_eq!(data.len(), expected.len());
    assert_eq!(Sha256::digest(data), Sha256::digest(expected));
}

fn number(value: u32) -> Payload {
    let spec = CompiledSpec::compile(NUMBER_SPEC).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_u32(value).unwrap();
    builder
        .freeze()
        .unwrap()
        .execute(BuildPolicy::AllowStaging)
        .unwrap()
        .payload
}

fn read_number(payload: &Payload) -> u32 {
    payload
        .entry_view(0)
        .unwrap()
        .at(0)
        .unwrap()
        .get_u32()
        .unwrap()
}

#[derive(Default)]
struct Resource {
    pings: AtomicUsize,
    applies: AtomicUsize,
    publisher: OnceLock<Publisher<Payload>>,
}

struct Service(Arc<Resource>);

impl contract::Service for Service {
    fn method_0_ping(&self) -> Result<(), CTwoError> {
        if self.0.pings.fetch_add(1, Ordering::SeqCst) == 1 {
            assert_eq!(self.0.publisher.get().unwrap().publish(&number(20))?, 2);
        }
        Ok(())
    }

    fn method_1_echo(&self, input: &Payload) -> Result<Payload, CTwoError> {
        verify_echo(input);
        Ok(input.clone())
    }

    fn method_2_apply(&self, input: &Payload) -> Result<Payload, CTwoError> {
        let previous = self.0.applies.fetch_add(1, Ordering::SeqCst);
        Ok(number(read_number(input) + previous as u32 + 1))
    }

    fn method_3_scan(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<Payload, CTwoError>> + Send>, CTwoError> {
        Ok(Box::new((31..=33).map(|value| Ok(number(value)))))
    }
}

fn runtime() -> Result<Runtime, c_two::LifecycleError> {
    Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    })
}

fn host(server_id: &str, stop: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = runtime()?;
    let host = runtime.host(
        HostOptions::default()
            .without_relay()
            .with_server_id(server_id)
            .with_admission_hook(Arc::new(|context: &AdmissionContext<'_>| {
                if context.metadata().get("interop") == Some("deny") {
                    AdmissionDecision::Deny(
                        AdmissionRejection::new("fixture denial").with_detail("reason", "interop"),
                    )
                } else {
                    AdmissionDecision::allow()
                }
            })),
    )?;
    let resource = Arc::new(Resource::default());
    let registration = host.register(
        contract::topic_service(ROUTE, Service(Arc::clone(&resource)))?
            .initial_topic_0_events(&number(10))?
            .build(),
    )?;
    assert!(
        resource
            .publisher
            .set(contract::registration_topics(&registration).topic_0_events()?)
            .is_ok()
    );
    println!("READY {}", registration.outcome().ipc_address);
    std::io::stdout().flush()?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !stop.try_exists()? {
        assert!(Instant::now() < deadline, "Host shutdown signal");
        std::thread::sleep(Duration::from_millis(25));
    }
    let outcome = host.shutdown_with_timeout(TIMEOUT);
    assert!(
        outcome.route_close_error.is_none() && outcome.runtime_barrier_error.is_none(),
        "{outcome:?}"
    );
    assert_eq!(outcome.removed_routes, [ROUTE]);
    assert!(
        outcome
            .route_outcomes
            .iter()
            .all(|route| route.local_removed && route.active_drained)
    );
    assert_eq!(resource.pings.load(Ordering::SeqCst), 2);
    assert_eq!(resource.applies.load(Ordering::SeqCst), 1);
    println!("PASS host shutdown");
    Ok(())
}

fn client(transport: &str, endpoint: &str) -> Result<(), Box<dyn Error>> {
    let mode = match transport {
        "direct" => Connect::DirectIpc {
            address: endpoint.into(),
        },
        "relay" => Connect::ExplicitRelay {
            relay_url: endpoint.into(),
        },
        _ => return Err(format!("Unknown transport {transport}").into()),
    };
    let runtime = runtime()?;
    let client =
        contract::ContractClient::new(runtime.connect(contract::expected_route(ROUTE)?, mode)?)?;
    let options = CallOptions::default().with_timeout_override(CallTimeout::After(TIMEOUT));
    let client = client.with_call_options(options.clone());
    client.method_0_ping()?;
    verify_echo(&client.method_1_echo(&echo())?);

    let request = number(41);
    for _ in 0..2 {
        assert_eq!(
            read_number(&client.method_2_apply_with_call_id(&request, "apply-once")?),
            42
        );
    }
    let outcome = client.query_call("apply-once")?;
    assert_eq!(outcome.status, CallOutcomeStatus::Succeeded);
    assert!(outcome.result_available);
    assert_eq!(
        read_number(&client.recover_method_2_apply("apply-once")?),
        42
    );

    let mut stream = client.method_3_scan()?;
    for expected in 31..=33 {
        assert_eq!(read_number(&stream.next().expect("scan item")?), expected);
    }
    assert!(stream.next().is_none(), "scan terminal");

    let subscription = client.subscribe_topic_0_events(SubscribeOptions::default())?;
    let initial = subscription.next_delivery()?;
    assert_eq!(
        (
            initial.seq(),
            initial.dropped(),
            read_number(initial.value())
        ),
        (1, 0, 10)
    );
    client.method_0_ping()?;
    let next = subscription.next_delivery()?;
    assert_eq!(
        (next.seq(), next.dropped(), read_number(next.value())),
        (2, 0, 20)
    );
    assert!(subscription.close_with_timeout(Some(TIMEOUT)).completed);

    let denied = client.with_call_options(options.with_metadata_entry("interop", "deny")?);
    let Err(CTwoError::Semantic(error)) = denied.method_0_ping() else {
        panic!("metadata must deny the call before dispatch");
    };
    assert_eq!(error.code, ErrorCode::AdmissionDenied);
    assert_eq!(
        error.details.get("reason").map(String::as_str),
        Some("interop")
    );
    println!("PASS client operations");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, server_id, stop] if mode == "host" => host(server_id, Path::new(stop)),
        [mode, transport, endpoint] if mode == "client" => client(transport, endpoint),
        _ => Err("Expected host SERVER_ID STOP_FILE or client TRANSPORT ENDPOINT".into()),
    }
}
