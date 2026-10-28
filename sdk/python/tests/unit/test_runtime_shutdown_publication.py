"""Real publication/cleanup boundaries for a retiring public Runtime."""
from __future__ import annotations

from contextlib import contextmanager
from threading import Barrier, Event, Lock, Thread, current_thread

import pytest

import c_two as cc
from c_two.crm.contract import CRMContract
from c_two.transport import registry
from c_two.transport.client.proxy import CRMProxy
from .test_runtime import Echo, EchoResource, isolated_settings, runtimes  # noqa: F401


def _share_endpoint(server, client):
    root = server.local_endpoint_context().root
    if root is not None:
        client.set_local_endpoint(root=root)


def _assert_retired(error):
    assert isinstance(error, cc.error.CoreError), repr(error)
    assert error.category == 'lifecycle'
    assert error.lifecycle_kind == 'runtime_retired'
    assert error.io_operation is None and error.raw_os_error is None


@contextmanager
def _paused_connect(client, server, monkeypatch, *, after_native):
    reached, resume = Event(), Event()
    returned, attempts, observations = [], [], []
    original_args = CRMContract.native_args
    original_call = registry._connect_native_call
    original_ipc = CRMProxy.ipc

    def native_args(contract):
        if not after_native and current_thread() is worker:
            reached.set()
            assert resume.wait(5), 'native entry was not released'
        return original_args(contract)

    def ipc(*args, **kwargs):
        proxy = original_ipc(*args, **kwargs)
        if current_thread() is worker:
            observations.append(proxy.connection_observation())
        return proxy

    def native_call(operation, *args, **kwargs):
        if after_native and current_thread() is worker and args == ('python_connect_finish',):
            attempts.append(operation.__self__)
            reached.set()
            assert resume.wait(5), 'final publication was not released'
        return original_call(operation, *args, **kwargs)

    def connect():
        try:
            returned.append(client.connect(
                Echo, name='echo', address=server.server_address(), timeout=3,
            ))
        except BaseException as error:
            returned.append(error)  # Retain traceback/old session until cleanup assertions.

    worker = Thread(target=connect, name='runtime-retirement-connect')
    with monkeypatch.context() as patch:
        patch.setattr(CRMContract, 'native_args', native_args)
        patch.setattr(CRMProxy, 'ipc', ipc)
        patch.setattr(registry, '_connect_native_call', native_call)
        worker.start()
        try:
            assert reached.wait(5), 'the requested race boundary was not reached'
            yield resume, worker, returned, attempts, observations
        finally:
            resume.set()
            worker.join(5)
            assert not worker.is_alive()
            for value in returned:
                if not isinstance(value, BaseException):
                    cc.close(value)


@pytest.mark.parametrize('hosted', [False, True], ids=['hostless', 'hosted'])
def test_captured_session_cannot_acquire_after_public_shutdown(runtimes, monkeypatch, hosted):
    server, client = runtimes
    _share_endpoint(server, client)
    server.register(Echo, EchoResource('echo:'), name='echo')
    if hosted:
        client.start()
    old_session = client._runtime_session
    events = client.connection_events()
    try:
        with _paused_connect(client, server, monkeypatch, after_native=False) as race:
            resume, worker, returned, attempts, observations = race
            outcome = client.shutdown(timeout=3)
            assert outcome['completed'] and outcome['ipc_clients_drained']
            assert client._runtime_session is not old_session
            resume.set()
            worker.join(5)
            assert not worker.is_alive() and len(returned) == 1
            _assert_retired(returned[0])
            assert not attempts and not observations
            # Hosted readiness can emit accepted-side probe events. No outgoing
            # managed physical connection may have been created by this acquire.
            while (event := events.recv(timeout=0)) is not None:
                assert event.side != 'client' or event.kind != 'opened'
    finally:
        events.close()


