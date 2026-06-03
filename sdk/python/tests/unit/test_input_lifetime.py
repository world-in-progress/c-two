import pytest

from c_two.crm.payload_plan import PayloadBinding, PayloadPlanKind, no_payload_binding
from c_two.crm.transferable import _build_transfer_wrapper
from c_two.transport.input_lifetime import InputLifetime
from c_two.transport.server.native import CRMSlot


def test_borrowed_dispatch_requires_fdb_payload_binding():
    class FakeCRM:
        def total(self, values):
            return values

    FakeCRM.total._input_payload_binding = PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=lambda *values: b'',
        deserialize=lambda data: (),
        view_from_buffer=lambda data: (),
        label='fake.python_pickle.with_view',
    )

    slot = CRMSlot(
        name='fake',
        crm_instance=FakeCRM(),
        direct_instance=object(),
        crm_ns='test.input_lifetime',
        crm_name='FakeCRM',
        crm_ver='0.1.0',
        abi_hash='0' * 64,
        signature_hash='1' * 64,
        method_table=object(),
        scheduler=object(),
        methods=['total'],
        input_lifetime={'total': InputLifetime.BORROWED},
    )

    with pytest.raises(ValueError, match='requires a buffer-view FDB input payload'):
        slot.build_dispatch_table()


def test_borrowed_transfer_wrapper_rejects_non_fdb_payload_binding():
    class Resource:
        def total(self, values):
            return None

    class Contract:
        direction = '<-'
        resource = Resource()

    def total(self, values):
        return None

    binding = PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=lambda *values: b'',
        deserialize=lambda data: (),
        view_from_buffer=lambda data: (),
        label='fake.python_pickle.with_view',
    )
    wrapped = _build_transfer_wrapper(
        total,
        input=binding,
        output=no_payload_binding(),
    )
    released = []

    err_bytes, payload = wrapped(
        Contract(),
        memoryview(b'payload'),
        _release_fn=lambda: released.append(True),
        _c2_input_buffer_mode='borrowed',
    )

    assert released == [True]
    assert payload == b''
    assert b'constructing resource input from buffer' in err_bytes
    assert b'buffer-view FDB input payload' in err_bytes
