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
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

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
        self.cancel_calls = 0

    def next_item(self):
        if not self._items:
            return None
        return self._items.pop(0)

    def check_deadline(self):
        return None

    def cancel(self):
        self.cancelled = True
        self.cancel_calls += 1

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

    def test_charge_capacity_e2e_projects_pre_dispatch(self, tmp_path):
        """A real FastDB stream charge over budget reaches the caller typed."""
        from tests.fixtures.stream_python import StreamPayloads, blob_payload

        class PayloadStepsResource:
            def steps(self, payload):
                yield payload

        runtime = cc.Runtime()
        if sys.platform != 'win32':
            runtime.set_local_endpoint(root=str(tmp_path))
        runtime.set_client(memory_budget_bytes=8 * 1024)
        proxy = source = None
        try:
            runtime.register(StreamPayloads, PayloadStepsResource(), name='charge-budget')
            proxy = runtime.connect(
                StreamPayloads, name='charge-budget', address=runtime.server_address(),
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
            runtime.shutdown()


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


@cc.crm(namespace='test.error-convergence.borrowed', version='0.1.0')
class _BorrowedFailureContract:
    @cc.transfer(input=ECHO_SPEC)
    def fail_unary(self, payload: Payload) -> None: ...

    @cc.stream
    @cc.transfer(input=ECHO_SPEC, output=ECHO_SPEC)
    def fail_stream(self, payload: Payload) -> Iterator[Payload]: ...


class _BorrowedFailureResource:
    def __init__(self) -> None:
        self.saved_unary = None
        self.saved_stream = None

    def fail_unary(self, payload: Payload) -> None:
        self.saved_unary = payload
        raise ValueError(10**5000)

    def fail_stream(self, payload: Payload) -> Iterator[Payload]:
        self.saved_stream = payload
        raise ValueError(10**5000)
        yield payload  # pragma: no cover


def test_borrowed_input_invalidated_when_resource_raises_unprintable_exception(tmp_path):
    from fastdb4py.payload import Payload, PayloadError
    from c_two.error import ResourceExecutionError

    # Ensure Python's int string conversion limit triggers on 10**5000.
    previous_limit = sys.get_int_max_str_digits()
    sys.set_int_max_str_digits(4300)
    runtime = cc.Runtime()
    if sys.platform != 'win32':
        runtime.set_local_endpoint(root=str(tmp_path))
    resource = _BorrowedFailureResource()
    runtime.register(
        _BorrowedFailureContract,
        resource,
        name='borrowed-unprintable',
        input_lifetime={'fail_unary': 'borrowed', 'fail_stream': 'borrowed'},
    )
    proxy = runtime.connect(
        _BorrowedFailureContract,
        name='borrowed-unprintable',
        address=runtime.server_address(),
    )
    payload_a, _ = echo_payload('alpha')
    payload_b, _ = echo_payload('beta')
    assert isinstance(payload_a, Payload)
    try:
        with pytest.raises(ResourceExecutionError) as unary_err:
            proxy.fail_unary(payload_a)
        assert unary_err.value.code == ErrorCode.ResourceFunctionExecuting
        assert unary_err.value.transport_phase is None
        assert resource.saved_unary is not None
        with pytest.raises(PayloadError):
            resource.saved_unary.entry_view(0)

        stream = proxy.fail_stream(payload_b)
        try:
            with pytest.raises(ResourceExecutionError) as stream_err:
                next(stream)
            assert stream_err.value.code == ErrorCode.ResourceFunctionExecuting
            assert stream_err.value.transport_phase is None
        finally:
            assert stream.close()['completed']
        assert resource.saved_stream is not None
        with pytest.raises(PayloadError):
            resource.saved_stream.entry_view(0)
    finally:
        sys.set_int_max_str_digits(previous_limit)
        payload_a.close()
        payload_b.close()
        cc.close(proxy)
        runtime.shutdown()


def test_unprintable_output_decode_error_cannot_leak_inner_pre_dispatch():
    from c_two.crm.payload_plan import PayloadBinding, PayloadPlanKind
    from c_two.crm.transferable import _build_transfer_wrapper

    class HostileDecodeError(Exception):
        def __str__(self) -> str:
            raise ResourceClosed('inner pre_dispatch', transport_phase='pre_dispatch')

    def bad_decode(_data):
        raise HostileDecodeError('ignored')

    binding = PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=lambda value: b'ok',
        deserialize=bad_decode,
    )

    def unary_op(self): ...

    wrapped_unary = _build_transfer_wrapper(unary_op, output=binding)

    class FakeClient:
        supports_direct_call = False

        def call(self, _method, _args):
            return b'payload'

    class FakeCrm:
        direction = '->'
        client = FakeClient()

    with pytest.raises(SerializationError) as unary_caught:
        wrapped_unary(FakeCrm())
    assert unary_caught.value.code == ErrorCode.ClientOutputDeserializing
    assert unary_caught.value.transport_phase is None
    assert isinstance(unary_caught.value.__cause__, HostileDecodeError)

    stream = Stream._from_encoded(_FakeNativeStream([b'payload']), bad_decode)
    with pytest.raises(SerializationError) as stream_caught:
        next(stream)
    assert stream_caught.value.code == ErrorCode.ClientOutputDeserializing
    assert stream_caught.value.transport_phase is None
    assert isinstance(stream_caught.value.__cause__, HostileDecodeError)


