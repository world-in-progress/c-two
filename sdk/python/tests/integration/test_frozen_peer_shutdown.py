"""Outgoing-client shutdown must not wait for a stopped resource process."""
from __future__ import annotations

import os
import signal
import subprocess
import sys
import tempfile
import time
import uuid

import pytest

import c_two as cc
from c_two.transport import registry as registry_module
from tests.fixtures.process_control import (
    interrupt_python_process,
    python_process_options,
    readline_with_timeout,
)


@cc.crm(namespace='test.frozen-peer-shutdown', version='0.1.0')
class Echo:
    def echo(self, value: str) -> str:
        ...


_RESOURCE = """
import os
import c_two as cc

@cc.crm(namespace='test.frozen-peer-shutdown', version='0.1.0')
class Echo:
    def echo(self, value: str) -> str: ...

class EchoResource:
    def echo(self, value: str) -> str:
        return value

cc.set_server(server_id=os.environ['C2_TEST_FROZEN_SERVER_ID'])
cc.register(Echo, EchoResource(), name='echo')
print(cc.server_address(), flush=True)
cc.serve()
"""


@pytest.mark.parametrize('freeze', [
    False,
    pytest.param(True, marks=pytest.mark.skipif(
        os.name == 'nt', reason='SIGSTOP/SIGCONT are POSIX process controls',
    )),
])
@pytest.mark.parametrize('hosted', [False, True])
def test_client_shutdown_with_live_proxy(monkeypatch, freeze: bool, hosted: bool) -> None:
    # This private namespace is never shared with historical test endpoints.
    with tempfile.TemporaryDirectory(prefix='c2-frozen-') as root:
        with monkeypatch.context() as environment:
            if os.name != 'nt':
                environment.setenv('C2_IPC_ROOT', root)
            environment.setenv('C2_ENV_FILE', '')
            environment.setenv('C2_RELAY_ANCHOR_ADDRESS', '')
            # Keep this test's immutable Runtime namespace independent
            # of any process session used by earlier tests.
            environment.setattr(registry_module, '_default_runtime', None)
            cc.shutdown()
            context = cc.local_endpoint_context()
            child_env = os.environ.copy()
            child_env['C2_TEST_FROZEN_SERVER_ID'] = f'frozen-peer-{uuid.uuid4().hex}'
            process = subprocess.Popen(
                [sys.executable, '-u', '-c', _RESOURCE],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                env=child_env, **python_process_options(),
            )
            address = None
            credential = None
            proxy = None
            try:
                assert process.stdout is not None
                address = readline_with_timeout(process.stdout, 5).strip()
                assert address.startswith('ipc://'), address
                if os.name != 'nt':
                    observed = cc.inspect_endpoint(address, context=context)
                    assert observed['status'] == 'present', observed
                    credential = observed['credential']
                    assert credential is not None
                proxy = cc.connect(Echo, name='echo', address=address)
                assert proxy.echo('before-freeze') == 'before-freeze'
                if hosted:
                    class LocalEcho:
                        def echo(self, value: str) -> str:
                            return value

                    cc.register(Echo, LocalEcho(), name='local-echo')
                if freeze:
                    process.send_signal(signal.SIGSTOP)
                    pid, status = os.waitpid(process.pid, os.WUNTRACED)
                    assert pid == process.pid and os.WIFSTOPPED(status), status

                started = time.monotonic()
                outcome = cc.shutdown()
                elapsed = time.monotonic() - started
                print(f'client shutdown: {hosted=}, frozen={freeze}, elapsed={elapsed:.3f}s, {outcome=}')
                assert elapsed < 2, (elapsed, outcome)
                assert outcome['completed'] is True, outcome
                assert outcome['ipc_clients_drained'] is True, outcome
                assert outcome['ipc_client_close_error'] is None, outcome
                assert outcome['removed_routes'] == (['local-echo'] if hosted else []), outcome
                assert all(route['active_drained'] for route in outcome['route_outcomes']), outcome
                assert outcome['ipc_clients_abnormally_closed'] == int(freeze), outcome
                abnormal = outcome['ipc_client_close_details']
                if freeze:
                    assert abnormal == [{
                        'address': address, 'reason': 'disconnect_ack_timeout',
                    }], outcome
                else:
                    assert abnormal == [], outcome
            finally:
                # Resume before any further cleanup, even after a failing
                # assertion. OS wait must precede credential-based reap.
                try:
                    if os.name != 'nt' and process.poll() is None:
                        process.send_signal(signal.SIGCONT)
                    if proxy is not None:
                        cc.close(proxy)
                    cc.shutdown()
                finally:
                    if process.poll() is None:
                        interrupt_python_process(process)
                    try:
                        process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate(timeout=5)
                    assert process.returncode is not None
                    if address is not None and credential is not None:
                        reaped = cc.reap_endpoint(address, credential, context=context)
                        assert reaped['status'] in {'reaped', 'already-absent'}, reaped
