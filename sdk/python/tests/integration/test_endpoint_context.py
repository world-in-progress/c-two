"""Real local context isolation and cleanup, with spawned Python participants.

Custom filesystem roots are Unix-only and deliberately short, pre-created
containers. Common Persistent/OwnerBound lifecycle cases run on both Unix and
Windows. No test changes the pytest process environment or assumes fork.
"""
from __future__ import annotations

from contextlib import ExitStack
import json
import queue
import subprocess
import sys
import tempfile
import textwrap
import threading
import uuid

import pytest

from tests.unit.test_endpoint_context import IS_WINDOWS, UNIX_ONLY, _isolated_env, _run_isolated


CONTRACT = '''
import c_two as cc

@cc.crm(namespace='test.endpoint.context', version='0.1.0')
class Echo:
    def value(self) -> str: ...

    @cc.on_shutdown
    def done(self) -> None: ...
'''

RESOURCE = CONTRACT + '''
import json
import sys

class Resource:
    def __init__(self, label):
        self.label = label
        self.hooks = 0

    def value(self):
        return self.label

    def done(self):
        self.hooks += 1

root, address, label = sys.argv[1:]
cc.set_local_endpoint(root=root)
cc.set_server(server_id=address.removeprefix('ipc://'))
resource = Resource(label)
cc.register(Echo, resource, name='same-route')
context = cc.local_endpoint_context()
observed = cc.inspect_endpoint(address)
assert observed['status'] == 'present', observed
print(json.dumps({
    'address': cc.server_address(), 'root': context.root,
    'namespace_id': context.namespace_id,
    'credential': observed['credential'].to_json(),
}), flush=True)
assert sys.stdin.readline().strip() == 'stop'
outcome = cc.shutdown(timeout=5)
assert outcome['completed'], outcome
assert resource.hooks == 1
print(json.dumps({'completed': True, 'hooks': resource.hooks}), flush=True)
'''


@pytest.fixture
def short_roots():
    if IS_WINDOWS:
        pytest.skip('Unix filesystem root capability')
    with ExitStack() as stack:
        roots = [
            stack.enter_context(tempfile.TemporaryDirectory(prefix='c2-', dir='/tmp')),
            stack.enter_context(tempfile.TemporaryDirectory(prefix='c2-', suffix=' ', dir='/tmp')),
        ]
        yield roots


class _ResourceProcess:
    def __init__(self, root: str, address: str, label: str):
        self.process = subprocess.Popen(
            [sys.executable, '-c', RESOURCE, root, address, label],
            env=_isolated_env(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )
        self.lines: queue.Queue[str | None] = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def row(self) -> dict:
        try:
            line = self.lines.get(timeout=8)
        except queue.Empty:
            self.close()
            raise AssertionError('resource did not reach native readiness')
        if line is None:
            self.process.wait(timeout=5)
            raise AssertionError(self.process.stderr.read())
        return json.loads(line)

    def stop(self) -> None:
        self.process.stdin.write('stop\n')
        self.process.stdin.flush()
        row = self.row()
        assert row == {'completed': True, 'hooks': 1}, row
        assert self.process.wait(timeout=8) == 0, self.process.stderr.read()

    def kill_and_wait(self) -> None:
        self.process.kill()
        assert self.process.wait(timeout=8) != 0

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=8)
        self.reader.join(timeout=2)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            stream.close()


