use std::process::Command;

use c2_codegen::{ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;

const SERVER_STREAM: &str =
    include_str!("../../c2-contract/tests/fixtures/server-stream.contract.json");

#[test]
fn generated_python_stream_client_reserves_before_encoding_and_decodes_each_item() {
    let release = ContractRelease::from_descriptor_json(SERVER_STREAM.as_bytes()).unwrap();
    let artifacts =
        compile_contract_artifacts(&release, ContractCodegenTarget::Python, &Default::default())
            .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let generated = temp.path().join("generated");
    artifacts.publish_new_tree(&generated).unwrap();
    let output = Command::new(if cfg!(windows) { "python" } else { "python3" })
        .args(["-c", CONSUMER])
        .arg(generated.join("python/c_two_contract.py"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated Python stream consumer failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

const CONSUMER: &str = r#"
import importlib.util
import sys
import types

events = []

class CompiledSpec:
    @staticmethod
    def compile(*args, **kwargs):
        return CompiledSpec()

class Payload:
    def __init__(self, data=b'input'):
        self.data = data
    @staticmethod
    def open_copy(spec, data):
        events.append(('decode', data))
        return Payload(data)
    def execution_report(self):
        events.append('size')
        return types.SimpleNamespace(used_bytes=len(self.data))
    def require_spec_sha256(self, digest):
        events.append('digest')
    def binary_bytes(self):
        events.append('encode')
        return self.data
    def invalidate(self):
        events.append(('invalidate', self.data))

class HeldResult:
    def __init__(self, value, invalidator):
        self.value = value
        self.invalidator = invalidator
    def release(self):
        self.invalidator(self.value)

class Stream:
    @classmethod
    def _from_encoded(cls, native, decoder, *, held=False, invalidator=None,
                      tracker=None, route_name='', method_name=''):
        events.append(('wrap', held, tracker, route_name, method_name))
        values = [decoder(data) for data in native]
        if held:
            values = [HeldResult(value, invalidator) for value in values]
        return values

fastdb = types.ModuleType('fastdb4py')
fastdb.__path__ = []
payload_module = types.ModuleType('fastdb4py.payload')
for name, value in {'Payload': Payload, 'CompiledSpec': CompiledSpec,
                    'Builder': type('Builder', (), {}),
                    'View': type('View', (), {}),
                    'GraphIdentity': type('GraphIdentity', (), {}),
                    'ObjectHandle': type('ObjectHandle', (), {}),
                    'PayloadError': Exception}.items():
    setattr(payload_module, name, value)
sys.modules['fastdb4py'] = fastdb
sys.modules['fastdb4py.payload'] = payload_module
c_two = types.ModuleType('c_two')
c_two.Stream = Stream
c_two.HeldResult = HeldResult
sys.modules['c_two'] = c_two

spec = importlib.util.spec_from_file_location('contract', sys.argv[1])
contract = importlib.util.module_from_spec(spec)
sys.modules['contract'] = contract
spec.loader.exec_module(contract)

assert [method.mode for method in contract.METHODS] == ['unary', 'server_stream']

class Prepared:
    def charge_input(self, size):
        events.append(('charge', size))
    def stream(self, data):
        events.append(('stream', data))
        return [b'first', b'second']
    def close(self):
        events.append('close')

class Bound:
    local_copy_tracker = object()
    route_name = 'generated-stream'

    def begin_stream(self, method):
        events.append(('begin', method))
        return Prepared()
    def call(self, method, data):
        events.append(('unary', method))
        return b''

client = contract.ContractClient(Bound())
client.method_0_ping()
assert events == [('unary', 'ping')]
events.clear()
items = client.method_1_echo(Payload())
assert [item.data for item in items] == [b'first', b'second']
assert events[:5] == [('begin', 'echo'), 'size', ('charge', 5), 'digest', 'encode'], events
assert events[5:8] == [
    ('stream', b'input'),
    ('wrap', False, Bound.local_copy_tracker, 'generated-stream', 'echo'),
    ('decode', b'first'),
], events
assert events[-1] == 'close', events
events.clear()
held = client.hold_method_1_echo(Payload())
assert [item.value.data for item in held] == [b'first', b'second']
assert ('wrap', True, Bound.local_copy_tracker, 'generated-stream', 'echo') in events, events
held[0].release()
assert events[-1] == ('invalidate', b'first'), events
assert events.count('close') == 1, events
"#;
