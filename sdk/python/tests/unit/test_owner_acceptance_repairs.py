"""Real native regressions for OwnerBound SDK acceptance boundaries."""
from __future__ import annotations

import ctypes
import os
import sys
import threading
import time

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.transport.registry import _ProcessRegistry


@cc.crm(namespace='test.owner.acceptance', version='0.1.0')
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
            registry = _ProcessRegistry.get()
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
def test_transport_swap_failure_preserves_native_owner_for_retry(monkeypatch, failure):
    from c_two import _native
    from c_two.transport import registry as registry_module

    cc.shutdown()
    keepalive, receiver = cc.owner_control_pair()
    cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(0), owner_control=receiver)
    registry = _ProcessRegistry.get()
    old = registry._runtime_session
    previous_threshold = settings._shm_threshold
    original_kwargs = registry_module._runtime_session_kwargs_from_settings

    def invalid_kwargs():
        return {**original_kwargs(), 'server_id': 'invalid/server'}

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
        # Inject at the SDK publication boundary while keeping every lifecycle,
        # receiver and shutdown operation in the actual native Runtime.
        old = AdoptionBoundary(old)
        registry._runtime_session = old
        expected_error, error_message = RuntimeError, 'failed adoption'
    try:
        with pytest.raises(expected_error, match=error_message):
            cc.set_transport_policy(shm_threshold=8192)
        assert registry._runtime_session is old
        assert old.owner_control_attached
        assert old.lifecycle_policy['policy'] == 'owner_bound'
        assert settings._shm_threshold == previous_threshold
        monkeypatch.setattr(registry_module, '_runtime_session_kwargs_from_settings', original_kwargs)
        AdoptionBoundary.fail_adoption = False
        cc.set_transport_policy(shm_threshold=8192)
        replacement = registry._runtime_session
        assert replacement is not old
        assert replacement.owner_control_attached
        assert replacement.lifecycle_policy['policy'] == 'owner_bound'
        assert not old.owner_control_attached
        resource = EchoResource()
        cc.register(Echo, resource, name='acceptance-owner')
        keepalive.shutdown()
        deadline = time.monotonic() + 5
        while cc.native_terminal_outcome() is None:
            assert time.monotonic() < deadline
            time.sleep(.01)
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
        keepalive.shutdown()
        assert cc.shutdown(timeout=5)['completed']
        settings.shm_threshold = previous_threshold


def test_owned_child_wait_reaps_and_caches_exit_status():
    keepalive, receiver = cc.owner_control_pair()
    try:
        child = cc.spawn_owned_child(receiver, sys.executable, ['-c', 'raise SystemExit(7)'])
        assert isinstance(child, cc.OwnedChild)
        assert child.id > 0
        assert child.wait(timeout=5) == 7
        assert child.poll() == 7
        assert child.wait(timeout=0) == 7
        if os.name != 'nt':
            with pytest.raises(ChildProcessError):
                os.waitpid(child.id, os.WNOHANG)
        child.close()
        child.close()
        with pytest.raises(RuntimeError, match='closed'):
            child.poll()
    finally:
        keepalive.shutdown()


def test_owned_child_timeout_kill_and_wait_release_the_gil():
    keepalive, receiver = cc.owner_control_pair()
    try:
        child = cc.spawn_owned_child(receiver, sys.executable, ['-c', 'import time; time.sleep(30)'])
        assert child.poll() is None
        with pytest.raises(TimeoutError):
            child.wait(timeout=.02)
        for timeout in [-1, float('inf'), float('nan'), 1e300]:
            with pytest.raises(ValueError):
                child.wait(timeout=timeout)
        def kill_after_wait_begins():
            time.sleep(.05)
            child.kill()

        killer = threading.Thread(target=kill_after_wait_begins)
        killer.start()
        assert child.wait(timeout=5) != 0
        killer.join(5)
        assert not killer.is_alive()
        child.kill()  # Already reaped is harmless.
        child.close()
    finally:
        keepalive.shutdown()