@UNIX_ONLY
def test_same_logical_address_registers_connects_and_stays_isolated(short_roots) -> None:
    first, second = short_roots
    address = f'ipc://context-{uuid.uuid4().hex[:12]}'
    with ExitStack() as cleanup:
        a = _ResourceProcess(first, address, 'a')
        cleanup.callback(a.close)
        b = _ResourceProcess(second, address, 'b')
        cleanup.callback(b.close)
        ready_a, ready_b = a.row(), b.row()
        assert ready_a['address'] == ready_b['address'] == address
        assert ready_a['root'] == first
        assert ready_b['root'] == second  # The real container ends in a space.
        assert ready_a['namespace_id'] != ready_b['namespace_id']
        for root, label, other in ((first, 'a', second), (second, 'b', first)):
            _run_isolated(CONTRACT + textwrap.dedent(f'''
                from c_two.transport.client import util
                cc.set_local_endpoint(root={root!r})
                context = cc.local_endpoint_context()
                assert util._endpoint_name_from_address({address!r}) == context.endpoint_name({address!r})
                assert util.ping({address!r})
                proxy = cc.connect(Echo, name='same-route', address={address!r})
                assert proxy.value() == {label!r}
                cc.close(proxy)
                assert util.ping({address!r})
                # An explicit independent context reaches the other namespace.
                other = cc.local_endpoint_context(root={other!r})
                assert util.ping({address!r}, context=other)
                assert cc.inspect_endpoint({address!r})['credential'].context == context
                assert cc.shutdown()['completed']
                assert cc.local_endpoint_context() == context
            '''), C2_IPC_ROOT=other, C2_RELAY_ANCHOR_ADDRESS='http://127.0.0.1:1')
        a.stop()
        # Retirement in one namespace does not stop the identical name in the other.
        _run_isolated(CONTRACT + textwrap.dedent(f'''
            from c_two.transport.client import util
            cc.set_local_endpoint(root={second!r})
            assert util.ping({address!r})
            proxy = cc.connect(Echo, name='same-route', address={address!r})
            assert proxy.value() == 'b'
            cc.close(proxy)
            ack = util.shutdown({address!r})
            assert ack['acknowledged'] and ack['shutdown_started'], ack
            assert not ack['server_stopped'], ack
            assert cc.shutdown()['completed']
        '''))
        b.stop()


@UNIX_ONLY
@pytest.mark.parametrize('attempt', ['bind', 'connect', 'ping', 'shutdown'])
def test_first_failed_local_attempt_freezes_context_without_creating_missing_container(attempt, short_roots) -> None:
    missing = f'/tmp/q-{uuid.uuid4().hex[:8]}'
    changed = short_roots[1]
    address = f'ipc://failed-{uuid.uuid4().hex[:12]}'
    _run_isolated(CONTRACT + textwrap.dedent(f'''
        import os
        from pathlib import Path
        from c_two import _native
        from c_two.transport.registry import _ProcessRegistry
        from c_two.transport.client import util
        session = _ProcessRegistry.get()._runtime_session
        captured = cc.local_endpoint_context()
        assert captured.root == {missing!r}
        assert not session.local_endpoint_frozen
        if {attempt!r} == 'bind':
            try:
                session.ensure_host_started()
            except Exception as exc:
                assert 'container' in str(exc) or 'No such file' in str(exc), exc
            else:
                raise AssertionError('missing container was adopted or created')
        elif {attempt!r} == 'connect':
            try:
                cc.connect(Echo, name='same-route', address={address!r})
            except _native.CoreError as exc:
                assert exc.category == 'transport', exc
                assert exc.transport_kind == 'ipc', exc
                assert exc.transport_phase == 'pre_dispatch', exc
            else:
                raise AssertionError('absent resource connected')
        elif {attempt!r} == 'ping':
            assert not util.ping({address!r}, timeout=.01)
        else:
            stopped = util.shutdown({address!r}, timeout=.01)
            assert stopped['acknowledged'] and stopped['server_stopped'], stopped
            assert not stopped['shutdown_started'], stopped
        assert session.local_endpoint_frozen
        assert not Path({missing!r}).exists()
        os.environ['C2_IPC_ROOT'] = {changed!r}
        assert cc.local_endpoint_context() == captured
        assert util._endpoint_name_from_address({address!r}) == captured.endpoint_name({address!r})
        cc.set_local_endpoint(root={missing!r})
        session.set_local_endpoint(context=captured)
        try:
            cc.set_local_endpoint(root={changed!r})
        except _native.CoreError as exc:
            assert exc.lifecycle_kind == 'config_frozen', exc
        else:
            raise AssertionError('failed local attempt left root mutable')
        assert cc.shutdown()['completed']
        replacement = _ProcessRegistry.get()._runtime_session
        assert replacement is not session
        assert not replacement.local_endpoint_frozen
        assert cc.local_endpoint_context() == captured
        cc.set_local_endpoint(root={changed!r})
        assert cc.local_endpoint_context().root == {changed!r}
    '''), C2_IPC_ROOT=missing)


