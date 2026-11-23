from concurrent.futures import ThreadPoolExecutor
from dataclasses import FrozenInstanceError
import os
import pickle
from threading import Event

import pytest

import c_two as cc
from c_two.error import (
    CallDeadlineExceeded, CallIdConflict, CallOutcomeUnknown, CallResultUnavailable,
    CoreError, ErrorCode, ResourceExecutionError,
)


@cc.crm(namespace='test.call-outcomes', version='1.0.0')
class Contract:
    def step(self, value: int | str | bytes, copies: int = 1) -> list[int | str | bytes]: ...

    def query_call(self) -> str: ...


class Resource:
    def __init__(self, entered=None, release=None):
        self.calls = 0
        self.last_result = None
        self.entered = entered
        self.release = release

    def step(self, value, copies):
        self.calls += 1
        if self.entered is not None:
            self.entered.set()
            assert self.release.wait(8)
        self.last_result = [value] * copies
        return self.last_result

    def query_call(self):
        return 'contract method'


@pytest.fixture
def runtimes(tmp_path):
    host = cc.Runtime()
    caller = cc.Runtime()
    if os.name != 'nt':
        host.set_local_endpoint(root=str(tmp_path))
        caller.set_local_endpoint(root=str(tmp_path))
    try:
        yield host, caller
    finally:
        caller.shutdown()
        host.shutdown()


def connect(runtimes, transport, start_c3_relay, tmp_path, resource):
    host, caller = runtimes
    if transport == 'relay':
        relay_url = start_c3_relay(ipc_root=tmp_path).url
        host.set_relay_anchor(relay_url)
    host.register(Contract, resource, name='outcomes')
    if transport == 'local':
        proxy = host.connect(Contract, name='outcomes')
        assert proxy.client.observed_path == 'thread_local'
        return proxy
    address = relay_url if transport == 'relay' else host.server_address()
    proxy = caller.connect(Contract, name='outcomes', address=address)
    assert proxy.client.observed_path == ('explicit_relay' if transport == 'relay' else 'direct_ipc')
    return proxy


@pytest.mark.parametrize('transport', ['local', 'ipc', 'relay'])
def test_submit_recover_query_and_conflicts(runtimes, transport, start_c3_relay, tmp_path):
    resource = Resource()
    with connect(runtimes, transport, start_c3_relay, tmp_path, resource) as proxy:
        with cc.hold(cc.with_call_id(proxy.step, 'key'))(value=4, copies=2) as held:
            assert held.value == [4, 4]
            assert held.value is not resource.last_result
            held.value.append('local mutation')
        with cc.with_call_id(cc.hold(proxy.step), 'key')(4, 2) as repeated:
            assert repeated.value == [4, 4]
        with cc.hold(cc.recover_call(proxy.step, 'key'))() as recovered:
            assert recovered.value == [4, 4]
        with cc.recover_call(cc.hold(proxy.step), 'key')() as recovered:
            assert recovered.value == [4, 4]
        assert cc.recover_call(proxy.step, 'key')() == [4, 4]
        assert proxy.query_call() == 'contract method'
        assert cc.with_call_id(proxy.query_call, 'query-method')() == 'contract method'
        assert cc.query_call(proxy, 'query-method').method_name == 'query_call'

        outcome = cc.query_call(proxy, 'key')
        assert outcome == cc.CallOutcome('succeeded', 'step', True, None, None, False)
        with pytest.raises(FrozenInstanceError):
            outcome.status = 'failed'
        for conflict in (
            lambda: cc.with_call_id(proxy.step, 'key')(5, 2),
            cc.with_call_id(proxy.query_call, 'key'),
            cc.recover_call(proxy.query_call, 'key'),
        ):
            with pytest.raises(CallIdConflict) as error:
                conflict()
            assert error.value.code == ErrorCode.CallIdConflict
            assert error.value.transport_phase == 'pre_dispatch'
            assert error.value.call_id is None
        assert cc.query_call(proxy, 'missing') == cc.CallOutcome('unknown', None, False, None, None, False)
        with pytest.raises(CallOutcomeUnknown) as missing:
            cc.recover_call(proxy.step, 'missing')()
        assert missing.value.code == ErrorCode.CallOutcomeUnknown
        assert missing.value.transport_phase == 'pre_dispatch'
        assert missing.value.call_id is None
        assert resource.calls == 1


