"""Method mode belongs to the registered dispatch entry."""

from types import SimpleNamespace

import pytest

from c_two import CallContext
from c_two._call_context import _run_in_call_context
from c_two.transport.server.native import CRMSlot, NativeServerBridge


@pytest.mark.parametrize('registered_stream', [False, True])
def test_dispatch_uses_registered_mode_after_method_metadata_changes(registered_stream):
    stream_result = object()
    operation = 'server_stream' if registered_stream else 'unary'
    context = CallContext('registered-mode', 'echo', operation, None, {'trace': 'dispatch'})

    def method(
        data, *, _release_fn, _c2_input_buffer_mode, _detach_input,
        _response_sink=None, _c2_call_context=None,
    ):
        assert bytes(data) == b'input'
        assert _c2_input_buffer_mode == 'view'
        assert _c2_call_context is (context if registered_stream else None)
        return stream_result if registered_stream else b'output'

    method.__cc_stream__ = registered_stream
    slot = CRMSlot(
        name='registered-mode',
        crm_instance=SimpleNamespace(echo=method),
        direct_instance=object(),
        crm_ns='test.dispatch',
        crm_name='Echo',
        crm_ver='0.1.0',
        abi_hash='0' * 64,
        signature_hash='1' * 64,
        method_table=SimpleNamespace(_idx_to_name={0: 'echo'}),
        scheduler=None,
        methods=['echo'],
    )
    slot.build_dispatch_table()
    dispatch = NativeServerBridge._make_dispatcher(
        SimpleNamespace(_local_copy_tracker=None), slot.name, slot,
    )
    method.__cc_stream__ = not registered_stream

    class Request(bytearray):
        released = False

        def release(self):
            self.released = True

    request = Request(b'input')
    actual = _run_in_call_context(context, dispatch, (slot.name, 0, request))
    if registered_stream:
        # A stream's running step owns its input; the dispatcher hands the
        # wrapper's return through untouched and does not release for it.
        assert actual is stream_result
        assert not request.released
    else:
        assert actual == b'output'
        assert request.released


def test_dispatch_rejects_non_bytes_unary_returns():
    def method(data, *, _release_fn, _c2_input_buffer_mode, _detach_input, _response_sink=None):
        return ('not', 'serialized-bytes')

    slot = CRMSlot(
        name='non-bytes',
        crm_instance=SimpleNamespace(echo=method),
        direct_instance=object(),
        crm_ns='test.dispatch',
        crm_name='Echo',
        crm_ver='0.1.0',
        abi_hash='0' * 64,
        signature_hash='1' * 64,
        method_table=SimpleNamespace(_idx_to_name={0: 'echo'}),
        scheduler=None,
        methods=['echo'],
    )
    slot.build_dispatch_table()
    dispatch = NativeServerBridge._make_dispatcher(
        SimpleNamespace(_local_copy_tracker=None), slot.name, slot,
    )

    class Request(bytearray):
        released = False

        def release(self):
            self.released = True

    request = Request(b'input')
    with pytest.raises(TypeError, match='must return serialized bytes'):
        dispatch(slot.name, 0, request)
    assert request.released
