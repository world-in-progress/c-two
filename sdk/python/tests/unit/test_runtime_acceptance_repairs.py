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
def test_transport_swap_failure_preserves_native_session_for_retry(monkeypatch, failure):
    from c_two import _native
    from c_two.transport import registry as registry_module

    cc.shutdown()
    cc.set_server(startup_timeout=1.0)
    registry = _get_default_runtime()
    old = registry._runtime_session
    previous_threshold = settings._shm_threshold
    original_kwargs = registry_module._runtime_session_kwargs_from_settings

    def invalid_kwargs(code_overrides):
        return {**original_kwargs(code_overrides), 'server_id': 'invalid/server'}

    class AdoptionBoundary:
        fail_adoption = True

        def __init__(self, native_session=None, **kwargs):
            self.native_session = native_session or _native.RuntimeSession(**kwargs)

        def __getattr__(self, name):
            return getattr(self.native_session, name)

        def inherit_local_endpoint_selection(self, previous):
            self.native_session.inherit_local_endpoint_selection(previous.native_session)

        def adopt_retired_memory_observation(self, observation):
            if self.fail_adoption:
                raise RuntimeError('injected failed adoption')
            self.native_session.adopt_retired_memory_observation(observation)

        def transfer_unstarted_lifecycle_to(self, replacement):
            self.native_session.transfer_unstarted_lifecycle_to(replacement.native_session)

    if failure == 'construction':
        monkeypatch.setattr(registry_module, '_runtime_session_kwargs_from_settings', invalid_kwargs)
        expected_error, error_message = ValueError, 'server_id'
    else:
        # Inject at the SDK publication boundary while keeping lifecycle and
        # shutdown operations in the actual native Runtime.
        old = AdoptionBoundary(old)
        registry._runtime_session = old
        expected_error, error_message = RuntimeError, 'failed adoption'
    try:
        with pytest.raises(expected_error, match=error_message):
            cc.set_transport_policy(shm_threshold=8192)
        assert registry._runtime_session is old
        assert old.startup_timeout == 1.0
        assert settings._shm_threshold == previous_threshold
        monkeypatch.setattr(registry_module, '_runtime_session_kwargs_from_settings', original_kwargs)
        AdoptionBoundary.fail_adoption = False
        cc.set_transport_policy(shm_threshold=8192)
        replacement = registry._runtime_session
        assert replacement is not old
        assert replacement.startup_timeout == 1.0
        resource = EchoResource()
        cc.register(Echo, resource, name='acceptance-runtime')
        outcome = cc.shutdown(timeout=5)
        assert outcome['completed']
        assert resource.hooks == 1
    finally:
        # Undo failure injection before cleanup, even if an assertion failed.
        # The boundary wrapper must not escape into another test's registry.
        monkeypatch.setattr(registry_module, '_runtime_session_kwargs_from_settings', original_kwargs)
        AdoptionBoundary.fail_adoption = False
        if isinstance(registry._runtime_session, AdoptionBoundary):
            registry._runtime_session = registry._runtime_session.native_session
        assert cc.shutdown(timeout=5)['completed']
        settings.shm_threshold = previous_threshold


def test_failed_persistent_restart_retains_terminal_journal(monkeypatch):
    from c_two._native import RuntimeSession
    session = RuntimeSession()
    try:
        session.ensure_host_started()
        terminal = session.shutdown(timeout_seconds=5)
        monkeypatch.setenv('C2_RELAY_USE_PROXY', 'invalid-boolean')
        monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
        with pytest.raises(Exception):
            session.ensure_host_started()
        assert session.native_terminal_outcome() == terminal
        assert not session.host_started
        monkeypatch.delenv('C2_RELAY_USE_PROXY')
        monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS')
        session.ensure_host_started()
        assert session.host_started
        assert session.native_terminal_outcome() is None
    finally:
        session.shutdown(timeout_seconds=5)
