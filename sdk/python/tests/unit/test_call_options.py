"""SDK call-entry gates preserve caller-thread serialization and CRM metadata."""
from __future__ import annotations

import inspect
import pickle
import threading

import pytest

import c_two as cc
from fastdb4py.payload import Payload
from c_two.transport.client.proxy import CRMProxy


@pytest.fixture(params=['bound', 'class'])
def invoke(request):
    if request.param == 'bound':
        return lambda crm, *args, **kwargs: crm.echo(*args, **kwargs)
    return lambda crm, *args, **kwargs: type(crm).echo(crm, *args, **kwargs)


@cc.crm(namespace='test.call_options.unit', version='0.1.0')
class OptionsContract:
    def echo(self, value: object, timeout: float = 3.0) -> object:
        ...


class Resource:
    def __init__(self):
        self.calls = 0

    def echo(self, value, timeout=3.0):
        self.calls += 1
        return value, timeout


def thread_connection():
    resource = Resource()
    crm = OptionsContract()
    crm.client = CRMProxy.thread_local(resource)
    return crm, resource


@pytest.mark.parametrize('timeout', [0, 0.001, 1])
def test_finite_thread_policy_rejects_before_resource_or_serializer(timeout, invoke):
    connection, resource = thread_connection()
    view = cc.with_call_options(connection, timeout=timeout)
    events = []
    with pytest.raises(cc.error.UnsupportedCallMode) as failure:
        invoke(view, NonSendValue(events))
    assert int(failure.value.code) == 716
    assert resource.calls == 0
    assert events == []


@pytest.mark.parametrize('timeout', [None, ...])
def test_unlimited_thread_keeps_direct_objects_and_business_timeout(timeout, invoke):
    connection, resource = thread_connection()
    view = cc.with_call_options(connection, timeout=timeout)
    value = object()
    result, business_timeout = invoke(view, value, timeout=9.0)
    assert result is value
    assert business_timeout == 9.0
    assert resource.calls == 1
    cc.close(view)
    assert connection.echo(value)[0] is value


@pytest.mark.parametrize('timeout', [-1, float('nan'), float('inf'), float('-inf'), 1e300])
def test_timeout_validation_is_native(timeout):
    connection, _ = thread_connection()
    with pytest.raises((ValueError, OverflowError)):
        cc.with_call_options(connection, timeout=timeout)


def test_bound_method_metadata_and_hold_survive_options_view():
    connection, _ = thread_connection()
    view = cc.with_call_options(connection, timeout=None)
    assert view.echo.__self__ is view
    assert view.echo.__name__ == 'echo'
    assert inspect.signature(view.echo) == inspect.signature(connection.echo)
    assert view.echo._input_payload_binding is connection.echo._input_payload_binding
    with cc.hold(view.echo)('value', timeout=8) as held:
        assert held.value == ('value', 8)


class Prepared:
    def __init__(self, events):
        self.events = events

    def call(self, data):
        self.events.append(('call', threading.get_ident()))
        return pickle.dumps(pickle.loads(data))

    def close(self):
        self.events.append(('close', threading.get_ident()))


class NativeClient:
    def __init__(self, events):
        self.events = events

    def begin_call(self, method):
        self.events.append(('begin', threading.get_ident()))
        return Prepared(self.events)


class NonSendValue:
    def __init__(self, events):
        self.events = events

    def __reduce__(self):
        self.events.append(('serialize', threading.get_ident()))
        return str, ('serialized',)


def test_begin_precedes_serializer_and_keeps_it_on_caller_thread(invoke):
    events = []
    connection = OptionsContract()
    connection.client = CRMProxy.ipc(NativeClient(events), 'options')
    assert invoke(connection, NonSendValue(events), timeout=7) == ('serialized', 7)
    assert [stage for stage, _ in events] == ['begin', 'serialize', 'call', 'close']
    assert {thread for _, thread in events} == {threading.get_ident()}


def test_rejected_slot_does_not_invoke_serializer(invoke):
    events = []

    class RejectClient(NativeClient):
        def begin_call(self, method):
            raise cc.error.CallCapacityExceeded('no slot')

    connection = OptionsContract()
    connection.client = CRMProxy.ipc(RejectClient(events), 'options')
    with pytest.raises(cc.error.CallCapacityExceeded):
        invoke(connection, NonSendValue(events))
    assert events == []


