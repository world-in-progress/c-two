"""Real native regressions for Runtime SDK completion and configuration boundaries."""
from __future__ import annotations

import ctypes
import os
import sys
import threading

import pytest

import c_two as cc

import c_two.observe
from c_two.config.settings import settings
from c_two.transport.registry import _get_default_runtime


@cc.crm(namespace='test.runtime.acceptance', version='0.1.0')
class Echo:
    def ping(self) -> str: ...

    @cc.on_shutdown
    def done(self) -> None: ...


class EchoResource:
    def __init__(self):
        self.hooks = 0

    def ping(self):
        return 'pong'

    def done(self):
        self.hooks += 1


def _hold_gil_for_native_completion():
    # PyDLL retains the GIL on both platforms while Rust's drain can finish.
    if os.name == 'nt':
        sleep = ctypes.PyDLL('kernel32').Sleep
        sleep.argtypes = [ctypes.c_ulong]
        sleep(100)
    else:
        sleep = ctypes.PyDLL(None).usleep
        sleep.argtypes = [ctypes.c_uint]
        sleep(100_000)


def test_shutdown_completion_and_hook_consume_the_same_native_snapshot():
    previous_interval = sys.getswitchinterval()
    sys.setswitchinterval(1)
    try:
        for _ in range(10):
            resource = EchoResource()
            cc.register(Echo, resource, name='acceptance-echo')
            registry = _get_default_runtime()
            session = registry._runtime_session
            entered = threading.Event()
            results = []

            def shutdown():
                entered.set()
                results.append(cc.shutdown(timeout=0))

            worker = threading.Thread(target=shutdown)
            worker.start()
            assert entered.wait(5)
            _hold_gil_for_native_completion()
            worker.join(5)
            assert not worker.is_alive()
            outcome = results[0]
            if outcome['completed']:
                assert outcome['runtime_barrier_error'] is None
                assert outcome['route_close_error'] is None
                assert len(outcome['route_outcomes']) == 1
                assert resource.hooks == 1
            else:
                assert registry._runtime_session is session
                assert resource.hooks == 0
                assert registry.names == ['acceptance-echo']
            final = cc.shutdown(timeout=5)
            assert final['completed']
            assert resource.hooks == 1
    finally:
        sys.setswitchinterval(previous_interval)
        cc.shutdown(timeout=5)


@pytest.mark.parametrize('failure', ['construction', 'adoption'])
def test_shutdown_swap_failure_preserves_native_session_for_retry(monkeypatch, failure):
    from c_two import _native

    runtime = cc.Runtime()
    runtime.set_server(startup_timeout=1.0)
    hold = runtime._runtime_session.local_copy_tracker().track_retained(
        'acceptance-runtime', 'echo', 'client_response', 16,
    )

    class ReplacementBoundary:
        fail_at = None

        def __init__(self, native_session=None):
            if self.fail_at == 'construction':
                raise RuntimeError('injected construction failure')
            self.native_session = native_session or _native.RuntimeSession()

        def __getattr__(self, name):
            return getattr(self.native_session, name)

        def inherit_local_endpoint_selection(self, previous):
            self.native_session.inherit_local_endpoint_selection(previous.native_session)

        def inherit_retirement(self, previous):
            if self.fail_at == 'adoption':
                raise RuntimeError('injected adoption failure')
            self.native_session.inherit_retirement(previous.native_session)

    old = ReplacementBoundary(runtime._runtime_session)
    runtime._runtime_session = old
    monkeypatch.setattr(ReplacementBoundary, 'fail_at', failure)
    try:
        with pytest.raises(RuntimeError, match=f'injected {failure} failure'):
            runtime.shutdown()
        assert runtime._runtime_session is old
        assert runtime.memory_stats()['holds']['total_held_bytes'] == 16
        monkeypatch.setattr(ReplacementBoundary, 'fail_at', None)
        assert runtime.shutdown()['completed']
        assert runtime._runtime_session is not old
        assert runtime._runtime_session.startup_timeout == 1.0
        assert runtime.memory_stats()['holds']['total_held_bytes'] == 16
    finally:
        hold.release()
        monkeypatch.setattr(ReplacementBoundary, 'fail_at', None)
        runtime.shutdown()


def test_failed_persistent_restart_records_its_own_terminal_journal():
    from c_two._native import RuntimeSession

    session = RuntimeSession()
    other = None
    try:
        session.ensure_host_started()
        terminal = session.shutdown(timeout_seconds=5)
        other = RuntimeSession(server_id=session.server_id, use_process_relay_anchor=False)
        other.ensure_host_started()
        with pytest.raises(cc.error.CoreError):
            session.ensure_host_started()
        failed = session.native_terminal_outcome()
        assert terminal['server_was_started'] and terminal['completed']
        assert failed['completed'] and not failed['server_was_started']
        assert failed['runtime_barrier_error']
        assert not session.host_started
        assert other.shutdown(timeout_seconds=5)['completed']
        session.ensure_host_started()
        assert session.host_started
        assert session.native_terminal_outcome() is None
    finally:
        if other is not None:
            other.shutdown(timeout_seconds=5)
        session.shutdown(timeout_seconds=5)