def test_generated_python_stream_client_real_sdk_fastdb_errors_not_double_wrapped(
    tmp_path, monkeypatch,
):
    from fastdb4py.payload import Payload, PayloadError

    fixture = (
        Path(__file__).resolve().parents[4]
        / 'core/foundation/c2-contract/tests/fixtures/server-stream.contract.json'
    )
    compiled = cc.contract.compile_artifacts(fixture.read_bytes(), target='python')
    out_dir = tmp_path / 'generated_stream'
    for artifact in compiled.artifacts:
        dest = out_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    module_path = out_dir / 'python/c_two_contract.py'
    spec = importlib.util.spec_from_file_location('_gen_stream_real_sdk', module_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
        request_payload, _ = echo_payload('request')
        valid_payload, _ = echo_payload('reply')
        valid_bytes = valid_payload.binary_bytes()

        other_spec = CompiledSpec.compile(
            json.dumps(
                {
                    'schema': 'fastdb.payload.v1',
                    'profile': 'record.v1',
                    'entries': [
                        {'id': 'value', 'cardinality': 'one', 'type': {'kind': 'u16', 'nullable': False}},
                    ],
                    'components': [],
                },
                separators=(',', ':'),
            ).encode()
        )
        other_builder = Builder.create(other_spec)
        other_builder.entry_begin(0, 1).value_u16(9)
        other_plan = other_builder.freeze()
        wrong_payload = other_plan.execute(BuildPolicy.ALLOW_STAGING).payload
        wrong_bytes = wrong_payload.binary_bytes()
        other_plan.close()
        other_builder.close()
        other_spec.close()

        next_items: list[bytes] = []

        class Prepared:
            def charge_input(self, _nbytes: int) -> None:
                return None

            def stream(self, _data: bytes):
                return _FakeNativeStream(next_items)

            def close(self) -> None:
                return None

        class Bound:
            local_copy_tracker = None
            route_name = 'generated-real-sdk'

            def begin_stream(self, _method: str):
                return Prepared()

            def call(self, _method: str, _data: bytes) -> bytes:
                return b''

        client = module.ContractClient(Bound())
        expected_fastdb_keys = {
            'cause_owner',
            'fastdb_code',
            'fastdb_symbol',
            'fastdb_path',
            'fastdb_message',
            'fastdb_details_json',
        }

        try:
            # 1. Corrupt item bytes on both regular and hold_* stream -> code 8 + all 6 FastDB keys.
            for opener in (client.method_1_echo, client.hold_method_1_echo):
                next_items[:] = [b'\x00corrupt-fastdb-item']
                stream = opener(request_payload)
                with pytest.raises(SerializationError) as corrupt_err:
                    next(stream)
                assert corrupt_err.value.code == ErrorCode.ClientOutputFromBuffer
                assert corrupt_err.value.transport_phase is None
                assert isinstance(corrupt_err.value.__cause__, PayloadError)
                assert expected_fastdb_keys <= corrupt_err.value.details.keys()
                assert corrupt_err.value.details['cause_owner'] == 'fastdb'

            # 2a. Wrong-spec binary rejected in Payload.open_copy -> code 8 + all 6 FastDB keys.
            next_items[:] = [wrong_bytes]
            stream = client.method_1_echo(request_payload)
            with pytest.raises(SerializationError) as wrong_open_err:
                next(stream)
            assert wrong_open_err.value.code == ErrorCode.ClientOutputFromBuffer
            assert wrong_open_err.value.transport_phase is None
            assert isinstance(wrong_open_err.value.__cause__, PayloadError)
            assert expected_fastdb_keys <= wrong_open_err.value.details.keys()

            # 2b. require_spec_sha256 mismatch after open_copy -> code 6 + all 6 FastDB keys.
            payload_mod_name = next(name for name in vars(module) if name.startswith('_PAYLOAD_'))
            payload_mod = getattr(module, payload_mod_name)
            next_items[:] = [valid_bytes]
            stream = client.method_1_echo(request_payload)
            monkeypatch.setattr(payload_mod, 'PAYLOAD_SHA256_BYTES', b'\x00' * 32)
            with pytest.raises(SerializationError) as mismatch_err:
                next(stream)
            assert mismatch_err.value.code == ErrorCode.ClientOutputDeserializing
            assert mismatch_err.value.transport_phase is None
            assert isinstance(mismatch_err.value.__cause__, PayloadError)
            assert expected_fastdb_keys <= mismatch_err.value.details.keys()
            monkeypatch.undo()

            # 3. HeldResult.release() with real PayloadError from Payload.invalidate -> code 8 + all 6 FastDB keys.
            closed_sample, _ = echo_payload('closed')
            closed_sample.close()
            try:
                closed_sample.entry_view(0)
            except PayloadError as captured_payload_error:
                real_payload_error = captured_payload_error
            else:
                raise AssertionError('expected PayloadError from closed payload')

            original_invalidate = Payload.invalidate

            def failing_invalidate(self):
                original_invalidate(self)
                raise real_payload_error

            monkeypatch.setattr(Payload, 'invalidate', failing_invalidate)
            # Re-bind ContractClient method against updated Payload.invalidate
            next_items[:] = [valid_bytes]
            held_stream = client.hold_method_1_echo(request_payload)
            held_item = next(held_stream)
            assert isinstance(held_item, cc.HeldResult)
            with pytest.raises(SerializationError) as release_err:
                held_item.release()
            assert release_err.value.code == ErrorCode.ClientOutputFromBuffer
            assert release_err.value.transport_phase is None
            assert release_err.value.__cause__ is real_payload_error
            assert expected_fastdb_keys <= release_err.value.details.keys()
            assert release_err.value.details['cause_owner'] == 'fastdb'
        finally:
            wrong_payload.close()
            valid_payload.close()
            request_payload.close()
    finally:
        sys.modules.pop(spec.name, None)


def test_post_dispatch_prepared_close_failure_strips_pre_dispatch_markers():
    from c_two.crm.payload_plan import no_payload_binding, python_pickle_output_binding
    from c_two.crm.transferable import _build_transfer_wrapper
    import pickle

    cleanup_cause = ResourceClosed(
        'handle closed after dispatch',
        details={
            'transport_phase': 'pre_dispatch',
            'dispatch_phase': 'pre_dispatch',
            'fallback_eligible': 'true',
            'route_withdrawal': 'true',
            'marker': 'keep',
        },
        transport_phase='pre_dispatch',
    )

    class PreparedUnary:
        def call(self, _method, _data):
            return pickle.dumps('ok')

        def close(self):
            raise cleanup_cause

    class UnaryClient:
        supports_direct_call = False

        def prepare_call(self, _method):
            return PreparedUnary()

    class UnaryCrm:
        direction = '->'
        client = UnaryClient()

    def unary_fn(self) -> str: ...

    wrapped_unary = _build_transfer_wrapper(
        unary_fn,
        input=no_payload_binding(),
        output=python_pickle_output_binding(unary_fn),
    )
    with pytest.raises(ResourceClosed) as unary_err:
        wrapped_unary(UnaryCrm())
    assert unary_err.value is not cleanup_cause
    assert unary_err.value.__cause__ is cleanup_cause
    assert unary_err.value.transport_phase is None
    assert unary_err.value.details == {'marker': 'keep'}

    class PreparedStream:
        def stream(self, _data):
            return _FakeNativeStream([])

        def close(self):
            raise cleanup_cause

    class StreamClient:
        supports_direct_call = False
        local_copy_tracker = None
        route_name = 'post-dispatch-stream'

        def begin_stream(self, _method):
            return PreparedStream()

    class StreamCrm:
        direction = '->'
        client = StreamClient()

    def stream_fn(self) -> Iterator[str]: ...
    stream_fn.__cc_stream__ = True

    wrapped_stream = _build_transfer_wrapper(
        stream_fn,
        input=no_payload_binding(),
        output=python_pickle_output_binding(stream_fn),
    )
    with pytest.raises(ResourceClosed) as stream_err:
        wrapped_stream(StreamCrm())
    assert stream_err.value is not cleanup_cause
    assert stream_err.value.__cause__ is cleanup_cause
    assert stream_err.value.transport_phase is None
    assert stream_err.value.details == {'marker': 'keep'}


class _BatchClosedError(ResourceClosed):
    __slots__ = ('batch',)

    def __init__(self, batch: int, message: str, details: dict[str, str]):
        super().__init__(message, details=details, transport_phase='pre_dispatch')
        self.batch = batch

    def __str__(self) -> str:
        return f'batch={self.batch}:{self.message}'


def test_local_and_ipc_unary_cancelled_error_and_custom_slot_subclass(tmp_path):
    import asyncio
    from c_two.error import ResourceExecutionError

    mode = {'kind': 'cancel'}

    class Resource:
        def echo(self, value):
            if mode['kind'] == 'cancel':
                raise asyncio.CancelledError('cancelled inside resource')
            raise _BatchClosedError(
                42,
                'closed batch',
                {
                    'transport_phase': 'pre_dispatch',
                    'dispatch_phase': 'pre_dispatch',
                    'fallback_eligible': 'true',
                    'route_withdrawal': 'true',
                    'marker': 'retained',
                },
            )

        def steps(self, value):
            yield self.echo(value)

    runtime = cc.Runtime()
    if sys.platform != 'win32':
        runtime.set_local_endpoint(root=str(tmp_path))
    runtime.register(CodecBoundary, Resource(), name='local-parity')
    local = runtime.connect(CodecBoundary, name='local-parity')
    ipc = runtime.connect(CodecBoundary, name='local-parity', address=runtime.server_address())
    try:
        # 1. asyncio.CancelledError becomes ResourceExecutionError(3) across local & IPC unary and streams.
        for proxy in (local, ipc):
            with pytest.raises(ResourceExecutionError) as unary_cancel:
                proxy.echo(None)
            assert unary_cancel.value.code == ErrorCode.ResourceFunctionExecuting
            assert unary_cancel.value.transport_phase is None
            if proxy is local:
                assert isinstance(unary_cancel.value.__cause__, asyncio.CancelledError)

            stream = proxy.steps(None)
            try:
                with pytest.raises(ResourceExecutionError) as stream_cancel:
                    next(stream)
                assert stream_cancel.value.code == ErrorCode.ResourceFunctionExecuting
                assert stream_cancel.value.transport_phase is None
            finally:
                assert stream.close()['completed']

        # 2. Custom CCError subclass with __slots__ preserves slot state on local unary and clears controls.
        mode['kind'] = 'batch'
        with pytest.raises(_BatchClosedError) as slot_err:
            local.echo(None)
        assert slot_err.value.batch == 42
        assert str(slot_err.value) == 'batch=42:closed batch'
        assert slot_err.value.transport_phase is None
        assert slot_err.value.details == {'marker': 'retained'}

        stream = local.steps(None)
        try:
            with pytest.raises(ResourceClosed) as local_stream_err:
                next(stream)
            assert local_stream_err.value.transport_phase is None
            assert local_stream_err.value.details == {'marker': 'retained'}
        finally:
            assert stream.close()['completed']
    finally:
        cc.close(ipc)
        cc.close(local)
        runtime.shutdown()


def test_custom_plain_bytes_stream_hold_releases_cleanly():
    invalidated: list[bytes] = []
    stream = Stream._from_encoded(
        _FakeNativeStream([b'plain-bytes-item']),
        bytes,
        held=True,
        tracker=object(),
        route_name='plain-stream',
        method_name='items',
        invalidator=invalidated.append,
    )
    held = next(stream)
    assert held.value == b'plain-bytes-item'
    assert bytes(held.unsafe_buffer) == b'plain-bytes-item'
    held.release()
    assert invalidated == [b'plain-bytes-item']
    with pytest.raises(RuntimeError, match='no longer accessible'):
        _ = held.value
    with pytest.raises(RuntimeError, match='no longer accessible'):
        _ = held.unsafe_buffer


class _HijackingCopyError(ResourceClosed):
    __slots__ = ('tag',)

    def __init__(self, tag: str):
        super().__init__('executed failure', details={'transport_phase': 'pre_dispatch', 'keep': '1'})
        self.tag = tag

    def __copy__(self):
        raise ResourceClosed('hijacked pre_dispatch', transport_phase='pre_dispatch')


class _SelfReturningCopyError(ResourceClosed):
    def __copy__(self):
        return self


def test_hostile_custom_copy_hooks_cannot_hijack_phase_or_self_link(tmp_path):
    import copy

    hijack = _HijackingCopyError('slot-ok')
    with pytest.raises(ResourceClosed, match='hijacked pre_dispatch'):
        copy.copy(hijack)

    self_return = _SelfReturningCopyError(
        'self failure',
        details={'transport_phase': 'pre_dispatch', 'keep': '2'},
        transport_phase='pre_dispatch',
    )
    assert copy.copy(self_return) is self_return

    calls = 0
    current = {'err': hijack}

    class Resource:
        def echo(self, value):
            nonlocal calls
            calls += 1
            raise current['err']

        def steps(self, value):
            yield self.echo(value)

    runtime = cc.Runtime()
    if sys.platform != 'win32':
        runtime.set_local_endpoint(root=str(tmp_path))
    runtime.register(CodecBoundary, Resource(), name='hostile-copy')
    local = runtime.connect(CodecBoundary, name='hostile-copy')
    ipc = runtime.connect(CodecBoundary, name='hostile-copy', address=runtime.server_address())
    try:
        for original in (hijack, self_return):
            current['err'] = original
            with pytest.raises(type(original)) as local_err:
                local.echo(None)
            assert local_err.value is not original
            assert local_err.value.__cause__ is original
            assert local_err.value.__cause__ is not local_err.value
            assert local_err.value.transport_phase is None
            assert 'transport_phase' not in local_err.value.details
            assert original.transport_phase == 'pre_dispatch'
            assert original.details['transport_phase'] == 'pre_dispatch'
            if isinstance(local_err.value, _HijackingCopyError):
                assert local_err.value.tag == 'slot-ok'

            with pytest.raises(ResourceClosed) as ipc_err:
                ipc.echo(None)
            assert ipc_err.value.transport_phase is None
            assert 'transport_phase' not in ipc_err.value.details
        assert calls == 4
    finally:
        cc.close(ipc)
        cc.close(local)
        runtime.shutdown()


def test_production_error_paths_release_without_cyclic_gc():
    import gc
    import weakref
    from c_two.crm.payload_plan import no_payload_binding, python_pickle_output_binding
    from c_two.crm.transferable import _build_transfer_wrapper

    def op(self) -> str: ...

    wrapped = _build_transfer_wrapper(
        op,
        input=no_payload_binding(),
        output=python_pickle_output_binding(op),
    )

    class ServerResource:
        def op(self) -> str:
            raise ResourceClosed('callback closed', transport_phase='pre_dispatch')

    class ServerContract:
        direction = '<-'
        resource = ServerResource()

    class DualFailingPrepared:
        def call(self, _method, _args):
            raise ResourceNotFound('dispatch failed', transport_phase='pre_dispatch')

        def close(self):
            raise RuntimeError('close also failed')

    class DualFailingClient:
        supports_direct_call = False

        def prepare_call(self, _method):
            return DualFailingPrepared()

    class ClientContract:
        direction = '->'
        client = DualFailingClient()

    was_enabled = gc.isenabled()
    gc.collect()
    gc.disable()
    try:
        server_ref = None
        try:
            wrapped(ServerContract())
        except ResourceClosed as err:
            server_ref = weakref.ref(err)
            err = None
        assert server_ref is not None and server_ref() is None

        dual_ref = None
        try:
            wrapped(ClientContract())
        except ResourceNotFound as err:
            dual_ref = weakref.ref(err)
            err = None
        assert dual_ref is not None and dual_ref() is None
    finally:
        if was_enabled:
            gc.enable()


def test_held_memoryview_stream_and_undelivered_stream_cancel(tmp_path):
    fixture = (
        Path(__file__).resolve().parents[4]
        / 'core/foundation/c2-contract/tests/fixtures/server-stream.contract.json'
    )
    compiled = cc.contract.compile_artifacts(fixture.read_bytes(), target='python')
    out_dir = tmp_path / 'gen_mv_stream'
    for artifact in compiled.artifacts:
        dest = out_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    module_path = out_dir / 'python/c_two_contract.py'
    spec = importlib.util.spec_from_file_location('_gen_mv_stream_mod', module_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
        req_payload, _ = echo_payload('req')
        reply_payload, _ = echo_payload('mv-ok')
        backing = bytearray(reply_payload.binary_bytes())
        mv_item = memoryview(backing)
        stream_holder: dict[str, _FakeNativeStream] = {}
        fail_close = {'active': False}

        class Prepared:
            def charge_input(self, _n: int) -> None:
                return None

            def stream(self, _data: bytes):
                ns = _FakeNativeStream([mv_item])
                stream_holder['last'] = ns
                return ns

            def close(self) -> None:
                if fail_close['active']:
                    raise ResourceClosed('close boom', transport_phase='pre_dispatch')

        class Bound:
            local_copy_tracker = None
            route_name = 'mv-route'

            def begin_stream(self, _method: str):
                return Prepared()

            def call(self, _method: str, _data: bytes) -> bytes:
                return b''

        try:
            from fastdb4py.payload import PayloadError

            client = module.ContractClient(Bound())
            held_stream = client.hold_method_1_echo(req_payload)
            held = next(held_stream)
            unwrapped = held.value
            assert unwrapped.binary_bytes() == bytes(backing)
            held.release()
            with pytest.raises(PayloadError):
                unwrapped.entry_view(0)
            with pytest.raises(RuntimeError, match='no longer accessible'):
                _ = held.value
            # Both child memoryview and source memoryview are released so bytearray can resize.
            backing.append(0)

            # Undelivered stream is cancelled exactly once when prepared.close() fails.
            fail_close['active'] = True
            with pytest.raises(ResourceClosed) as gen_close_err:
                client.method_1_echo(req_payload)
            assert gen_close_err.value.transport_phase is None
            assert stream_holder['last'].cancelled is True
            assert stream_holder['last'].cancel_calls == 1

            from c_two.crm.payload_plan import no_payload_binding, python_pickle_output_binding
            from c_two.crm.transferable import _build_transfer_wrapper

            def crm_stream_fn(self) -> Iterator[str]: ...
            crm_stream_fn.__cc_stream__ = True

            wrapped_crm_stream = _build_transfer_wrapper(
                crm_stream_fn,
                input=no_payload_binding(),
                output=python_pickle_output_binding(crm_stream_fn),
            )

            class CrmClient:
                supports_direct_call = False
                local_copy_tracker = None
                route_name = 'crm-cancel-route'

                def begin_stream(self, _method: str):
                    return Prepared()

            class CrmInstance:
                direction = '->'
                client = CrmClient()

            with pytest.raises(ResourceClosed) as crm_close_err:
                wrapped_crm_stream(CrmInstance())
            assert crm_close_err.value.transport_phase is None
            assert stream_holder['last'].cancelled is True
            assert stream_holder['last'].cancel_calls == 1
        finally:
            reply_payload.close()
            req_payload.close()
    finally:
        sys.modules.pop(spec.name, None)


class _EvilDetailsDict(dict):
    def items(self):
        raise RuntimeError('EvilDetailsDict.items must not run')

    def __iter__(self):
        raise RuntimeError('EvilDetailsDict.__iter__ must not run')

    def __contains__(self, _key):
        raise RuntimeError('EvilDetailsDict.__contains__ must not run')


class _SlottedControlClosed(ResourceClosed):
    __slots__ = ('details', '_transport_phase', 'custom')

    def __init__(self, message: str, details: dict[str, str], custom: str):
        super().__init__(message, details=details, transport_phase='pre_dispatch')
        self.details = _EvilDetailsDict(details)
        self.custom = custom


class _DomainIOClosed(ResourceClosed, OSError):
    __slots__ = ('custom_io',)

    def __init__(self, message: str, details: dict[str, str]):
        super().__init__(message, details=details, transport_phase='pre_dispatch')
        self.errno = 5
        self.strerror = 'Input/output error'
        self.filename = 'grid.bin'
        self.custom_io = 'io-slot'


def test_slotted_control_fields_evil_dict_and_oserror_multiple_inheritance(tmp_path):
    import copy
    import pickle
    from c_two import error as cc_err

    slotted = _SlottedControlClosed(
        'slotted closed',
        {
            'transport_phase': 'pre_dispatch',
            'dispatch_phase': 'pre_dispatch',
            'biz': 'kept',
        },
        'slot-tag',
    )
    assert slotted.transport_phase == 'pre_dispatch'

    cleaned_post = cc_err._post_dispatch_cleanup_error(slotted)
    assert isinstance(cleaned_post, _SlottedControlClosed)
    assert cleaned_post is not slotted
    assert cleaned_post.transport_phase is None
    assert type(cleaned_post.details) is dict
    assert cleaned_post.details == {'biz': 'kept'}
    assert cleaned_post.custom == 'slot-tag'
    assert slotted.transport_phase == 'pre_dispatch'

    domain_io = _DomainIOClosed(
        'disk gone',
        {'dispatch_phase': 'pre_dispatch', 'biz': 'io-kept'},
    )
    copied_io = copy.copy(domain_io)
    pickled_io = pickle.loads(pickle.dumps(domain_io))
    for clone in (copied_io, pickled_io):
        assert isinstance(clone, _DomainIOClosed)
        assert clone.code == ErrorCode.ResourceClosed
        assert clone.errno == 5
        assert clone.filename == 'grid.bin'
        assert clone.custom_io == 'io-slot'

    executed = 0
    current = {'err': slotted}

    class Resource:
        def echo(self, value):
            nonlocal executed
            executed += 1
            raise current['err']

        def steps(self, value):
            yield self.echo(value)

    runtime = cc.Runtime()
    if sys.platform != 'win32':
        runtime.set_local_endpoint(root=str(tmp_path))
    runtime.register(CodecBoundary, Resource(), name='slot-and-io')
    local = runtime.connect(CodecBoundary, name='slot-and-io')
    ipc = runtime.connect(CodecBoundary, name='slot-and-io', address=runtime.server_address())
    try:
        with pytest.raises(_SlottedControlClosed) as local_slot:
            local.echo(None)
        assert executed == 1
        assert local_slot.value is not slotted
        assert local_slot.value.transport_phase is None
        assert type(local_slot.value.details) is dict
        assert local_slot.value.details == {'biz': 'kept'}
        assert local_slot.value.custom == 'slot-tag'
        assert slotted.transport_phase == 'pre_dispatch'

        with pytest.raises(ResourceClosed) as ipc_slot:
            ipc.echo(None)
        assert executed == 2
        assert ipc_slot.value.code == ErrorCode.ResourceClosed
        assert ipc_slot.value.transport_phase is None
        assert ipc_slot.value.details == {'biz': 'kept'}

        current['err'] = domain_io
        with pytest.raises(_DomainIOClosed) as local_io:
            local.echo(None)
        assert executed == 3
        assert local_io.value is not domain_io
        assert local_io.value.code == ErrorCode.ResourceClosed
        assert local_io.value.errno == 5
        assert local_io.value.filename == 'grid.bin'
        assert local_io.value.custom_io == 'io-slot'
        assert local_io.value.transport_phase is None
        assert local_io.value.details == {'biz': 'io-kept'}
        assert domain_io.transport_phase == 'pre_dispatch'

        with pytest.raises(ResourceClosed) as ipc_io:
            ipc.echo(None)
        assert executed == 4
        assert ipc_io.value.code == ErrorCode.ResourceClosed
        assert ipc_io.value.transport_phase is None
        assert ipc_io.value.details == {'biz': 'io-kept'}
    finally:
        cc.close(ipc)
        cc.close(local)
        runtime.shutdown()


def test_http_probe_conflict_without_wire_phase_and_corrupt_utf8_on_acquire():
    import json
    import threading
    from http.server import BaseHTTPRequestHandler
    from c_two.crm.contract import crm_contract
    from c_two.error import CoreError, ProtocolViolation, RouteConflict
    from tests.fixtures.http_server import LoopbackHTTPServer

    crm_ns, crm_name, crm_ver, abi_hash, signature_hash = crm_contract(CodecBoundary).native_args()
    posts: list[str] = []
    server_url_box: dict[str, str] = {}

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path.startswith('/_resolve/probe-conflict'):
                body = json.dumps([{
                    'name': 'probe-conflict',
                    'relay_url': server_url_box['url'],
                    'crm_ns': crm_ns,
                    'crm_name': crm_name,
                    'crm_ver': crm_ver,
                    'abi_hash': abi_hash,
                    'signature_hash': signature_hash,
                    'max_payload_size': 1024,
                }]).encode('utf-8')
                self.send_response(200)
            elif self.path.startswith('/_probe/probe-conflict'):
                # Wire probe response intentionally omits dispatch_phase.
                body = json.dumps({
                    'version': 1,
                    'code': 719,
                    'name': 'RouteConflict',
                    'message': 'concurrent registration conflict',
                    'details': {'route': 'probe-conflict'},
                }).encode('utf-8')
                self.send_response(409)
            elif self.path.startswith('/_resolve/corrupt-utf8'):
                body = (
                    b'{"version":1,"code":719,"name":"RouteConflict",'
                    b'"message":"bad \xff byte","details":{"dispatch_phase":"pre_dispatch"}}'
                )
                self.send_response(409)
            else:
                body = b'{}'
                self.send_response(404)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            posts.append(self.path)
            self.send_response(500)
            self.end_headers()

        def log_message(self, *_args):
            pass

    server = LoopbackHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = False
    server_url = f'http://127.0.0.1:{server.server_port}'
    server_url_box['url'] = server_url
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    runtime = cc.Runtime()
    runtime.set_relay_anchor(server_url)
    try:
        for kwargs in ({'address': server_url}, {}):
            with pytest.raises(RouteConflict) as conflict_err:
                runtime.connect(CodecBoundary, name='probe-conflict', timeout=2.0, **kwargs)
            assert conflict_err.value.code == ErrorCode.RouteConflict
            assert conflict_err.value.transport_phase == 'pre_dispatch'
            assert isinstance(conflict_err.value.__cause__, CoreError)
            assert conflict_err.value.__cause__.transport_phase == 'pre_dispatch'

            with pytest.raises(ProtocolViolation) as corrupt_err:
                runtime.connect(CodecBoundary, name='corrupt-utf8', timeout=2.0, **kwargs)
            assert corrupt_err.value.code == ErrorCode.ProtocolViolation
            assert corrupt_err.value.transport_phase == 'pre_dispatch'
            assert isinstance(corrupt_err.value.__cause__, CoreError)
            assert corrupt_err.value.__cause__.transport_phase == 'pre_dispatch'
        assert posts == []
    finally:
        runtime.shutdown()
        server.shutdown()
        server.server_close()
        worker.join(2)
        assert not worker.is_alive()


class _BlockingClosedError(ResourceClosed, BlockingIOError):
    def __init__(self, message: str):
        ResourceClosed.__init__(
            self,
            message,
            details={'dispatch_phase': 'pre_dispatch', 'biz': 'blocking'},
            transport_phase='pre_dispatch',
        )
        self.errno = 11
        self.characters_written = 4096


class _NoDictClosedError(ResourceClosed):
    @property
    def __dict__(self):  # type: ignore[override]
        raise AttributeError('instance __dict__ is hidden')


def test_nodict_stream_wrap_cancels_once_and_blocking_callback_preserves_getset(tmp_path):
    from c_two.crm.payload_plan import no_payload_binding, python_pickle_output_binding
    from c_two.crm.transferable import _build_transfer_wrapper

    blocking = _BlockingClosedError('partial write before close')
    runtime = cc.Runtime()
    if sys.platform != 'win32':
        ipc_dir = tmp_path / 'ipc'
        ipc_dir.mkdir()
        runtime.set_local_endpoint(root=str(ipc_dir))

    class Resource:
        def echo(self, value):
            raise blocking

        def steps(self, value):
            yield value

    runtime.register(CodecBoundary, Resource(), name='blocking-io')
    local = runtime.connect(CodecBoundary, name='blocking-io')
    try:
        with pytest.raises(_BlockingClosedError) as cb_err:
            local.echo(None)
        assert cb_err.value is not blocking
        assert cb_err.value.transport_phase is None
        assert cb_err.value.characters_written == 4096
        assert str(cb_err.value) == str(blocking)
        assert cb_err.value.details == {'biz': 'blocking'}
        assert blocking.transport_phase == 'pre_dispatch'
    finally:
        cc.close(local)
        runtime.shutdown()

    fixture = (
        Path(__file__).resolve().parents[4]
        / 'core/foundation/c2-contract/tests/fixtures/server-stream.contract.json'
    )
    compiled = cc.contract.compile_artifacts(fixture.read_bytes(), target='python')
    out_dir = tmp_path / 'gen_nodict'
    for artifact in compiled.artifacts:
        dest = out_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    spec = importlib.util.spec_from_file_location('_gen_nodict_mod', out_dir / 'python/c_two_contract.py')
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    req_payload, _ = echo_payload('req')
    try:
        spec.loader.exec_module(module)
        holder: dict[str, _FakeNativeStream] = {}

        class Prepared:
            def charge_input(self, _n: int) -> None:
                return None

            def stream(self, _data: bytes):
                ns = _FakeNativeStream([])
                holder['last'] = ns
                return ns

            def close(self) -> None:
                return None

        class BrokenTrackerClient:
            supports_direct_call = False
            route_name = 'nodict-route'

            @property
            def local_copy_tracker(self):
                raise _NoDictClosedError(
                    'tracker boom',
                    details={'dispatch_phase': 'pre_dispatch', 'biz': 'nodict'},
                    transport_phase='pre_dispatch',
                )

            def begin_stream(self, _method: str):
                return Prepared()

            def call(self, _method: str, _data: bytes) -> bytes:
                return b''

        gen_client = module.ContractClient(BrokenTrackerClient())
        with pytest.raises(_NoDictClosedError) as gen_wrap_err:
            gen_client.method_1_echo(req_payload)
        assert gen_wrap_err.value.transport_phase is None
        assert gen_wrap_err.value.details == {'biz': 'nodict'}
        assert holder['last'].cancel_calls == 1

        with pytest.raises(_NoDictClosedError) as gen_hold_wrap_err:
            gen_client.hold_method_1_echo(req_payload)
        assert gen_hold_wrap_err.value.transport_phase is None
        assert holder['last'].cancel_calls == 1

        def crm_stream_fn(self) -> Iterator[str]: ...
        crm_stream_fn.__cc_stream__ = True

        wrapped_crm = _build_transfer_wrapper(
            crm_stream_fn,
            input=no_payload_binding(),
            output=python_pickle_output_binding(crm_stream_fn),
        )

        class CrmInstance:
            direction = '->'
            client = BrokenTrackerClient()

        with pytest.raises(_NoDictClosedError) as crm_wrap_err:
            wrapped_crm(CrmInstance())
        assert crm_wrap_err.value.transport_phase is None
        assert crm_wrap_err.value.details == {'biz': 'nodict'}
        assert holder['last'].cancel_calls == 1
    finally:
        req_payload.close()
        sys.modules.pop(spec.name, None)


def test_register_409_namespace_mismatch_and_malformed_envelopes_never_fake_duplicate(tmp_path):
    import json
    import threading
    from http.server import BaseHTTPRequestHandler
    from c_two.error import ProtocolViolation, ResourceAlreadyRegistered
    from tests.fixtures.http_server import LoopbackHTTPServer

    mode = {'kind': 'namespace'}

    class RegisterRelay(BaseHTTPRequestHandler):
        def do_POST(self):
            _ = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            if mode['kind'] == 'namespace':
                body = json.dumps({
                    'error': 'LocalEndpointNamespaceMismatch',
                    'message': 'resource and relay local endpoint namespaces differ',
                }).encode('utf-8')
            elif mode['kind'] == 'truncated_json':
                body = b'{"version":1,"code":703'
            elif mode['kind'] == 'json_array':
                body = json.dumps([1, 703, 'ResourceAlreadyRegistered', 'array dup', {}]).encode('utf-8')
            else:
                body = b'{"version":1,"code":719,"name":"RouteConflict","message":"bad \xff byte","details":{}}'
            self.send_response(409)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    class Resource:
        def echo(self, value):
            return value

        def steps(self, value):
            yield value

    server = LoopbackHTTPServer(('127.0.0.1', 0), RegisterRelay)
    server.daemon_threads = False
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    reg_runtime = cc.Runtime()
    if sys.platform != 'win32':
        reg_dir = tmp_path / 'reg_ipc'
        reg_dir.mkdir()
        reg_runtime.set_local_endpoint(root=str(reg_dir))
    reg_runtime.set_relay_anchor(f'http://127.0.0.1:{server.server_port}')
    try:
        # 1. LocalEndpointNamespaceMismatch keeps CoreError(status_code=409, relay_duplicate=False, rollback).
        mode['kind'] = 'namespace'
        with pytest.raises(CoreError) as ns_err:
            reg_runtime.register(CodecBoundary, Resource(), name='ns-mismatch')
        assert not isinstance(ns_err.value, ResourceAlreadyRegistered)
        assert ns_err.value.lifecycle_kind == 'register_failure'
        assert ns_err.value.status_code == 409
        assert ns_err.value.relay_duplicate is False
        assert ns_err.value.semantic is None
        assert 'LocalEndpointNamespaceMismatch' in ns_err.value.message
        assert ns_err.value.rollback is not None
        assert ns_err.value.rollback['closed_reason'] == 'registration_rollback'

        # 2. Truncated JSON, JSON array, and corrupt UTF-8 409 all project ProtocolViolation(713).
        for kind in ('truncated_json', 'json_array', 'corrupt_utf8'):
            mode['kind'] = kind
            with pytest.raises(ProtocolViolation) as bad_err:
                reg_runtime.register(CodecBoundary, Resource(), name=f'bad-{kind}')
            assert bad_err.value.code == ErrorCode.ProtocolViolation
            assert isinstance(bad_err.value.__cause__, CoreError)
            assert bad_err.value.__cause__.status_code == 409
            assert bad_err.value.__cause__.relay_duplicate is False
            assert bad_err.value.__cause__.rollback is not None
    finally:
        reg_runtime.shutdown()
        server.shutdown()
        server.server_close()
        worker.join(2)
        assert not worker.is_alive()


def test_http_call_404_701_contradiction_409_719_reacquire_and_array_float_envelopes():
    """HTTP business calls align phase and re-acquisition across 404/701, 409/719, arrays and floats."""
    import json
    import pickle
    import threading
    from http.server import BaseHTTPRequestHandler
    from c_two.crm.contract import crm_contract
    from c_two.error import ProtocolViolation, ResourceNotFound, RouteConflict
    from tests.fixtures.http_server import LoopbackHTTPServer

    crm_ns, crm_name, crm_ver, abi_hash, signature_hash = crm_contract(CodecBoundary).native_args()
    state = {
        'resolves': 0,
        'probes': 0,
        'posts': 0,
        'first_post': (404, b'{}'),
    }

    class CallRelay(BaseHTTPRequestHandler):
        def do_GET(self):
            if '/_resolve/' in self.path:
                state['resolves'] += 1
                body = json.dumps([{
                    'name': 'route',
                    'relay_url': f'http://127.0.0.1:{self.server.server_port}',
                    'crm_ns': crm_ns,
                    'crm_name': crm_name,
                    'crm_ver': crm_ver,
                    'abi_hash': abi_hash,
                    'signature_hash': signature_hash,
                    'max_payload_size': 65536,
                }]).encode('utf-8')
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if '/_probe/' in self.path:
                state['probes'] += 1
                self.send_response(200)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            self.send_response(404)
            self.end_headers()

        def do_POST(self):
            _ = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            state['posts'] += 1
            if state['posts'] == 1:
                status, body = state['first_post']
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            reply = pickle.dumps('ok')
            self.send_response(200)
            self.send_header('Content-Type', 'application/octet-stream')
            self.send_header('Content-Length', str(len(reply)))
            self.end_headers()
            self.wfile.write(reply)

        def log_message(self, *_args):
            pass

    server = LoopbackHTTPServer(('127.0.0.1', 0), CallRelay)
    server.daemon_threads = False
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    try:
        for status, body_bytes, expected_cls, expected_phase, expected_resolves in (
            (
                404,
                json.dumps({
                    'version': 1,
                    'code': 701,
                    'name': 'ResourceNotFound',
                    'message': 'missing',
                    'details': {},
                }).encode('utf-8'),
                ResourceNotFound,
                'pre_dispatch',
                2,
            ),
            (
                404,
                json.dumps({
                    'version': 1,
                    'code': 701,
                    'name': 'ResourceNotFound',
                    'message': 'missing',
                    'details': {'dispatch_phase': 'dispatch_uncertain'},
                }).encode('utf-8'),
                ResourceNotFound,
                'dispatch_uncertain',
                1,
            ),
            (
                409,
                json.dumps({
                    'version': 1,
                    'code': 719,
                    'name': 'RouteConflict',
                    'message': 'conflict on call',
                    'details': {'route': 'route', 'dispatch_phase': 'pre_dispatch'},
                }).encode('utf-8'),
                RouteConflict,
                'pre_dispatch',
                2,
            ),
            (
                503,
                json.dumps([
                    1, 717, 'CallCapacityExceeded', 'array', {'dispatch_phase': 'pre_dispatch'},
                ]).encode('utf-8'),
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717.0,"name":"CallCapacityExceeded","message":"float","details":{"dispatch_phase":"pre_dispatch"}}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                404,
                b'{"version":1,"code":701,"name":"ResourceNotFound","message":"missing","details":{},"extra":"bad\xff"}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'\xef\xbb\xbf{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap","details":{"dispatch_phase":"pre_dispatch"}}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717,"name":"CallCapacityExceeded","message":"bad \\ud800","details":{"dispatch_phase":"pre_dispatch"}}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap","details":{"k":1,"k":"ok","dispatch_phase":"pre_dispatch"}}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                (
                    '{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap",'
                    '"details":{"dispatch_phase":"pre_dispatch"},'
                    f'"ext":{"[" * 126}{"]" * 126}}}'
                ).encode('utf-8'),
                CallCapacityExceeded,
                'pre_dispatch',
                1,
            ),
            (
                503,
                (
                    '{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap",'
                    '"details":{"dispatch_phase":"pre_dispatch"},'
                    f'"ext":{"[" * 127}{"]" * 127}}}'
                ).encode('utf-8'),
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap","details":{"dispatch_phase":"pre_dispatch"},"ext":17976931348623158e292}',
                CallCapacityExceeded,
                'pre_dispatch',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap","details":{"dispatch_phase":"pre_dispatch"},"ext":179769313486231581e291}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
            (
                503,
                b'{"version":1,"code":717,"name":"CallCapacityExceeded","message":"cap","details":{"dispatch_phase":"pre_dispatch"},"ext":179769313486231590e291}',
                ProtocolViolation,
                'dispatch_uncertain',
                1,
            ),
        ):
            state.update(resolves=0, probes=0, posts=0, first_post=(status, body_bytes))
            rt = cc.Runtime()
            rt.set_relay_anchor(f'http://127.0.0.1:{server.server_port}')
            proxy = rt.connect(CodecBoundary, name='route')
            try:
                with pytest.raises(expected_cls) as call_err:
                    proxy.echo('hello')
                assert call_err.value.transport_phase == expected_phase
                assert proxy.echo('hello') == 'ok'
                assert (state['resolves'], state['probes'], state['posts']) == (
                    expected_resolves,
                    expected_resolves,
                    2,
                )
            finally:
                cc.close(proxy)
                rt.shutdown()
    finally:
        server.shutdown()
        server.server_close()
        worker.join(2)
        assert not worker.is_alive()


class _NotesList(list):
    pass


class _SlottedNotesClosed(ResourceClosed):
    __slots__ = ('__notes__',)


def test_post_dispatch_prepared_close_exception_group_and_fastdb_hostile_class_getter(tmp_path):
    """Prepared close never leaks pre_dispatch on rewritten ExceptionGroup, and FastDB cause checks avoid __class__."""
    import builtins as py_builtins
    import importlib.util
    import pickle
    import fastdb4py.payload  # noqa: F401 - ensure fastdb4py.payload is in sys.modules
    from c_two.crm.transferable import _build_transfer_wrapper
    from c_two.crm.payload_plan import no_payload_binding, python_pickle_output_binding
    from c_two.error import (
        CCBaseError,
        CoreError,
        ErrorCode,
        ResourceExecutionError,
        SerializationError,
    )

    class _HostileClassCoreError(CoreError):
        @property
        def __class__(self):  # type: ignore[override]
            raise RuntimeError('__class__ getter must never execute')

    # 1. Unary resource adapter & resource stream with fastdb4py.payload loaded in sys.modules.
    runtime = cc.Runtime()
    if sys.platform != 'win32':
        ipc_dir = tmp_path / 'hostile_ipc'
        ipc_dir.mkdir()
        runtime.set_local_endpoint(root=str(ipc_dir))

    class HostileResource:
        def echo(self, value):
            raise _HostileClassCoreError('transport', 'resource unary hostile')

        def steps(self, value):
            raise _HostileClassCoreError('transport', 'resource stream hostile')
            yield value

    runtime.register(CodecBoundary, HostileResource(), name='hostile')
    remote = runtime.connect(CodecBoundary, name='hostile', address=runtime.server_address())
    try:
        with pytest.raises(ResourceExecutionError) as u_err:
            remote.echo('x')
        assert 'resource unary hostile' in u_err.value.message
        assert '__class__ getter' not in u_err.value.message

        stream = remote.steps('x')
        with pytest.raises(ResourceExecutionError) as s_err:
            next(stream)
        assert 'resource stream hostile' in s_err.value.message
        assert '__class__ getter' not in s_err.value.message
    finally:
        cc.close(remote)
        runtime.shutdown()

    # 2. Generated Python ContractClient response release raising _HostileClassCoreError -> code 8 SerializationError.
    fixture = (
        Path(__file__).resolve().parents[4]
        / 'tests/fixtures/contracts/portable-release.contract.json'
    )
    compiled = cc.contract.compile_artifacts(fixture.read_bytes(), target='python')
    out_dir = tmp_path / 'gen_hostile'
    for artifact in compiled.artifacts:
        dest = out_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    spec = importlib.util.spec_from_file_location('_gen_hostile_mod', out_dir / 'python/c_two_contract.py')
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    req_payload, _ = echo_payload('hello')
    try:
        spec.loader.exec_module(module)

        class BadReleaseResponse:
            def __bytes__(self) -> bytes:
                return b''

            def release(self) -> None:
                raise _HostileClassCoreError('lifecycle', 'release hostile')

        class BoundClient:
            def call(self, _method: str, _request: bytes):
                return BadReleaseResponse()

        gen_client = module.ContractClient(BoundClient())
        with pytest.raises(SerializationError) as rel_err:
            gen_client.method_1_echo(req_payload)
        assert rel_err.value.code == ErrorCode.ClientOutputFromBuffer
        assert 'release hostile' in rel_err.value.message
        assert '__class__ getter' not in rel_err.value.message
    finally:
        req_payload.close()
        sys.modules.pop(spec.name, None)

    # 3. Unary prepared.call() succeeds, then prepared.close() raises CCBaseError + ExceptionGroup with rewritten args.
    if hasattr(py_builtins, 'ExceptionGroup'):
        class _RewrittenGroupCloseError(CCBaseError, py_builtins.ExceptionGroup):  # type: ignore[misc]
            pass

        orig_close_err = _RewrittenGroupCloseError('close group', [ValueError('inner')])
        orig_close_err.args = ('rewritten-close-arg',)
        orig_close_err._transport_phase = 'pre_dispatch'  # type: ignore[attr-defined]

        class PreparedCloseBoom:
            def charge_input(self, _n: int) -> None:
                return None

            def call(self, _method: str, _data: bytes | None) -> bytes:
                return pickle.dumps('ok')

            def close(self) -> None:
                raise orig_close_err

        class UnaryClient:
            supports_direct_call = False
            local_copy_tracker = None

            def prepare_call(self, _method: str):
                return PreparedCloseBoom()

        def unary_fn(self) -> str: ...

        wrapped_unary = _build_transfer_wrapper(
            unary_fn,
            input=no_payload_binding(),
            output=python_pickle_output_binding(unary_fn),
        )

        class CrmObj:
            direction = '->'
            client = UnaryClient()

        with pytest.raises(_RewrittenGroupCloseError) as close_caught:
            wrapped_unary(CrmObj())
        assert close_caught.value is not orig_close_err
        assert close_caught.value.transport_phase is None
        assert close_caught.value.args == ('rewritten-close-arg',)
        assert close_caught.value.__cause__ is orig_close_err

    # 4. Bare CCBaseError and custom CCBaseError forwarding inner pre_dispatch raised inside
    # a stateful resource method project to ResourceExecutionError(3, transport_phase=None)
    # identically across same-process unary, IPC unary, and stream calls.
    from c_two.error import ResourceClosed

    class _WrappedDownstreamError(CCBaseError):
        def __init__(self, inner: CCBaseError):
            super().__init__('business wrapper around downstream refusal')
            self.inner = inner

        @property
        def transport_phase(self):  # type: ignore[override]
            return self.inner.transport_phase

    for make_exc in (
        lambda: CCBaseError('bare base error after state mutation'),
        lambda: _WrappedDownstreamError(ResourceClosed('inner closed', transport_phase='pre_dispatch')),
    ):
        mutations = {'local_unary': 0, 'ipc_unary': 0, 'stream': 0}

        class StatefulBaseResource:
            def __init__(self, tag: str):
                self.tag = tag

            def echo(self, value):
                mutations[self.tag] += 1
                raise make_exc()

            def steps(self, value):
                mutations['stream'] += 1
                raise make_exc()
                yield value

        rt = cc.Runtime()
        if sys.platform != 'win32':
            base_dir = tmp_path / f'base_ipc_{id(make_exc)}'
            base_dir.mkdir()
            rt.set_local_endpoint(root=str(base_dir))
        rt.register(CodecBoundary, StatefulBaseResource('local_unary'), name='local-base')
        rt.register(CodecBoundary, StatefulBaseResource('ipc_unary'), name='ipc-base')
        local_proxy = rt.connect(CodecBoundary, name='local-base')
        ipc_proxy = rt.connect(CodecBoundary, name='ipc-base', address=rt.server_address())
        try:
            with pytest.raises(ResourceExecutionError) as local_u:
                local_proxy.echo('go')
            assert local_u.value.code == ErrorCode.ResourceFunctionExecuting
            assert local_u.value.transport_phase is None
            assert isinstance(local_u.value.__cause__, CCBaseError)
            assert mutations['local_unary'] == 1

            with pytest.raises(ResourceExecutionError) as ipc_u:
                ipc_proxy.echo('go')
            assert ipc_u.value.code == ErrorCode.ResourceFunctionExecuting
            assert ipc_u.value.transport_phase is None
            assert mutations['ipc_unary'] == 1

            s = local_proxy.steps('go')
            with pytest.raises(ResourceExecutionError) as local_s:
                next(s)
            assert local_s.value.code == ErrorCode.ResourceFunctionExecuting
            assert local_s.value.transport_phase is None
            assert mutations['stream'] == 1
        finally:
            cc.close(local_proxy)
            cc.close(ipc_proxy)
            rt.shutdown()

    # 5. Prepared handle close() raising _WrappedDownstreamError after call()/stream() dispatch
    # across hand-written unary, hand-written stream, generated stream and generated hold_* stream;
    # plus ResourceClosed subclass overriding @property transport_phase inside a local resource.
    prop_getter_calls = 0

    class _ForwardingClosed(ResourceClosed):
        def __init__(self):
            super().__init__('closed with property override', details={'biz': 'kept'})

        @property
        def transport_phase(self):  # type: ignore[override]
            nonlocal prop_getter_calls
            prop_getter_calls += 1
            return 'pre_dispatch'

    local_closed_runs = 0

    class ClosedPropertyResource:
        def echo(self, value):
            nonlocal local_closed_runs
            local_closed_runs += 1
            raise _ForwardingClosed()

        def steps(self, value):
            yield value

    rt2 = cc.Runtime()
    if sys.platform != 'win32':
        prop_dir = tmp_path / 'closed_prop_ipc'
        prop_dir.mkdir()
        rt2.set_local_endpoint(root=str(prop_dir))
    rt2.register(CodecBoundary, ClosedPropertyResource(), name='closed-prop')
    prop_proxy = rt2.connect(CodecBoundary, name='closed-prop')
    try:
        with pytest.raises(ResourceClosed) as prop_err:
            prop_proxy.echo('go')
        assert prop_getter_calls == 0
        assert type(prop_err.value) is ResourceClosed
        assert prop_err.value.code == ErrorCode.ResourceClosed
        assert prop_err.value.transport_phase is None
        assert prop_err.value.details == {'biz': 'kept'}
        assert isinstance(prop_err.value.__cause__, _ForwardingClosed)
        assert local_closed_runs == 1
    finally:
        cc.close(prop_proxy)
        rt2.shutdown()

    def make_wrapped_close_err():
        return _WrappedDownstreamError(ResourceClosed('inner closed', transport_phase='pre_dispatch'))

    # 5a. Hand-written unary close() after call()
    w_unary_err = make_wrapped_close_err()

    class WrappedCloseUnaryPrepared:
        def charge_input(self, _n: int) -> None:
            return None

        def call(self, _method: str, _data: bytes | None) -> bytes:
            return pickle.dumps('ok')

        def close(self) -> None:
            raise w_unary_err

    class WrappedCloseClient:
        supports_direct_call = False
        local_copy_tracker = None
        route_name = 'wrapped-close'

        def prepare_call(self, _method: str):
            return WrappedCloseUnaryPrepared()

        def begin_stream(self, _method: str):
            return WrappedCloseStreamPrepared()

    def hw_unary(self) -> str: ...
    def hw_stream(self) -> Iterator[str]: ...
    hw_stream.__cc_stream__ = True

    class WrappedCrm:
        direction = '->'
        client = WrappedCloseClient()

    wrapped_hw_unary = _build_transfer_wrapper(
        hw_unary, input=no_payload_binding(), output=python_pickle_output_binding(hw_unary),
    )
    with pytest.raises(CCBaseError) as hw_u_caught:
        wrapped_hw_unary(WrappedCrm())
    assert hw_u_caught.value is not w_unary_err
    assert hw_u_caught.value.transport_phase is None
    assert hw_u_caught.value.__cause__ is w_unary_err

    # 5b. Hand-written stream close() after stream()
    w_stream_err = make_wrapped_close_err()

    class WrappedCloseStreamPrepared:
        def charge_input(self, _n: int) -> None:
            return None

        def stream(self, _data: bytes | None):
            return _FakeNativeStream([])

        def close(self) -> None:
            raise w_stream_err

    wrapped_hw_stream = _build_transfer_wrapper(
        hw_stream, input=no_payload_binding(), output=python_pickle_output_binding(hw_stream),
    )
    with pytest.raises(CCBaseError) as hw_s_caught:
        wrapped_hw_stream(WrappedCrm())
    assert hw_s_caught.value is not w_stream_err
    assert hw_s_caught.value.transport_phase is None
    assert hw_s_caught.value.__cause__ is w_stream_err

    # 5c & 5d. Generated ContractClient stream and hold_* stream close() after stream()
    stream_fixture = (
        Path(__file__).resolve().parents[4]
        / 'core/foundation/c2-contract/tests/fixtures/server-stream.contract.json'
    )
    compiled_stream = cc.contract.compile_artifacts(stream_fixture.read_bytes(), target='python')
    gen_stream_dir = tmp_path / 'gen_wrapped_close'
    for artifact in compiled_stream.artifacts:
        dest = gen_stream_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    gspec = importlib.util.spec_from_file_location(
        '_gen_wrapped_close_mod', gen_stream_dir / 'python/c_two_contract.py',
    )
    assert gspec is not None and gspec.loader is not None
    gmod = importlib.util.module_from_spec(gspec)
    sys.modules[gspec.name] = gmod
    gpayload, _ = echo_payload('stream-in')
    try:
        gspec.loader.exec_module(gmod)
        for method_attr in ('method_1_echo', 'hold_method_1_echo'):
            gen_close_err = make_wrapped_close_err()

            class GenStreamPrepared:
                def charge_input(self, _n: int) -> None:
                    return None

                def stream(self, _data: bytes):
                    return _FakeNativeStream([])

                def close(self) -> None:
                    raise gen_close_err

            class GenStreamBound:
                local_copy_tracker = None
                route_name = 'gen-stream'

                def begin_stream(self, _method: str):
                    return GenStreamPrepared()

                def call(self, _method: str, _data: bytes) -> bytes:
                    return b''

            gen_sc = gmod.ContractClient(GenStreamBound())
            with pytest.raises(CCBaseError) as gs_caught:
                getattr(gen_sc, method_attr)(gpayload)
            assert gs_caught.value is not gen_close_err
            assert gs_caught.value.transport_phase is None
            assert gs_caught.value.__cause__ is gen_close_err
    finally:
        gpayload.close()
        sys.modules.pop(gspec.name, None)

    # 6. Post-BaseException MRO mixin, unhashable metaclass + hostile __class__ attribute,
    # CoreError semantic slot cycle prevention, and diagnostic preservation on rebuild.
    import gc
    import weakref
    from c_two.error import CoreError

    class _PostBaseMixin:
        details = {
            'dispatch_phase': 'pre_dispatch',
            'fallback_eligible': 'true',
            'biz': 'from-mixin',
        }

    class _MixinClosedError(ResourceClosed, _PostBaseMixin):
        def __init__(self):
            Exception.__init__(self, 'mixin closed')
            self.code = ErrorCode.ResourceClosed
            self.message = 'mixin closed'
            self._transport_phase = 'pre_dispatch'

    class _HostileAttr:
        @property
        def __class__(self):  # type: ignore[override]
            raise RuntimeError('__class__ accessed during descriptor check')

    class _UnhashableErrorMeta(type):
        __hash__ = None  # type: ignore[assignment]

        def __eq__(cls, other: object) -> bool:
            return cls is other

    class _UnhashableClosed(ResourceClosed, metaclass=_UnhashableErrorMeta):
        marker = _HostileAttr()

        def __init__(self):
            super().__init__(
                'unhashable closed',
                details={'dispatch_phase': 'pre_dispatch', 'biz': 'unhashable-kept'},
                transport_phase='pre_dispatch',
            )

    class _SetterDetailsClosed(ResourceClosed):
        def __init__(self):
            self._stored: dict[str, str] = {}
            super().__init__(
                'setter closed',
                details={'batch': '7', 'transport_phase': 'pre_dispatch'},
            )

        @property
        def details(self) -> dict[str, str]:  # type: ignore[override]
            return self._stored

        @details.setter
        def details(self, value: dict[str, str]) -> None:
            self._stored = dict(value)

    for idx, make_cls_err in enumerate((_MixinClosedError, _UnhashableClosed, _SetterDetailsClosed)):
        expected_biz = (
            {'biz': 'from-mixin'}
            if idx == 0
            else {'biz': 'unhashable-kept'}
            if idx == 1
            else {'batch': '7'}
        )

        class EdgeSemanticResource:
            def echo(self, value):
                raise make_cls_err()

            def steps(self, value):
                yield value

        rt_edge = cc.Runtime()
        if sys.platform != 'win32':
            edge_dir = tmp_path / f'edge_sem_{idx}'
            edge_dir.mkdir()
            rt_edge.set_local_endpoint(root=str(edge_dir))
        rt_edge.register(CodecBoundary, EdgeSemanticResource(), name=f'edge-{idx}')
        edge_proxy = rt_edge.connect(CodecBoundary, name=f'edge-{idx}')
        try:
            with pytest.raises(ResourceClosed) as edge_caught:
                edge_proxy.echo('go')
            assert edge_caught.value.code == ErrorCode.ResourceClosed
            assert edge_caught.value.transport_phase is None
            assert edge_caught.value.details == expected_biz
        finally:
            cc.close(edge_proxy)
            rt_edge.shutdown()

    # 6b. CoreError subclass shadowing semantic via __slots__ in prepared.close() after dispatch
    # through generated ContractClient (both unary _project_call and stream _close_prepared):
    # never projects ResourceClosed(pre_dispatch) and forms zero refcycle under gc.disable().
    from c_two.error import _post_dispatch_cleanup_error

    class _SlottedSemanticCore(CoreError):
        __slots__ = ('semantic',)

    unary_fixture = Path(__file__).resolve().parents[4] / 'tests/fixtures/contracts/portable-release.contract.json'
    compiled_unary = cc.contract.compile_artifacts(unary_fixture.read_bytes(), target='python')
    gen_unary_dir = tmp_path / 'gen_slotted_sem_core'
    for artifact in compiled_unary.artifacts:
        dest = gen_unary_dir / artifact.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(artifact.bytes)
    uspec = importlib.util.spec_from_file_location(
        '_gen_slotted_sem_mod', gen_unary_dir / 'python/c_two_contract.py',
    )
    assert uspec is not None and uspec.loader is not None
    umod = importlib.util.module_from_spec(uspec)
    sys.modules[uspec.name] = umod
    upayload_in, _ = echo_payload('in')
    upayload_out, _ = echo_payload('out')
    out_bytes = bytes(upayload_out.binary_bytes())
    try:
        uspec.loader.exec_module(umod)
        gc_was_enabled = gc.isenabled()
        gc.disable()
        try:
            def run_slotted_semantic_close():
                inner_sem = ResourceClosed('shadowed pre_dispatch', transport_phase='pre_dispatch')
                slotted_core = _SlottedSemanticCore('transport', 'close failed after reply')
                slotted_core.semantic = inner_sem  # type: ignore[misc]
                core_ref = weakref.ref(slotted_core)
                sem_ref = weakref.ref(inner_sem)

                class StreamSlottedPrepared:
                    def __init__(self, err: CoreError):
                        self._err = err

                    def charge_input(self, _n: int) -> None:
                        return None

                    def stream(self, _data: bytes):
                        return _FakeNativeStream([])

                    def _pop_err(self) -> CoreError:
                        err = self._err
                        self._err = None  # type: ignore[assignment]
                        return err

                    def close(self) -> None:
                        raise self._pop_err()

                class StreamSlottedBound:
                    local_copy_tracker = None
                    route_name = 'slotted-stream'

                    def __init__(self, err: CoreError):
                        self._err = err

                    def begin_stream(self, _method: str):
                        err = self._err
                        self._err = None  # type: ignore[assignment]
                        return StreamSlottedPrepared(err)

                    def call(self, _method: str, _data: bytes) -> bytes:
                        return b''

                gen_stream_client = gmod.ContractClient(StreamSlottedBound(slotted_core))
                try:
                    gen_stream_client.method_1_echo(upayload_in)
                    raise AssertionError('expected CoreError')
                except CoreError as caught_core:
                    assert type(caught_core) is CoreError
                    assert caught_core.transport_phase is None
                    assert caught_core.semantic is None

                # Also exercise uncleaned _SlottedSemanticCore reaching unary ContractClient._project_call
                inner_sem2 = ResourceClosed('direct shadowed pre_dispatch', transport_phase='pre_dispatch')
                slotted_core2 = _SlottedSemanticCore('transport', 'direct pre-cleaned close')
                slotted_core2.semantic = inner_sem2  # type: ignore[misc]
                core_ref2 = weakref.ref(slotted_core2)
                sem_ref2 = weakref.ref(inner_sem2)

                class RawSlottedBound:
                    def __init__(self, err: CoreError):
                        self._err = err

                    def _pop_err(self) -> CoreError:
                        err = self._err
                        self._err = None  # type: ignore[assignment]
                        return err

                    def call(self, _method: str, _data: bytes) -> bytes:
                        raise self._pop_err()

                gen_uc2 = umod.ContractClient(RawSlottedBound(slotted_core2))
                try:
                    gen_uc2.method_1_echo(upayload_in)
                    raise AssertionError('expected _SlottedSemanticCore')
                except CoreError as raw_caught:
                    assert raw_caught is slotted_core2
                    assert raw_caught.transport_phase is None

                del inner_sem, slotted_core, gen_stream_client, inner_sem2, slotted_core2, gen_uc2
                return core_ref, sem_ref, core_ref2, sem_ref2

            core_ref, sem_ref, core_ref2, sem_ref2 = run_slotted_semantic_close()
            assert core_ref() is None
            assert sem_ref() is None
            assert core_ref2() is None
            assert sem_ref2() is None
        finally:
            if gc_was_enabled:
                gc.enable()
    finally:
        upayload_in.close()
        upayload_out.close()
        sys.modules.pop(uspec.name, None)

    # 6c. Slotted CoreError diagnostic preservation and bare CCBaseError details preservation on rebuild.

    class _SlottedDiagCore(CoreError):
        __slots__ = ('io_kind', 'raw_os_error')

        @property
        def transport_phase(self):  # type: ignore[override]
            return 'pre_dispatch'

    diag_core = _SlottedDiagCore('transport', 'bind permission denied')
    diag_core.io_kind = 'permission_denied'
    diag_core.raw_os_error = 13
    diag_core.details = {'dispatch_phase': 'pre_dispatch', 'path': '/tmp/sock'}
    rebuilt_core = _post_dispatch_cleanup_error(diag_core)
    assert type(rebuilt_core) is CoreError
    assert rebuilt_core.transport_phase is None
    assert rebuilt_core.io_kind == 'permission_denied'
    assert rebuilt_core.raw_os_error == 13
    assert rebuilt_core.details == {'path': '/tmp/sock'}
    assert rebuilt_core.__cause__ is diag_core

    class _DetailsBaseError(CCBaseError):
        @property
        def transport_phase(self):  # type: ignore[override]
            return 'pre_dispatch'

    base_with_details = _DetailsBaseError('base cleanup error')
    base_with_details.details = {'biz': 'base-kept', 'transport_phase': 'pre_dispatch'}  # type: ignore[attr-defined]
    rebuilt_base = _post_dispatch_cleanup_error(base_with_details)
    assert type(rebuilt_base) is CCBaseError
    assert rebuilt_base.transport_phase is None
    assert rebuilt_base.details == {'biz': 'base-kept'}  # type: ignore[attr-defined]
    assert rebuilt_base.__cause__ is base_with_details

    # 7. str subclass __str__ never replaces resource error, spoofed __module__ == 'builtins' is caught,
    # list-subclass and slotted __notes__ stay independent, and updated/deleted details reflect throw-time state.
    import copy

    class _HostileMsgStr(str):
        def __str__(self) -> str:
            raise ResourceClosed('inner pre_dispatch from str.__str__', transport_phase='pre_dispatch')

    class _StrSubRebuildClosed(ResourceClosed):
        @property
        def transport_phase(self):  # type: ignore[override]
            return 'pre_dispatch'

    SpoofedBuiltinClosed = type(
        '_SpoofedBuiltinClosed',
        (ResourceClosed,),
        {
            '__module__': 'builtins',
            'transport_phase': property(lambda _self: 'pre_dispatch'),
        },
    )

    class _MutableSetterClosed(ResourceClosed):
        def __init__(self):
            self._d: dict[str, str] = {}
            super().__init__('mutable setter', details={'batch': 'before', 'transport_phase': 'pre_dispatch'})

        @property
        def details(self) -> dict[str, str]:  # type: ignore[override]
            return self._d

        @details.setter
        def details(self, val: dict[str, str]) -> None:
            self._d = dict(val)

    class _ClassDefaultDetailsClosed(ResourceClosed):
        details = {'batch': 'class-default', 'dispatch_phase': 'pre_dispatch'}  # type: ignore[assignment]

    r13_runs = 0

    class R13Resource:
        def echo(self, value):
            nonlocal r13_runs
            r13_runs += 1
            if value == 'str-sub':
                raise _StrSubRebuildClosed(_HostileMsgStr('business closed text'), details={'k': _HostileMsgStr('v')})
            if value == 'spoof-builtin':
                raise SpoofedBuiltinClosed('spoofed module closed', details={'spoof': 'kept'})
            if value == 'setter-mutated':
                err = _MutableSetterClosed()
                err.details['batch'] = 'after'
                err.details['added'] = 'yes'
                raise err
            err_del = _ClassDefaultDetailsClosed('deleted inst details', details={'batch': 'temp'})
            del err_del.details
            raise err_del

        def steps(self, value):
            yield value

    rt_r13 = cc.Runtime()
    if sys.platform != 'win32':
        r13_dir = tmp_path / 'r13_ipc'
        r13_dir.mkdir()
        rt_r13.set_local_endpoint(root=str(r13_dir))
    rt_r13.register(CodecBoundary, R13Resource(), name='r13')
    r13_proxy = rt_r13.connect(CodecBoundary, name='r13')
    try:
        with pytest.raises(ResourceClosed) as c_str:
            r13_proxy.echo('str-sub')
        assert c_str.value.code == ErrorCode.ResourceClosed
        assert c_str.value.transport_phase is None
        assert type(c_str.value.message) is str
        assert c_str.value.message == 'business closed text'
        assert c_str.value.details == {'k': 'v'}
        assert type(c_str.value.details['k']) is str
        assert r13_runs == 1

        with pytest.raises(ResourceClosed) as c_spoof:
            r13_proxy.echo('spoof-builtin')
        assert c_spoof.value.code == ErrorCode.ResourceClosed
        assert c_spoof.value.transport_phase is None
        assert c_spoof.value.details == {'spoof': 'kept'}
        assert r13_runs == 2

        with pytest.raises(ResourceClosed) as c_mut:
            r13_proxy.echo('setter-mutated')
        assert c_mut.value.code == ErrorCode.ResourceClosed
        assert c_mut.value.transport_phase is None
        assert c_mut.value.details == {'batch': 'after', 'added': 'yes'}
        assert r13_runs == 3

        with pytest.raises(ResourceClosed) as c_del:
            r13_proxy.echo('deleted-inst')
        assert c_del.value.code == ErrorCode.ResourceClosed
        assert c_del.value.transport_phase is None
        assert c_del.value.details == {'batch': 'class-default'}
        assert r13_runs == 4
    finally:
        cc.close(r13_proxy)
        rt_r13.shutdown()

    # 7b. __notes__ using list subclass and __notes__ in __slots__ remain independent across copy, pickle and cleanup.
    orig_notes_err = ResourceClosed('notes list sub', transport_phase='pre_dispatch')
    orig_notes_err.__notes__ = _NotesList(['biz-note'])  # type: ignore[assignment]
    cleaned_notes_err = _post_dispatch_cleanup_error(orig_notes_err)
    cleaned_notes_err.add_note('cleanup-note')
    assert list(orig_notes_err.__notes__) == ['biz-note']
    assert cleaned_notes_err.__notes__ == ['biz-note', 'cleanup-note']

    pickled_notes_err = pickle.loads(pickle.dumps(orig_notes_err))
    pickled_notes_err.add_note('pickle-note')
    assert list(orig_notes_err.__notes__) == ['biz-note']
    assert pickled_notes_err.__notes__ == ['biz-note', 'pickle-note']

    copied_slotted = _SlottedNotesClosed('slotted notes', transport_phase='pre_dispatch')
    copied_slotted.add_note('slot-note-1')
    shallow_slotted = copy.copy(copied_slotted)
    shallow_slotted.add_note('slot-note-2')
    roundtrip_slotted = pickle.loads(pickle.dumps(copied_slotted))
    roundtrip_slotted.add_note('slot-note-3')
    cleaned_slotted = _post_dispatch_cleanup_error(copied_slotted)
    cleaned_slotted.add_note('slot-note-4')
    assert copied_slotted.__notes__ == ['slot-note-1']
    assert shallow_slotted.__notes__ == ['slot-note-1', 'slot-note-2']
    assert roundtrip_slotted.__notes__ == ['slot-note-1', 'slot-note-3']
    assert cleaned_slotted.__notes__ == ['slot-note-1', 'slot-note-4']