@UNIX_ONLY
@pytest.mark.parametrize('abnormal', [False, True], ids=['normal', 'abnormal'])
def test_exact_reap_uses_credential_context_and_mismatch_does_not_touch_state(abnormal, short_roots) -> None:
    root, other = short_roots
    address = f'ipc://reap-{uuid.uuid4().hex[:12]}'
    resource = _ResourceProcess(root, address, 'cleanup')
    try:
        ready = resource.row()
        document = ready['credential']
        _run_isolated(f'''
            import c_two as cc
            from c_two import _native
            from pathlib import Path
            credential = cc.EndpointCredential.from_json({document!r})
            assert credential.context.root == {root!r}
            assert cc.local_endpoint_context().root == {other!r}
            assert cc.reap_endpoint({address!r}, credential)['status'] == 'busy'
            def snapshot():
                return {{str(p): p.read_bytes() for p in Path({root!r}).rglob('*') if p.is_file()}}
            before = snapshot()
            assert before  # Real ownership/gate records, not fabricated state.
            for result in (
                cc.reap_endpoint({address!r}, credential, root={other!r}),
                cc.reap_endpoint({address!r}, credential, context=cc.local_endpoint_context()),
                _native.reap_endpoint_credential({address!r}, credential._native, root={other!r}),
            ):
                assert result['status'] == 'stale-target', result
                assert result['reason'] == 'credential-context-mismatch', result
            assert snapshot() == before
        ''', C2_IPC_ROOT=other)
        if abnormal:
            resource.kill_and_wait()  # OS wait precedes native reap.
        else:
            resource.stop()
        _run_isolated(f'''
            import c_two as cc
            credential = cc.EndpointCredential.from_json({document!r})
            result = cc.reap_endpoint({address!r}, credential)
            assert result['status'] in ('reaped', 'already-absent'), result
            assert cc.inspect_endpoint({address!r}, context=credential.context)['status'] == 'absent'
            with cc.sweep_endpoints(addresses=[{address!r}], context=credential.context) as sweep:
                assert sweep.context == credential.context
                for _ in range(100):
                    batch = sweep.next_batch(max_entries=1)
                    assert not batch['round_interrupted'], batch
                    assert not batch['io_errors'], batch
                    if batch['round_complete']:
                        break
                else:
                    raise AssertionError('captured-root sweep did not finish')
        ''', C2_IPC_ROOT=other)
    finally:
        resource.close()


@UNIX_ONLY
def test_sweep_captures_code_context_once_and_keeps_it_across_env_flip(short_roots) -> None:
    root, other = short_roots
    address = f'ipc://sweep-context-{uuid.uuid4().hex[:12]}'
    resource = _ResourceProcess(root, address, 'sweep')
    try:
        document = resource.row()['credential']
        resource.kill_and_wait()
        _run_isolated(f'''
            import os
            import c_two as cc
            from c_two import _native
            credential = cc.EndpointCredential.from_json({document!r})
            cc.set_local_endpoint(root={root!r})
            with cc.sweep_endpoints(addresses=[{address!r}], max_entries=1) as sweep:
                assert sweep.context == credential.context
                os.environ['C2_IPC_ROOT'] = {other!r}
                cc.set_local_endpoint(root={other!r})
                assert cc.local_endpoint_context() != sweep.context
                reaped = 0
                for _ in range(100):
                    batch = sweep.next_batch()
                    assert not batch['round_interrupted'], batch
                    assert not batch['io_errors'], batch
                    reaped += batch['reaped']
                    if batch['round_complete']:
                        break
                else:
                    raise AssertionError('sweep did not complete')
                assert reaped == 1, reaped
            assert cc.inspect_endpoint({address!r}, context=credential.context)['status'] == 'absent'
            # The raw native constructor also captures env once for its round.
            os.environ['C2_IPC_ROOT'] = {root!r}
            native = _native.PyEndpointSweep(addresses=[])
            os.environ['C2_IPC_ROOT'] = {other!r}
            assert native.context == credential.context
            batch = native.next_batch()
            assert not batch['round_interrupted'], batch
            native.close()
        ''', C2_IPC_ROOT=other)
    finally:
        resource.close()


