import pytest

import c_two as cc
from c_two.crm.codec import _clear_codec_registry_for_tests
from c_two.transport.registry import _ProcessRegistry


pytest.importorskip('pyarrow', reason='Arrow view provider smoke requires pyarrow')
from c_two.providers import arrow


@pytest.fixture(autouse=True)
def clean_runtime():
    _ProcessRegistry.reset()
    _clear_codec_registry_for_tests()
    try:
        yield
    finally:
        _ProcessRegistry.reset()
        _clear_codec_registry_for_tests()


def test_arrow_explicit_batch_view_works_with_hold_over_direct_ipc():
    @arrow.record
    class Cell:
        level: int
        global_id: int
        active: bool

    @cc.crm(namespace='test.arrow-view-ipc', version='0.1.0')
    class CellStore:
        def view_cells(self) -> arrow.Batch[Cell]:
            ...

    class CellStoreImpl:
        def view_cells(self) -> arrow.Batch[Cell]:
            return [
                Cell(level=1, global_id=10, active=True),
                Cell(level=1, global_id=11, active=False),
            ]

    output_transferable = CellStore.view_cells._output_transferable
    original_from_buffer = output_transferable.from_buffer
    from_buffer_calls: list[str] = []

    def spy_from_buffer(data: memoryview):
        from_buffer_calls.append(type(data).__name__)
        return original_from_buffer(data)

    output_transferable.from_buffer = staticmethod(spy_from_buffer)

    cc.register(CellStore, CellStoreImpl(), name='arrow-view-ipc')
    address = cc.server_address()
    assert address is not None

    client = cc.connect(CellStore, name='arrow-view-ipc', address=address)
    try:
        normal_view = client.view_cells()
        assert isinstance(normal_view, arrow.ArrowBatchView)
        assert normal_view.to_records() == [
            Cell(level=1, global_id=10, active=True),
            Cell(level=1, global_id=11, active=False),
        ]
        assert from_buffer_calls == []

        held = cc.hold(client.view_cells)()
        try:
            assert isinstance(held.value, arrow.ArrowBatchView)
            assert held.value.to_records() == normal_view.to_records()
            assert from_buffer_calls == ['memoryview']
        finally:
            held.release()
    finally:
        cc.close(client)


def test_arrow_list_output_hold_returns_view_and_list_input_stays_materialized():
    @arrow.record
    class Cell:
        level: int
        global_id: int
        active: bool

    @cc.crm(namespace='test.arrow-list-hold-ipc', version='0.1.0')
    class CellStore:
        def list_cells(self) -> list[Cell]:
            ...

        def count_cells(self, cells: list[Cell]) -> int:
            ...

    class CellStoreImpl:
        def list_cells(self) -> list[Cell]:
            return [
                Cell(level=1, global_id=10, active=True),
                Cell(level=1, global_id=11, active=False),
            ]

        def count_cells(self, cells: list[Cell]) -> int:
            assert isinstance(cells, list)
            assert all(isinstance(cell, Cell) for cell in cells)
            return len(cells)

    output_transferable = CellStore.list_cells._output_transferable
    original_from_buffer = output_transferable.from_buffer
    from_buffer_calls: list[str] = []

    def spy_from_buffer(data: memoryview):
        from_buffer_calls.append(type(data).__name__)
        return original_from_buffer(data)

    output_transferable.from_buffer = staticmethod(spy_from_buffer)

    cc.register(CellStore, CellStoreImpl(), name='arrow-list-hold-ipc')
    address = cc.server_address()
    assert address is not None

    client = cc.connect(CellStore, name='arrow-list-hold-ipc', address=address)
    try:
        normal_cells = client.list_cells()
        assert normal_cells == [
            Cell(level=1, global_id=10, active=True),
            Cell(level=1, global_id=11, active=False),
        ]
        assert from_buffer_calls == []
        assert client.count_cells(normal_cells) == 2
        assert CellStore.count_cells._input_buffer_mode == 'view'

        held = cc.hold(client.list_cells)()
        try:
            assert isinstance(held.value, arrow.ArrowBatchView)
            assert held.value.to_records() == normal_cells
            assert bytes(held.buffer)
            assert from_buffer_calls == ['memoryview']
        finally:
            held.release()
        with pytest.raises(RuntimeError, match='released'):
            _ = held.buffer
    finally:
        cc.close(client)
