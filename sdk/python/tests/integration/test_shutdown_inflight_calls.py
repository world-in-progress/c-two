"""The default Runtime drains its own dispatched IPC calls after shutdown times out."""
from __future__ import annotations

import os
import threading
import uuid

import c_two as cc
import c_two.endpoint
from c_two.config.settings import settings
from c_two.transport import registry


@cc.crm(namespace='test.shutdown.inflight', version='0.1.0')
class Echo:
    def echo(self, value: bytes) -> bytes: ...


def test_default_runtime_shutdown_preserves_its_own_inflight_ipc_call(monkeypatch, tmp_path):
    cc.shutdown(timeout=5)
    monkeypatch.setattr(registry, '_default_runtime', None)
    monkeypatch.setenv('C2_ENV_FILE', '')
    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
    monkeypatch.setattr(settings, '_shm_threshold', None)
    if os.name != 'nt':
        cc.endpoint.set_local_endpoint(root=str(tmp_path))
    entered = threading.Event()
    resume = threading.Event()
    results: dict[str, object] = {}

    class EchoResource:
        def echo(self, value: bytes) -> bytes:
            entered.set()
            assert resume.wait(5), 'test did not release the active callback'
            return value

    name = f'inflight-{uuid.uuid4().hex}'
    cc.set_transport_policy(shm_threshold=1)
    cc.set_server(server_id=name)
    cc.register(Echo, EchoResource(), name=name)
    proxy = cc.connect(Echo, name=name, address=cc.server_address())
    value = b'original result' * 8192

    def call() -> None:
        try:
            results['value'] = proxy.echo(value)
        except BaseException as error:
            results['error'] = error

    worker = threading.Thread(target=call, daemon=True)
    try:
        worker.start()
        assert entered.wait(5), 'explicit IPC call never entered its resource'
        pending = cc.shutdown(timeout=0.02)
        assert pending['completed'] is False, pending
        assert worker.is_alive(), results
        assert results == {}, results
        resume.set()
        worker.join(5)
        assert not worker.is_alive(), 'dispatched IPC caller did not receive its reply'
        assert results == {'value': value}, results
        completed = cc.shutdown(timeout=5)
        assert completed['completed'] is True, completed
        assert completed['ipc_clients_drained'] is True, completed
    finally:
        resume.set()
        worker.join(5)
        cc.close(proxy)
        cc.shutdown(timeout=5)