@pytest.mark.parametrize('hosted', [False, True], ids=['hostless', 'hosted'])
@pytest.mark.parametrize('expired', [False, True], ids=['valid-budget', 'expired-budget'])
def test_admitted_connection_closes_before_publication_returns(runtimes, monkeypatch, hosted, expired):
    server, client = runtimes
    _share_endpoint(server, client)
    server.register(Echo, EchoResource('echo:'), name='echo')
    if hosted:
        client.start()
    old_session = client._runtime_session
    with _paused_connect(client, server, monkeypatch, after_native=True) as race:
        resume, worker, returned, attempts, observations = race
        # This gate is after real native acquisition, so early rejection cannot
        # make the expired-budget regression pass without exercising its path.
        assert len(attempts) == len(observations) == 1
        observation = observations[0]
        assert observation.snapshot() is None
        outcome = client.shutdown(timeout=3)
        assert outcome['completed'] and outcome['ipc_clients_drained']
        assert client._runtime_session is not old_session
        assert observation.snapshot() is not None  # Before peer shutdown/fixture cleanup.
        original_init = cc.error.CoreError.__init__

        def observe_error(error, category, message):
            if current_thread() is worker:
                assert not client._lock.locked(), 'final refusal kept the binding lock'
                client.names
            original_init(error, category, message)

        monkeypatch.setattr(cc.error.CoreError, '__init__', observe_error)
        if expired:
            blocked = Lock()
            blocked.acquire()
            try:
                with pytest.raises(cc.error.CoreError):
                    attempts[0].acquire_lock(blocked, 'fixture_expire_original_budget')
            finally:
                blocked.release()
        resume.set()
        worker.join(5)
        assert not worker.is_alive() and len(returned) == 1
        error = returned[0]
        if expired:
            assert isinstance(error, cc.error.CallDeadlineExceeded), repr(error)
            assert error.details['operation'] == 'connect'
            assert error.details['stage'] == 'python_connect_finish'
        else:
            _assert_retired(error)
        assert observation.snapshot() is not None


def test_shutdown_hook_registers_into_the_replacement_session(runtimes):
    runtime = runtimes[0]
    replacement = EchoResource('new:')

    class ReplacingResource(EchoResource):
        def stop(self):
            super().stop()
            runtime.register(Echo, replacement, name='new')

    resource = ReplacingResource('old:')
    runtime.register(Echo, resource, name='old')
    assert runtime.shutdown()['completed']
    assert resource.stopped == 1 and replacement.stopped == 0
    assert runtime.names == ['new']
    with runtime.connect(Echo, name='new') as proxy:
        assert proxy.echo('alive') == 'new:alive'
    assert runtime.shutdown()['completed']
    assert replacement.stopped == 1


def test_failed_replacement_keeps_slots_and_original_close_journal(runtimes, monkeypatch):
    runtime = runtimes[0]
    resource = EchoResource('old:')
    runtime.register(Echo, resource, name='old')
    old_session = runtime._runtime_session
    original = runtime._swap_runtime_session

    def reject_replacement(**kwargs):
        raise RuntimeError('injected replacement construction failure')

    with monkeypatch.context() as patch:
        patch.setattr(runtime, '_swap_runtime_session', reject_replacement)
        with pytest.raises(RuntimeError, match='injected replacement construction'):
            runtime.shutdown()
    assert runtime._runtime_session is old_session
    assert runtime.names == ['old'] and resource.stopped == 0
    with pytest.raises(cc.error.CoreError) as rejected:
        runtime.start()
    _assert_retired(rejected.value)
    assert runtime._swap_runtime_session == original
    outcome = runtime.shutdown()
    assert outcome['completed'] and outcome['removed_routes'] == ['old']
    assert runtime._runtime_session is not old_session and resource.stopped == 1


