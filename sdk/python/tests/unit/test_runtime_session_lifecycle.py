"""Focused tests for the Python owner-bound lifecycle facade.

These exercise the SDK-facing boundary only: the typed lifecycle config, the
``RuntimeSession`` lifecycle/owner-control projection, ``cc.set_server``
keyword validation, and the opaque receiver contract. The real
controller/service process behaviour lives in
``tests/integration/test_owner_bound_lifecycle.py``.
"""
from __future__ import annotations

import time

import pytest

import c_two as cc
from c_two.config.lifecycle import (
    LifecycleConfig,
)


class TestLifecycleConfig:
    def test_persistent_is_the_default_and_carries_no_grace(self):
        assert LifecycleConfig().native_args() == ('persistent', None)

    def test_owner_bound_requires_an_explicit_bounded_grace(self):
        assert LifecycleConfig.owner_bound(1.5).native_args() == ('owner_bound', 1.5)

    @pytest.mark.parametrize('grace', [None, -1, float('inf'), float('nan'), 61])
    def test_owner_bound_rejects_missing_negative_and_unbounded_grace(self, grace):
        from c_two._native import RuntimeSession
        session = RuntimeSession()
        with pytest.raises(ValueError):
            session.set_lifecycle_policy(*LifecycleConfig.owner_bound(grace).native_args())
        assert session.lifecycle_policy['policy'] == 'persistent'

    def test_persistent_rejects_a_grace_window(self):
        from c_two._native import RuntimeSession
        with pytest.raises(ValueError):
            RuntimeSession().set_lifecycle_policy(*LifecycleConfig('persistent', 1).native_args())

    def test_unknown_policy_is_rejected(self):
        from c_two._native import RuntimeSession
        with pytest.raises(ValueError):
            RuntimeSession().set_lifecycle_policy(*LifecycleConfig('takeover').native_args())

    def test_normalize_is_data_only(self):
        # Python stores the exact caller value; native parsing alone validates.
        config = LifecycleConfig('takeover', -1)
        assert config.native_args() == ('takeover', -1)


class TestRuntimeSessionLifecycleProjection:
    def test_policy_projection_is_read_only_until_the_host_freezes_it(self) -> None:
        from c_two._native import RuntimeSession

        session = RuntimeSession(server_id='unit-lifecycle-policy')
        assert session.lifecycle_policy == {
            'policy': 'persistent',
            'owner_bound': False,
            'owner_missing_grace_seconds': None,
        }
        session.set_lifecycle_policy('owner_bound', 2.0)
        policy = session.lifecycle_policy
        assert policy['policy'] == 'owner_bound'
        assert policy['owner_missing_grace_seconds'] == 2.0
        # Python never builds a second _started/policy state: the native value
        # is the only source, and a rejected selection leaves it untouched.
        with pytest.raises(ValueError, match='exceeds the maximum'):
            session.set_lifecycle_policy(
                'owner_bound',
                61.0,
            )
        assert session.lifecycle_policy['owner_missing_grace_seconds'] == 2.0
        session.shutdown(route_names=[], timeout_seconds=1.0)

    def test_persistent_policy_rejects_a_grace_and_unknown_policies(self) -> None:
        from c_two._native import RuntimeSession

        session = RuntimeSession(server_id='unit-lifecycle-rejects')
        with pytest.raises(ValueError, match='only valid with the owner_bound'):
            session.set_lifecycle_policy('persistent', 1.0)
        with pytest.raises(ValueError, match='invalid lifecycle policy'):
            session.set_lifecycle_policy('takeover', 1.0)
        with pytest.raises(ValueError, match='explicit owner_missing_grace_seconds'):
            session.set_lifecycle_policy('owner_bound', None)
        session.shutdown(route_names=[], timeout_seconds=1.0)

    def test_host_started_reports_the_real_native_running_state(self) -> None:
        from c_two._native import RuntimeSession

        session = RuntimeSession(server_id='unit-host-started')
        assert session.host_started is False
        assert session.native_lifecycle_snapshot() is None
        session.ensure_host_started()
        assert session.host_started is True
        snapshot = session.native_lifecycle_snapshot()
        assert snapshot['phase'] == 'persistent'
        assert snapshot['terminal'] is False
        assert snapshot['listener_closed'] is False
        # Observing the terminal state never initiates or consumes shutdown.
        assert session.native_terminal_outcome() is None
        session.shutdown(route_names=[], timeout_seconds=5.0)
        # A completed shutdown releases the session's host handle.
        assert session.host_started is False
        assert session.native_lifecycle_snapshot()['terminal'] is True
        assert session.native_terminal_outcome() == session.shutdown(route_names=[], timeout_seconds=0)

    def test_policy_name_alone_never_creates_an_owner_bound_host(self) -> None:
        from c_two._native import RuntimeSession

        session = RuntimeSession(server_id='unit-policy-without-capability')
        session.set_lifecycle_policy('owner_bound', 1.0)
        with pytest.raises(Exception, match='requires an attached native owner control'):
            session.ensure_host_started()
        assert session.owner_control_attached is False
        # The refused attempt left no host and no consumed capability behind.
        assert session.native_lifecycle_snapshot() is None
        session.shutdown(route_names=[], timeout_seconds=1.0)


