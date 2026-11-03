"""Real local context isolation and cleanup, with spawned Python participants.

Custom filesystem roots are Unix-only and deliberately short, pre-created
containers. Common Host lifecycle cases run on both Unix and
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

from tests.unit.test_endpoint_context import IS_WINDOWS, _isolated_env, _run_isolated


CONTRACT = '''
import c_two as cc

import c_two.endpoint
import c_two.observe

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
cc.endpoint.set_local_endpoint(root=root)
cc.set_server(server_id=address.removeprefix('ipc://'))
resource = Resource(label)
cc.register(Echo, resource, name='same-route')
context = cc.endpoint.local_endpoint_context()
credential = cc.endpoint.server_credential()
assert credential is not None
print(json.dumps({
    'address': cc.server_address(), 'root': context.root,
    'namespace_id': context.namespace_id,
    'credential': credential.to_json(),
}), flush=True)
assert sys.stdin.readline().strip() == 'stop'
outcome = cc.shutdown(timeout=5)
assert outcome['completed'], outcome
assert resource.hooks == 1
print(json.dumps({'completed': True, 'hooks': resource.hooks}), flush=True)
'''


@pytest.mark.platform('posix')
def test_server_credential_does_not_wait_for_the_namespace_gate() -> None:
    with tempfile.TemporaryDirectory(prefix='c2-own-', dir='/tmp') as root:
        _run_isolated(CONTRACT + textwrap.dedent('''
            import json
            from pathlib import Path
            import subprocess
            import sys
            from c_two.transport.registry import _get_default_runtime

            class Resource:
                def value(self): return 'ready'
                def done(self): pass

            assert cc.endpoint.server_credential() is None
            assert not _get_default_runtime()._runtime_session.local_endpoint_frozen
            holder = None
            try:
                cc.register(Echo, Resource(), name='own')
                address = cc.server_address()
                credential = cc.endpoint.server_credential()
                observed = cc.endpoint.inspect_endpoint(address)
                assert observed['status'] == 'present', observed
                assert credential.to_json() == observed['credential'].to_json()
                document = json.loads(credential.to_json())
                assert document['schemaVersion'] == 4 and 'device' not in document
                gate = Path(credential.context.root) / '.gate'
                holder = subprocess.Popen([
                    sys.executable, '-c',
                    "import fcntl,sys; f=open(sys.argv[1],'rb'); "
                    "fcntl.flock(f,fcntl.LOCK_EX); print('locked',flush=True); "
                    "assert sys.stdin.readline().strip() == 'release'", str(gate),
                ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                assert holder.stdout.readline().strip() == 'locked'
                held = cc.endpoint.inspect_endpoint(address)
                assert (held['status'], held['io_kind'], held['retryable']) == (
                    'io-error', 'WouldBlock', True,
                ), held
                # The holder cannot release until the own-listener query returns.
                assert cc.endpoint.server_credential().to_json() == credential.to_json()
                _, stderr = holder.communicate('release\\n', timeout=5)
                assert holder.returncode == 0, stderr
                assert cc.endpoint.reap_endpoint(address, credential)['status'] == 'busy'
                cc.unregister('own')
                assert cc.endpoint.server_credential().to_json() == credential.to_json()
            finally:
                if holder is not None and holder.poll() is None:
                    holder.kill()
                    holder.communicate(timeout=5)
                stopped = cc.shutdown(timeout=5)
            assert stopped['completed'], stopped
            assert cc.endpoint.server_credential() is None
            assert cc.endpoint.reap_endpoint(address, credential)['status'] == 'already-absent'
        '''), C2_IPC_ROOT=root)


@pytest.fixture
def short_roots():
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


@pytest.mark.platform('posix')
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
                cc.endpoint.set_local_endpoint(root={root!r})
                context = cc.endpoint.local_endpoint_context()
                assert util._endpoint_name_from_address({address!r}) == context.endpoint_name({address!r})
                assert util.ping({address!r})
                proxy = cc.connect(Echo, name='same-route', address={address!r})
                assert proxy.value() == {label!r}
                cc.close(proxy)
                assert util.ping({address!r})
                # An explicit independent context reaches the other namespace.
                other = cc.endpoint.local_endpoint_context(root={other!r})
                assert util.ping({address!r}, context=other)
                assert cc.endpoint.inspect_endpoint({address!r})['credential'].context == context
                assert cc.shutdown()['completed']
                assert cc.endpoint.local_endpoint_context() == context
            '''), C2_IPC_ROOT=other, C2_RELAY_ANCHOR_ADDRESS='http://127.0.0.1:1')
        a.stop()
        # Retirement in one namespace does not stop the identical name in the other.
        _run_isolated(CONTRACT + textwrap.dedent(f'''
            from c_two.transport.client import util
            cc.endpoint.set_local_endpoint(root={second!r})
            assert util.ping({address!r})
            proxy = cc.connect(Echo, name='same-route', address={address!r})
            assert proxy.value() == 'b'
            cc.close(proxy)
            ack = util.shutdown({address!r})
            assert ack == 'initiated', ack
            assert cc.shutdown()['completed']
        '''))
        b.stop()


@pytest.mark.platform('posix')
@pytest.mark.parametrize('attempt', ['bind', 'connect', 'ping', 'shutdown'])
def test_first_failed_local_attempt_freezes_context_without_creating_missing_container(attempt, short_roots) -> None:
    missing = f'/tmp/q-{uuid.uuid4().hex[:8]}'
    changed = short_roots[1]
    address = f'ipc://failed-{uuid.uuid4().hex[:12]}'
    _run_isolated(CONTRACT + textwrap.dedent(f'''
        import os
        from pathlib import Path
        from c_two import _native
        from c_two.transport.registry import _get_default_runtime
        from c_two.transport.client import util
        session = _get_default_runtime()._runtime_session
        captured = cc.endpoint.local_endpoint_context()
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
            except cc.error.CoreError as exc:
                assert exc.category == 'transport', exc
                assert exc.transport_kind == 'ipc', exc
                assert exc.transport_phase == 'pre_dispatch', exc
            else:
                raise AssertionError('absent resource connected')
        elif {attempt!r} == 'ping':
            assert not util.ping({address!r}, timeout=.01)
        else:
            stopped = util.shutdown({address!r}, timeout=.01)
            assert stopped == 'absent', stopped
        assert session.local_endpoint_frozen
        assert not Path({missing!r}).exists()
        os.environ['C2_IPC_ROOT'] = {changed!r}
        assert cc.endpoint.local_endpoint_context() == captured
        assert util._endpoint_name_from_address({address!r}) == captured.endpoint_name({address!r})
        cc.endpoint.set_local_endpoint(root={missing!r})
        session.set_local_endpoint(context=captured)
        try:
            cc.endpoint.set_local_endpoint(root={changed!r})
        except cc.error.CoreError as exc:
            assert exc.lifecycle_kind == 'config_frozen', exc
        else:
            raise AssertionError('failed local attempt left root mutable')
        assert cc.shutdown()['completed']
        replacement = _get_default_runtime()._runtime_session
        assert replacement is not session
        assert not replacement.local_endpoint_frozen
        assert cc.endpoint.local_endpoint_context() == captured
        cc.endpoint.set_local_endpoint(root={changed!r})
        assert cc.endpoint.local_endpoint_context().root == {changed!r}
    '''), C2_IPC_ROOT=missing)


@pytest.mark.platform('posix')
@pytest.mark.parametrize('source', ['environment', 'code'])
def test_used_selection_survives_repeated_shutdown_then_refreezes(source: str) -> None:
    missing = f'/tmp/q-{uuid.uuid4().hex[:8]}'
    changed = f'/tmp/q-{uuid.uuid4().hex[:8]}'
    _run_isolated(f'''
        import os
        from pathlib import Path
        import c_two as cc

        import c_two.endpoint
        import c_two.observe
        from c_two import _native
        from c_two.transport.client import util
        from c_two.transport.registry import _get_default_runtime
        if {source!r} == 'code':
            cc.endpoint.set_local_endpoint(root={missing!r})
        context = cc.endpoint.local_endpoint_context()
        assert not util.ping('ipc://failed-inheritance', .01)
        assert _get_default_runtime()._runtime_session.local_endpoint_frozen
        os.environ['C2_IPC_ROOT'] = {changed!r}
        for _ in range(2):
            assert cc.shutdown()['completed']
            assert not _get_default_runtime()._runtime_session.local_endpoint_frozen
            assert cc.endpoint.local_endpoint_context() == context
        assert not util.ping('ipc://failed-inheritance', .01)
        assert _get_default_runtime()._runtime_session.local_endpoint_frozen
        try:
            cc.endpoint.set_local_endpoint(root={changed!r})
        except cc.error.CoreError as error:
            assert error.lifecycle_kind == 'config_frozen', error
        else:
            raise AssertionError('inherited local selection did not freeze')
        assert not Path({missing!r}).exists()
        assert not Path({changed!r}).exists()
        assert cc.shutdown()['completed']
        cc.endpoint.set_local_endpoint(root={changed!r})
        assert cc.endpoint.local_endpoint_context().root == {changed!r}
    ''', C2_IPC_ROOT=missing)


@pytest.mark.platform('posix')
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

            import c_two.endpoint
            import c_two.observe
            from c_two import _native
            from pathlib import Path
            credential = cc.endpoint.EndpointCredential.from_json({document!r})
            assert credential.context.root == {root!r}
            assert cc.endpoint.local_endpoint_context().root == {other!r}
            assert cc.endpoint.reap_endpoint({address!r}, credential)['status'] == 'busy'
            def snapshot():
                return {{str(p): p.read_bytes() for p in Path({root!r}).rglob('*') if p.is_file()}}
            before = snapshot()
            assert before  # Real ownership/gate records, not fabricated state.
            for result in (
                cc.endpoint.reap_endpoint({address!r}, credential, root={other!r}),
                cc.endpoint.reap_endpoint({address!r}, credential, context=cc.endpoint.local_endpoint_context()),
                _native.reap_endpoint_credential({address!r}, credential, root={other!r}),
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

            import c_two.endpoint
            import c_two.observe
            credential = cc.endpoint.EndpointCredential.from_json({document!r})
            result = cc.endpoint.reap_endpoint({address!r}, credential)
            assert result['status'] in ('reaped', 'already-absent'), result
            assert cc.endpoint.inspect_endpoint({address!r}, context=credential.context)['status'] == 'absent'
            with cc.endpoint.sweep_endpoints(addresses=[{address!r}], context=credential.context) as sweep:
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


@pytest.mark.platform('posix')
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

            import c_two.endpoint
            import c_two.observe
            from c_two import _native
            credential = cc.endpoint.EndpointCredential.from_json({document!r})
            cc.endpoint.set_local_endpoint(root={root!r})
            with cc.endpoint.sweep_endpoints(addresses=[{address!r}], max_entries=1) as sweep:
                assert sweep.context == credential.context
                os.environ['C2_IPC_ROOT'] = {other!r}
                cc.endpoint.set_local_endpoint(root={other!r})
                assert cc.endpoint.local_endpoint_context() != sweep.context
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
            assert cc.endpoint.inspect_endpoint({address!r}, context=credential.context)['status'] == 'absent'
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


def test_common_active_incomplete_shutdown_retains_session_bindings_and_context() -> None:
    _run_isolated('''
        import threading
        import c_two as cc

        import c_two.endpoint
        import c_two.observe
        from c_two.transport.registry import _get_default_runtime
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
        session = _get_default_runtime()._runtime_session
        context = cc.endpoint.local_endpoint_context()
        worker = threading.Thread(target=lambda: results.append(proxy.wait()))
        worker.start()
        try:
            assert resource.entered.wait(5)
            pending = cc.shutdown(timeout=0)
            assert not pending['completed'], pending
            assert _get_default_runtime()._runtime_session is session
            assert _get_default_runtime().names == ['blocking']
            assert cc.endpoint.local_endpoint_context() == context
            assert resource.hooks == 0
        finally:
            resource.release.set()
            worker.join(5)
            assert not worker.is_alive()
            assert cc.shutdown(timeout=5)['completed']
        assert results == ['drained']
        assert resource.hooks == 1
        assert cc.endpoint.local_endpoint_context() == context
        assert _get_default_runtime()._runtime_session is not session
        cc.close(proxy)
    ''')
