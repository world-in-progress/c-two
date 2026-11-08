"""Method mode belongs to the registered dispatch entry."""

from types import SimpleNamespace

import pytest

from c_two.transport.server.native import CRMSlot, NativeServerBridge


@pytest.mark.parametrize('registered_stream', [False, True])
def test_dispatch_uses_registered_mode_after_method_metadata_changes(registered_stream):
    result = (b'', b'output')

    def method(data, *, _release_fn, _c2_input_buffer_mode):
        assert bytes(data) == b'input'
        assert _c2_input_buffer_mode == 'view'
        return result

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
    actual = dispatch(slot.name, 0, request)
    if registered_stream:
        assert actual is result
        assert not request.released
    else:
        assert actual == b'output'
        assert request.released
