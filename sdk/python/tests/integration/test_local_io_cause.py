"""OS causes through public register/connect calls, after participant cleanup.

All Unix roots and logical addresses belong to this test invocation. Windows
uses UUID pipe names, never filesystem endpoint roots. Old-handle restart is
already covered by c2-local's listener_restarts_after_server_closes_even_with_an_old_client_handle.
"""
from __future__ import annotations

import errno
import json
import os
from pathlib import Path
import tempfile
import textwrap
import uuid

import pytest

from tests.unit.test_endpoint_context import IS_WINDOWS, UNIX_ONLY, _run_isolated


COMMON = '''
import json
import c_two as cc
from c_two import _native

@cc.crm(namespace='test.local.io.cause', version='0.1.0')
class Echo:
    def echo(self, value: str) -> str: ...

class Resource:
    def echo(self, value): return value

def diagnostic(error):
    return {key: getattr(error, key, None) for key in (
        'category', 'lifecycle_kind', 'transport_kind', 'transport_phase',
        'fallback_eligible', 'io_kind', 'raw_os_error', 'io_operation', 'name', 'details', 'message',
        'startup_stage', 'timeout_seconds',
    )}
'''


def _assert_io(report: dict, operation: str, kind: str, raw: int | None) -> None:
    assert report['completed'], report
    error = report['error']
    assert error is not None, 'public operation unexpectedly succeeded'
    old = ('category', 'lifecycle_kind', 'transport_kind', 'transport_phase', 'fallback_eligible')
    expected = ('lifecycle', 'server', None, None, None) if operation == 'bind' else (
        'transport', None, 'ipc', 'pre_dispatch', True,
    )
    assert tuple(error[key] for key in old) == expected, error
    assert (error['io_kind'], error['raw_os_error'], error['io_operation']) == (
        kind, raw, operation,
    ), error
    assert 'config error' not in error['message'], error


@UNIX_ONLY
@pytest.mark.parametrize('case,operation,kind,raw', [
    ('unwritable', 'bind', 'permission_denied', errno.EACCES),
    ('not_directory', 'bind', 'other', errno.ENOTDIR),
    ('missing', 'connect', 'not_found', errno.ENOENT),
    ('refused', 'connect', 'connection_refused', errno.ECONNREFUSED),
    ('inaccessible', 'connect', 'permission_denied', errno.EACCES),
], ids=['unwritable', 'not_directory', 'missing', 'refused', 'inaccessible'])
def test_unix_local_io_cause(case, operation, kind, raw) -> None:
    if case in ('unwritable', 'inaccessible') and os.geteuid() == 0:
        pytest.skip('permission experiments require a non-root effective uid')
    address = f'ipc://io-{uuid.uuid4().hex}'
    with tempfile.TemporaryDirectory(prefix='c2-', dir='/tmp') as container:
        root = Path(container) / 'r'
        if case == 'not_directory':
            root.write_bytes(b'ordinary file')
        else:
            root.mkdir(mode=0o700)
        try:
            result = _run_isolated(COMMON + textwrap.dedent(f'''
                import socket
                from pathlib import Path
                root = Path({str(root)!r})
                address = {address!r}
                cc.set_local_endpoint(root=str(root))
                endpoint = Path(cc.local_endpoint_context().endpoint_name(address))
                raw_socket = client = error = None
                try:
                    if {case!r} in ('unwritable', 'inaccessible'):
                        root.chmod(0o500 if {case!r} == 'unwritable' else 0o000)
                    if {case!r} == 'refused':
                        raw_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                        raw_socket.bind(str(endpoint))  # Deliberately never listen().
                        endpoint.chmod(0o600)
                    if {operation!r} == 'bind':
                        cc.set_server(server_id=address.removeprefix('ipc://'))
                        cc.register(Echo, Resource(), name='io')
                    else:
                        client = cc.connect(Echo, name='io', address=address, timeout=2)
                except _native.CoreError as exc:
                    error = diagnostic(exc)
                finally:
                    if root.is_dir():
                        root.chmod(0o700)
                    try:
                        if client is not None:
                            cc.close(client)
                    finally:
                        try:
                            stopped = cc.shutdown(timeout=3)
                        finally:
                            if raw_socket is not None:
                                raw_socket.close()
                                endpoint.unlink(missing_ok=True)
                if root.is_dir():
                    assert all(not entry.is_socket() and not entry.name.endswith('.lease')
                               for entry in root.rglob('*')), 'socket/lease residue after shutdown'
                print(json.dumps({{'error': error, 'completed': stopped['completed']}}))
            '''))
        finally:
            # Also restore permissions if the interpreter exits early or is killed/waited by run().
            if root.is_dir():
                root.chmod(0o700)
    _assert_io(json.loads(result.stdout), operation, kind, raw)


