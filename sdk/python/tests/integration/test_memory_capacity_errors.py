"""Response admission keeps canonical capacity errors and never replays work."""
from __future__ import annotations

import pytest

import c_two as cc
from c_two.transport.client.util import ping


@cc.crm(namespace='test.memory-capacity-errors', version='0.1.0')
class Producer:
    def produce(self) -> bytes:
        ...

    def calls(self) -> int:
        ...


class ProducerResource:
    def __init__(self) -> None:
        self.executions = 0

    def produce(self) -> bytes:
        self.executions += 1
        return b'x' * (256 * 1024)

    def calls(self) -> int:
        return self.executions


@pytest.mark.parametrize('failure', ['reassembly', 'backing'])
def test_chunked_response_capacity_is_canonical_and_resource_executes_once(failure: str) -> None:
    cc.shutdown()
    cc.set_server(ipc_overrides={
        'pool_enabled': False, 'shm_backing_budget_bytes': 0, 'chunk_size': 65536,
    })
    cc.set_client(ipc_overrides={
        'pool_enabled': False, 'shm_backing_budget_bytes': 0, 'chunk_size': 65536,
        'file_backing_budget_bytes': 1024 * 1024 if failure == 'reassembly' else 0,
        'live_reassembly_budget_bytes': 0 if failure == 'reassembly' else 1024 * 1024,
    })
    cc.set_transport_policy(shm_threshold=4096)
    resource = ProducerResource()
    proxy = None
    try:
        cc.register(Producer, resource, name='capacity-producer')
        address = cc.server_address()
        proxy = cc.connect(Producer, name='capacity-producer', address=address)
        assert proxy.calls() == 0
        with pytest.raises(cc.error.ResourceUnavailable) as caught:
            proxy.produce()
        assert resource.executions == 1
        error = caught.value
        assert error.details['stage'] == 'response_reassembly_admission'
        assert error.details['transport_phase'] == 'dispatch_uncertain'
        assert error.details['fallback_eligible'] == 'false'
        native = error.__cause__
        assert native.code == 702
        assert native.transport_phase == 'dispatch_uncertain'
        assert native.fallback_eligible is False
        assert isinstance(cc.error.CCError.deserialize(memoryview(native.error_bytes)), cc.error.ResourceUnavailable)
        # Both control traffic and an unrelated small CRM reply remain usable.
        assert ping(address, timeout=0.5)
        assert proxy.calls() == 1
        cells = cc.memory_stats()['runtime_outgoing']['cells']
        assert cells['reassembly']['used_bytes'] == 0
        assert cells['file']['used_bytes'] == 0
        assert cells['shm']['used_bytes'] == 0
        assert cc.hold_stats()['active_leases'] == 0
        if failure == 'reassembly':
            assert cells['reassembly']['rejected_allocations'] >= 1
        else:
            # Reservation succeeded, then storage allocation failed: the
            # admission guard must refund that already acquired charge.
            assert cells['reassembly']['peak_bytes'] > 0
    finally:
        if proxy is not None:
            cc.close(proxy)
        cc.shutdown()
