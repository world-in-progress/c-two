from __future__ import annotations

import time

import numpy as np
import pytest

import c_two as cc
import fastdb4py as fdb
from c_two.transport.registry import _ProcessRegistry


@fdb.feature
class PreparedPoint:
    x: fdb.F64


@cc.crm(namespace='test.fastdb-prepared', version='0.1.0')
class PreparedGrid:
    def swap(
        self,
        left: fdb.Batch[PreparedPoint],
        right: fdb.Batch[PreparedPoint],
    ) -> tuple[fdb.Batch[PreparedPoint], fdb.Batch[PreparedPoint]]:
        ...

    def require_points(self) -> fdb.Batch[PreparedPoint]:
        ...


class PreparedGridResource:
    def __init__(self):
        self.require_direct_context_seen = False

    def swap(self, left, right):
        return right, left

    def require_points(self):
        values = np.arange(2048, dtype=np.float64)
        batch = fdb.require(fdb.batch(PreparedPoint, rows=len(values)))
        envelope = getattr(batch, '_fastdb_require_envelope', None)
        self.require_direct_context_seen = getattr(envelope, 'direct_context', None) is not None
        batch.fill(x=values)
        return batch


@pytest.fixture(autouse=True)
def _cleanup_runtime():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


def _batch(values: list[float]) -> fdb.Batch[PreparedPoint]:
    batch = fdb.Batch.allocate(PreparedPoint, len(values))
    batch.fill(x=np.asarray(values, dtype=np.float64))
    return batch


@pytest.mark.timeout(30)
def test_fastdb_prepared_payload_roundtrips_direct_ipc(monkeypatch):
    def fail_to_bytes(self):
        raise AssertionError('prepared direct IPC payload should write into native SHM, not call to_bytes()')

    monkeypatch.setattr(fdb.FastdbPreparedCallDb, 'to_bytes', fail_to_bytes)

    resource = PreparedGridResource()
    cc.register(PreparedGrid, resource, name='prepared-grid')
    time.sleep(0.2)
    address = cc.server_address()
    assert address is not None

    proxy = cc.connect(PreparedGrid, name='prepared-grid', address=address)
    try:
        left = _batch([float(i) for i in range(768)])
        right = _batch([float(i) + 1000.0 for i in range(1024)])

        out_right, out_left = proxy.swap(left, right)

        assert proxy.client._mode == 'ipc'  # noqa: SLF001
        assert isinstance(out_right, fdb.Batch)
        assert isinstance(out_left, fdb.Batch)
        assert list(out_right.column.x[:3]) == [1000.0, 1001.0, 1002.0]
        assert list(out_left.column.x[:3]) == [0.0, 1.0, 2.0]
        assert len(out_right) == 1024
        assert len(out_left) == 768

        required = proxy.require_points()

        assert isinstance(required, fdb.Batch)
        assert len(required) == 2048
        assert list(required.column.x[:3]) == [0.0, 1.0, 2.0]
        assert resource.require_direct_context_seen is True
    finally:
        cc.close(proxy)
