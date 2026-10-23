"""Physical IPC facts with idle real processes on UDS and Windows Named Pipes."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeout
from contextlib import ExitStack, contextmanager
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


def test_one_client_subscription_observes_multiple_idle_connections():
    events = cc.connection_events()
    with ExitStack() as stack:
        peers = [stack.enter_context(peer_process('server')) for _ in range(3)]
        clients = [cc.connect(ObservationPeer, name='peer', address=ready['address'])
                   for _, ready in peers]
        observations = [cc.connection_observation(client) for client in clients]
        ids = {observation.connection_id for observation in observations}
        assert len(ids) == 3
        opened = [events.recv(5.0) for _ in range(3)]
        assert {event.connection_id for event in opened} == ids
        assert all(event.side == 'client' and event.kind == 'opened' for event in opened)
        assert all(observation.snapshot() is None for observation in observations)
        assert not hasattr(observations[0], 'wait_closed')
        shared = cc.connect(ObservationPeer, name='peer', address=peers[0][1]['address'])
        assert cc.connection_observation(shared).connection_id == observations[0].connection_id
        cc.close(shared)
        cc.close(clients[1])
        assert observations[1].snapshot() is None
        assert events.recv(0.0) is None
        for index, terminate in enumerate(('kill', 'close')):
            server = peers[index][0]
            if terminate == 'kill':
                server.kill()
            else:
                server.stdin.close()
            server.wait(timeout=5.0)
            event = events.recv(5.0)
            assert event.side == 'client' and event.kind == 'closed'
            assert event.connection_id == observations[index].connection_id
            assert event.value.reason in {'peer_eof', 'connection_reset'}
            assert event.value == observations[index].snapshot()
        cc.shutdown()
        event = events.recv(5.0)
        assert event.side == 'client' and event.kind == 'closed'
        assert event.connection_id == observations[2].connection_id
        assert event.value.reason == 'local_close'
        assert event.value == observations[2].snapshot()
        events.close()
        with pytest.raises(StopIteration):
            events.recv(0.0)


def test_subscription_wait_releases_gil_and_close_wakes_only_that_subscription():
    with peer_process('server') as (_, ready):
        events = cc.connection_events()
        other = cc.connection_events()
        entered = threading.Event()
        def wait():
            entered.set()
            return events.recv()
        with ThreadPoolExecutor(max_workers=1) as executor:
            waiting = executor.submit(wait)
            try:
                assert entered.wait(2.0)
                with pytest.raises(FutureTimeout):
                    waiting.result(timeout=0.05)
                with pytest.raises(RuntimeError, match='already has a receiver'):
                    events.recv(0.0)
                events.close()
                with pytest.raises(StopIteration):
                    waiting.result(timeout=2.0)
            finally:
                events.close()
                if not waiting.done():
                    cc.shutdown()
        events.close()
        client = cc.connect(ObservationPeer, name='peer', address=ready['address'])
        opened = other.recv(5.0)
        assert opened.side == 'client' and opened.kind == 'opened'
        assert client.echo(b'after observer close') == b'after observer close'
        assert cc.connection_observation(client).snapshot() is None
        with pytest.raises(StopIteration):
            events.recv(0.0)
        other.close()
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
        event = events.recv(5.0)
        assert event.side == 'server' and event.kind == 'closed'
        closed = event.value
        assert closed is not None
        assert closed.reason in ({'peer_eof', 'connection_reset'} if terminate == 'kill' else {'peer_close_message'})
        assert event.connection_id == opened.connection_id
        assert observation.snapshot() == closed
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
        closed_event = progress.recv(5.0)
        assert closed_event.kind == 'closed'
        assert closed_event.connection_id == observation.connection_id
        closed = observation.snapshot()
        assert closed_event.value == closed
        assert closed is not None
        assert closed.reason in {'peer_eof', 'connection_reset'}
