"""Direct IPC control helpers are backed by Rust c2-ipc and do not use relay."""
from __future__ import annotations

import os
import threading
import time
import uuid
from pathlib import Path

import pytest

import c_two as cc
from c_two.transport import Server
from c_two.transport.client.util import ping, shutdown

from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


def _unique_region(prefix: str = 'direct_ctl') -> str:
    return f'{prefix}_{os.getpid()}_{uuid.uuid4().hex[:12]}'


def _wait_for_ping(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(address, timeout=0.2):
            return
        time.sleep(0.05)
    raise TimeoutError(f'{address} did not respond to ping')


def _hello_server(address: str, *, name: str = 'hello') -> Server:
    server = Server(bind_address=address)
    server.register_crm(Hello, HelloImpl(), name=name)
    return server


def _assert_shutdown_initiated(outcome: dict[str, object]) -> None:
    assert outcome == {
        'acknowledged': True,
        'shutdown_started': True,
        'server_stopped': False,
        'route_outcomes': [],
    }


def _assert_shutdown_accepted_not_finished(outcome: dict[str, object]) -> None:
    _assert_shutdown_initiated(outcome)


@pytest.fixture
def direct_server(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region()}'
    server = _hello_server(address)
    server.start()
    _wait_for_ping(address)
    yield address, server
    server.shutdown()


def test_ping_returns_true_against_direct_ipc_without_relay(direct_server):
    address, _server = direct_server
    assert ping(address, timeout=0.5) is True


def test_ping_ignores_bad_relay_env(monkeypatch, direct_server):
    address, _server = direct_server
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
    assert ping(address, timeout=0.5) is True


def test_explicit_ipc_connect_ignores_bad_relay_anchor(monkeypatch):
    import c_two as cc

    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)

    @cc.crm(namespace='test.identity.direct', version='0.1.0')
    class Direct:
        def ping(self) -> str:
            ...

    class DirectResource:
        def ping(self) -> str:
            return 'direct-ipc'

    cc.shutdown()
    try:
        cc.register(Direct, DirectResource(), name='direct-identity')
        address = cc.server_address()
        assert address is not None
        monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
        client = cc.connect(Direct, name='direct-identity', address=address)
        try:
            assert client.ping() == 'direct-ipc'
        finally:
            cc.close(client)
    finally:
        cc.shutdown()


def test_shutdown_stops_direct_ipc_without_relay(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("shutdown")}'
    server = _hello_server(address)
    server.start()
    _wait_for_ping(address)

    _assert_shutdown_initiated(shutdown(address, timeout=0.5))

    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if not ping(address, timeout=0.2):
            break
        time.sleep(0.05)
    else:
        pytest.fail('server still responds to ping after shutdown signal')

    server.shutdown()


def test_shutdown_returns_structured_outcome(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("shutdown_outcome")}'
    server = _hello_server(address)
    server.start()
    _wait_for_ping(address)

    outcome = shutdown(address, timeout=0.5)

    _assert_shutdown_initiated(outcome)

    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if not ping(address, timeout=0.2):
            break
        time.sleep(0.05)
    else:
        pytest.fail('server still responds to ping after shutdown signal')

    server.shutdown()


def test_control_helpers_reject_invalid_addresses():
    assert ping('tcp://not-ipc', timeout=0.01) is False
    assert shutdown('tcp://not-ipc', timeout=0.01) == {
        'acknowledged': False,
        'shutdown_started': False,
        'server_stopped': False,
        'route_outcomes': [],
    }


def test_server_start_returns_only_after_direct_ipc_ready(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("ready")}'
    server = _hello_server(address)
    try:
        server.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
    finally:
        server.shutdown()


def test_server_start_readiness_ignores_bad_relay_env(monkeypatch):
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
    address = f'ipc://{_unique_region("ready_bad_relay")}'
    server = _hello_server(address)
    try:
        server.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
    finally:
        server.shutdown()


