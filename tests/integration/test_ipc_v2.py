"""
Integration tests for IPC v2 transport (UDS control plane + SharedMemory data plane).
"""

import struct
import threading
import time
import uuid

import pytest

import c_two as cc
from c_two.rpc import ConcurrencyConfig, ConcurrencyMode, Server, ServerConfig
from c_two.rpc.server import _start
from c_two.rpc.ipc.ipc_server import IPCConfig

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


@pytest.fixture
def ipc_address():
    return f'ipc-v2://ipc_test_{uuid.uuid4().hex[:8]}'


def _start_server(address, concurrency=None, ipc_config=None):
    kwargs = dict(
        name='TestIPC',
        crm=Hello(),
        icrm=IHello,
        bind_address=address,
    )
    if concurrency:
        kwargs['concurrency'] = concurrency

    server = Server(ServerConfig(**kwargs))
    _start(server._state)

    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)
    return server


def _shutdown(address, server):
    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


class TestIPCv2Basic:
    """Basic connectivity and RPC tests."""

    def test_ping(self, ipc_address):
        server = _start_server(ipc_address)
        try:
            assert cc.rpc.Client.ping(ipc_address, timeout=1.0) is True
        finally:
            _shutdown(ipc_address, server)

    def test_shutdown(self, ipc_address):
        server = _start_server(ipc_address)
        result = cc.rpc.Client.shutdown(ipc_address, timeout=2.0)
        assert result is True
        time.sleep(0.2)
        assert cc.rpc.Client.ping(ipc_address, timeout=0.5) is False

    def test_crm_call_inline(self, ipc_address):
        """Small payload — should use inline transport."""
        server = _start_server(ipc_address)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('IPC')
            assert result == 'Hello, IPC!'
        finally:
            _shutdown(ipc_address, server)

    def test_crm_call_multiple(self, ipc_address):
        """Multiple sequential calls."""
        server = _start_server(ipc_address)
        try:
            for i in range(5):
                with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                    result = crm.greeting(f'call_{i}')
                assert result == f'Hello, call_{i}!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2SharedMemory:
    """Tests for SharedMemory data plane with large payloads."""

    def test_large_response(self, ipc_address):
        """CRM returns data larger than SHM threshold — should trigger SHM path."""
        server = _start_server(ipc_address)
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('world')
            assert result == 'Hello, world!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2Concurrency:
    """Concurrent access tests on ipc-v2://."""

    def test_concurrent_reads(self, ipc_address):
        server = _start_server(
            ipc_address,
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL, max_workers=4),
        )
        try:
            results = []
            errors = []
            barrier = threading.Barrier(4)

            def worker(name):
                barrier.wait()
                try:
                    with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                        result = crm.greeting(name)
                    results.append(result)
                except Exception as exc:
                    errors.append(exc)

            threads = [threading.Thread(target=worker, args=(f't{i}',)) for i in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)

            assert len(errors) == 0, f'Errors: {errors}'
            assert len(results) == 4
            for i in range(4):
                assert f'Hello, t{i}!' in results
        finally:
            _shutdown(ipc_address, server)

    def test_exclusive_mode(self, ipc_address):
        server = _start_server(
            ipc_address,
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE),
        )
        try:
            with cc.compo.runtime.connect_crm(ipc_address, IHello) as crm:
                result = crm.greeting('exclusive')
            assert result == 'Hello, exclusive!'
        finally:
            _shutdown(ipc_address, server)


class TestIPCv2EdgeCases:
    """Edge cases and error handling."""

    def test_ping_nonexistent(self):
        assert cc.rpc.Client.ping('ipc-v2://nonexistent_server_xyz', timeout=0.3) is False

    def test_shutdown_nonexistent(self):
        assert cc.rpc.Client.shutdown('ipc-v2://nonexistent_server_xyz', timeout=0.3) is True
