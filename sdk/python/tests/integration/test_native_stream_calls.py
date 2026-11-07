"""Pickle calls exercise native preparation without portable-payload fixtures."""

from collections.abc import Iterator
import os

import pytest

import c_two as cc


@cc.crm(namespace='test.native_stream_calls', version='0.1.0')
class Values:
    @cc.stream
    def values(self, value: bytes) -> Iterator[bytes]: ...

    def echo(self, value: bytes) -> bytes: ...


class Resource:
    def __init__(self):
        self.cleanups = 0

    def values(self, value):
        try:
            yield value
            yield value + b'!'
        finally:
            self.cleanups += 1

    def echo(self, value):
        return value


@pytest.mark.parametrize('transport', ['ipc', 'http'])
@pytest.mark.parametrize('finite', [False, True])
def test_native_stream_then_unary_preserves_input_and_terminal(
    monkeypatch, tmp_path, start_c3_relay, transport, finite,
):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    runtime = cc.Runtime()
    if transport == 'ipc' and os.name != 'nt':
        runtime.set_local_endpoint(root=str(tmp_path))
    resource = Resource()
    proxy = view = stream = None
    try:
        relay = start_c3_relay() if transport == 'http' else None
        if relay is not None:
            runtime.set_relay_anchor(relay.url)
        runtime.register(Values, resource, name='values')
        address = runtime.server_address() if relay is None else relay.url
        proxy = runtime.connect(Values, name='values', address=address)
        assert proxy.client.observed_path == (
            'direct_ipc' if relay is None else 'explicit_relay'
        )
        view = cc.with_call_options(proxy, timeout=5) if finite else proxy
        stream = view.values(b'input')
        assert list(stream) == [b'input', b'input!']
        assert stream.close() == {
            'completed': True, 'error': None, 'complete_items': 2,
        }
        assert resource.cleanups == 1
        assert view.echo(b'after') == b'after'
        assert runtime.call_execution_snapshot()['used_operations'] == 0
    finally:
        if stream is not None:
            stream.close()
        if view is not None and view is not proxy:
            cc.close(view)
        if proxy is not None:
            cc.close(proxy)
        assert runtime.shutdown(timeout=5)['completed']
