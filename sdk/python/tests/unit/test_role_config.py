"""Python projections of Core's role configuration and retirement contract."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import os
import threading
from typing import Iterator

import pytest

import c_two as cc
from c_two.observe import (
    CallExecutionSnapshot,
    HoldStats,
    MemoryStats,
    RetiredRoleMemoryStats,
    RoleCallExecutionStats,
    RoleMemoryStats,
)


@cc.crm(namespace='test.role-config', version='1.0.0')
class Echo:
    @cc.read
    def echo(self, value: bytes) -> bytes: ...

    @cc.stream
    def stream(self) -> Iterator[int]: ...


class EchoResource:
    def echo(self, value: bytes) -> bytes:
        return value

    def stream(self):
        yield 1
        yield 2


@pytest.fixture
def runtimes(tmp_path, monkeypatch):
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')

    with ExitStack() as stack:
        def create():
            runtime = cc.Runtime()
            if os.name != 'nt':
                runtime.set_local_endpoint(root=str(tmp_path))
            stack.callback(runtime.shutdown)
            return runtime

        yield create


def test_read_only_observation_does_not_resolve_environment(runtimes, monkeypatch):
    runtime = runtimes()
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', 'invalid')
    stats = runtime.memory_stats()
    slots = runtime.call_execution_snapshot()
    assert set(stats) == MemoryStats.__required_keys__
    assert set(slots) == CallExecutionSnapshot.__required_keys__
    assert set(stats['holds']) == HoldStats.__required_keys__
    for role in ('client', 'server'):
        assert set(stats[role]) == RoleMemoryStats.__required_keys__
        assert stats[role]['domain_id'] is stats[role]['memory'] is stats[role]['file_spill'] is None
        assert not stats[role]['frozen']
        assert set(slots[role]) == RoleCallExecutionStats.__required_keys__
        assert slots[role]['max_operations'] is slots[role]['used_operations'] is None
    assert stats['retired'] == slots['retired'] == []


@pytest.mark.parametrize('client_first', [False, True])
def test_independent_freeze(runtimes, client_first):
    runtime, target = runtimes(), runtimes()
    target.register(Echo, EchoResource(), name='target')
    if client_first:
        with runtime.connect(Echo, name='target', address=target.server_address()) as proxy:
            assert proxy.echo(b'client') == b'client'
        assert runtime.memory_stats()['client']['frozen']
        assert not runtime.memory_stats()['server']['frozen']
        runtime.set_server(memory_budget_bytes=1024, max_execution_workers=1)
        runtime.register(Echo, EchoResource(), name='local')
    else:
        runtime.set_server(memory_budget_bytes=1024, max_execution_workers=1)
        runtime.register(Echo, EchoResource(), name='local')
        assert runtime.memory_stats()['server']['frozen']
        assert not runtime.memory_stats()['client']['frozen']
        runtime.set_client(memory_budget_bytes=2048)
        with runtime.connect(Echo, name='target', address=target.server_address()) as proxy:
            assert proxy.echo(b'client') == b'client'
    assert runtime.memory_stats()['server']['memory']['limit_bytes'] == 1024
    runtime.set_server()
    runtime.set_server(server_id=runtime.server_id(), memory_budget_bytes=1024)
    runtime.set_client()
    with pytest.raises(cc.error.CoreError, match='frozen') as error:
        runtime.set_server(memory_budget_bytes=1025)
    assert error.value.lifecycle_kind == 'config_frozen'
    with pytest.raises(cc.error.CoreError, match='frozen') as error:
        runtime.set_client(memory_budget_bytes=2049)
    assert error.value.lifecycle_kind == 'config_frozen'


@pytest.mark.parametrize('role', ['client', 'server'])
def test_incremental_patches_are_atomic(runtimes, role, monkeypatch):
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', '1024')
    monkeypatch.setenv('C2_FILE_SPILL_BUDGET_BYTES', '2048')
    runtime = runtimes()
    setter = getattr(runtime, f'set_{role}')
    setter(memory_budget_bytes=4, max_outstanding_calls=7, file_spill_budget_bytes=8)
    setter(memory_budget_bytes=None)
    for invalid, kind in ((True, TypeError), (-1, OverflowError), (2**64, OverflowError)):
        with pytest.raises(kind):
            setter(memory_budget_bytes=300, file_spill_budget_bytes=invalid)
    with pytest.raises(ValueError, match='must be zero'):
        setter(memory_budget_bytes=0, file_spill_budget_bytes=2048)
    if role == 'server':
        runtime.register(Echo, EchoResource(), name='bound')
    else:
        host = runtimes()
        host.register(Echo, EchoResource(), name='bound')
        with runtime.connect(Echo, name='bound', address=host.server_address()) as proxy:
            assert proxy.echo(b'bytes') == b'bytes'
    assert runtime.memory_stats()[role]['memory']['limit_bytes'] == 1024
    assert runtime.memory_stats()[role]['file_spill']['limit_bytes'] == 8
    assert runtime.call_execution_snapshot()[role]['max_operations'] == 7
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', 'invalid')
    setter(memory_budget_bytes=1024)
    setter(memory_budget_bytes=None)


def test_completed_retirement_adopts_both_roles_and_owner_backed_stats(runtimes, monkeypatch):
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', '1024')
    runtime = runtimes()
    runtime.set_client(memory_budget_bytes=4096, max_outstanding_calls=3)
    runtime.set_server(file_spill_budget_bytes=8192, max_outstanding_calls=5)
    runtime.register(Echo, EchoResource(), name='before')
    old_server_id = runtime.server_id()
    with runtime.connect(Echo, name='before', address=runtime.server_address()) as proxy:
        prepared = proxy.client.begin_call('echo')
        prepared.charge_input(32)
        hold = runtime._runtime_session.local_copy_tracker().track_retained(
            'before', 'echo', 'client_response', 24,
        )
        try:
            domain_id = runtime.memory_stats()['client']['domain_id']
            assert runtime.shutdown()['completed']
            stats = runtime.memory_stats()
            assert not stats['client']['frozen'] and not stats['server']['frozen']
            assert stats['holds']['total_held_bytes'] == 24
            retired = next(role for role in stats['retired'] if role['domain_id'] == domain_id)
            assert set(retired) == RetiredRoleMemoryStats.__required_keys__
            assert retired['role'] == 'client' and retired['closed']
            assert retired['memory']['used_bytes'] == 32
            assert any(
                role['role'] == 'client' and role['used_operations'] == 1 and role['closed']
                for role in runtime.call_execution_snapshot()['retired']
            )
        finally:
            prepared.close()
            hold.release()
        stats = runtime.memory_stats()
        assert stats['holds']['active_holds'] == 0
        assert all(
            role['memory']['used_bytes'] == 0 for role in stats['retired']
            if role['role'] == 'client'
        )
    monkeypatch.setenv('C2_MEMORY_BUDGET_BYTES', '2048')
    runtime.register(Echo, EchoResource(), name='after')
    assert runtime.server_id() != old_server_id
    with runtime.connect(Echo, name='after', address=runtime.server_address()) as proxy:
        assert proxy.echo(b'after') == b'after'
    stats = runtime.memory_stats()
    assert stats['client']['memory']['limit_bytes'] == 4096
    assert stats['server']['memory']['limit_bytes'] == 2048
    assert stats['server']['file_spill']['limit_bytes'] == 8192
    slots = runtime.call_execution_snapshot()
    assert slots['client']['max_operations'] == 3
    assert slots['server']['max_operations'] == 5


@pytest.mark.parametrize('mode', ['parallel', 'read_parallel'])
def test_local_stream_owns_the_host_slot(runtimes, mode):
    runtime = runtimes()
    runtime.set_server(max_outstanding_calls=1)
    runtime.register(Echo, EchoResource(), name='echo', concurrency=cc.ConcurrencyConfig(mode=mode))
    with runtime.connect(Echo, name='echo') as proxy:
        assert proxy.client._scheduler.is_unconstrained == (mode == 'parallel')
        stream = proxy.stream()
        try:
            assert next(stream) == 1
            assert runtime.call_execution_snapshot()['server']['used_operations'] == 1
            with pytest.raises(cc.error.CallCapacityExceeded) as error:
                proxy.echo(b'blocked')
            assert error.value.code == 717
            assert error.value.details['stage'] == 'call_execution_admission'
            assert error.value.details['dispatch_phase'] == 'pre_dispatch'
            assert not runtime.memory_stats()['client']['frozen']
        finally:
            assert stream.close()['completed']
        assert proxy.echo(b'accepted') == b'accepted'


def test_local_unary_holds_its_permit_through_callback_return(runtimes):
    entered = threading.Event()
    release = threading.Event()

    class Blocked(EchoResource):
        def echo(self, value):
            entered.set()
            assert release.wait(5)
            return value

    runtime = runtimes()
    runtime.set_server(max_outstanding_calls=1)
    runtime.register(Echo, Blocked(), name='echo', concurrency=cc.ConcurrencyConfig(mode='read_parallel'))
    with runtime.connect(Echo, name='echo') as proxy, ThreadPoolExecutor(1) as workers:
        future = workers.submit(proxy.echo, b'active')
        try:
            assert entered.wait(5)
            assert runtime.call_execution_snapshot()['server']['used_operations'] == 1
            with pytest.raises(cc.error.CallCapacityExceeded) as error:
                proxy.stream()
            assert error.value.details['dispatch_phase'] == 'pre_dispatch'
            assert not runtime.memory_stats()['client']['frozen']
        finally:
            release.set()
        assert future.result(5) == b'active'
        assert runtime.call_execution_snapshot()['server']['used_operations'] == 0


def test_zero_server_memory_refuses_local_unary_and_stream(runtimes):
    class NeverEntered:
        def echo(self, value):
            pytest.fail('entered refused unary call')

        def stream(self):
            pytest.fail('entered refused stream')

    runtime = runtimes()
    runtime.set_server(memory_budget_bytes=0)
    runtime.register(Echo, NeverEntered(), name='closed')
    with runtime.connect(Echo, name='closed') as proxy:
        for invoke in (lambda: proxy.echo(b''), proxy.stream):
            with pytest.raises(cc.error.CallCapacityExceeded) as error:
                invoke()
            assert error.value.details['stage'] == 'call_execution_admission'
            assert error.value.details['dispatch_phase'] == 'pre_dispatch'
        assert runtime.call_execution_snapshot()['server']['rejected_reservations'] == 2
        assert not runtime.memory_stats()['client']['frozen']


def test_local_callbacks_strip_downstream_error_phases(runtimes):
    runtime = runtimes()
    phases = {'transport_phase': 'pre_dispatch', 'dispatch_phase': 'pre_dispatch', 'route': 'downstream'}

    class FailsAfterEntry:
        calls = 0

        def echo(self, value):
            self.calls += 1
            raise cc.error.CallCapacityExceeded(details=phases)

        def stream(self):
            yield 1
            self.calls += 1
            raise cc.error.CallCapacityExceeded(details=phases)

    resource = FailsAfterEntry()
    runtime.register(Echo, resource, name='failure')
    with runtime.connect(Echo, name='failure') as proxy:
        with pytest.raises(cc.error.CallCapacityExceeded) as error:
            proxy.echo(b'once')
        assert error.value.details == {'route': 'downstream'}
        stream = proxy.stream()
        try:
            assert next(stream) == 1
            with pytest.raises(cc.error.CallCapacityExceeded) as error:
                next(stream)
            assert error.value.details == {'route': 'downstream'}
        finally:
            assert stream.close()['completed']
    assert resource.calls == 2


def test_capacity_saturated_host_can_call_downstream(runtimes):
    from c_two.crm.payload_plan import _pickle_serialize_values

    value = b'forward'
    upstream, middle, downstream = runtimes(), runtimes(), runtimes()
    memory_limit = len(_pickle_serialize_values(value))
    middle.set_server(memory_budget_bytes=memory_limit, max_execution_workers=1, max_outstanding_calls=1)
    middle.set_client(memory_budget_bytes=2048, max_outstanding_calls=1)
    downstream.register(Echo, EchoResource(), name='downstream')

    class Forward(EchoResource):
        def echo(self, value):
            assert middle.memory_stats()['server']['memory']['used_bytes'] == memory_limit
            assert middle.call_execution_snapshot()['server']['used_operations'] == 1
            assert not middle.memory_stats()['client']['frozen']
            with middle.connect(Echo, name='downstream', address=downstream.server_address(), timeout=2) as proxy:
                return cc.with_call_options(proxy, timeout=2).echo(value)

    middle.register(Echo, Forward(), name='middle')
    with upstream.connect(Echo, name='middle', address=middle.server_address()) as proxy:
        assert cc.with_call_options(proxy, timeout=5).echo(value) == value
    assert middle.memory_stats()['client']['frozen']
    assert middle.call_execution_snapshot()['client']['peak_operations'] == 1