@pytest.mark.parametrize('release', ['close', 'drop'])
def test_owned_child_released_handle_is_reaped_without_blocking(tmp_path, release):
    keepalive, receiver = cc.owner_control_pair()
    marker = tmp_path / 'finished'
    script = 'import time,pathlib; time.sleep(.15); pathlib.Path(%r).write_text("done")' % str(marker)
    child = cc.spawn_owned_child(receiver, sys.executable, ['-c', script])
    pid = child.id
    started = time.monotonic()
    if release == 'close':
        child.close()
    else:
        del child
    assert time.monotonic() - started < .1
    try:
        deadline = time.monotonic() + 5
        while not marker.exists():
            assert time.monotonic() < deadline
            time.sleep(.01)
        if os.name != 'nt':
            # A background native observer retains the OS process until exit.
            # Avoid waitpid until it has had time to reap; this test must never
            # accidentally perform the cleanup it is intended to verify.
            time.sleep(.1)
            with pytest.raises(ChildProcessError):
                os.waitpid(pid, os.WNOHANG)
    finally:
        keepalive.shutdown()


def test_failed_native_lifecycle_handoff_keeps_both_capabilities_usable():
    from c_two._native import RuntimeSession
    first_keepalive, first_receiver = cc.owner_control_pair()
    second_keepalive, second_receiver = cc.owner_control_pair()
    first, second = RuntimeSession(), RuntimeSession()
    try:
        for session, receiver in [(first, first_receiver), (second, second_receiver)]:
            session.set_lifecycle_policy('owner_bound', 0)
            session.attach_owner_control(receiver)
        with pytest.raises(Exception, match='already owns a lifecycle'):
            first.transfer_unstarted_lifecycle_to(second)
        assert first.owner_control_attached
        assert second.owner_control_attached
        first.ensure_host_started()
        second.ensure_host_started()
        with pytest.raises(Exception, match='after Runtime use'):
            first.transfer_unstarted_lifecycle_to(RuntimeSession())
        first_keepalive.shutdown()
        second_keepalive.shutdown()
        assert first.shutdown(route_names=[], timeout_seconds=5)['completed']
        assert second.shutdown(route_names=[], timeout_seconds=5)['completed']
    finally:
        first_keepalive.shutdown()
        second_keepalive.shutdown()
        first.shutdown(route_names=[], timeout_seconds=5)
        second.shutdown(route_names=[], timeout_seconds=5)


def test_shared_reaper_observes_short_child_while_another_child_is_alive():
    first_keepalive, first_receiver = cc.owner_control_pair()
    second_keepalive, second_receiver = cc.owner_control_pair()
    first = cc.spawn_owned_child(first_receiver, sys.executable, ['-c', 'import time; time.sleep(30)'])
    second = cc.spawn_owned_child(second_receiver, sys.executable, ['-c', 'pass'])
    try:
        assert second.wait(timeout=5) == 0
        assert first.poll() is None
    finally:
        first_keepalive.close()
        second_keepalive.close()
        first.kill()
        first.wait(timeout=5)
        second.wait(timeout=5)
        first.close()
        second.close()


def test_owner_bound_restart_refuses_reuse_and_retains_terminal_journal():
    from c_two._native import RuntimeSession
    keepalive, receiver = cc.owner_control_pair()
    session = RuntimeSession()
    try:
        session.set_lifecycle_policy('owner_bound', 0)
        session.attach_owner_control(receiver)
        session.ensure_host_started()
        terminal = session.shutdown(route_names=[], timeout_seconds=5)
        with pytest.raises(RuntimeError, match='already consumed'):
            session.ensure_host_started()
        assert session.native_terminal_outcome() == terminal
        assert not session.host_started
    finally:
        keepalive.close()
        session.shutdown(route_names=[], timeout_seconds=5)


def test_failed_persistent_restart_retains_terminal_journal(monkeypatch):
    from c_two._native import RuntimeSession
    session = RuntimeSession()
    try:
        session.ensure_host_started()
        terminal = session.shutdown(route_names=[], timeout_seconds=5)
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
        session.shutdown(route_names=[], timeout_seconds=5)