WIN32 = '''
import ctypes
import time
from ctypes import wintypes as w

class SecurityAttributes(ctypes.Structure):
    _fields_ = [('length', w.DWORD), ('descriptor', w.LPVOID), ('inherit', w.BOOL)]

kernel = ctypes.WinDLL('kernel32', use_last_error=True)
advapi = ctypes.WinDLL('advapi32', use_last_error=True)
kernel.CreateNamedPipeW.argtypes = [w.LPCWSTR] + [w.DWORD] * 6 + [ctypes.POINTER(SecurityAttributes)]
kernel.CreateNamedPipeW.restype = w.HANDLE
kernel.CreateFileW.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, w.LPVOID, w.DWORD, w.DWORD, w.HANDLE]
kernel.CreateFileW.restype = w.HANDLE
kernel.CloseHandle.argtypes, kernel.CloseHandle.restype = [w.HANDLE], w.BOOL
kernel.LocalFree.argtypes, kernel.LocalFree.restype = [w.LPVOID], w.LPVOID
advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
    w.LPCWSTR, w.DWORD, ctypes.POINTER(w.LPVOID), ctypes.POINTER(w.DWORD),
]
advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = w.BOOL
invalid = ctypes.c_void_p(-1).value
'''


@pytest.mark.skipif(not IS_WINDOWS, reason='Windows named-pipe platform contract')
@pytest.mark.parametrize('case, timeout', [
    ('missing', 2), ('denied', 2), ('busy', 0.2), ('busy', None), ('busy', 10),
])
def test_windows_connect_cause_and_busy_deadline(case, timeout) -> None:
    result = _run_isolated(COMMON + WIN32 + textwrap.dedent(f'''
        address = 'ipc://io-' + {uuid.uuid4().hex!r}
        name = cc.local_endpoint_context().endpoint_name(address)
        descriptor = w.LPVOID()
        handles = []
        client = error = details = None
        try:
            attributes = None
            if {case!r} == 'denied':
                # Deny Everyone, including the current token: a real pipe DACL.
                # https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
                if not advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    'D:P(D;;GA;;;WD)', 1, ctypes.byref(descriptor), None,
                ):
                    raise ctypes.WinError(ctypes.get_last_error())
                security = SecurityAttributes(ctypes.sizeof(SecurityAttributes), descriptor, False)
                attributes = ctypes.byref(security)
            if {case!r} != 'missing':
                pipe = kernel.CreateNamedPipeW(name, 3, 0, 1, 4096, 4096, 0, attributes)
                if pipe == invalid:
                    raise ctypes.WinError(ctypes.get_last_error())
                handles.append(pipe)
            if {case!r} == 'busy':
                # Occupy the only instance; the next real open must report PIPE_BUSY.
                occupied = kernel.CreateFileW(name, 0xC0000000, 0, None, 3, 0, None)
                if occupied == invalid:
                    raise ctypes.WinError(ctypes.get_last_error())
                handles.append(occupied)
            probe = kernel.CreateFileW(name, 0xC0000000, 0, None, 3, 0, None)
            probe_code = ctypes.get_last_error() if probe == invalid else None
            if probe != invalid:
                handles.append(probe)
            started = time.monotonic()
            try:
                client = cc.connect(Echo, name='io', address=address,
                                    timeout={timeout!r})
            except _native.CoreError as exc:
                error = diagnostic(exc)
            except cc.error.CallDeadlineExceeded as exc:
                error, details = diagnostic(exc.__cause__), exc.details
            elapsed = time.monotonic() - started
        finally:
            try:
                if client is not None:
                    cc.close(client)
            finally:
                closed = all([bool(kernel.CloseHandle(handle)) for handle in reversed(handles)])
                freed = not descriptor.value or not kernel.LocalFree(descriptor)
                stopped = cc.shutdown(timeout=3)
        print(json.dumps({{'error': error, 'completed': stopped['completed'],
                          'closed': closed, 'freed': freed, 'probe': probe_code,
                          'elapsed': elapsed, 'details': details}}))
    '''))
    report = json.loads(result.stdout)  # run() has waited; every native handle is closed.
    assert report['closed'] and report['freed'] and report['completed'], report
    assert report['probe'] == {'missing': 2, 'denied': 5, 'busy': 231}[case], report
    if case != 'busy':
        _assert_io(report, 'connect', 'not_found' if case == 'missing' else 'permission_denied',
                   2 if case == 'missing' else 5)
    elif timeout in (None, 10):
        error = report['error']
        assert (error['category'], error['transport_kind'], error['transport_phase'],
                error['fallback_eligible']) == ('transport', 'ipc', 'pre_dispatch', True), report
        assert all(error[key] is None for key in ('io_kind', 'raw_os_error', 'io_operation')), report
        assert 4.5 <= report['elapsed'] < 8, report
    else:
        error = report['error']
        assert error['category'] == 'semantic' and error['name'] == 'CallDeadlineExceeded', report
        assert error['transport_phase'] == 'pre_dispatch' and error['fallback_eligible'] is False, report
        assert report['details']['operation'] == 'connect', report
        assert report['details']['stage'] == 'os_connect', report
        assert all(error[key] is None for key in ('io_kind', 'raw_os_error', 'io_operation')), report
        assert 0.15 <= report['elapsed'] < 2, report