@pytest.mark.parametrize('transport', ['local', 'ipc', 'relay'])
def test_query_while_business_slots_are_full(
    runtimes, transport, start_c3_relay, tmp_path, monkeypatch,
):
    monkeypatch.setenv('C2_CALL_MAX_OUTSTANDING', '1')
    host, caller = runtimes
    host.set_server(max_outstanding_calls=1)
    for runtime in runtimes:
        runtime.set_client(max_outstanding_calls=1)
    entered = Event()
    release = Event()
    with connect(runtimes, transport, start_c3_relay, tmp_path, Resource(entered, release)) as proxy:
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(cc.with_call_id(proxy.step, 'busy'), 8)
            try:
                assert entered.wait(5)
                owner = host if transport == 'local' else caller
                assert owner.call_execution_snapshot()['client']['used_operations'] == 1
                assert host.call_execution_snapshot()['server']['used_operations'] == 1
                assert cc.query_call(proxy, 'busy', timeout=2) == cc.CallOutcome(
                    'in_flight', 'step', False, None, None, False,
                )
                assert not future.done()
            finally:
                release.set()
            assert future.result(timeout=5) == [8]
        assert cc.query_call(proxy, 'busy').status == 'succeeded'


def test_keyed_deadline_retains_read_only_call_id(runtimes, start_c3_relay, tmp_path):
    entered = Event()
    release = Event()
    with connect(runtimes, 'local', start_c3_relay, tmp_path, Resource(entered, release)) as proxy:
        with cc.with_call_options(proxy, timeout=1) as timed:
            with ThreadPoolExecutor(max_workers=1) as pool:
                future = pool.submit(cc.with_call_id(timed.step, 'uncertain'), 9)
                try:
                    assert entered.wait(5)
                    with pytest.raises(CallDeadlineExceeded) as failure:
                        future.result(timeout=5)
                    assert failure.value.transport_phase == 'dispatch_uncertain'
                    assert failure.value.call_id == 'uncertain'
                    assert failure.value.__cause__.call_id == 'uncertain'
                    with pytest.raises(AttributeError):
                        failure.value.call_id = 'other'
                    assert cc.query_call(proxy, 'uncertain').status == 'in_flight'
                finally:
                    release.set()
        assert cc.recover_call(proxy.step, 'uncertain')() == [9]


def test_native_owned_submit_and_recover(runtimes, start_c3_relay, tmp_path):
    resource = Resource()
    with connect(runtimes, 'ipc', start_c3_relay, tmp_path, resource) as proxy:
        client = proxy.client._client
        prepared = client.begin_call('step', call_id='owned')
        assert pickle.loads(prepared.call_owned(pickle.dumps((3, 2)))) == [3, 3]
        assert pickle.loads(client.recover_call_owned('step', 'owned')) == [3, 3]
        assert resource.calls == 1


@pytest.mark.parametrize(('transport', 'budget', 'reason'), [
    ('ipc', 0, 'not_retained'),
    ('ipc', 600, 'evicted'),
    ('relay', 0, 'not_retained'),
])
def test_result_retention_and_metadata_limits(
    runtimes, start_c3_relay, tmp_path, transport, budget, reason,
):
    host, _ = runtimes
    host.set_server(outcome_max_entries=2, outcome_max_retained_bytes=budget)
    resource = Resource()
    with connect(runtimes, transport, start_c3_relay, tmp_path, resource) as proxy:
        host.set_server(outcome_max_entries=2, outcome_max_retained_bytes=budget)
        for changes in ({'outcome_max_entries': 3}, {'outcome_max_retained_bytes': budget + 1}):
            with pytest.raises(CoreError) as frozen:
                host.set_server(**changes)
            assert frozen.value.lifecycle_kind == 'config_frozen'
        payload = b'd' * 400
        assert cc.with_call_id(proxy.step, 'first')(payload) == [payload]
        assert cc.query_call(proxy, 'first').result_available is (budget != 0)
        assert cc.with_call_id(proxy.step, 'second')(payload) == [payload]
        assert cc.query_call(proxy, 'first') == cc.CallOutcome(
            'succeeded', 'step', False, reason, None, False,
        )
        with pytest.raises(CallResultUnavailable) as unavailable:
            cc.recover_call(proxy.step, 'first')()
        assert unavailable.value.code == ErrorCode.CallResultUnavailable
        assert unavailable.value.transport_phase is None
        assert unavailable.value.call_id is None
        assert unavailable.value.details['outcome'] == 'succeeded'
        assert unavailable.value.details['reason'] == reason
        cc.with_call_id(proxy.step, 'third')(payload)
        assert cc.query_call(proxy, 'first').status == 'unknown'
        assert resource.calls == 3


