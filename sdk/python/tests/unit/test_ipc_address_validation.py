import sys

import pytest

from c_two.transport.client import util


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

    def fake_socket_path(address: str) -> str:
        calls.append(address)
        return '/tmp/native.sock'

    import c_two._native as native

    monkeypatch.setattr(native, 'ipc_endpoint_name', fake_socket_path)

    assert util._endpoint_name_from_address('ipc://unit-server') == '/tmp/native.sock'
    assert calls == ['ipc://unit-server']


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
        util.ping('ipc://unit-server', timeout=timeout)


@pytest.mark.parametrize('timeout', [-1.0, float('nan'), float('inf')])
def test_shutdown_rejects_invalid_timeout(timeout):
    with pytest.raises(ValueError, match='timeout'):
        util.shutdown('ipc://unit-server', timeout=timeout)