@pytest.mark.skipif(not IS_WINDOWS, reason='Windows named-pipe platform contract')
def test_windows_real_listener_conflict_has_no_raw_system_code() -> None:
    address = f'ipc://io-{uuid.uuid4().hex}'
    contender = COMMON + textwrap.dedent(f'''
        error = None
        try:
            cc.set_server(server_id={address.removeprefix('ipc://')!r})
            cc.register(Echo, Resource(), name='io')
        except _native.CoreError as exc:
            error = diagnostic(exc)
        finally:
            stopped = cc.shutdown(timeout=3)
        print(json.dumps({{'error': error, 'completed': stopped['completed']}}))
    ''')
    result = _run_isolated(COMMON + textwrap.dedent(f'''
        import subprocess
        import sys
        try:
            cc.set_server(server_id={address.removeprefix('ipc://')!r})
            cc.register(Echo, Resource(), name='io')
            # Inherit the environment sanitized by _run_isolated/_isolated_env.
            child = subprocess.run([sys.executable, '-c', {contender!r}],
                                   capture_output=True, text=True, timeout=8)
        finally:
            stopped = cc.shutdown(timeout=3)
        print(json.dumps({{'returncode': child.returncode, 'stdout': child.stdout,
                          'stderr': child.stderr, 'completed': stopped['completed']}}))
    '''))
    owner = json.loads(result.stdout)  # Both participants have exited before diagnostics.
    assert owner['completed'] and owner['returncode'] == 0, owner
    _assert_io(json.loads(owner['stdout']), 'bind', 'address_in_use', None)
