"""Unit tests for the NativeRelay (Rust-backed HTTP relay server)."""
from __future__ import annotations

import time

import pytest

from c_two._native import NativeRelay


class TestNativeRelayLifecycle:
    """Basic lifecycle: create → start → stop."""

    def test_create_not_running(self):
        relay = NativeRelay('127.0.0.1:0')
        assert relay.is_running is False
        assert relay.bind_address == '127.0.0.1:0'

    def test_start_and_stop(self):
        relay = NativeRelay('127.0.0.1:19900')
        try:
            relay.start()
            assert relay.is_running is True
        finally:
            relay.stop()
        assert relay.is_running is False

    def test_double_start_raises(self):
        relay = NativeRelay('127.0.0.1:19901')
        try:
            relay.start()
            with pytest.raises(RuntimeError, match='already running'):
                relay.start()
        finally:
            relay.stop()

    def test_stop_without_start_raises(self):
        relay = NativeRelay('127.0.0.1:0')
        with pytest.raises(RuntimeError, match='not running'):
            relay.stop()

    def test_list_routes_empty(self):
        relay = NativeRelay('127.0.0.1:19902')
        relay.start()
        try:
            routes = relay.list_routes()
            assert routes == []
        finally:
            relay.stop()


class TestNativeRelayUpstreamErrors:
    """Test upstream management error paths (no real IPC server)."""

    def test_register_upstream_not_running(self):
        relay = NativeRelay('127.0.0.1:0')
        with pytest.raises(RuntimeError, match='not running'):
            relay.register_upstream('test', 'ipc://nonexistent')

    def test_unregister_upstream_not_running(self):
        relay = NativeRelay('127.0.0.1:0')
        with pytest.raises(RuntimeError, match='not running'):
            relay.unregister_upstream('test')

    def test_unregister_unknown_name(self):
        relay = NativeRelay('127.0.0.1:19903')
        relay.start()
        try:
            with pytest.raises(RuntimeError, match='not registered'):
                relay.unregister_upstream('nonexistent')
        finally:
            relay.stop()

    def test_register_connection_failure(self):
        """register_upstream with unreachable address raises RuntimeError."""
        relay = NativeRelay('127.0.0.1:19904')
        relay.start()
        try:
            with pytest.raises(RuntimeError, match='Failed to connect'):
                relay.register_upstream('bad', 'ipc://nonexistent_server_xyz')
        finally:
            relay.stop()


class TestNativeRelayWithServer:
    """Integration test: NativeRelay + real Server via cc.register."""

    def test_register_and_list_routes(self):
        """Register an upstream to a real Server, verify routes appear."""
        import os
        import c_two as cc
        from c_two.transport.registry import _ProcessRegistry
        from tests.fixtures.hello import Hello
        from tests.fixtures.ihello import IHello

        _ProcessRegistry.reset()

        try:
            cc.register(IHello, Hello(), name='hello')
            ipc_addr = cc.server_address()

            relay = NativeRelay('127.0.0.1:19905')
            relay.start()
            try:
                relay.register_upstream('hello', ipc_addr)
                routes = relay.list_routes()
                assert len(routes) == 1
                assert routes[0]['name'] == 'hello'
                assert routes[0]['address'] == ipc_addr

                # Unregister
                relay.unregister_upstream('hello')
                assert relay.list_routes() == []
            finally:
                relay.stop()
        finally:
            cc.shutdown()
            _ProcessRegistry.reset()

    def test_http_data_plane_call(self):
        """Full chain: httpx → NativeRelay → IPC → CRM."""
        import os
        import httpx
        import c_two as cc
        from c_two._native import RustHttpClientPool
        from c_two.transport.registry import _ProcessRegistry
        from c_two.transport.client.proxy import ICRMProxy
        from tests.fixtures.hello import Hello
        from tests.fixtures.ihello import IHello

        _ProcessRegistry.reset()

        try:
            cc.register(IHello, Hello(), name='hello')
            ipc_addr = cc.server_address()

            relay = NativeRelay('127.0.0.1:19906')
            relay.start()
            time.sleep(0.2)  # small settle time
            try:
                relay.register_upstream('hello', ipc_addr)

                pool = RustHttpClientPool.instance()
                url = 'http://127.0.0.1:19906'
                client = pool.acquire(url)
                try:
                    icrm = IHello()
                    icrm.client = ICRMProxy.http(client, 'hello')
                    result = icrm.greeting('NativeRelay')
                    assert result == 'Hello, NativeRelay!'
                finally:
                    pool.release(url)
            finally:
                relay.stop()
        finally:
            cc.shutdown()
            _ProcessRegistry.reset()
