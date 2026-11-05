use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use c_two::generated::{
    C2Error, EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, ErrorCode,
    HeldResponse, MethodAccess, MethodDefinition,
};
use c_two::{
    CallOptions, CallTimeout, Connect, ContractRelease, Error, HostOptions, ResponseStream,
    Runtime, RuntimeOptions, ServiceDefinition,
};

const DESCRIPTOR: &[u8] = include_bytes!(
    "../../../core/foundation/c2-contract/tests/fixtures/server-stream.contract.json"
);
static DECODE: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> = Mutex::new(None);

struct Resource;
impl EncodedService for Resource {
    fn invoke(&self, _: u16, _: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        Ok(Vec::new())
    }
}
impl EncodedStreamService for Resource {
    fn open_stream(&self, _: u16, _: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
        Ok(Box::new(OneItem(false)))
    }
}
struct OneItem(bool);
struct Item;
impl EncodedStreamItem for Item {
    fn len(&self) -> usize {
        1
    }
    fn write_range(&self, _: usize, dst: &mut [u8]) -> Result<(), C2Error> {
        dst.fill(7);
        Ok(())
    }
}
impl EncodedStream for OneItem {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        if self.0 {
            Ok(None)
        } else {
            self.0 = true;
            Ok(Some(Box::new(Item)))
        }
    }
    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        Ok(())
    }
}
fn decode(response: HeldResponse) -> Result<u8, Error> {
    let (entered, release) = DECODE.lock().unwrap().take().unwrap();
    entered.send(()).unwrap();
    release.recv().unwrap();
    Ok(response.bytes()[0])
}

#[test]
fn typed_item_decoder_does_not_escape_the_original_iteration_deadline() {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(format!("rust-stream-decoder-{}", std::process::id())),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let route = "decoder";
    let mut registration = host
        .register(
            ServiceDefinition::new_stream(
                &release,
                release.reference(),
                route,
                [
                    MethodDefinition {
                        index: 0,
                        name: "ping".into(),
                        access: MethodAccess::Read,
                    },
                    MethodDefinition {
                        index: 1,
                        name: "echo".into(),
                        access: MethodAccess::Write,
                    },
                ],
                Arc::new(Resource),
            )
            .unwrap(),
        )
        .unwrap();
    let client = runtime
        .connect(
            release.expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().into(),
            },
        )
        .unwrap()
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(
            Duration::from_secs(2),
        )));
    let core = client
        .begin_stream("echo")
        .unwrap()
        .encode_vec(Vec::new())
        .unwrap()
        .stream()
        .unwrap();
    let (entered, waiting) = mpsc::channel();
    let (resume, release_decoder) = mpsc::channel();
    *DECODE.lock().unwrap() = Some((entered, release_decoder));
    let iteration = std::thread::spawn(move || {
        let mut stream = ResponseStream::from_core(core, decode);
        let result = stream.next().unwrap();
        (stream, result)
    });
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    // The decoder is already gated, so this clock boundary is later than the
    // original call deadline and does not establish any resource interleaving.
    let expired = Instant::now() + Duration::from_secs(2);
    while Instant::now() < expired {
        std::thread::yield_now();
    }
    resume.send(()).unwrap();
    let (stream, result) = iteration.join().unwrap();
    assert!(
        matches!(result, Err(Error::Semantic(error)) if error.code == ErrorCode::CallDeadlineExceeded)
    );
    let receipt = stream.close_with_timeout(None);
    assert!(receipt.completed);
    assert!(
        receipt.terminal.unwrap().is_ok(),
        "deadline must preserve the real natural terminal"
    );
    assert_eq!(receipt.complete_items, Some(1));
    registration.close().unwrap();
}