def test_standalone_server_rejects_explicit_relay_without_runtime_session(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("standalone_relay")}'
    server = Server(bind_address=address)
    try:
        with pytest.raises(ValueError, match='runtime_session-owned'):
            server.register_crm(
                Hello,
                HelloImpl(),
                name='hello',
                relay_anchor_address='http://127.0.0.1:9',
            )
        assert server.names == []

        server.register_crm(Hello, HelloImpl(), name='hello')
        with pytest.raises(ValueError, match='runtime_session-owned'):
            server.unregister_crm(
                'hello',
                relay_anchor_address='http://127.0.0.1:9',
            )
        assert server.names == ['hello']

        with pytest.raises(ValueError, match='runtime_session-owned'):
            server.shutdown(relay_anchor_address='http://127.0.0.1:9')
        assert server.names == ['hello']
    finally:
        server.shutdown()


def test_starting_second_server_does_not_unlink_active_socket(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("active")}'
    first = _hello_server(address)
    second = _hello_server(address, name='hello2')
    try:
        first.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
        with pytest.raises(
            RuntimeError,
            match='active listener|address already in use|failed to start',
        ):
            second.start(timeout=1.0)
        assert ping(address, timeout=0.5) is True
    finally:
        try:
            second.shutdown()
        except Exception:
            pass
        first.shutdown()


def test_bridge_shutdown_after_direct_ipc_shutdown_allows_new_server(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("external_shutdown")}'
    server = _hello_server(address)
    try:
        server.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
        _assert_shutdown_initiated(shutdown(address, timeout=0.5))

        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not ping(address, timeout=0.2):
                break
            time.sleep(0.05)
        else:
            pytest.fail('server still responds to ping after IPC shutdown')

        server.shutdown()

        replacement = _hello_server(address)
        try:
            replacement.start(timeout=5.0)
            assert ping(address, timeout=0.5) is True
        finally:
            replacement.shutdown()
    finally:
        server.shutdown()


def test_bridge_shutdown_after_direct_ipc_shutdown_still_invokes_on_shutdown(
    monkeypatch,
    tmp_path: Path,
):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)

    marker = tmp_path / 'cleanup.txt'

    @cc.crm(namespace='test.direct_ipc_shutdown.cleanup', version='0.1.0')
    class DirectCleanup:
        def ping(self) -> str:
            ...

        @cc.on_shutdown
        def cleanup(self) -> None:
            ...

    class DirectCleanupImpl:
        def __init__(self, marker_path: Path) -> None:
            self._marker_path = marker_path
            self.cleanup_calls = 0

        def ping(self) -> str:
            return 'ok'

        def cleanup(self) -> None:
            self.cleanup_calls += 1
            self._marker_path.write_text('done', encoding='utf-8')

    address = f'ipc://{_unique_region("external_shutdown_cleanup")}'
    server = Server(bind_address=address)
    impl = DirectCleanupImpl(marker)
    try:
        server.register_crm(DirectCleanup, impl, name='cleanup')
        server.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
        _assert_shutdown_initiated(shutdown(address, timeout=0.5))

        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not ping(address, timeout=0.2):
                break
            time.sleep(0.05)
        else:
            pytest.fail('server still responds to ping after IPC shutdown')

        server.shutdown()
        assert marker.read_text(encoding='utf-8') == 'done'
        assert impl.cleanup_calls == 1
    finally:
        server.shutdown()
        assert impl.cleanup_calls == 1