class TestOwnerControlCapability:
    def test_receiver_is_opaque_one_shot_and_never_renders_an_endpoint(self) -> None:
        keepalive, receiver = cc.owner_control_pair()
        assert receiver.is_available is True
        assert 'opaque owner capability' in repr(receiver)
        assert 'opaque owner capability' in repr(keepalive)
        # No descriptor, handle, endpoint path, or token leaks.
        text = f'{receiver!r} {keepalive!r}'
        for forbidden in ('fd', 'handle', 'pipe', 'tmp', 'token', '/'):
            assert forbidden not in text
        assert keepalive.is_alive is True
        keepalive.shutdown()
        assert keepalive.is_alive is False

    def test_capability_is_consumed_exactly_once(self) -> None:
        from c_two._native import RuntimeSession

        keepalive, receiver = cc.owner_control_pair()
        session = RuntimeSession(server_id='unit-consume-once')
        try:
            session.set_lifecycle_policy('owner_bound', 1.0)
            session.attach_owner_control(receiver)
            assert receiver.is_available is False
            assert session.owner_control_attached is True
            with pytest.raises(Exception, match='already attached'):
                session.attach_owner_control(receiver)
            session.ensure_host_started()
            assert session.host_started is True
            assert session.owner_control_attached is False
            # A late attach is refused before the wrapper can even take the
            # (already consumed) capability.
            with pytest.raises(Exception, match='before the Core host starts'):
                session.attach_owner_control(receiver)
        finally:
            keepalive.shutdown()
            session.shutdown(route_names=[], timeout_seconds=5.0)

    def test_owner_eof_reaches_a_real_terminal_outcome(self) -> None:
        from c_two._native import RuntimeSession

        keepalive, receiver = cc.owner_control_pair()
        session = RuntimeSession(server_id='unit-owner-eof')
        try:
            session.set_lifecycle_policy('owner_bound', 0.1)
            session.attach_owner_control(receiver)
            session.ensure_host_started()
            assert session.native_terminal_outcome() is None
            keepalive.shutdown()
            deadline = time.monotonic() + 10.0
            while session.native_terminal_outcome() is None:
                assert time.monotonic() < deadline, 'owner EOF never became terminal'
                time.sleep(0.02)
            snapshot = session.native_lifecycle_snapshot()
            assert snapshot['phase'] == 'finished'
            assert snapshot['terminal'] is True
            assert snapshot['work_drained'] is True
            assert session.host_started is False
            outcome = dict(session.native_terminal_outcome())
            assert outcome['runtime_barrier_error'] is None
            assert outcome['route_close_error'] is None
        finally:
            session.shutdown(route_names=[], timeout_seconds=5.0)


class TestSetServerKeywords:
    def teardown_method(self) -> None:
        cc.shutdown()

    def test_owner_control_requires_the_owner_bound_policy(self) -> None:
        keepalive, receiver = cc.owner_control_pair()
        try:
            with pytest.raises(ValueError, match='requires the owner_bound'):
                cc.set_server(
                    lifecycle=cc.LifecycleConfig.persistent(),
                    owner_control=receiver,
                )
        finally:
            keepalive.shutdown()
        # The rejected call attached nothing.
        from c_two.transport.registry import _ProcessRegistry

        session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001
        assert session.owner_control_attached is False

    def test_owner_bound_set_server_arms_a_real_running_host(self) -> None:
        from c_two.transport.registry import _ProcessRegistry

        keepalive, receiver = cc.owner_control_pair()
        try:
            cc.set_server(
                server_id='unit-set-server-owner',
                lifecycle=cc.LifecycleConfig.owner_bound(0.2),
                owner_control=receiver,
            )
            session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001
            assert session.lifecycle_policy['policy'] == 'owner_bound'
            assert session.owner_control_attached is True

            @cc.crm(namespace='test.set-server-owner', version='0.1.0')
            class Echo:
                def ping(self) -> str:
                    ...

            class EchoResource:
                def ping(self) -> str:
                    return 'pong'

            cc.register(Echo, EchoResource(), name='owner-bound-set-server')
            server = _ProcessRegistry.get()._server  # noqa: SLF001
            assert server.is_started() is True
            assert session.owner_control_attached is False
            assert server.native_lifecycle_snapshot()['phase'] == 'armed'
            assert server.native_terminal_outcome() is None

            keepalive.shutdown()
            deadline = time.monotonic() + 10.0
            while server.native_terminal_outcome() is None:
                assert time.monotonic() < deadline, 'owner EOF never became terminal'
                time.sleep(0.02)
            assert server.is_started() is False
            assert server.native_lifecycle_snapshot()['phase'] == 'finished'
        finally:
            keepalive.shutdown()
            cc.shutdown()

    def test_ordinary_business_clients_never_end_an_owner_bound_server(self) -> None:
        from c_two.transport.registry import _ProcessRegistry

        keepalive, receiver = cc.owner_control_pair()
        proxy = None
        try:
            cc.set_server(
                server_id='unit-owner-business',
                lifecycle=cc.LifecycleConfig.owner_bound(0.2),
                owner_control=receiver,
            )

            @cc.crm(namespace='test.owner-business', version='0.1.0')
            class Echo:
                def ping(self) -> str:
                    ...

            class EchoResource:
                def ping(self) -> str:
                    return 'pong'

            cc.register(Echo, EchoResource(), name='owner-business')
            address = cc.server_address()
            assert address
            proxy = cc.connect(Echo, name='owner-business', address=address)
            assert proxy.ping() == 'pong'
            cc.close(proxy)
            proxy = None
            # The server is armed and idle; its slot is still registered.
            server = _ProcessRegistry.get()._server  # noqa: SLF001
            assert 'owner-business' in server.names
            assert server.native_lifecycle_snapshot()['phase'] == 'armed'
        finally:
            if proxy is not None:
                cc.close(proxy)
            keepalive.shutdown()
            cc.shutdown()


