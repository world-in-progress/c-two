"""Public instances use the same real Core lifecycle as the default API."""
from __future__ import annotations

import os
from pathlib import Path
import signal
import subprocess
import sys
import textwrap
import uuid

import pytest

import c_two as cc
from c_two._native import RuntimeSession
from c_two.config.settings import C2Settings, settings
from c_two.transport import registry


@cc.crm(namespace='test.public-runtime', version='0.1.0')
class Echo:
    def echo(self, value: str) -> str: ...

    @cc.on_shutdown
    def stop(self) -> None: ...


class EchoResource:
    def __init__(self, prefix: str):
        self.prefix = prefix
        self.stopped = 0

    def echo(self, value: str) -> str:
        return self.prefix + value

    def stop(self) -> None:
        self.stopped += 1


@pytest.fixture(autouse=True)
def isolated_settings(monkeypatch):
    registry._reset_default_runtime()
    for key in tuple(os.environ):
        if key.startswith('C2_'):
            monkeypatch.delenv(key)
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_SHM_THRESHOLD', '8192')
    for key in ('_shm_threshold', '_remote_payload_chunk_size', '_relay_anchor_address'):
        monkeypatch.setattr(settings, key, None)
    yield
    registry._reset_default_runtime()


@pytest.fixture
def runtimes(tmp_path):
    pair = (cc.Runtime(), cc.Runtime())
    roots = (tmp_path / 'first', tmp_path / 'second')
    for runtime, root in zip(pair, roots):
        root.mkdir()
        if os.name != 'nt':
            runtime.set_local_endpoint(root=str(root))
    yield pair
    for runtime, root in zip(pair, roots):
        outcome = runtime.shutdown()
        assert outcome['completed']
        assert outcome['runtime_barrier_error'] is None
        assert outcome['route_close_error'] is None
        assert outcome['ipc_client_close_error'] is None
        assert {p.name for p in root.iterdir()} <= {'.gate', '.gate.marker'}


def test_unconfigured_settings_do_not_read_another_objects_override():
    settings.shm_threshold = 16384
    assert C2Settings().shm_threshold == 8192


def test_new_runtime_does_not_copy_another_runtimes_transport_override(runtimes):
    first, second = runtimes
    first.set_transport_policy(shm_threshold=16384)
    second.set_transport_policy(remote_payload_chunk_size=2048)
    assert first._runtime_session.client_ipc_config['shm_threshold'] == 16384
    assert second._runtime_session.client_ipc_config['shm_threshold'] == 8192
    assert settings._shm_threshold is None
    assert registry._default_runtime is None


@pytest.mark.parametrize('global_threshold,session_threshold,valid', [
    (16384, 4096, True),
    (4096, 16384, False),
])
def test_bridge_preflight_uses_its_own_native_transport_policy(global_threshold, session_threshold, valid):
    settings.shm_threshold = global_threshold
    session = RuntimeSession(
        shm_threshold=session_threshold,
        server_ipc_overrides={'max_frame_size': 8192},
        use_process_relay_anchor=False,
    )
    try:
        if valid:
            session.ensure_server_bridge()
        else:
            with pytest.raises(ValueError, match='shm_threshold'):
                session.ensure_server_bridge()
    finally:
        assert not session.host_started
        assert session.shutdown()['completed']


def test_empty_start_uses_native_readiness_and_leaves_directory_owned_by_caller(tmp_path, monkeypatch):
    runtime = cc.Runtime()
    root = tmp_path
    if os.name != 'nt':
        while len(os.fsencode(root)) <= 512:
            root /= '独立 runtime 的长目录'
        root.mkdir(parents=True)
        assert len(os.fsencode(root)) > 512
        runtime.set_local_endpoint(root=str(root))
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:1')
    before_signal = signal.getsignal(signal.SIGINT)
    try:
        assert runtime.server_credential() is None
        runtime.set_server(startup_timeout=5.0)
        runtime.start()
        assert runtime.names == []
        assert runtime.native_lifecycle_snapshot()['listener_closed'] is False
        assert runtime.native_terminal_outcome() is None
        credential = runtime.server_credential()
        assert credential.address == runtime.server_address()
        assert registry._default_runtime is None
        assert signal.getsignal(signal.SIGINT) is before_signal
    finally:
        outcome = runtime.shutdown()
        assert outcome['completed'] and outcome['runtime_barrier_error'] is None
    assert runtime.server_credential() is None
    assert root.is_dir()
    assert {p.name for p in root.iterdir()} <= {'.gate', '.gate.marker'}