def test_common_owner_bound_transfer_default_context_and_business_disconnect() -> None:
    address = f'ipc://owner-context-{uuid.uuid4().hex[:12]}'
    business = CONTRACT + textwrap.dedent(f'''
        proxy = cc.connect(Echo, name='same-route', address={address!r})
        assert proxy.value() == 'owner'
        cc.close(proxy)
        assert cc.shutdown()['completed']
    ''')
    _run_isolated(CONTRACT + textwrap.dedent(f'''
        import subprocess
        import sys
        import time
        from c_two.transport.registry import _ProcessRegistry
        keepalive, receiver = cc.owner_control_pair()
        class Resource:
            hooks = 0
            def value(self): return 'owner'
            def done(self): self.hooks += 1
        resource = Resource()
        try:
            cc.set_server(server_id={address.removeprefix('ipc://')!r},
                lifecycle=cc.LifecycleConfig.owner_bound(.05), owner_control=receiver)
            original = _ProcessRegistry.get()._runtime_session
            context = cc.local_endpoint_context()
            cc.set_transport_policy(shm_threshold=8192)
            replacement = _ProcessRegistry.get()._runtime_session
            assert replacement is not original
            assert replacement.owner_control_attached
            assert not original.owner_control_attached
            assert cc.local_endpoint_context() == context
            cc.register(Echo, resource, name='same-route')
            assert replacement.local_endpoint_frozen
            result = subprocess.run([sys.executable, '-c', {business!r}],
                capture_output=True, text=True, timeout=8)
            assert result.returncode == 0, result.stderr
            assert cc.native_lifecycle_snapshot()['phase'] == 'armed'
            assert cc.native_terminal_outcome() is None
            assert resource.hooks == 0
            keepalive.shutdown()
            deadline = time.monotonic() + 5
            while cc.native_terminal_outcome() is None:
                assert time.monotonic() < deadline, 'owner EOF did not drain'
                time.sleep(.01)
            outcome = cc.shutdown(timeout=5)
            assert outcome['completed'], outcome
            assert resource.hooks == 1
            assert cc.local_endpoint_context() == context
        finally:
            keepalive.shutdown()
            cc.shutdown(timeout=5)
    '''))


def test_common_active_incomplete_shutdown_retains_session_bindings_and_context() -> None:
    _run_isolated('''
        import threading
        import c_two as cc
        from c_two.transport.registry import _ProcessRegistry
        @cc.crm(namespace='test.endpoint.active', version='0.1.0')
        class Blocking:
            def wait(self) -> str: ...
            @cc.on_shutdown
            def done(self) -> None: ...
        class Resource:
            hooks = 0
            entered = threading.Event()
            release = threading.Event()
            def wait(self):
                self.entered.set()
                assert self.release.wait(8)
                return 'drained'
            def done(self): self.hooks += 1
        resource = Resource()
        results = []
        cc.register(Blocking, resource, name='blocking')
        proxy = cc.connect(Blocking, name='blocking')
        session = _ProcessRegistry.get()._runtime_session
        context = cc.local_endpoint_context()
        worker = threading.Thread(target=lambda: results.append(proxy.wait()))
        worker.start()
        try:
            assert resource.entered.wait(5)
            pending = cc.shutdown(timeout=0)
            assert not pending['completed'], pending
            assert _ProcessRegistry.get()._runtime_session is session
            assert _ProcessRegistry.get().names == ['blocking']
            assert cc.local_endpoint_context() == context
            assert resource.hooks == 0
        finally:
            resource.release.set()
            worker.join(5)
            assert not worker.is_alive()
            assert cc.shutdown(timeout=5)['completed']
        assert results == ['drained']
        assert resource.hooks == 1
        assert cc.local_endpoint_context() == context
        assert _ProcessRegistry.get()._runtime_session is not session
        cc.close(proxy)
    ''')