def test_fastdb_known_length_gate_precedes_binary_materialization(monkeypatch, invoke):
    import json
    from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

    spec_value = {
        'schema': 'fastdb.payload.v1', 'profile': 'record.v1',
        'entries': [{'id': 'value', 'cardinality': 'one',
                     'type': {'kind': 'u8', 'nullable': False}}],
        'components': [],
    }

    @cc.crm(namespace='test.call_options.known_length', version='0.1.0')
    class Portable:
        @cc.transfer(input=spec_value, output=spec_value)
        def echo(self, payload: Payload) -> Payload:
            ...

    spec = CompiledSpec.compile(json.dumps(spec_value).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(7)
    plan = builder.freeze()
    builder.close()
    source = plan.execute(BuildPolicy.ALLOW_STAGING).payload
    plan.close()
    spec.close()
    events = []

    class RejectBytes(Prepared):
        def charge_input(self, nbytes):
            events.append(('charge', nbytes))
            raise cc.error.CallCapacityExceeded('byte budget')

    class KnownClient(NativeClient):
        def begin_call(self, method):
            return RejectBytes(events)

    original = Payload.binary_bytes

    def counted_copy(self):
        events.append(('copy', None))
        return original(self)

    monkeypatch.setattr(Payload, 'binary_bytes', counted_copy)
    connection = Portable()
    connection.client = CRMProxy.ipc(KnownClient(events), 'portable')
    try:
        with pytest.raises(cc.error.CallCapacityExceeded):
            invoke(connection, source)
        assert events[0] == ('charge', source.execution_report().used_bytes)
        assert [event for event, _ in events] == ['charge', 'close']
    finally:
        source.close()




def test_options_view_keeps_the_original_class_method_descriptor():
    connection, _ = thread_connection()
    descriptor = type(connection).echo
    view = cc.with_call_options(connection, timeout=None)
    assert type(view).echo is descriptor
    assert view.echo.__func__ is connection.echo.__func__ is descriptor
    assert 'echo' not in view.__dict__ and 'echo' not in connection.__dict__


@pytest.mark.parametrize('error_class', [cc.error.CallDeadlineExceeded, cc.error.CallCapacityExceeded])
def test_entry_refusal_preserves_canonical_phase_and_never_serializes(error_class, invoke):
    events = []
    refusal = error_class('native refused', details={
        'transport_phase': 'pre_dispatch', 'fallback_eligible': 'false',
        'route_withdrawal': 'false',
    })

    class Refuse(NativeClient):
        def begin_call(self, method):
            # Exercise the native semantic projection boundary as well.
            core = cc.error.CoreError('semantic', 'outer admission failure')
            core.code = int(refusal.code)
            core.name = refusal.code.name
            core.details = dict(refusal.details)
            core.semantic_message = refusal.message
            core._transport_phase = 'pre_dispatch'
            raise core

    connection = OptionsContract()
    connection.client = CRMProxy.ipc(Refuse(events), 'options')
    with pytest.raises(error_class) as failure:
        invoke(connection, NonSendValue(events))
    assert failure.value.code == refusal.code
    assert failure.value.message == 'native refused'
    assert failure.value.transport_phase == 'pre_dispatch'
    assert failure.value.details == refusal.details
    assert isinstance(failure.value.__cause__, cc.error.CoreError)
    assert events == []


class AccountingClient(NativeClient):
    def __init__(self, events):
        super().__init__(events)
        self.operations = 0
        self.bytes = 0

    def begin_call(self, method):
        owner = self
        owner.operations += 1

        class AccountingPrepared(Prepared):
            def charge_input(self, nbytes):
                owner.bytes = nbytes

            def call(self, data):
                raise AssertionError('invalid input must never dispatch')

            def close(self):
                owner.operations = 0
                owner.bytes = 0
                super().close()

        return AccountingPrepared(self.events)


@pytest.fixture
def portable_input():
    import json
    from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec

    spec_value = {
        'schema': 'fastdb.payload.v1', 'profile': 'record.v1',
        'entries': [{'id': 'value', 'cardinality': 'one',
                     'type': {'kind': 'u8', 'nullable': False}}],
        'components': [],
    }

    @cc.crm(namespace='test.call_options.report_errors', version='0.1.0')
    class Portable:
        @cc.transfer(input=spec_value, output=spec_value)
        def echo(self, payload: Payload) -> Payload:
            ...

    spec = CompiledSpec.compile(json.dumps(spec_value).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(7)
    plan = builder.freeze()
    builder.close()
    source = plan.execute(BuildPolicy.ALLOW_STAGING).payload
    plan.close()
    spec.close()
    try:
        yield Portable, source
    finally:
        source.close()


@pytest.mark.parametrize('canonical', [False, True])
def test_report_failure_uses_serializer_boundary_and_releases_preparation(portable_input, monkeypatch, invoke, canonical):
    contract, source = portable_input
    events = []
    native = AccountingClient(events)
    connection = contract()
    connection.client = CRMProxy.ipc(native, 'portable')
    primary = (cc.error.CallCapacityExceeded('report refusal', details={'transport_phase': 'pre_dispatch'})
               if canonical else RuntimeError('report failed'))

    def fail_report(self):
        raise primary

    monkeypatch.setattr(Payload, 'execution_report', fail_report)
    expected = cc.error.CallCapacityExceeded if canonical else cc.error.SerializationError
    with pytest.raises(expected) as failure:
        invoke(connection, source)
    if canonical:
        assert failure.value is primary
        assert failure.value.transport_phase == 'pre_dispatch'
    else:
        assert failure.value.code == cc.error.ErrorCode.ClientInputSerializing
        assert failure.value.transport_phase == 'pre_dispatch'
        assert failure.value.__cause__ is primary
    assert native.operations == native.bytes == 0
    assert [stage for stage, _ in events] == ['close']


def test_invalid_fastdb_owner_keeps_cause_details_and_releases_preparation(portable_input, invoke):
    contract, source = portable_input
    native = AccountingClient([])
    connection = contract()
    connection.client = CRMProxy.ipc(native, 'portable')
    source.close()
    with pytest.raises(cc.error.SerializationError) as failure:
        invoke(connection, source)
    assert failure.value.code == cc.error.ErrorCode.ClientInputSerializing
    assert failure.value.transport_phase == 'pre_dispatch'
    assert failure.value.details['cause_owner'] == 'fastdb'
    assert failure.value.details['fastdb_symbol']
    assert native.operations == native.bytes == 0


def test_closed_remote_view_refusal_is_pre_dispatch_and_never_serializes(invoke):
    events = []
    connection = OptionsContract()
    connection.client = CRMProxy.ipc(NativeClient(events), 'options')
    connection.client.terminate()
    with pytest.raises(cc.error.ResourceClosed) as failure:
        invoke(connection, NonSendValue(events))
    assert failure.value.message == 'CRM proxy is closed'
    assert failure.value.transport_phase == 'pre_dispatch'
    assert events == []