def test_two_runtime_names_connections_and_shutdown_are_independent(runtimes):
    first, second = runtimes
    resources = (EchoResource('first:'), EchoResource('second:'))
    for runtime, resource in zip(runtimes, resources):
        runtime.register(Echo, resource, name='same')
    with first.connect(Echo, name='same') as direct:
        assert direct.client.supports_direct_call
        assert direct.echo('local') == 'first:local'
    with second.connect(Echo, name='same', address=second.server_address()) as remote:
        assert remote.client.observed_path == 'direct_ipc'
        observation = cc.connection_observation(remote)
        assert first.shutdown()['completed']
        assert resources[0].stopped == 1
        assert resources[1].stopped == 0
        assert observation.snapshot() is None
        assert remote.echo('alive') == 'second:alive'
        with cc.with_call_options(remote, timeout=1.0) as bounded:
            assert bounded.echo('view') == 'second:view'
    assert registry._default_runtime is None
    assert second.shutdown()['completed']
    assert resources[1].stopped == 1


def test_explicit_instances_communicate_through_real_ipc(runtimes):
    server, client = runtimes
    context = server.local_endpoint_context()
    if context.root is not None:
        client.set_local_endpoint(root=context.root)
    server.register(Echo, EchoResource('remote:'), name='echo')
    events = client.connection_events()
    try:
        with client.connect(Echo, name='echo', address=server.server_address()) as proxy:
            assert proxy.echo('ok') == 'remote:ok'
            observation = cc.connection_observation(proxy)
            opened = events.recv(timeout=1.0)
            assert opened.kind == 'opened' and opened.side == 'client'
            assert opened.connection_id == observation.connection_id
        assert client.shutdown()['completed']
        closed = events.recv(timeout=1.0)
        assert closed.kind == 'closed' and closed.connection_id == opened.connection_id
    finally:
        events.close()
    assert registry._default_runtime is None


def test_start_then_failed_registration_preserves_host(runtimes):
    runtime = runtimes[0]
    runtime.start()
    credential = runtime.server_credential().to_json()
    with pytest.raises((ValueError, cc.error.CoreError)):
        runtime.register(Echo, EchoResource('bad:'), name='')
    assert runtime.server_credential().to_json() == credential
    assert runtime.native_lifecycle_snapshot()['listener_closed'] is False
    runtime.register(Echo, EchoResource('ok:'), name='valid')
    with runtime.connect(Echo, name='valid', address=runtime.server_address()) as proxy:
        assert proxy.echo('after') == 'ok:after'


def test_relay_override_rejection_and_session_replacement_are_atomic(runtimes):
    runtime = runtimes[0]
    original = 'http://127.0.0.1:1'
    runtime.set_relay_anchor(original)
    runtime.start()  # An empty local Host needs no relay registration.
    with pytest.raises(RuntimeError, match='frozen'):
        runtime.set_relay_anchor('http://127.0.0.1:2')
    assert runtime._settings.relay_anchor_address == original
    assert runtime._runtime_session.relay_anchor_address_override == original
    assert runtime.shutdown()['completed']
    assert runtime._runtime_session.relay_anchor_address_override == original
    runtime.start()
    assert runtime._runtime_session.relay_anchor_address_override == original
    assert settings._relay_anchor_address is None


def test_default_api_and_explicit_runtime_keep_separate_owners(runtimes):
    runtime = runtimes[0]
    context = runtime.local_endpoint_context()
    if context.root is not None:
        cc.set_local_endpoint(root=context.root)
    cc.set_transport_policy(shm_threshold=16384)
    cc.register(Echo, EchoResource('default:'), name='same')
    runtime.register(Echo, EchoResource('explicit:'), name='same')
    assert registry._get_default_runtime()._runtime_session.client_ipc_config['shm_threshold'] == 16384
    assert runtime._runtime_session.client_ipc_config['shm_threshold'] == 8192
    with cc.connect(Echo, name='same') as default_proxy:
        with runtime.connect(Echo, name='same') as explicit_proxy:
            assert default_proxy.echo('x') == 'default:x'
            assert explicit_proxy.echo('x') == 'explicit:x'
        assert runtime.shutdown()['completed']
        assert default_proxy.echo('alive') == 'default:alive'
    assert cc.shutdown()['completed']


