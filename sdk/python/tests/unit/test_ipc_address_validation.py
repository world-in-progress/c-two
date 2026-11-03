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
        import re
        from pathlib import Path
        from c_two.transport.registry import _get_default_runtime

        context = _get_default_runtime()._runtime_session.local_endpoint_context()
        assert Path(endpoint).parent == Path(context.root)
        assert re.fullmatch(r'[0-9a-f]{32}', Path(endpoint).name)


def test_client_util_uses_native_endpoint_name(monkeypatch):
    import c_two._native as native
    from c_two.transport.registry import _get_default_runtime

    calls = []
    captured = native.resolve_local_endpoint_context()

    class SessionSpy:
        def local_endpoint_context(self) -> native.LocalEndpointContext:
            calls.append('local_endpoint_context')
            return captured

    def unexpected_process_resolution(*_args, **_kwargs):
        pytest.fail('util must use the current Runtime context')

    registry = _get_default_runtime()
    with monkeypatch.context() as patch:
        patch.setattr(registry, '_runtime_session', SessionSpy())
        patch.setattr(native, 'ipc_endpoint_name', unexpected_process_resolution)
        patch.setattr(native, 'resolve_local_endpoint_context', unexpected_process_resolution)

        assert util._endpoint_name_from_address('ipc://unit-server') == captured.endpoint_name(
            'ipc://unit-server',
        )
        assert calls == ['local_endpoint_context']


def test_client_util_keeps_the_historical_call_shapes():
    """Optional context selection preserves the positional address/timeout.

    Both probes address a fresh UUID region, so the two-argument shapes remain
    exercisable without ever touching a live server.
    """
    assert util.ping(_absent_address(), 0.01) is False
    assert util.shutdown(_absent_address(), 0.01) == 'absent'


def test_ping_invalid_address_raises():
    with pytest.raises(ValueError):
        util.ping('tcp://not-ipc')


def test_shutdown_invalid_address_raises():
    with pytest.raises(ValueError):
        util.shutdown('tcp://not-ipc')


@pytest.mark.parametrize('timeout', [-1.0, float('nan'), float('inf')])
def test_ping_rejects_invalid_timeout(timeout):
    with pytest.raises(ValueError, match='timeout'):
        util.ping(_absent_address(), timeout=timeout)


@pytest.mark.parametrize('timeout', [-1.0, float('nan'), float('inf')])
def test_shutdown_rejects_invalid_timeout(timeout):
    with pytest.raises(ValueError, match='timeout'):
        util.shutdown(_absent_address(), timeout=timeout)


@pytest.mark.parametrize('outcome', ['initiated', 'absent', 'unconfirmed'])
@pytest.mark.parametrize('selection', ['runtime', 'context'])
def test_shutdown_preserves_native_string_outcomes(monkeypatch, outcome, selection):
    from typing import Literal, get_type_hints
    from c_two import _native

    address = _absent_address()
    context = _native.resolve_local_endpoint_context()
    calls = []

    class Session:
        def shutdown_direct_ipc(self, target, timeout):
            calls.append((target, timeout, None))
            return outcome

    def native_shutdown(target, timeout, *, context):
        calls.append((target, timeout, context))
        return outcome

    monkeypatch.setattr(util, '_session', Session)
    monkeypatch.setattr(_native, 'ipc_shutdown', native_shutdown)
    kwargs = {'context': context} if selection == 'context' else {}
    result = util.shutdown(address, 0.25, **kwargs)
    assert type(result) is str
    assert result == outcome
    assert calls == [(address, 0.25, context if selection == 'context' else None)]
    assert get_type_hints(util.shutdown)['return'] == Literal['initiated', 'absent', 'unconfirmed']
