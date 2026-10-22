"""Physical IPC facts with idle real processes on UDS and Windows Named Pipes."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

import pytest
import c_two as cc
from c_two.config.settings import settings
from c_two.transport.registry import _ProcessRegistry
from tests.fixtures.process_control import python_process_options, readline_with_timeout


@cc.crm(namespace='test.connection-observation', version='1.0.0')
class ObservationPeer:
    def echo(self, value: bytes) -> bytes: ...


class PeerResource:
    def echo(self, value: bytes) -> bytes:
        return value


@pytest.fixture(autouse=True)
def isolated_runtime(tmp_path, monkeypatch):
    _ProcessRegistry.reset()
    for name in ('_relay_anchor_address', '_shm_threshold', '_remote_payload_chunk_size'):
        monkeypatch.setattr(settings, name, None)
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
    monkeypatch.delenv('C2_IPC_ROOT', raising=False)
    if os.name != 'nt':
        root = tmp_path / 'ipc'
        root.mkdir(mode=0o700)
        monkeypatch.setenv('C2_IPC_ROOT', str(root))
        cc.set_local_endpoint(root=str(root))
    # A dead peer must be observable without heartbeat or business calls.
    cc.set_server(ipc_overrides={'heartbeat_interval': 0.0})
    context = cc.local_endpoint_context()
    yield
    address = cc.server_address()
    _ProcessRegistry.reset()
    if address:
        assert cc.inspect_endpoint(address, context=context)['status'] in {'absent', 'not-applicable'}


@contextmanager
def peer_process(role: str, address: str | None = None):
    script = """
import json, sys
import c_two as cc
from tests.integration.test_connection_observation import ObservationPeer, PeerResource
role, address = sys.argv[1:]
if role == 'server':
    cc.set_server(ipc_overrides={'heartbeat_interval': 0.0})
    cc.register(ObservationPeer, PeerResource(), name='peer')
    address = cc.server_address()
    credential = cc.inspect_endpoint(address)['credential']
    print(json.dumps({'address': address, 'credential': credential.to_json() if credential else None}), flush=True)
else:
    proxy = cc.connect(ObservationPeer, name='peer', address=address)
    print(json.dumps({'address': address}), flush=True)
try:
    sys.stdin.read()
finally:
    cc.shutdown()
