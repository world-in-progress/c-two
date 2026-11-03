"""Caller startup budgets through the real Python registration boundary."""
from __future__ import annotations

import json
import sys
import tempfile

import pytest

from c_two import _native
from tests.integration.test_local_io_cause import COMMON
from tests.unit.test_endpoint_context import _run_isolated


def test_register_zero_startup_budget_has_structured_timeout() -> None:
    with tempfile.TemporaryDirectory(prefix='c2-start-', dir=None if sys.platform == 'win32' else '/tmp') as root:
        result = _run_isolated(COMMON + f'''
from pathlib import Path
root = Path({root!r})
if {sys.platform!r} != 'win32':
    cc.endpoint.set_local_endpoint(root=str(root))
error = None
try:
    cc.set_server(startup_timeout=0)
    cc.register(Echo, Resource(), name='startup')
except cc.error.CoreError as exc:
    error = diagnostic(exc)
finally:
    stopped = cc.shutdown(timeout=3)
assert all(not entry.is_socket() and not entry.name.endswith('.lease') for entry in root.rglob('*'))
print(json.dumps({{'error': error, 'completed': stopped['completed']}}))
''')
    report = json.loads(result.stdout)
    assert report['completed'], report
    error = report['error']
    assert error is not None, report
    assert (error['category'], error['lifecycle_kind']) == ('lifecycle', 'startup_timeout')
    assert error['timeout_seconds'] == 0
    assert error['startup_stage'] in ('readiness', 'responsiveness')
    assert 'config error' not in error['message']
    assert all(error[key] is None for key in ('io_operation', 'io_kind', 'raw_os_error'))


@pytest.mark.parametrize('invalid', [-1.0, float('nan'), float('inf'), 1e300, float(2**64)])
def test_invalid_startup_timeout_does_not_mutate_configuration(invalid: float) -> None:
    with pytest.raises(ValueError):
        _native.RuntimeSession(startup_timeout=invalid, use_process_relay_anchor=False)
    session = _native.RuntimeSession(server_id='startup-config', startup_timeout=0.25, use_process_relay_anchor=False)
    try:
        with pytest.raises(ValueError):
            session.set_server_options('changed', None, invalid)
        assert session.server_id_override == 'startup-config'
        assert session.startup_timeout == 0.25
        assert session.server_id is None
        session.set_server_options(startup_timeout=None)
        assert session.startup_timeout is None
    finally:
        assert session.shutdown()['completed']


def test_startup_budget_survives_session_replacement_and_failed_register() -> None:
    with tempfile.TemporaryDirectory(prefix='c2-start-', dir=None if sys.platform == 'win32' else '/tmp') as root:
        _run_isolated(COMMON + f'''
from pathlib import Path
from c_two.transport.registry import Runtime, _get_default_runtime
root = Path({root!r})
if {sys.platform!r} != 'win32':
    cc.endpoint.set_local_endpoint(root=str(root))
try:
    cc.set_server(startup_timeout=0)
    old = _get_default_runtime()._runtime_session
    cc.set_transport_policy(shm_threshold=128)
    current = _get_default_runtime()._runtime_session
    assert current is not old and current.startup_timeout == 0
    try:
        cc.register(Echo, Resource(), name='startup')
    except cc.error.CoreError as error:
        assert error.lifecycle_kind == 'startup_timeout'
    else:
        raise AssertionError('zero startup budget unexpectedly registered a route')
    assert not current.host_started
    assert cc.server_address() is None
    assert current.ensure_server_bridge().names == []
    try:
        current.set_server_options(startup_timeout=30)
    except cc.error.CoreError as error:
        assert error.lifecycle_kind == 'config_frozen'
    else:
        raise AssertionError('failed startup unfroze server configuration')
finally:
    assert cc.shutdown(timeout=3)['completed']
# A full shutdown creates a fresh session that can choose another budget.
if {sys.platform!r} != 'win32':
    cc.endpoint.set_local_endpoint(root=str(root))
try:
    cc.set_server(startup_timeout=30)
    cc.register(Echo, Resource(), name='startup')
    running = _get_default_runtime()._runtime_session
    try:
        running.set_server_options('bad/id', {{'unknown': True}}, float('nan'))
    except RuntimeError as error:
        assert 'frozen' in str(error)
    else:
        raise AssertionError('running configuration was reparsed or changed')
    with cc.connect(Echo, name='startup', address=cc.server_address()) as proxy:
        assert proxy.echo('ready') == 'ready'
finally:
    assert cc.shutdown(timeout=3)['completed']
assert all(not entry.is_socket() and not entry.name.endswith('.lease') for entry in root.rglob('*'))
''')


def test_invalid_ipc_configuration_precedes_zero_startup_budget() -> None:
    _run_isolated(COMMON + '''
try:
    cc.set_server(ipc_overrides={'pool_segment_size': 0}, startup_timeout=0)
    try:
        cc.register(Echo, Resource(), name='startup')
    except ValueError:
        pass
    else:
        raise AssertionError('invalid IPC configuration was not rejected first')
    assert cc.server_address() is None
finally:
    assert cc.shutdown(timeout=3)['completed']
''')