def test_bridge_shutdown_after_direct_ipc_shutdown_waits_for_active_call_before_hook(
    monkeypatch,
    tmp_path: Path,
):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)

    marker = tmp_path / 'cleanup-active.txt'

    @cc.crm(namespace='test.direct_ipc_shutdown.active_cleanup', version='0.1.0')
    class DirectActiveCleanup:
        def block(self) -> str:
            ...

        @cc.on_shutdown
        def cleanup(self) -> None:
            ...

    class DirectActiveCleanupImpl:
        def __init__(self, marker_path: Path) -> None:
            self._marker_path = marker_path
            self.cleanup_calls = 0
            self.started = threading.Event()
            self.release = threading.Event()

        def block(self) -> str:
            self.started.set()
            if not self.release.wait(timeout=5.0):
                raise TimeoutError('release not signalled')
            return 'done'

        def cleanup(self) -> None:
            self.cleanup_calls += 1
            self._marker_path.write_text('done', encoding='utf-8')

    address = f'ipc://{_unique_region("external_shutdown_active_cleanup")}'
    server = Server(bind_address=address)
    impl = DirectActiveCleanupImpl(marker)
    client = None
    call_thread = None
    shutdown_thread = None
    shutdown_result: dict[str, object] = {}
    try:
        server.register_crm(DirectActiveCleanup, impl, name='cleanup')
        server.start(timeout=5.0)
        _wait_for_ping(address)
        client = cc.connect(DirectActiveCleanup, name='cleanup', address=address)
        assert client.client._mode == 'ipc'  # noqa: SLF001

        def invoke_block() -> None:
            shutdown_result['call_value'] = client.block()

        call_thread = threading.Thread(target=invoke_block)
        call_thread.start()
        assert impl.started.wait(timeout=5.0), 'active call did not start'
        assert server.get_slot_info('cleanup').snapshot().active_workers == 1

        _assert_shutdown_accepted_not_finished(shutdown(address, timeout=0.5))
        assert call_thread.is_alive(), 'active call ended before release after shutdown initiate'
        _assert_shutdown_accepted_not_finished(shutdown(address, timeout=0.5))

        def invoke_shutdown() -> None:
            shutdown_result['bridge_outcome'] = server.shutdown()

        shutdown_thread = threading.Thread(target=invoke_shutdown)
        shutdown_thread.start()
        time.sleep(0.2)

        assert shutdown_thread.is_alive(), 'bridge shutdown returned before active call drained'
        assert not marker.exists(), '@on_shutdown ran before active call finished'
        assert impl.cleanup_calls == 0

        impl.release.set()
        call_thread.join(timeout=5.0)
        assert not call_thread.is_alive(), 'active call thread did not finish'
        shutdown_thread.join(timeout=5.0)
        assert not shutdown_thread.is_alive(), 'bridge shutdown did not finish after release'

        assert shutdown_result['call_value'] == 'done'
        assert marker.read_text(encoding='utf-8') == 'done'
        assert impl.cleanup_calls == 1
    finally:
        impl.release.set()
        if call_thread is not None:
            call_thread.join(timeout=1.0)
        if shutdown_thread is not None:
            shutdown_thread.join(timeout=1.0)
        if client is not None:
            cc.close(client)
        server.shutdown()
        assert impl.cleanup_calls == 1


def test_same_bridge_can_start_after_normal_shutdown(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("restart_same_bridge")}'
    server = _hello_server(address)
    try:
        for _ in range(50):
            server.start(timeout=5.0)
            assert ping(address, timeout=0.5) is True
            server.shutdown()
            assert server.is_started() is False
            assert ping(address, timeout=0.5) is False
    finally:
        server.shutdown()


def test_failed_start_can_retry_after_active_socket_released(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    address = f'ipc://{_unique_region("retry_failed_start")}'
    first = _hello_server(address)
    second = _hello_server(address, name='hello2')
    try:
        first.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True

        with pytest.raises(
            RuntimeError,
            match='active listener|address already in use|failed to start',
        ):
            second.start(timeout=1.0)

        first.shutdown()
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not ping(address, timeout=0.2):
                break
            time.sleep(0.05)
        else:
            pytest.fail('first server still responds after shutdown')

        second.start(timeout=5.0)
        assert ping(address, timeout=0.5) is True
    finally:
        try:
            second.shutdown()
        finally:
            first.shutdown()
