"""Pure endpoint projections; environment experiments run in spawned interpreters.

Unix root cases are explicitly Unix-only. Common default, opacity, invalid
argument and native lifecycle projection cases also execute on Windows.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import textwrap
import uuid

import pytest

import c_two as cc
from c_two import _native
from c_two.transport.client import util

IS_WINDOWS = sys.platform == 'win32'
UNIX_ONLY = pytest.mark.skipif(IS_WINDOWS, reason='Unix filesystem root capability')
REPO = Path(__file__).resolve().parents[4]


def _isolated_env(**updates: str) -> dict[str, str]:
    env = os.environ.copy()
    env.pop('C2_IPC_ROOT', None)
    env.pop('C2_RELAY_ANCHOR_ADDRESS', None)
    env['C2_ENV_FILE'] = ''
    env['PYTHONPATH'] = os.pathsep.join(
        [str(REPO / 'sdk/python/src'), env.get('PYTHONPATH', '')],
    ).strip(os.pathsep)
    env.update(updates)
    return env


def _run_isolated(source: str, **env_updates: str) -> subprocess.CompletedProcess[str]:
    # Popen/run starts a fresh interpreter on every OS; no fork assumptions or
    # process-wide monkeypatches can race with another test's native resolver.
    result = subprocess.run(
        [sys.executable, '-c', textwrap.dedent(source)],
        env=_isolated_env(**env_updates), capture_output=True, text=True, timeout=20,
    )
    assert result.returncode == 0, f'stdout:\n{result.stdout}\nstderr:\n{result.stderr}'
    return result


def _unused_root() -> str:
    return f'/tmp/q-{uuid.uuid4().hex[:8]}'


def test_common_default_is_opaque_native_and_query_does_not_freeze() -> None:
    _run_isolated('''
        import sys
        import c_two as cc
        from c_two import _native
        session = _native.RuntimeSession(use_process_relay_anchor=False)
        context = session.local_endpoint_context()
        assert isinstance(context, cc.LocalEndpointContext)
        assert len(context.namespace_id) == 64
        assert not session.local_endpoint_frozen
        assert not session.client_config_frozen
        assert session.server_id is None
        assert not session.host_started
        name = session.local_endpoint('ipc://context-default')
        assert name == context.endpoint_name('ipc://context-default')
        if sys.platform == 'win32':
            assert context.platform == 'windows'
            assert context.root is None
            assert context.layout == 'named-pipe.v1'
            assert name.startswith('\\\\\\\\.\\\\pipe\\\\c_two-')
        else:
            assert context.platform == 'unix'
            assert context.root == '/tmp'
            assert context.layout == 'v2.2'
        try:
            cc.LocalEndpointContext()
        except TypeError:
            pass
        else:
            raise AssertionError('Python assembled a native context')
        try:
            context.root = 'forged'
        except AttributeError:
            pass
        else:
            raise AssertionError('Python mutated native identity')
        assert not session.local_endpoint_frozen
    ''')


@UNIX_ONLY
def test_query_validates_without_creating_root_or_runtime_domains() -> None:
    root = _unused_root()
    session = _native.RuntimeSession(use_process_relay_anchor=False)
    session.set_local_endpoint(root=root)
    context = session.local_endpoint_context()
    assert context.root == root
    assert not Path(root).exists()
    assert session.local_endpoint('ipc://query-only') == context.endpoint_name('ipc://query-only')
    assert not Path(root).exists()
    assert not session.local_endpoint_frozen
    assert not session.client_config_frozen
    assert session.server_id is None
    assert not session.host_started
    # A pure query remains mutable and never silently pins process discovery.
    session.set_local_endpoint(root=root + ' ')
    assert session.local_endpoint_context().root == root + ' '


@UNIX_ONLY
def test_public_code_override_wins_env_and_dotenv_and_survives_session_replacement(tmp_path: Path) -> None:
    code, env, file = (_unused_root() for _ in range(3))
    dotenv = tmp_path / 'endpoint.env'
    dotenv.write_text(f'C2_IPC_ROOT={file}\n', encoding='utf-8')
    _run_isolated(f'''
        import os
        import c_two as cc
        from c_two.transport.registry import _ProcessRegistry
        assert cc.local_endpoint_context().root == {env!r}
        cc.set_local_endpoint(root={code!r})
        captured = cc.local_endpoint_context()
        assert captured.root == {code!r}
        old = _ProcessRegistry.get()._runtime_session
        assert not old.local_endpoint_frozen
        os.environ['C2_IPC_ROOT'] = {file!r}
        assert cc.local_endpoint_context() == captured
        assert cc.shutdown()['completed']
        new = _ProcessRegistry.get()._runtime_session
        assert new is not old
        assert not new.local_endpoint_frozen
        assert cc.local_endpoint_context() == captured
        cc.set_transport_policy(shm_threshold=8192)
        assert cc.local_endpoint_context() == captured
        cc.set_local_endpoint(root=None)
        assert cc.local_endpoint_context().root == {file!r}
        assert cc.shutdown()['completed']
    ''', C2_IPC_ROOT=env, C2_ENV_FILE=str(dotenv))


@UNIX_ONLY
def test_env_over_dotenv_and_captured_context_survives_env_flip(tmp_path: Path) -> None:
    env, file, next_env = (_unused_root() for _ in range(3))
    dotenv = tmp_path / 'endpoint.env'
    dotenv.write_text(f'C2_IPC_ROOT={file}\n', encoding='utf-8')
    _run_isolated(f'''
        import os
        from c_two import _native
        session = _native.RuntimeSession(use_process_relay_anchor=False)
        captured = session.local_endpoint_context()
        assert captured.root == {env!r}
        original_name = captured.endpoint_name('ipc://same-name')
        os.environ['C2_IPC_ROOT'] = {next_env!r}
        assert captured.root == {env!r}
        assert captured.endpoint_name('ipc://same-name') == original_name
        current = session.local_endpoint_context()
        assert current.root == {next_env!r}
        assert current.namespace_id != captured.namespace_id
        assert not session.local_endpoint_frozen
        del os.environ['C2_IPC_ROOT']
        assert session.local_endpoint_context().root == {file!r}
    ''', C2_IPC_ROOT=env, C2_ENV_FILE=str(dotenv))


@UNIX_ONLY
@pytest.mark.parametrize('root', ['', 'relative', ' /tmp/leading', '/tmp/a/../b', '/tmp/a\0b'])
def test_root_validation_is_native(root: str) -> None:
    session = _native.RuntimeSession(use_process_relay_anchor=False)
    with pytest.raises(ValueError):
        session.set_local_endpoint(root=root)
    with pytest.raises(ValueError):
        _native.resolve_local_endpoint_context(root=root)
    assert not session.local_endpoint_frozen


@UNIX_ONLY
def test_space_in_root_is_not_trimmed() -> None:
    root = _unused_root() + ' '
    _run_isolated(f'''
        import c_two as cc
        from c_two import _native
        assert cc.local_endpoint_context().root == {root!r}
        cc.set_local_endpoint(root={root!r})
        assert cc.local_endpoint_context().root == {root!r}
        without_space = _native.resolve_local_endpoint_context(root={root.rstrip()!r})
        assert without_space.namespace_id != cc.local_endpoint_context().namespace_id
    ''', C2_IPC_ROOT=root)


@UNIX_ONLY
def test_native_context_handoff_ignores_new_process_selection() -> None:
    first, second = _unused_root(), _unused_root()
    _run_isolated(f'''
        import os
        from c_two import _native
        old = _native.RuntimeSession(use_process_relay_anchor=False)
        captured = old.local_endpoint_context()
        os.environ['C2_IPC_ROOT'] = {second!r}
        replacement = _native.RuntimeSession(use_process_relay_anchor=False)
        assert replacement.local_endpoint_context().root == {second!r}
        replacement.set_local_endpoint(context=captured)
        assert replacement.local_endpoint_context() == captured
        assert not replacement.local_endpoint_frozen
        assert not replacement.client_config_frozen
        assert old.local_endpoint_context().root == {second!r}
    ''', C2_IPC_ROOT=first)


@UNIX_ONLY
def test_admin_name_defaults_to_runtime_code_context_without_freezing() -> None:
    root, other = _unused_root(), _unused_root()
    _run_isolated(f'''
        import os
        import c_two as cc
        from c_two import _native
        from c_two.transport.client import util
        from c_two.transport.registry import _ProcessRegistry
        cc.set_local_endpoint(root={root!r})
        context = cc.local_endpoint_context()
        session = _ProcessRegistry.get()._runtime_session
        address = 'ipc://admin-name-context'
        os.environ['C2_IPC_ROOT'] = {other!r}
        assert util._endpoint_name_from_address(address) == context.endpoint_name(address)
        independent = cc.local_endpoint_context(root={other!r})
        assert independent != context
        assert util._endpoint_name_from_address(address, root={other!r}) == independent.endpoint_name(address)
        assert util._endpoint_name_from_address(address, context=independent) == independent.endpoint_name(address)
        assert _native.ipc_endpoint_name(address, context=context) == context.endpoint_name(address)
        assert not session.local_endpoint_frozen
        assert not session.client_config_frozen
        assert cc.local_endpoint_context() == context
        assert cc.shutdown()['completed']
    ''', C2_IPC_ROOT=other)


@pytest.mark.parametrize('root', [False, 1, b'/tmp/root', Path('/tmp/root'), {}])
def test_public_facades_validate_shapes_without_mirroring_root_rules(root: object) -> None:
    with pytest.raises(TypeError, match='root'):
        cc.set_local_endpoint(root=root)
    with pytest.raises(TypeError, match='root'):
        cc.local_endpoint_context(root=root)
    with pytest.raises(TypeError, match='root'):
        cc.inspect_endpoint('ipc://shape', root=root)
    with pytest.raises(TypeError, match='root'):
        util.ping('ipc://shape', root=root)


def test_context_shape_and_conflicting_selection_reject_before_io() -> None:
    context = _native.RuntimeSession(use_process_relay_anchor=False).local_endpoint_context()
    with pytest.raises(TypeError, match='context'):
        cc.inspect_endpoint('ipc://shape', context={})
    with pytest.raises(TypeError, match='mutually exclusive'):
        cc.sweep_endpoints(root='/tmp', context=context)
    with pytest.raises(ValueError, match='mutually exclusive'):
        _native.PyEndpointSweep(root='/tmp', context=context)
    with pytest.raises(ValueError, match='mutually exclusive'):
        _native.RuntimeSession().set_local_endpoint(root='/tmp', context=context)


def test_timeout_rejection_is_native_and_does_not_freeze_context() -> None:
    session = _native.RuntimeSession(use_process_relay_anchor=False)
    for value in (-1.0, float('nan'), float('inf'), 1e300):
        with pytest.raises(ValueError, match='timeout'):
            session.ping_direct_ipc('ipc://timeout-pure', value)
        with pytest.raises(ValueError, match='timeout'):
            session.shutdown_direct_ipc('ipc://timeout-pure', value)
        with pytest.raises(ValueError, match='timeout'):
            _native.ipc_ping('ipc://timeout-pure', value)
        with pytest.raises(ValueError, match='timeout'):
            _native.ipc_shutdown('ipc://timeout-pure', value)
    assert not session.local_endpoint_frozen


@UNIX_ONLY
def test_public_replacement_preserves_root_and_native_retirement_observations() -> None:
    root = _unused_root()
    _run_isolated(f'''
        import c_two as cc
        from c_two.transport.registry import _ProcessRegistry
        cc.set_local_endpoint(root={root!r})
        old = _ProcessRegistry.get()._runtime_session
        tracker = old.lease_tracker()
        lease = tracker.track_retained(route_name='endpoint-retirement', method_name='echo',
            direction='client_response', storage='inline', bytes=64)
        captured = cc.local_endpoint_context()
        for _ in range(3):
            assert cc.shutdown()['completed']
            assert cc.local_endpoint_context() == captured
            assert cc.hold_stats()['active_holds'] == 1
            assert cc.memory_stats()['holds']['total_held_bytes'] == 64
        lease.release()
        assert cc.hold_stats()['active_holds'] == 0
        # An old producer remains observable after the endpoint handoff.
        late = tracker.track_retained(route_name='endpoint-retirement', method_name='echo',
            direction='client_response', storage='inline', bytes=96)
        assert cc.hold_stats()['active_holds'] == 1
        assert cc.memory_stats()['holds']['total_held_bytes'] == 96
        late.release()
        assert cc.hold_stats()['active_holds'] == 0
        assert cc.local_endpoint_context() == captured
    ''')


@UNIX_ONLY
def test_schema2_credential_retains_historical_context_under_new_environment() -> None:
    document = json.dumps({
        'schemaVersion': 2, 'address': 'ipc://historical-context',
        'protocol': 'managed-v2', 'platform': 'unix',
        'incarnation': '00112233445566778899aabbccddeeff',
        'device': 1, 'inode': 2, 'changedSecs': 3, 'changedNanos': 4,
    })
    root = _unused_root()
    _run_isolated(f'''
        import c_two as cc
        from c_two import _native
        credential = cc.EndpointCredential.from_json({document!r})
        assert cc.local_endpoint_context().root == {root!r}
        assert credential.context.root == '/tmp'
        assert credential.to_json() == cc.EndpointCredential.from_json(credential.to_json()).to_json()
        for result in (
            cc.reap_endpoint(credential.address, credential, root={root!r}),
            cc.reap_endpoint(credential.address, credential, context=cc.local_endpoint_context()),
            _native.reap_endpoint_credential(credential.address, credential._native, root={root!r}),
        ):
            assert result['status'] == 'stale-target', result
            assert result['reason'] == 'credential-context-mismatch', result
    ''', C2_IPC_ROOT=root)


@pytest.mark.skipif(not IS_WINDOWS, reason='Windows named-pipe platform contract')
def test_windows_root_override_is_explicitly_unsupported_and_preserves_default() -> None:
    _run_isolated('''
        import c_two as cc
        from c_two import _native
        from c_two.transport.client import util
        context = cc.local_endpoint_context()
        session = _native.RuntimeSession(use_process_relay_anchor=False)
        for operation in (
            lambda: cc.set_local_endpoint(root='C:\\\\tmp'),
            lambda: cc.local_endpoint_context(root='C:\\\\tmp'),
            lambda: session.set_local_endpoint(root='C:\\\\tmp'),
            lambda: _native.ipc_endpoint_name('ipc://windows-root', root='C:\\\\tmp'),
            lambda: util.ping('ipc://windows-root', root='C:\\\\tmp'),
            lambda: util.shutdown('ipc://windows-root', root='C:\\\\tmp'),
            lambda: cc.inspect_endpoint('ipc://windows-root', root='C:\\\\tmp'),
            lambda: cc.sweep_endpoints(addresses=[], root='C:\\\\tmp'),
        ):
            try:
                operation()
            except ValueError as exc:
                assert 'not applicable' in str(exc), exc
            else:
                raise AssertionError('Windows silently ignored a Unix root')
        assert cc.local_endpoint_context() == context
        assert session.local_endpoint_context() == context
        assert not session.local_endpoint_frozen
        assert cc.shutdown()['completed']
        assert cc.local_endpoint_context() == context
    ''')
