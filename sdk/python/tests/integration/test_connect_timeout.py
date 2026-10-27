"""Real native connection budgets through IPC and stalled relay controls."""
from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler

import pytest

from tests.fixtures import local_server

import c_two as cc
from c_two.transport.registry import _get_default_runtime, _reset_default_runtime
from tests.fixtures.http_server import LoopbackHTTPServer


@cc.crm(namespace='test.connect_timeout.integration', version='0.1.0')
class ConnectContract:
    def echo(self, value: object) -> object:
        ...


class Resource:
    calls = 0

    def echo(self, value):
        self.calls += 1
        return value


def test_ipc_timeout_zero_then_finite_and_default_connect_are_usable(unique_ipc_address):
    _reset_default_runtime()
    resource = Resource()
    server = local_server(unique_ipc_address)
    server.register_crm(ConnectContract, resource, name='connect-timeout')
    server.start()
    try:
        with pytest.raises(cc.error.CallDeadlineExceeded):
            cc.connect(ConnectContract, name='connect-timeout', address=unique_ipc_address, timeout=0)
        assert resource.calls == 0
        # Warm the native pool, release the proxy, then exercise live acquisition
        # on the cached client with a fresh connect budget.
        for timeout in [0.5, None, 0.5]:
            connection = cc.connect(
                ConnectContract, name='connect-timeout', address=unique_ipc_address,
                timeout=timeout,
            )
            try:
                assert connection.echo('after') == 'after'
            finally:
                cc.close(connection)
        assert resource.calls == 3
    finally:
        server.shutdown()
        _reset_default_runtime()


@pytest.fixture
def stalled_relay():
    release = threading.Event()
    entered = threading.Event()
    paths = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            paths.append(self.path)
            entered.set()
            if not release.wait(5):
                return
            body = json.dumps({
                'version': 1, 'code': 701, 'name': 'ResourceNotFound',
                'message': 'route not found: connect-timeout',
                'details': {'transport_phase': 'application_detail', 'route_name': 'connect-timeout'},
            }).encode()
            self.send_response(404)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                # The timed-out caller has cancelled its HTTP response wait.
                pass

        def log_message(self, *_args):
            pass

    server = LoopbackHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = False
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    try:
        yield f'http://127.0.0.1:{server.server_port}', entered, release, paths
    finally:
        release.set()
        server.shutdown()
        server.server_close()
        worker.join(2)
        assert not worker.is_alive()


@pytest.mark.parametrize('mode', ['explicit', 'discovery'])
def test_stalled_relay_connect_budget_and_recovery(mode, stalled_relay):
    _reset_default_runtime()
    url, entered, release, paths = stalled_relay
    registry = _get_default_runtime()
    # Use the session projection for discovery to avoid changing global settings.
    from c_two.crm.contract import crm_contract
    args = ('connect-timeout', *crm_contract(ConnectContract).native_args())

    def connect(timeout):
        if mode == 'explicit':
            return cc.connect(ConnectContract, name='connect-timeout', address=url, timeout=timeout)
        registry._runtime_session.set_relay_anchor_address(url)
        return registry._runtime_session.connect_via_relay(*args, timeout_seconds=timeout)

    try:
        started = time.monotonic()
        expected_error = cc.error.CallDeadlineExceeded if mode == 'explicit' else cc.error.CoreError
        with pytest.raises(expected_error) as failure:
            connect(0.1)
        elapsed = time.monotonic() - started
        assert entered.is_set()
        assert 0.05 <= elapsed < 1.0
        assert failure.value.details['operation'] == 'connect'
        assert failure.value.details['transport_phase'] == 'pre_dispatch'
        assert failure.value.details['stage']
        native = failure.value.__cause__ if mode == 'explicit' else failure.value
        assert type(native) is cc.error.CoreError
        assert isinstance(native, cc.error.CCBaseError) and isinstance(native, RuntimeError)
        assert set(cc.error.CoreError.__annotations__).issubset(vars(native))
        assert (native.io_operation, native.io_kind, native.raw_os_error) == (None, None, None)
        assert paths and all(path.startswith(('/_resolve', '/_probe')) for path in paths)
        # Let the late control response finish. A new caller must reach the
        # server promptly and observe its authoritative 404, not a stuck waiter.
        release.set()
        previous_requests = len(paths)
        started = time.monotonic()
        recovered_error = cc.error.ResourceNotFound if mode == 'explicit' else cc.error.CoreError
        with pytest.raises(recovered_error) as recovered:
            connect(1)
        assert time.monotonic() - started < 1
        projected = recovered.value if mode == 'explicit' else cc.error.CCError.deserialize(recovered.value.error_bytes)
        native = recovered.value.__cause__ if mode == 'explicit' else recovered.value
        assert type(native) is cc.error.CoreError
        assert isinstance(projected, cc.error.ResourceNotFound)
        assert projected.message == 'route not found: connect-timeout'
        assert projected.details['route_name'] == 'connect-timeout'
        assert native.transport_phase == projected.details['transport_phase'] == 'application_detail'
        assert len(paths) > previous_requests
    finally:
        release.set()
        _reset_default_runtime()