def test_outcome_configuration_can_clear_overrides(tmp_path, monkeypatch):
    from c_two.endpoint import set_local_endpoint
    from c_two.transport.registry import _reset_default_runtime

    monkeypatch.setenv('C2_OUTCOME_MAX_ENTRIES', '3')
    monkeypatch.setenv('C2_OUTCOME_MAX_RETAINED_BYTES', '4096')
    _reset_default_runtime()
    try:
        if os.name != 'nt':
            set_local_endpoint(root=str(tmp_path))
        cc.set_server(outcome_max_entries=1, outcome_max_retained_bytes=0)
        cc.set_server(outcome_max_entries=None, outcome_max_retained_bytes=None)
        cc.register(Contract, Resource(), name='outcomes')
        with cc.connect(Contract, name='outcomes') as proxy:
            for call_id in ('first', 'second'):
                cc.with_call_id(proxy.step, call_id)(call_id)
            assert cc.recover_call(proxy.step, 'first')() == ['first']
    finally:
        _reset_default_runtime()


def test_downstream_result_unavailable_cannot_claim_outer_success(runtimes):
    host, _ = runtimes
    host.set_server(outcome_max_retained_bytes=0)
    resource = Resource()
    host.register(Contract, resource, name='inner')
    with host.connect(Contract, name='inner') as inner:
        cc.with_call_id(inner.step, 'inner')(4)

        class Outer(Resource):
            def step(self, value, copies):
                return cc.recover_call(inner.step, 'inner')()

        host.register(Contract, Outer(), name='outer')
        with host.connect(Contract, name='outer') as outer:
            for invoke in (lambda: outer.step(2), lambda: cc.with_call_id(outer.step, 'outer')(2)):
                with pytest.raises(ResourceExecutionError) as failure:
                    invoke()
                assert failure.value.details['cause_code'] == '722'
                assert failure.value.transport_phase is None
            assert cc.query_call(outer, 'outer') == cc.CallOutcome(
                'failed', 'step', False, None, ErrorCode.ResourceFunctionExecuting, False,
            )
            with pytest.raises(ResourceExecutionError) as replay:
                cc.recover_call(outer.step, 'outer')()
            assert replay.value.code == ErrorCode.ResourceFunctionExecuting
            assert replay.value.transport_phase is None
            assert resource.calls == 1


def test_recovered_fastdb_holds_keep_distinct_decoded_owners(runtimes, monkeypatch):
    from fastdb4py.payload import PayloadError
    from c_two.crm.payload_plan import _DeferredFastDbPlan
    from .test_portable_payload_runtime import BlobExchange, BlobExchangeResource, blob_bytes, blob_plan

    data = b'x' * (64 * 1024)
    host, _ = runtimes
    writes = []
    write_slab = _DeferredFastDbPlan._c2_write_slab

    def record_write(plan, target):
        writes.append(plan.label)
        return write_slab(plan, target)

    monkeypatch.setattr(_DeferredFastDbPlan, '_c2_write_slab', record_write)
    host.register(BlobExchange, BlobExchangeResource(data), name='blob')
    with host.connect(BlobExchange, name='blob') as proxy:
        with blob_plan(data) as plan, cc.with_call_id(proxy.echo, 'echo')(plan) as echoed:
            assert blob_bytes(echoed) == data
        with cc.hold(cc.with_call_id(proxy.build, 'build'))(False) as held:
            assert blob_bytes(held.value) == data
        assert writes == ['echo.input', 'build.output']
        recover = cc.hold(cc.recover_call(proxy.build, 'build'))
        with recover() as first, recover() as second:
            first_owner = first.value
            assert first_owner is not second.value
            assert host.memory_stats()['holds']['active_holds'] == 2
            first.release()
            with pytest.raises(PayloadError):
                first_owner.entry_view(0)
            assert host.memory_stats()['holds']['active_holds'] == 1
            assert blob_bytes(second.value) == data
        assert host.memory_stats()['holds']['active_holds'] == 0
        assert writes == ['echo.input', 'build.output']
