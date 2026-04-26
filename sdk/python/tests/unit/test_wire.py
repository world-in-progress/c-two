"""Unit tests for wire MethodTable and handshake protocol."""
from __future__ import annotations

import pytest

from c_two.transport.protocol import (
    HANDSHAKE_VERSION,
    CAP_CALL,
    CAP_METHOD_IDX,
    MethodEntry,
    RouteInfo,
    encode_client_handshake,
    encode_server_handshake,
    decode_handshake,
)
from c_two.transport.wire import MethodTable


# ---------------------------------------------------------------------------
# MethodTable
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Handshake round-trip
# ---------------------------------------------------------------------------

class TestHandshake:

    def test_client_handshake_roundtrip(self):
        segments = [("seg_abc", 268435456)]
        cap = CAP_CALL | CAP_METHOD_IDX
        encoded = encode_client_handshake(segments, cap)
        assert encoded[0] == HANDSHAKE_VERSION
        hs = decode_handshake(encoded)
        assert hs.segments == segments
        assert hs.capability_flags == cap
        assert hs.routes == []

    def test_server_handshake_roundtrip(self):
        segments = [("srv_seg", 134217728)]
        cap = CAP_CALL | CAP_METHOD_IDX
        route = RouteInfo(
            name="hello",
            methods=[
                MethodEntry(name="add", index=0),
                MethodEntry(name="greeting", index=1),
            ],
        )
        encoded = encode_server_handshake(segments, cap, [route])
        assert encoded[0] == HANDSHAKE_VERSION
        hs = decode_handshake(encoded)
        assert hs.segments == segments
        assert hs.capability_flags == cap
        assert len(hs.routes) == 1
        r = hs.routes[0]
        assert r.name == "hello"
        assert len(r.methods) == 2
        assert r.method_by_name("add") == 0
        assert r.method_by_name("greeting") == 1
        assert r.method_by_index(0) == "add"

    def test_multi_segment_multi_route(self):
        segs = [("s1", 100), ("s2", 200)]
        r1 = RouteInfo("route_a", [MethodEntry("m1", 0)])
        r2 = RouteInfo("route_b", [MethodEntry("m2", 0), MethodEntry("m3", 1)])
        encoded = encode_server_handshake(segs, CAP_CALL, [r1, r2])
        hs = decode_handshake(encoded)
        assert len(hs.segments) == 2
        assert len(hs.routes) == 2
        assert hs.routes[0].name == "route_a"
        assert hs.routes[1].name == "route_b"
        assert len(hs.routes[1].methods) == 2

    def test_empty_segments(self):
        encoded = encode_client_handshake([], CAP_CALL)
        hs = decode_handshake(encoded)
        assert hs.segments == []

    def test_bad_version_raises(self):
        bad = bytes([4, 0, 0])
        with pytest.raises(ValueError, match="version"):
            decode_handshake(bad)

    def test_truncated_raises(self):
        with pytest.raises(ValueError):
            decode_handshake(b"\x06")


class TestHandshakePrefixExchange:
    """Handshake v6: prefix field."""

    def test_client_handshake_with_prefix(self):
        segments = [("/cc3b00001234_b0000", 262144)]
        prefix = "/cc3b00001234"
        encoded = encode_client_handshake(segments, CAP_CALL | CAP_METHOD_IDX, prefix=prefix)
        assert encoded[0] == HANDSHAKE_VERSION
        hs = decode_handshake(encoded)
        assert hs.prefix == prefix
        assert hs.segments == segments
        assert hs.capability_flags & CAP_CALL

    def test_server_handshake_with_prefix(self):
        segments = [("/cc3bABCD0000_b0000", 262144)]
        prefix = "/cc3bABCD0000"
        routes = [RouteInfo(name="grid", methods=[MethodEntry(name="get", index=0)])]
        encoded = encode_server_handshake(segments, CAP_CALL, routes, prefix=prefix)
        hs = decode_handshake(encoded)
        assert hs.prefix == prefix
        assert len(hs.routes) == 1
        assert hs.routes[0].name == "grid"

    def test_empty_prefix_defaults(self):
        segments = [("/seg0", 65536)]
        encoded = encode_client_handshake(segments, CAP_CALL, prefix="")
        hs = decode_handshake(encoded)
        assert hs.prefix == ""

    def test_prefix_none_uses_empty(self):
        segments = [("/seg0", 65536)]
        encoded = encode_client_handshake(segments, CAP_CALL)
        hs = decode_handshake(encoded)
        assert hs.prefix == ""