"""
    env = os.environ.copy()
    sdk = Path(__file__).resolve().parents[2]
    env['PYTHONPATH'] = os.pathsep.join([str(sdk / 'src'), str(sdk), env.get('PYTHONPATH', '')])
    env['C2_ENV_FILE'] = ''
    context = cc.local_endpoint_context()
    ready = {}
    with tempfile.TemporaryFile(mode='w+', encoding='utf-8') as stderr:
        process = subprocess.Popen([sys.executable, '-u', '-c', script, role, address or ''],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, env=env,
            **python_process_options())
        try:
            line = readline_with_timeout(process.stdout, 10.0)
            if not line:
                stderr.seek(0)
                raise AssertionError(stderr.read())
            ready = json.loads(line)
            yield process, ready
        finally:
            if process.stdin and not process.stdin.closed:
                process.stdin.close()
            try:
                process.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5.0)
            process.stdout.close()
            if role == 'server' and ready:
                if ready['credential']:
                    credential = cc.EndpointCredential.from_json(ready['credential'])
                    assert cc.reap_endpoint(ready['address'], credential, context=context)['status'] in {'reaped', 'already-absent'}
                assert cc.inspect_endpoint(ready['address'], context=context)['status'] in {'absent', 'not-applicable'}


def test_server_kill_notifies_idle_client_and_wait_releases_gil():
    with peer_process('server') as (server, ready):
        client = cc.connect(ObservationPeer, name='peer', address=ready['address'])
        observation = cc.connection_observation(client)
        assert observation.snapshot() is None
        assert observation.wait_closed(0.0) is None
        entered = threading.Event()
        def wait():
            entered.set()
            return observation.wait_closed(5.0)
        with ThreadPoolExecutor(max_workers=1) as executor:
            waiting = executor.submit(wait)
            assert entered.wait(2.0)
            server.kill()
            server.wait(timeout=5.0)
            closed = waiting.result(timeout=6.0)
        assert closed is not None
        assert closed.reason in {'peer_eof', 'connection_reset'}
        assert closed.connection_id == observation.connection_id
        assert observation.snapshot() == closed
        assert observation.wait_closed(0.0) == closed
        cc.close(client)


def finish_startup_connections(events):
    # Native start_and_wait performs real PING connections. Account for those
    # accepted before readiness and await their EOF before starting our peer.
    startup_connections = set()
    while (event := events.recv(0.0)) is not None:
        if event.kind == 'opened':
            startup_connections.add(event.connection_id)
        else:
            startup_connections.remove(event.connection_id)
    while startup_connections:
        event = events.recv(5.0)
        assert event.kind == 'closed'
        startup_connections.remove(event.connection_id)


@pytest.mark.parametrize('terminate', ['kill', 'close'])
@pytest.mark.parametrize('reconfigure', [False, True], ids=['default', 'reconfigured'])
def test_idle_client_end_notifies_server(terminate, reconfigure):
    events = cc.connection_events()
    if reconfigure:
        cc.set_transport_policy(shm_threshold=8192, remote_payload_chunk_size=8192)
        assert events.recv(0.0) is None
        runtime = _ProcessRegistry.get()._runtime_session
        assert runtime.client_ipc_config['shm_threshold'] == 8192
        assert not runtime.client_config_frozen
        assert runtime.server_address is None
    cc.register(ObservationPeer, PeerResource(), name='peer')
    finish_startup_connections(events)
    with peer_process('client', cc.server_address()) as (client, _):
        opened = events.recv(5.0)
        assert opened.kind == 'opened'
        observation = opened.value
        assert observation.snapshot() is None
        if terminate == 'kill':
            client.kill()
        else:
            client.stdin.close()
        client.wait(timeout=5.0)
        closed = observation.wait_closed(5.0)
        assert closed is not None
        assert closed.reason in ({'peer_eof', 'connection_reset'} if terminate == 'kill' else {'peer_close_message'})
        event = events.recv(5.0)
        assert event.kind == 'closed'
        assert event.connection_id == opened.connection_id
        assert event.value == closed
        assert events.recv(0.0) is None


def test_subscription_is_lazy_and_timeout_does_not_close_it():
    from c_two._native import RuntimeSession
    session = RuntimeSession(server_id='observation-not-started', use_process_relay_anchor=False)
    events = session.connection_events()
    assert events.recv(0.0) is None
    for timeout in (-1.0, float('nan'), float('inf'), 1e300):
        with pytest.raises(ValueError, match='timeout'):
            events.recv(timeout)
    assert session.server_address is None
    assert session.server_id is None
    del session
    with pytest.raises(StopIteration):
        events.recv(1.0)


def test_non_ipc_and_unconnected_values_are_explicitly_rejected():
    from c_two.transport.client.proxy import CRMProxy
    from c_two.error import UnsupportedCallMode
    with pytest.raises(TypeError, match='connected CRM'):
        cc.connection_observation(object())
    for native in [CRMProxy.thread_local(PeerResource()), CRMProxy.http(object(), 'peer')]:
        proxy = ObservationPeer()
        proxy.client = native
        with pytest.raises(UnsupportedCallMode, match='physical IPC'):
            cc.connection_observation(proxy)


def test_lagged_subscription_ends_but_existing_observation_remains_usable():
    from c_two.transport.client.util import ping

    events = cc.connection_events()
    cc.register(ObservationPeer, PeerResource(), name='peer')
    finish_startup_connections(events)
    with peer_process('client', cc.server_address()) as (client, _):
        opened = events.recv(5.0)
        assert opened.kind == 'opened'
        observation = opened.value
        progress = cc.connection_events()
        # A second consumer accounts for every real probe pair while the first
        # intentionally stops consuming. No timing assumption about reader EOF.
        for _ in range(513):
            assert ping(cc.server_address(), timeout=2.0)
            probe = progress.recv(5.0)
            closed = progress.recv(5.0)
            assert probe.kind == 'opened'
            assert closed.kind == 'closed'
            assert probe.connection_id == closed.connection_id
        with pytest.raises(cc.ConnectionEventsLagged) as error:
            events.recv(0.0)
        assert error.value.missed == 2
        with pytest.raises(StopIteration):
            events.recv(0.0)
        assert observation.snapshot() is None
        client.kill()
        client.wait(timeout=5.0)
        closed = observation.wait_closed(5.0)
        assert closed is not None
        assert closed.reason in {'peer_eof', 'connection_reset'}