def test_pending_retirement_retains_session_until_sdk_consumes_terminal_outcome(runtimes):
    runtime, caller = runtimes
    _share_endpoint(runtime, caller)
    entered, release = Event(), Event()
    returned = []

    class BlockingResource(EchoResource):
        def echo(self, value):
            entered.set()
            assert release.wait(5)
            return value

    resource = BlockingResource('blocked:')
    runtime.register(Echo, resource, name='blocked')
    proxy = caller.connect(Echo, name='blocked', address=runtime.server_address())

    def call():
        try:
            returned.append(proxy.echo('value'))
        except Exception as error:
            returned.append(error)

    worker = Thread(target=call)
    old_session = runtime._runtime_session
    worker.start()
    try:
        assert entered.wait(5)
        pending = runtime.shutdown(timeout=0)
        assert not pending['completed']
        assert runtime._runtime_session is old_session and resource.stopped == 0
        with pytest.raises(cc.error.CoreError) as rejected:
            runtime.register(Echo, EchoResource('late:'), name='late')
        _assert_retired(rejected.value)
        release.set()
        worker.join(5)
        assert not worker.is_alive()
        # Native observation consumes neither the Python slots nor their hooks.
        terminal = old_session.begin_shutdown(timeout_seconds=3).observe()
        assert terminal['completed']
        assert runtime._runtime_session is old_session and resource.stopped == 0
        with pytest.raises(cc.error.CoreError) as rejected:
            runtime.start()
        _assert_retired(rejected.value)
        assert runtime.shutdown()['completed']
        assert runtime._runtime_session is not old_session and resource.stopped == 1
    finally:
        release.set()
        worker.join(5)
        assert not worker.is_alive()
        cc.close(proxy)


@pytest.mark.parametrize('timeout', ['5', -1, float('nan')])
def test_invalid_shutdown_input_does_not_retire_the_session(runtimes, timeout):
    runtime = runtimes[0]
    runtime.start()
    session = runtime._runtime_session
    credential = runtime.server_credential().to_json()
    with pytest.raises((TypeError, ValueError)):
        runtime.shutdown(timeout=timeout)
    assert runtime._runtime_session is session
    session.check_active()
    assert runtime.server_credential().to_json() == credential


@pytest.mark.parametrize('operation', ['start', 'register', 'policy'])
def test_retirement_entry_errors_allow_runtime_observation(runtimes, monkeypatch, operation):
    runtime = runtimes[0]
    session = runtime._runtime_session
    assert session.begin_shutdown().observe()['completed']
    original = cc.error.CoreError.__init__

    def observe_error(error, category, message):
        assert not runtime._lock.locked()
        assert runtime.names == []
        original(error, category, message)

    with monkeypatch.context() as patch:
        patch.setattr(cc.error.CoreError, '__init__', observe_error)
        with pytest.raises(cc.error.CoreError) as caught:
            if operation == 'start':
                runtime.start()
            elif operation == 'register':
                runtime.register(Echo, EchoResource('late:'), name='late')
            else:
                runtime.set_transport_policy(shm_threshold=16384)
        _assert_retired(caught.value)
    assert runtime._runtime_session is session
    assert runtime.shutdown()['completed']


def test_two_completed_shutdown_observers_swap_and_invoke_hooks_once(runtimes, monkeypatch):
    runtime = runtimes[0]
    resource = EchoResource('old:')
    runtime.register(Echo, resource, name='old')
    session = runtime._runtime_session
    observed = Barrier(2)
    results, swaps = [], []
    original_swap = runtime._swap_runtime_session

    def outcome_dict(value):
        # Both callers have observed the real native terminal receipt, before
        # either can enter the existing SDK handoff lock.
        if current_thread() in workers and 'completed' in value:
            observed.wait(timeout=5)
        return dict(value)

    def swap(**kwargs):
        swaps.append(None)
        return original_swap(**kwargs)

    def shutdown():
        try:
            results.append(runtime.shutdown())
        except BaseException as error:
            results.append(error)

    workers = [Thread(target=shutdown) for _ in range(2)]
    with monkeypatch.context() as patch:
        patch.setattr(registry, 'dict', outcome_dict, raising=False)
        patch.setattr(runtime, '_swap_runtime_session', swap)
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(5)
        assert all(not worker.is_alive() for worker in workers)
    assert len(results) == 2 and results[0] == results[1], results
    assert results[0]['completed'] and results[0]['removed_routes'] == ['old']
    assert len(swaps) == 1 and resource.stopped == 1
    assert runtime._runtime_session is not session
