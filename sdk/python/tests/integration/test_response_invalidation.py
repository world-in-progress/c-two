"""Bound callback/GIL deadlocks to a child process, not the pytest runner."""

import subprocess
import sys
import textwrap

import pytest


@pytest.mark.parametrize('probe', ['memoryview', 'len', 'bytes'])
@pytest.mark.parametrize('fail_invalidator', [False, True])
def test_response_invalidator_releases_state_lock_before_python_callback(
    tmp_path, probe, fail_invalidator,
):
    code = textwrap.dedent(r'''
        import os
        import pickle
        import sys
        import threading
        import time
        import c_two as cc

        os.environ.pop('C2_RELAY_ANCHOR_ADDRESS', None)

        @cc.crm(namespace='test.response_invalidation', version='0.1.0')
        class Echo:
            def echo(self, value: bytes) -> bytes: ...

        class Resource:
            def echo(self, value):
                return value

        runtime = cc.Runtime()
        if os.name != 'nt':
            runtime.set_local_endpoint(root=sys.argv[1])
        proxy = None
        try:
            runtime.register(Echo, Resource(), name='echo')
            proxy = runtime.connect(Echo, name='echo', address=runtime.server_address())
            prepared = proxy.client.begin_call('echo')
            try:
                response = prepared.call(pickle.dumps((b'held',), protocol=4))
            finally:
                prepared.close()
            assert pickle.loads(bytes(response)) == b'held'
            tracker = proxy.client.local_copy_tracker
            response.track_retained(tracker, 'echo', 'echo')
            assert tracker.stats()['active_holds'] == 1
            entered = threading.Event()
            probing = threading.Event()
            calls = []
            errors = []
            failure = RuntimeError('original invalidator error')

            def invalidate(value):
                calls.append(value)
                print('invalidator entered', flush=True)
                entered.set()
                assert probing.wait(3)
                # Give the other thread the GIL before invalidation returns.
                time.sleep(0.1)
                if sys.argv[3] == 'True':
                    raise failure

            def release():
                try:
                    response.invalidate_then_release(invalidate, b'held')
                except BaseException as error:
                    errors.append(error)

            worker = threading.Thread(target=release)
            worker.start()
            assert entered.wait(3)
            probing.set()
            print('probing ' + sys.argv[2], flush=True)
            operation = {'memoryview': memoryview, 'len': len, 'bytes': bytes}[sys.argv[2]]
            try:
                operation(response)
            except (BufferError, ValueError) as error:
                expected = BufferError if sys.argv[2] == 'memoryview' else ValueError
                assert isinstance(error, expected)
                assert 'already released' in str(error)
            else:
                raise AssertionError('response remained accessible after invalidation began')
            worker.join(3)
            assert not worker.is_alive()
            assert errors == ([failure] if sys.argv[3] == 'True' else [])
            assert calls == [b'held']
            assert response.is_released
            assert tracker.stats()['active_holds'] == 0
            response.invalidate_then_release(lambda value: calls.append(value), b'again')
            response.release()
            assert calls == [b'held']
            print('completed', flush=True)
        finally:
            if proxy is not None:
                cc.close(proxy)
            assert runtime.shutdown(timeout=3)['completed']
    ''')
    try:
        result = subprocess.run(
            [sys.executable, '-c', code, str(tmp_path), probe, str(fail_invalidator)],
            capture_output=True, text=True, timeout=6,
        )
    except subprocess.TimeoutExpired as error:
        pytest.fail(
            f'response invalidation child timed out: '
            f'stdout={error.stdout!r}; stderr={error.stderr!r}',
        )
    assert result.returncode == 0, result.stdout + result.stderr
    assert 'completed' in result.stdout
