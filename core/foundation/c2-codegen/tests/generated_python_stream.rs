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
import gc
import sys
import types
import weakref

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

class CCBaseError(Exception):
    pass

class ErrorCode:
    Unknown = 0
    ResourceInputDeserializing = 1
    ResourceOutputSerializing = 2
    ResourceFunctionExecuting = 3
    ResourceInputFromBuffer = 4
    ClientInputSerializing = 5
    ClientOutputDeserializing = 6
    ClientCallingResource = 7
    ClientOutputFromBuffer = 8

def SerializationError(code, message=None, details=None, transport_phase=None):
    error = CCBaseError(f'{int(code)}: {message}')
    error.code = int(code)
    error.details = dict(details or {})
    error.transport_phase = transport_phase
    return error

class NativeFailure(Exception):
    pass

projected_refs = []
def _project_native_error(exc):
    if isinstance(exc, NativeFailure):
        projected = CCBaseError('projected')
        projected_refs.append(weakref.ref(projected))
        return projected
    return None

c_two = types.ModuleType('c_two')
c_two.Stream = Stream
c_two.HeldResult = HeldResult
error_module = types.ModuleType('c_two.error')
error_module.CCBaseError = CCBaseError
error_module.ErrorCode = ErrorCode
error_module.SerializationError = SerializationError
error_module._project_native_error = _project_native_error
c_two.error = error_module
sys.modules['c_two'] = c_two
sys.modules['c_two.error'] = error_module

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

# Projection must not retain the projected exception through its own frame.
gc.disable()
def fail_begin(method):
    raise NativeFailure('begin')
bound = Bound()
bound.begin_stream = fail_begin
try:
    contract.ContractClient(bound).method_1_echo(Payload())
except CCBaseError as error:
    assert isinstance(error.__cause__, NativeFailure)
else:
    raise AssertionError('begin_stream failure was not projected')
assert projected_refs[-1]() is None
gc.enable()

class FailingPrepared(Prepared):
    def stream(self, data):
        raise NativeFailure('stream')
    def close(self):
        raise RuntimeError('close failed')
bound.begin_stream = lambda method: FailingPrepared()
try:
    contract.ContractClient(bound).method_1_echo(Payload())
except CCBaseError as error:
    assert isinstance(error.__cause__, NativeFailure)
    if sys.version_info >= (3, 11):
        assert any('close failed' in note for note in error.__notes__)
else:
    raise AssertionError('stream failure was replaced')

class ClosingPrepared(Prepared):
    def close(self):
        raise RuntimeError('close failed')
bound.begin_stream = lambda method: ClosingPrepared()
try:
    contract.ContractClient(bound).method_1_echo(Payload())
except RuntimeError as error:
    assert str(error) == 'close failed'
else:
    raise AssertionError('close failure disappeared')
try:
    raise LookupError('outer exception')
except LookupError:
    try:
        contract.ContractClient(bound).method_1_echo(Payload())
    except RuntimeError as error:
        assert str(error) == 'close failed'
    else:
        raise AssertionError('outer exception hid close failure')

original_invalidate = Payload.invalidate
def bad_invalidate(self):
    raise ValueError('invalidating failed')
Payload.invalidate = bad_invalidate
try:
    contract._invalidate_payload(Payload())
except CCBaseError as error:
    assert error.code == ErrorCode.ClientOutputFromBuffer
    assert isinstance(error.__cause__, ValueError)
else:
    raise AssertionError('invalidation failure disappeared')
Payload.invalidate = original_invalidate

# Payload hooks are classified at this adapter boundary, while native charge
# failures retain their original error and phase.
nested = CCBaseError('nested call')
nested.transport_phase = 'pre_dispatch'
class BadReport(Payload):
    def execution_report(self):
        raise nested
bound.begin_stream = lambda method: Prepared()
try:
    contract.ContractClient(bound).method_1_echo(BadReport())
except CCBaseError as error:
    assert error is not nested and error.__cause__ is nested
    assert error.code == ErrorCode.ClientInputSerializing
    assert error.transport_phase == 'pre_dispatch'
else:
    raise AssertionError('report failure was not classified')

class RejectCharge(Prepared):
    def charge_input(self, size):
        raise nested
bound.begin_stream = lambda method: RejectCharge()
try:
    contract.ContractClient(bound).method_1_echo(Payload())
except CCBaseError as error:
    assert error is nested
else:
    raise AssertionError('native charge failure disappeared')

def nested_invalidate(self):
    raise nested
Payload.invalidate = nested_invalidate
try:
    contract._invalidate_payload(Payload())
except CCBaseError as error:
    assert error is not nested and error.__cause__ is nested
    assert error.code == ErrorCode.ClientOutputFromBuffer and error.transport_phase is None
else:
    raise AssertionError('nested invalidation failure was not classified')
Payload.invalidate = original_invalidate
"#;
