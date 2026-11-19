"""Phase-3 error convergence coverage: decode stages, closed proxies, generated clients."""
from __future__ import annotations

from collections.abc import Iterator
import importlib.util
import json
from pathlib import Path
import sys

import pytest

import c_two as cc
import c_two.contract
from c_two.crm.payload_plan import fastdb_payload_binding, python_pickle_output_binding
from c_two.crm.stream import Stream
from c_two.error import (
    CCBaseError, CCError, CallCapacityExceeded, CallDeadlineExceeded, CoreError,
    ErrorCode, ResourceClosed, ResourceNotFound, SerializationError,
)
pytest.importorskip("fastdb4py.payload")
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec

ECHO_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one", "type": {"kind": "str", "nullable": True}},
    ],
    "components": [],
}


def echo_payload(text: str = 'converged'):
    spec = CompiledSpec.compile(json.dumps(ECHO_SPEC, separators=(",", ":")).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_str(text)
    plan = builder.freeze()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload, spec.sha256()
    finally:
        plan.close()
        builder.close()
        spec.close()


class _FakeNativeStream:
    """Minimal native stream surface feeding fixed item bytes."""

    def __init__(self, items):
        self._items = list(items)
        self.cancelled = False

    def next_item(self):
        if not self._items:
            return None
        return self._items.pop(0)

    def check_deadline(self):
        return None

    def cancel(self):
        self.cancelled = True

    def close(self, timeout=None):
        return {'completed': True, 'error': None, 'complete_items': 0}


class TestStreamDecodeStagePrecision:
    """The failing payload hook — not the retention mode — picks the code."""

    def test_held_pickle_decode_failure_reports_deserialize(self):
        binding = python_pickle_output_binding(lambda: None)
        stream = Stream._from_encoded(
            _FakeNativeStream([b'not-a-pickle']),
            binding.deserialize,
            held=True,
        )
        with pytest.raises(SerializationError) as failure:
            next(stream)
        error = failure.value
        assert int(error.code) == 6
        assert error.code == ErrorCode.ClientOutputDeserializing
        assert error.stage == 'deserialize'
        assert error.side == 'client'
        assert error.transport_phase is None

    def test_held_fastdb_open_copy_failure_reports_from_buffer(self):
        binding = fastdb_payload_binding(ECHO_SPEC, label='convergence.output')
        stream = Stream._from_encoded(
            _FakeNativeStream([b'\x00garbage-not-a-payload']),
            binding.deserialize,
            held=True,
        )
        with pytest.raises(SerializationError) as failure:
            next(stream)
        error = failure.value
        assert int(error.code) == 8
        assert error.code == ErrorCode.ClientOutputFromBuffer
        assert error.stage == 'from_buffer'
        assert error.side == 'client'
        assert error.details['cause_owner'] == 'fastdb'
        for field in ('fastdb_code', 'fastdb_symbol', 'fastdb_path', 'fastdb_message'):
            assert field in error.details, field

    def test_held_fastdb_wrong_spec_fails_at_open_with_header_digest(self):
        from tests.fixtures.stream_python import blob_payload

        binding = fastdb_payload_binding(ECHO_SPEC, label='convergence.output')
        wrong_spec_payload = blob_payload(b'mismatched')
        stream = Stream._from_encoded(
            _FakeNativeStream([wrong_spec_payload.binary_bytes()]),
            binding.deserialize,
            held=True,
        )
        with pytest.raises(SerializationError) as failure:
            next(stream)
        error = failure.value
        # FastDB's binary header carries the spec digest, so a payload built
        # for another spec is refused while opening the buffer view.
        assert int(error.code) == 8
        assert error.stage == 'from_buffer'
        assert error.details['cause_owner'] == 'fastdb'
        assert error.details['fastdb_symbol'] == 'DIGEST_MISMATCH'


class TestClosedProxyRefusals:
    """An explicitly closed CRMProxy refuses every entry point pre-dispatch."""

    @staticmethod
    def _closed_proxy():
        from c_two.transport.client.proxy import CRMProxy

        proxy = CRMProxy.thread_local(object(), name='closed-route')
        proxy.terminate()
        return proxy

    def test_prepare_call_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.prepare_call('echo')
        assert refused.value.message == 'CRM proxy is closed'
        assert refused.value.transport_phase == 'pre_dispatch'

    def test_begin_call_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.begin_call('echo')
        assert refused.value.transport_phase == 'pre_dispatch'

    def test_begin_stream_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.begin_stream('steps')
        assert refused.value.transport_phase == 'pre_dispatch'

    def test_call_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.call('echo', b'')
        assert refused.value.transport_phase == 'pre_dispatch'

    def test_call_direct_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.call_direct('echo', ())
        assert refused.value.transport_phase == 'pre_dispatch'

    def test_connection_observation_refused(self):
        proxy = self._closed_proxy()
        with pytest.raises(ResourceClosed) as refused:
            proxy.connection_observation()
        assert refused.value.transport_phase == 'pre_dispatch'


FIXTURE_CONTRACT = (
    Path(__file__).parents[4] / "tests" / "fixtures" / "contracts" / "portable-release.contract.json"
)
SERVER_STREAM_CONTRACT = (
    Path(__file__).parents[4] / "core" / "foundation" / "c2-contract"
    / "tests" / "fixtures" / "server-stream.contract.json"
)


def _load_generated_contract(tmp_path: Path, module_name: str, contract_path: Path):
    generated = cc.contract.compile_artifacts(
        contract_path.read_bytes(), target="python",
    )
    for artifact in generated.artifacts:
        output = tmp_path / artifact.relative_path
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(artifact.bytes)
    module_path = tmp_path / "python" / "c_two_contract.py"
    spec = importlib.util.spec_from_file_location(module_name, module_path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


class _CorruptBoundClient:
    def call(self, method_name, data):
        return b'\x00corrupted-upstream-bytes'


class TestGeneratedContractClientSerialization:
    """Generated clients normalize payload failures with structured causes."""

    def test_wrong_spec_input_is_client_input_serializing_pre_dispatch(self, tmp_path):
        from tests.fixtures.stream_python import blob_payload

        contract = _load_generated_contract(tmp_path, 'convergence_contract', FIXTURE_CONTRACT)
        wrong_spec = blob_payload(b'built-for-another-spec')
        with pytest.raises(SerializationError) as failure:
            contract.encode_method_1_echo_input(wrong_spec)
        error = failure.value
        assert int(error.code) == 5
        assert error.code == ErrorCode.ClientInputSerializing
        assert error.stage == 'serialize'
        assert error.side == 'client'
        assert error.transport_phase == 'pre_dispatch'
        assert error.details['cause_owner'] == 'fastdb'
        assert error.details['fastdb_symbol']
        assert error.__cause__ is not None

    def test_corrupted_output_open_is_client_output_from_buffer(self, tmp_path):
        contract = _load_generated_contract(tmp_path, 'convergence_contract', FIXTURE_CONTRACT)
        with pytest.raises(SerializationError) as failure:
            contract.decode_method_1_echo_output(b'\x00corrupted-output')
        error = failure.value
        assert int(error.code) == 8
        assert error.stage == 'from_buffer'
        assert error.details['cause_owner'] == 'fastdb'

    def test_contract_client_normalizes_corrupted_response(self, tmp_path):
        contract = _load_generated_contract(tmp_path, 'convergence_contract', FIXTURE_CONTRACT)
        payload, _digest = echo_payload()
        client = contract.ContractClient(_CorruptBoundClient())
        with pytest.raises(SerializationError) as failure:
            client.method_1_echo(payload)
        error = failure.value
        assert int(error.code) == 8
        assert error.stage == 'from_buffer'
        assert error.transport_phase is None

    def test_valid_round_trip_through_fake_bound_client(self, tmp_path):
        received = {}

        class Bound:
            def call(self, method_name, data):
                received[method_name] = bytes(data)
                return bytes(data)

        contract = _load_generated_contract(tmp_path, 'convergence_contract', FIXTURE_CONTRACT)
        payload, _digest = echo_payload('generated-ok')
        client = contract.ContractClient(Bound())
        result = client.method_1_echo(payload)
        assert received['echo'].startswith(b'FDBPAY')
        with result.entry_view(0) as values:
            with values.at(0) as value:
                with value.acquire() as acquired:
                    assert acquired.str() == 'generated-ok'


def _semantic_core(code: ErrorCode, message: str, details=None) -> CoreError:
    """A native CoreError carrying a canonical semantic snapshot."""
    core = CoreError('semantic', message)
    core.code = int(code)
    core.name = code.name
    core.details = dict(details or {})
    core.semantic_message = message
    core._transport_phase = 'pre_dispatch'
    core._semantic_snapshot = (int(code), message, tuple(core.details.items()), core.transport_phase)
    return core


class _StubPreparedStream:
    """Raw native prepared stream whose stages fail with fixed exceptions."""

    def __init__(self, failures):
        self.failures = failures
        self.closed = False

    def charge_input(self, nbytes):
        if 'charge_input' in self.failures:
            raise self.failures['charge_input']

    def stream(self, data):
        if 'stream' in self.failures:
            raise self.failures['stream']
        raise AssertionError('stub stream is not expected to succeed')

    def close(self):
        self.closed = True


class _StubNativeStreamClient:
    def __init__(self, prepared):
        self.prepared = prepared

    def begin_stream(self, method_name):
        return self.prepared


class TestPreparedStreamProjection:
    """``CRMProxy.begin_stream`` wraps its preparation like ``prepare_call``.

    Charging the encoded input and starting the stream must surface the same
    typed semantic projection as every other native call site instead of a
    bare ``CoreError(category='semantic')``.
    """

    @staticmethod
    def _proxy(prepared):
        from c_two.transport.client.proxy import CRMProxy

        return CRMProxy.ipc(_StubNativeStreamClient(prepared), 'prepared-stream')

    def test_charge_input_semantic_failure_projects_named_subclass(self):
        deadline = _semantic_core(ErrorCode.CallDeadlineExceeded, 'call deadline exceeded')
        prepared = _StubPreparedStream({'charge_input': deadline})
        proxy = self._proxy(prepared)
        wrapped = proxy.begin_stream('steps')
        with pytest.raises(CallDeadlineExceeded) as refused:
            wrapped.charge_input(16)
        error = refused.value
        assert error.__cause__ is deadline
        assert error.transport_phase == 'pre_dispatch'
        assert error.message == 'call deadline exceeded'

    def test_stream_semantic_failure_projects_named_subclass(self):
        missing = _semantic_core(
            ErrorCode.ResourceNotFound, "route not found: prepared-stream",
        )
        prepared = _StubPreparedStream({'stream': missing})
        proxy = self._proxy(prepared)
        wrapped = proxy.begin_stream('steps')
        with pytest.raises(ResourceNotFound) as refused:
            wrapped.stream(b'payload')
        error = refused.value
        assert error.__cause__ is missing
        assert error.transport_phase == 'pre_dispatch'
        assert error.message == "route not found: prepared-stream"
        wrapped.close()
        assert prepared.closed

    def test_already_typed_error_passes_through_unwrapped(self):
        typed = ResourceNotFound('already typed')
        prepared = _StubPreparedStream({'stream': typed})
        wrapped = self._proxy(prepared).begin_stream('steps')
        with pytest.raises(ResourceNotFound) as refused:
            wrapped.stream(b'payload')
        assert refused.value is typed
        assert refused.value.__cause__ is None

    def test_charge_capacity_e2e_projects_pre_dispatch(self):
        """A real FastDB stream charge over budget reaches the caller typed."""
        from c_two.transport.registry import _reset_default_runtime
        from tests.fixtures.stream_python import StreamPayloads, blob_payload

        class PayloadStepsResource:
            def steps(self, payload):
                yield payload

        _reset_default_runtime()
        cc.set_client(memory_budget_bytes=8 * 1024)
        proxy = source = None
        try:
            cc.register(StreamPayloads, PayloadStepsResource(), name='charge-budget')
            proxy = cc.connect(
                StreamPayloads, name='charge-budget', address=cc.server_address(),
            )
            source = blob_payload(b'x' * 64 * 1024)
            with pytest.raises(CallCapacityExceeded) as refused:
                proxy.steps(source)
            error = refused.value
            native = error.__cause__
            assert type(native) is CoreError
            assert native.category == 'semantic'
            assert int(native.code) == 717
            assert native.semantic.code == ErrorCode.CallCapacityExceeded
            assert error.transport_phase == native.transport_phase == 'pre_dispatch'
            assert error.details['stage'] == 'payload_buffer_admission'
        finally:
            if source is not None:
                source.close()
            if proxy is not None:
                cc.close(proxy)
            _reset_default_runtime()


class _ProjectionBound:
    """Custom native bound client: only the generated glue projects."""

    def __init__(self, prepared=None, call_failure=None):
        self.prepared = prepared
        self.call_failure = call_failure

    def begin_stream(self, method_name):
        return self.prepared

    def call(self, method_name, data):
        if self.call_failure is not None:
            raise self.call_failure
        return b''


class TestGeneratedContractClientStreamProjection:
    """Generated stream methods project native failures for custom clients."""

    def test_charge_input_semantic_failure_projects_named_subclass(self, tmp_path):
        contract = _load_generated_contract(
            tmp_path, 'convergence_stream_contract', SERVER_STREAM_CONTRACT,
        )
        deadline = _semantic_core(ErrorCode.CallDeadlineExceeded, 'call deadline exceeded')
        prepared = _StubPreparedStream({'charge_input': deadline})
        client = contract.ContractClient(_ProjectionBound(prepared=prepared))
        payload, _digest = echo_payload('stream-charge')
        with pytest.raises(CallDeadlineExceeded) as refused:
            client.method_1_echo(payload)
        error = refused.value
        assert error.__cause__ is deadline
        assert error.transport_phase == 'pre_dispatch'
        assert prepared.closed

    def test_stream_semantic_failure_projects_named_subclass(self, tmp_path):
        contract = _load_generated_contract(
            tmp_path, 'convergence_stream_contract', SERVER_STREAM_CONTRACT,
        )
        missing = _semantic_core(ErrorCode.ResourceNotFound, 'route not found')
        prepared = _StubPreparedStream({'stream': missing})
        client = contract.ContractClient(_ProjectionBound(prepared=prepared))
        payload, _digest = echo_payload('stream-dispatch')
        with pytest.raises(ResourceNotFound) as refused:
            client.method_1_echo(payload)
        error = refused.value
        assert error.__cause__ is missing
        assert error.transport_phase == 'pre_dispatch'
        assert prepared.closed

    def test_unary_call_semantic_failure_projects_named_subclass(self, tmp_path):
        contract = _load_generated_contract(
            tmp_path, 'convergence_stream_contract', SERVER_STREAM_CONTRACT,
        )
        missing = _semantic_core(ErrorCode.ResourceNotFound, 'route not found')
        client = contract.ContractClient(_ProjectionBound(call_failure=missing))
        with pytest.raises(ResourceNotFound) as refused:
            client.method_0_ping()
        error = refused.value
        assert error.__cause__ is missing
        assert error.transport_phase == 'pre_dispatch'

    def test_already_typed_error_passes_through_unwrapped(self, tmp_path):
        contract = _load_generated_contract(
            tmp_path, 'convergence_stream_contract', SERVER_STREAM_CONTRACT,
        )
        typed = ResourceNotFound('already typed')
        prepared = _StubPreparedStream({'stream': typed})
        client = contract.ContractClient(_ProjectionBound(prepared=prepared))
        payload, _digest = echo_payload('stream-typed')
        with pytest.raises(ResourceNotFound) as refused:
            client.method_1_echo(payload)
        assert refused.value is typed
        assert refused.value.__cause__ is None


@cc.crm(namespace='test.codec_error_boundaries', version='0.1.0')
class CodecBoundary:
    def echo(self, value: object) -> object: ...

    @cc.stream
    def steps(self, value: object) -> Iterator[object]: ...


class _CodecBoundaryResource:
    def __init__(self):
        self.calls = 0

    def echo(self, value):
        self.calls += 1
        return _OutputReconstruction()

    def steps(self, value):
        yield self.echo(value)


_closed_connection = None


def _call_closed_connection():
    return _closed_connection.echo(None)


class _OutputReconstruction:
    def __reduce__(self):
        return _call_closed_connection, ()


class _InputReduction:
    def __reduce__(self):
        _call_closed_connection()


@pytest.mark.parametrize('method', ['echo', 'steps'])
def test_pickle_hooks_cannot_leak_downstream_dispatch_phase(method, monkeypatch):
    runtime = cc.Runtime()
    resource = _CodecBoundaryResource()
    runtime.register(CodecBoundary, resource, name='codec-boundary')
    closed = runtime.connect(CodecBoundary, name='codec-boundary')
    cc.close(closed)
    monkeypatch.setattr(sys.modules[__name__], '_closed_connection', closed)
    ipc = runtime.connect(CodecBoundary, name='codec-boundary', address=runtime.server_address())
    try:
        call = getattr(ipc, method)
        with pytest.raises(SerializationError) as encoded:
            call(_InputReduction())
        assert encoded.value.code == ErrorCode.ClientInputSerializing
        assert encoded.value.transport_phase == 'pre_dispatch'
        assert isinstance(encoded.value.__cause__, ResourceClosed)
        assert resource.calls == 0

        stream = call(None) if method == 'steps' else None
        try:
            with pytest.raises(SerializationError) as decoded:
                if stream is None:
                    call(None)
                else:
                    next(stream)
            assert decoded.value.code == ErrorCode.ClientOutputDeserializing
            assert decoded.value.transport_phase is None
            assert isinstance(decoded.value.__cause__, ResourceClosed)
            assert decoded.value.__cause__.transport_phase == 'pre_dispatch'
            assert resource.calls == 1
        finally:
            if stream is not None:
                assert stream.close()['completed']
    finally:
        cc.close(ipc)
        runtime.shutdown()


@pytest.mark.parametrize('method', ['echo', 'steps'])
@pytest.mark.parametrize('use_core_error', [False, True])
def test_resource_error_projection_matches_local_and_ipc(method, use_core_error, monkeypatch):
    from c_two import _native

    controls = {
        'transport_phase': 'pre_dispatch', 'dispatch_phase': 'pre_dispatch',
        'fallback_eligible': 'true', 'route_withdrawal': 'true',
    }
    if use_core_error:
        with pytest.raises(CoreError) as caught:
            _native.ConnectAttempt(timeout_seconds=0)
        primary = caught.value
        expected = primary.semantic
        primary.code = int(ErrorCode.ResourceClosed)
        primary.semantic_message = 'changed diagnostic'
        primary.details.update(controls)
    else:
        primary = CallDeadlineExceeded('business', details={**controls, 'marker': 'keep'})
        expected = CallDeadlineExceeded('business', details={'marker': 'keep'})

    def fail(value):
        raise primary

    runtime = cc.Runtime()
    resource = _CodecBoundaryResource()
    monkeypatch.setattr(resource, 'echo', fail)
    runtime.register(CodecBoundary, resource, name='resource-error')
    local = runtime.connect(CodecBoundary, name='resource-error')
    ipc = runtime.connect(CodecBoundary, name='resource-error', address=runtime.server_address())
    try:
        for proxy in (local, ipc):
            stream = proxy.steps(None) if method == 'steps' else None
            try:
                with pytest.raises(CallDeadlineExceeded) as caught:
                    if stream is None:
                        proxy.echo(None)
                    else:
                        next(stream)
                projected = caught.value
                assert projected.transport_phase is None
                assert projected.message == expected.message
                assert projected.details == {
                    key: value for key, value in expected.details.items() if key not in controls
                }
                if proxy is local and method == 'echo':
                    assert projected.__cause__ is primary
                    if isinstance(primary, CoreError):
                        assert primary.fallback_eligible is False
                elif isinstance(projected.__cause__, CoreError):
                    assert projected.__cause__.fallback_eligible is None
                    assert projected.__cause__.route_withdrawal is None
            finally:
                if stream is not None:
                    assert stream.close()['completed']
    finally:
        cc.close(ipc)
        cc.close(local)
        runtime.shutdown()


def test_stream_buffer_open_failure_releases_item_and_cancels():
    class BrokenBuffer:
        release_calls = 0

        def release(self):
            self.release_calls += 1

    item = BrokenBuffer()
    native = _FakeNativeStream([item])
    stream = Stream._from_encoded(native, pickle_should_not_decode)
    with pytest.raises(SerializationError) as failure:
        next(stream)
    assert failure.value.code == ErrorCode.ClientOutputFromBuffer
    assert failure.value.transport_phase is None
    assert isinstance(failure.value.__cause__, TypeError)
    assert native.cancelled
    assert item.release_calls == 1


def pickle_should_not_decode(_value):
    raise AssertionError('could not open the stream item buffer')
