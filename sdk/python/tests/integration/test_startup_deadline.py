"""Caller startup budgets through the real Python registration boundary."""
from __future__ import annotations

import json
import sys
import tempfile

from tests.integration.test_local_io_cause import COMMON
from tests.unit.test_endpoint_context import _run_isolated


def test_register_zero_startup_budget_has_structured_timeout() -> None:
    with tempfile.TemporaryDirectory(prefix='c2-start-', dir=None if sys.platform == 'win32' else '/tmp') as root:
        result = _run_isolated(COMMON + f'''
from pathlib import Path
root = Path({root!r})
if {sys.platform!r} != 'win32':
    cc.set_local_endpoint(root=str(root))
error = None
try:
    cc.set_server(startup_timeout=0)
    cc.register(Echo, Resource(), name='startup')
except _native.CoreError as exc:
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
    assert 'config error' not in error['message']
    assert all(error[key] is None for key in ('io_operation', 'io_kind', 'raw_os_error'))
