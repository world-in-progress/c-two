import sys
import uuid

import pytest

from c_two.transport.client import util


def _absent_address() -> str:
    """A per-test logical address no live server can own.

    The admin probes here are real calls: a fixed, guessable name could address
    a developer's running server and ``shutdown`` would stop it. A fresh UUID
    keeps every probe harmless and its outcome deterministic.
    """
    return f'ipc://unused-absent-{uuid.uuid4().hex}'


@pytest.mark.parametrize(
    'address',
    [
        'ipc://../escape',
        'ipc://bad/name',
        'ipc://bad\\name',
        'ipc://.',
        'ipc://..',
        'ipc:// leading',
        'ipc://trailing ',
        'ipc://bad\nname',
        'tcp://not-ipc',
    ],
)
def test_client_util_rejects_path_like_ipc_region(address):
    with pytest.raises(ValueError):
        util._endpoint_name_from_address(address)


def test_client_util_accepts_plain_ipc_region():
    endpoint = util._endpoint_name_from_address('ipc://unit-server')
    if sys.platform == 'win32':
        assert endpoint.startswith('\\\\.\\pipe\\c_two-')
    else:
        assert endpoint == '/tmp/c_two_ipc/unit-server.sock'


def test_client_util_uses_native_endpoint_name(monkeypatch):
    calls = []

    def fake_socket_path(address: str, protocol=None) -> str:
        calls.append((address, protocol))
        return '/tmp/native.sock'

    import c_two._native as native

    monkeypatch.setattr(native, 'ipc_endpoint_name', fake_socket_path)

    assert util._endpoint_name_from_address('ipc://unit-server') == '/tmp/native.sock'
    assert calls == [('ipc://unit-server', None)]


def test_client_util_forwards_the_explicit_endpoint_protocol(monkeypatch):
    calls = []

    def fake_socket_path(address: str, protocol=None) -> str:
        calls.append((address, protocol))
        return '/tmp/native.sock'

    import c_two._native as native

    monkeypatch.setattr(native, 'ipc_endpoint_name', fake_socket_path)

    assert (
        util._endpoint_name_from_address('ipc://unit-server', endpoint_protocol='managed-v2')
        == '/tmp/native.sock'
    )
    assert calls == [('ipc://unit-server', 'managed-v2')]


def test_client_util_keeps_the_historical_call_shapes():
    """The optional protocol arguments must not break existing callers.

    Both probes address a fresh UUID region, so the historical one-argument and
    two-argument shapes stay exercisable without ever touching a live server.
    """
    assert util.ping(_absent_address(), 0.01) is False
    assert util.shutdown(_absent_address(), 0.01) == {
        'acknowledged': True,
        'shutdown_started': False,
        'server_stopped': True,
        'route_outcomes': [],
    }


def test_client_util_ping_and_shutdown_accept_only_canonical_protocols():
    """A rejected protocol is caller input, never an "absent server" answer.

    The probe helpers historically flatten a native ``ValueError`` into a
    negative result, which would report a non-canonical protocol as a missing
    server. An explicit protocol must surface the native rejection while an
    omitted protocol and an invalid address keep their historical shapes.
    """
    with pytest.raises(ValueError):
        util.ping(_absent_address(), timeout=0.01, endpoint_protocol='MANAGED-V2')
    with pytest.raises(ValueError):
        util.shutdown(_absent_address(), timeout=0.01, endpoint_protocol='future-v3')
    # An omitted protocol still resolves the process policy and still reports
    # absence rather than raising.
    assert util.ping(_absent_address(), 0.01) is False


def test_ping_invalid_address_returns_false():
    assert util.ping('tcp://not-ipc') is False


def test_shutdown_invalid_address_returns_false():
    assert util.shutdown('tcp://not-ipc') == {
        'acknowledged': False,
        'shutdown_started': False,
        'server_stopped': False,
        'route_outcomes': [],
    }


@pytest.mark.parametrize('timeout', [-1.0, float('nan'), float('inf')])
def test_ping_rejects_invalid_timeout(timeout):
    with pytest.raises(ValueError, match='timeout'):
        util.ping(_absent_address(), timeout=timeout)


@pytest.mark.parametrize('timeout', [-1.0, float('nan'), float('inf')])
def test_shutdown_rejects_invalid_timeout(timeout):
    with pytest.raises(ValueError, match='timeout'):
        util.shutdown(_absent_address(), timeout=timeout)
