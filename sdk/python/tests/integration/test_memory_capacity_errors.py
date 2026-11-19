"""Response admission keeps canonical capacity errors and never replays work."""
from __future__ import annotations

import pytest

import c_two as cc

import c_two.observe
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


def test_chunked_response_capacity_is_canonical_and_resource_executes_once() -> None:
    from c_two.transport.registry import _reset_default_runtime

    _reset_default_runtime()
    cc.set_server(memory_budget_bytes=64 * 1024)
    cc.set_client(memory_budget_bytes=128 * 1024, file_spill_budget_bytes=0)
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
        assert isinstance(native.semantic, cc.error.ResourceUnavailable)
        assert native.semantic.transport_phase == 'dispatch_uncertain'
        assert native.semantic.message == error.message
        # Both control traffic and an unrelated small CRM reply remain usable.
        assert ping(address, timeout=0.5)
        assert proxy.calls() == 1
        stats = cc.observe.memory_stats()
        assert stats['client']['memory']['used_bytes'] == 0
        assert stats['client']['memory']['rejected_allocations'] >= 1
        assert stats['client']['file_spill']['used_bytes'] == 0
        assert stats['holds']['active_holds'] == 0
    finally:
        if proxy is not None:
            cc.close(proxy)
        _reset_default_runtime()