def test_start_error_projection_allows_native_lifecycle_observation(tmp_path):
    script = textwrap.dedent('''
        import os, sys
        import c_two as cc
        from c_two._native import RuntimeSession
        session = RuntimeSession(startup_timeout=0, use_process_relay_anchor=False)
        if os.name != 'nt':
            session.set_local_endpoint(root=sys.argv[1])
        original = cc.error.CoreError.__init__
        def observing_error(self, category, message):
            session.native_lifecycle_snapshot()
            original(self, category, message)
        cc.error.CoreError.__init__ = observing_error
        try:
            session.ensure_host_started()
        except cc.error.CoreError as error:
            assert error.lifecycle_kind == 'startup_timeout'
        else:
            raise AssertionError('zero startup budget did not expire')
        assert session.shutdown()['completed']
    ''')
    result = subprocess.run([sys.executable, '-c', script, str(tmp_path)], capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
    assert {p.name for p in tmp_path.iterdir()} <= {'.gate', '.gate.marker'}


@pytest.mark.parametrize('restart', [False, True])
def test_failed_native_start_reports_cleanup_without_closing_other_listener(tmp_path, restart):
    identity = 'runtime-receipt-' + uuid.uuid4().hex
    session = RuntimeSession(server_id=identity, use_process_relay_anchor=False)
    owner = cc.Runtime()
    owner.set_server(server_id=identity)
    if os.name != 'nt':
        session.set_local_endpoint(root=str(tmp_path))
        owner.set_local_endpoint(root=str(tmp_path))
    try:
        if restart:
            session.ensure_host_started()
            assert session.shutdown()['completed']
        owner.start()
        credential = owner.server_credential().to_json()
        with pytest.raises(cc.error.CoreError) as caught:
            session.ensure_server_bridge().start()
        assert caught.value.io_operation == 'bind'
        assert caught.value.io_kind == 'address_in_use'
        assert not session.host_started
        terminal = session.shutdown(timeout_seconds=5)
        assert terminal['completed']
        assert terminal['runtime_barrier_error']
        assert str(caught.value).endswith(terminal['runtime_barrier_error'])
        assert terminal['route_close_error'] is None
        assert terminal['ipc_client_close_error'] is None
        assert session.native_terminal_outcome() == terminal
        assert session.shutdown(timeout_seconds=0) == terminal
        assert owner.server_credential().to_json() == credential
        owner.register(Echo, EchoResource('still:'), name='echo')
        with owner.connect(Echo, name='echo', address=owner.server_address()) as proxy:
            assert proxy.echo('owned') == 'still:owned'
    finally:
        assert session.shutdown()['completed']
        assert owner.shutdown()['completed']
    assert {p.name for p in tmp_path.iterdir()} <= {'.gate', '.gate.marker'}


@pytest.mark.platform('posix')
def test_public_start_preserves_native_bind_denial_and_cleanup(tmp_path):
    if sys.platform != 'darwin' and os.geteuid() == 0:
        pytest.skip('filesystem permission denial requires an unprivileged uid')
    script = textwrap.dedent('''
        import os, sys
        from pathlib import Path
        import c_two as cc
        root = Path(sys.argv[1])
        runtime = cc.Runtime()
        runtime.set_local_endpoint(root=str(root))
        if sys.platform != 'darwin':
            root.chmod(0o500)
        try:
            try:
                runtime.start()
            except cc.error.CoreError as error:
                assert error.io_operation == 'bind'
                assert error.io_kind == 'permission_denied'
                assert error.raw_os_error in (1, 13)
            else:
                raise AssertionError('bind denial unexpectedly started a Host')
        finally:
            outcome = runtime.shutdown()
            root.chmod(0o700)
            assert outcome['completed'], outcome
        assert runtime.server_credential() is None
    ''')
    command = [sys.executable, '-c', script, str(tmp_path)]
    if sys.platform == 'darwin':
        command = ['/usr/bin/sandbox-exec', '-p', '(version 1)(allow default)(deny network-bind)', *command]
    result = subprocess.run(command, capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stdout + result.stderr
    assert {p.name for p in tmp_path.iterdir()} <= {'.gate', '.gate.marker'}
