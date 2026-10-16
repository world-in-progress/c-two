"""Method table dispatch and lookup boundaries."""

import pytest

from c_two.transport.wire import MethodTable


class TestMethodTable:

    def test_from_methods(self):
        table = MethodTable.from_methods(["add", "greeting", "get_items"])
        assert len(table) == 3
        assert table.index_of("add") == 0
        assert table.name_of(0) == "add"
        assert table.index_of("greeting") == 1
        assert table.has_name("get_items")
        assert table.has_index(2)
        assert not table.has_name("missing")

    def test_add_manual(self):
        table = MethodTable()
        table.add("foo", 10)
        table.add("bar", 20)
        assert table.index_of("foo") == 10
        assert table.name_of(20) == "bar"
        assert table.names() == ["foo", "bar"]

    def test_missing_name_raises(self):
        table = MethodTable.from_methods(["m1"])
        with pytest.raises(KeyError):
            table.index_of("missing")

    def test_missing_index_raises(self):
        table = MethodTable.from_methods(["m1"])
        with pytest.raises(KeyError):
            table.name_of(99)

    def test_empty_table(self):
        t = MethodTable()
        assert len(t) == 0
        assert t.names() == []
        assert not t.has_name('x')
        assert not t.has_index(0)