def test_duplicate_and_configuration_errors_preserve_valid_receiver():
    from c_two._native import RuntimeSession
    first_keepalive, first = cc.owner_control_pair()
    second_keepalive, second = cc.owner_control_pair()
    session = RuntimeSession()
    try:
        session.set_lifecycle_policy('owner_bound', 0)
        session.attach_owner_control(first)
        with pytest.raises(Exception, match='already attached'):
            session.attach_owner_control(second)
        assert second.is_available
        with pytest.raises(ValueError):
            cc.set_server(lifecycle=LifecycleConfig('owner_bound', -1), owner_control=second)
        assert second.is_available
        with pytest.raises(ValueError):
            cc.set_server(ipc_overrides={'wrong_key': 1}, owner_control=second)
        assert second.is_available
        session.ensure_host_started()
        with pytest.raises(Exception, match='before the Core host starts'):
            session.attach_owner_control(second)
        assert second.is_available
    finally:
        first_keepalive.shutdown()
        second_keepalive.shutdown()
        session.shutdown(route_names=[], timeout_seconds=5)
        cc.shutdown()


def test_preclosed_owner_cannot_publish_readiness():
    from c_two._native import RuntimeSession
    keepalive, receiver = cc.owner_control_pair()
    session = RuntimeSession()
    session.set_lifecycle_policy('owner_bound', 0)
    session.attach_owner_control(receiver)
    keepalive.shutdown()
    with pytest.raises(Exception):
        session.ensure_host_started()
    assert not session.host_started
    assert session.native_lifecycle_snapshot() is None
    session.shutdown(route_names=[], timeout_seconds=5)


def test_failed_start_late_attach_preserves_valid_receiver():
    from c_two._native import RuntimeSession
    keepalive, receiver = cc.owner_control_pair()
    session = RuntimeSession()
    session.set_lifecycle_policy('owner_bound', 0)
    session.attach_owner_control(receiver)
    keepalive.shutdown()
    with pytest.raises(Exception):
        session.ensure_host_started()
    other_keepalive, other_receiver = cc.owner_control_pair()
    try:
        with pytest.raises(Exception, match='before the Core host starts'):
            session.attach_owner_control(other_receiver)
        assert other_receiver.is_available
    finally:
        other_keepalive.shutdown()
        session.shutdown(route_names=[], timeout_seconds=5)


def test_regular_business_stdin_cannot_be_adopted():
    import os
    import subprocess
    import sys
    script = "import c_two as cc; cc.adopt_owner_stdin(); print('UNEXPECTED_READY')"
    result = subprocess.run([sys.executable, '-c', script], stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=10)
    assert result.returncode != 0
    assert 'UNEXPECTED_READY' not in result.stdout


def test_failed_spawn_consumes_the_transferred_receiver():
    keepalive, receiver = cc.owner_control_pair()
    try:
        with pytest.raises(ValueError, match='spawn failed'):
            cc.spawn_owned_child(receiver, '/nonexistent-c2-owner-test-program')
        assert not receiver.is_available
        with pytest.raises(RuntimeError, match='already consumed'):
            cc.spawn_owned_child(receiver, '/nonexistent-c2-owner-test-program')
    finally:
        keepalive.shutdown()
