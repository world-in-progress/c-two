from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from typing import Any

import c_two as cc
from fastdb4py.payload import (
    BuildPolicy,
    Builder,
    CompiledSpec,
    GraphIdentity,
    Payload,
    PayloadError,
    View,
    ViewKind,
)


C_TWO_REPOSITORY = Path(__file__).resolve().parents[4]
FASTDB_REPOSITORY = C_TWO_REPOSITORY.parent / "fastdb"
_CANDIDATE_FIXTURE_ROOT = os.environ.get("C2_PORTABLE_MATRIX_FIXTURE_ROOT")
if _CANDIDATE_FIXTURE_ROOT:
    _fixture_root = Path(_CANDIDATE_FIXTURE_ROOT)
    RECORD_SPEC_PATH = _fixture_root / "record-all-types.source.json"
    GRAPH_SPEC_PATH = _fixture_root / "graph-all-values.source.json"
else:
    RECORD_SPEC_PATH = (
        FASTDB_REPOSITORY
        / "tests/golden/payload/v1/spec/valid/record-all-types.source.json"
    )
    GRAPH_SPEC_PATH = (
        FASTDB_REPOSITORY
        / "tests/golden/payload/v1/binary/spec/graph-all-values.source.json"
    )

RECORD_SPEC_BYTES = RECORD_SPEC_PATH.read_bytes()
GRAPH_SPEC_BYTES = GRAPH_SPEC_PATH.read_bytes()
RECORD_SPEC: dict[str, Any] = json.loads(RECORD_SPEC_BYTES)
GRAPH_SPEC: dict[str, Any] = json.loads(GRAPH_SPEC_BYTES)


@cc.crm(namespace="test.portable-matrix.no-payload", version="0.1.0")
class PortableNoPayload:
    @cc.read
    def ping(self) -> None:
        ...


@cc.crm(namespace="test.portable-matrix.record-v1", version="0.1.0")
class PortableRecord:
    @cc.transfer(input=RECORD_SPEC, output=RECORD_SPEC)
    def roundtrip(self, payload: Payload) -> Payload:
        ...


@cc.crm(namespace="test.portable-matrix.object-graph-v1", version="0.1.0")
class PortableObjectGraph:
    @cc.transfer(input=GRAPH_SPEC, output=GRAPH_SPEC)
    def roundtrip(self, payload: Payload) -> Payload:
        ...


@cc.crm(namespace="test.portable-interop", version="0.1.0")
class PortableInterop:
    @cc.transfer(input=GRAPH_SPEC, output=GRAPH_SPEC)
    def graph_roundtrip(self, payload: Payload) -> Payload:
        ...

    @cc.read
    def ping(self) -> None:
        ...

    @cc.transfer(input=RECORD_SPEC, output=RECORD_SPEC)
    def record_roundtrip(self, payload: Payload) -> Payload:
        ...


LOGICAL_RESULTS: dict[str, dict[str, Any]] = {
    "no-payload": {
        "profile": "no-payload",
        "result": {"ping": "ok"},
        "schema": "c-two.portable-logical-result.v1",
    },
    "record-v1": {
        "profile": "record-v1",
        "result": {
            "bytes_hex": "0001ff",
            "nested": [[], None, ["", None, "tail"]],
            "record_bool": True,
            "record_u8": 0xAB,
            "series": [None, [], [0, None, 0xFF], [7]],
            "str": "\ufeffA\0B",
            "wstr": "\ufeffA\0🌍Ω",
        },
        "schema": "c-two.portable-logical-result.v1",
    },
    "object-graph-v1": {
        "profile": "object-graph-v1",
        "result": {
            "bytes_hex": "00ff7e",
            "mutual_cycle": True,
            "nested_null": True,
            "self_cycle": True,
            "shared_reference": True,
            "str": "same",
            "wstr": "A😀",
        },
        "schema": "c-two.portable-logical-result.v1",
    },
}


def logical_result_sha256(payload: str) -> str:
    canonical = json.dumps(
        LOGICAL_RESULTS[payload],
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


def build_record_payload() -> Payload:
    spec = CompiledSpec.compile(RECORD_SPEC_BYTES)
    builder = Builder.create(spec)
    try:
        (
            builder.entry_begin(0, 1)
            .value_component_begin()
            .value_bool(True)
            .value_u8(0xAB)
            .value_u16(0x1234)
            .value_u32(0x89AB_CDEF)
            .value_i32(-42)
            .value_u8n(0.0)
            .value_u16n(1.0)
            .value_f32_bits(0x3FC0_0000)
            .value_f64_bits(0x4004_0000_0000_0000)
            .value_str("\ufeffA\0B")
            .value_wstr_units((0xFEFF, 0x0041, 0, 0xD83C, 0xDF0D, 0x03A9))
            .value_bytes(b"\x00\x01\xff")
            .value_component_begin()
            .value_list_begin(3)
            .value_list_begin(0)
            .value_null()
            .value_list_begin(3)
            .value_str("")
            .value_null()
            .value_str("tail")
        )
        (
            builder.entry_begin(1, 4)
            .value_null()
            .value_list_begin(0)
            .value_list_begin(3)
            .value_u8(0)
            .value_null()
            .value_u8(0xFF)
            .value_list_begin(1)
            .value_u8(7)
        )
        plan = builder.freeze()
    finally:
        builder.close()
        spec.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def build_graph_payload() -> Payload:
    spec = CompiledSpec.compile(GRAPH_SPEC_BYTES)
    node_index = spec.component_index("Node")
    asset_index = spec.component_index("Asset")
    builder = Builder.create(spec)
    spec.close()
    try:
        node = builder.declare_object(node_index)
        asset = builder.declare_object(asset_index)
        (
            builder.object_fill_begin(node)
            .value_bool(True)
            .value_u8(0x12)
            .value_u16(0x3456)
            .value_u32(0x789A_BCDE)
            .value_i32(-1_234_567)
            .value_u8n_bits(0x3FE0_0000_0000_0000)
            .value_u16n_bits(0)
            .value_f32_bits(0x7FA1_2345)
            .value_f64_bits(0xFFF8_0000_0000_1234)
            .value_str("same")
            .value_wstr_units((0x0041, 0xD83D, 0xDE00))
            .value_bytes(b"\x00\xff~")
            .value_component_begin()
            .value_null()
            .value_u16(0xBEEF)
            .value_list_begin(3)
            .value_f32_bits(0x8000_0000)
            .value_null()
            .value_f32_bits(0xFF80_0001)
            .value_ref(node)
            .value_ref(asset)
            .object_fill_begin(asset)
            .value_str("same")
            .value_ref(node)
            .entry_begin(0, 1)
            .value_object(node)
            .entry_begin(1, 1)
            .value_object(asset)
            .entry_begin(2, 2)
            .value_ref(node)
            .value_null()
            .entry_begin(3, 3)
            .value_u8n_bits(0)
            .value_u8n_bits(0x3FE0_0000_0000_0000)
            .value_u8n_bits(0x3FF0_0000_0000_0000)
            .entry_begin(4, 3)
            .value_u16n_bits(0xBFF0_0000_0000_0000)
            .value_u16n_bits(0)
            .value_u16n_bits(0x3FF0_0000_0000_0000)
        )
        plan = builder.freeze()
    finally:
        builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def inspect_record_payload(payload: Payload) -> tuple[View, View]:
    with payload.entry_view(0) as sequence:
        assert sequence.kind() is ViewKind.SEQUENCE
        assert sequence.length() == 1
        root = sequence.at(0)

    assert root.kind() is ViewKind.COMPONENT
    assert root.field_count() == 14
    with root.field(0) as value:
        assert value.get_bool() is True
    with root.field(1) as value:
        assert value.get_u8() == 0xAB
    with root.field(9) as text:
        with text.acquire() as access:
            assert access.str() == "\ufeffA\0B"
    with root.field(10) as wide:
        with wide.acquire() as access:
            assert access.wstr() == "\ufeffA\0🌍Ω"
    with root.field(11) as opaque:
        with opaque.acquire() as access:
            assert access.bytes() == b"\x00\x01\xff"
    with root.field(13) as nested:
        assert nested.kind() is ViewKind.LIST
        assert nested.length() == 3
        with nested.at(0) as empty:
            assert not empty.is_null()
            assert empty.length() == 0
        with nested.at(1) as null_list:
            assert null_list.is_null()
        with nested.at(2) as present:
            assert present.length() == 3
            with present.at(0) as empty_text:
                with empty_text.acquire() as access:
                    assert access.str() == ""
            with present.at(1) as null_text:
                assert null_text.is_null()
            with present.at(2) as tail:
                with tail.acquire() as access:
                    assert access.str() == "tail"
    with payload.entry_view(1) as series:
        assert series.length() == 4
        with series.at(0) as null_list:
            assert null_list.is_null()
        with series.at(1) as empty:
            assert not empty.is_null()
            assert empty.length() == 0
        with series.at(2) as values:
            assert values.length() == 3
            with values.at(0) as value:
                assert value.get_u8() == 0
            with values.at(1) as value:
                assert value.is_null()
            with values.at(2) as value:
                assert value.get_u8() == 0xFF
        with series.at(3) as values:
            with values.at(0) as value:
                assert value.get_u8() == 7
    return root, root.materialize()


def inspect_graph_payload(payload: Payload) -> tuple[View, View]:
    spec = CompiledSpec.compile(GRAPH_SPEC_BYTES)
    try:
        node_index = spec.component_index("Node")
        asset_index = spec.component_index("Asset")
    finally:
        spec.close()
    root_identity = GraphIdentity(node_index, 0)
    asset_identity = GraphIdentity(asset_index, 0)

    with payload.entry_view(0) as roots:
        root = roots.at(0)
    assert root.graph_identity() == root_identity
    with root.field(9) as text:
        with text.acquire() as access:
            assert access.str() == "same"
    with root.field(10) as wide:
        with wide.acquire() as access:
            assert access.wstr() == "A😀"
    with root.field(11) as opaque:
        with opaque.acquire() as access:
            assert access.bytes() == b"\x00\xff~"
    with root.field(13) as values:
        assert values.length() == 3
        with values.at(1) as null_value:
            assert null_value.is_null()
    with root.field(14) as self_ref:
        assert self_ref.kind() is ViewKind.REF
        assert self_ref.graph_identity() == root_identity
        with self_ref.ref_target() as target:
            assert target.graph_identity() == root_identity
    with root.field(15) as asset_ref:
        assert asset_ref.graph_identity() == asset_identity
        with asset_ref.ref_target() as asset:
            with asset.field(1) as owner_ref:
                assert owner_ref.graph_identity() == root_identity
                with owner_ref.ref_target() as owner:
                    assert owner.graph_identity() == root_identity
    with payload.entry_view(2) as refs:
        with refs.at(0) as shared_ref:
            assert shared_ref.graph_identity() == root_identity
            with shared_ref.ref_target() as target:
                assert target.graph_identity() == root_identity
        with refs.at(1) as null_ref:
            assert null_ref.is_null()
    return root, root.materialize()


def assert_record_detached(detached: View) -> None:
    with detached.field(1) as value:
        assert value.get_u8() == 0xAB
    with detached.field(9) as text:
        with text.acquire() as access:
            assert access.str() == "\ufeffA\0B"


def assert_graph_detached(detached: View) -> None:
    root_identity = detached.graph_identity()
    with detached.field(14) as self_ref:
        assert self_ref.graph_identity() == root_identity
        with self_ref.ref_target() as target:
            assert target.graph_identity() == root_identity
    with detached.field(15) as asset_ref:
        with asset_ref.ref_target() as asset:
            with asset.field(1) as owner_ref:
                with owner_ref.ref_target() as owner:
                    assert owner.graph_identity() == root_identity


def assert_view_invalidated(view: View) -> None:
    try:
        view.kind()
    except PayloadError as error:
        assert error.symbol == "VIEW_INVALIDATED"
        assert error.path == "/view"
    else:
        raise AssertionError("FastDB view remained usable after owner invalidation")


class PortableInteropResource:
    def __init__(self) -> None:
        self.graph_calls = 0
        self.ping_calls = 0
        self.record_calls = 0
        self.borrowed_payload: Payload | None = None
        self.borrowed_view: View | None = None
        self.borrowed_detached: View | None = None
        self.outputs: list[Payload] = []

    def graph_roundtrip(self, payload: Payload) -> Payload:
        self.graph_calls += 1
        self.borrowed_payload = payload
        self.borrowed_view, self.borrowed_detached = inspect_graph_payload(payload)
        output = build_graph_payload()
        self.outputs.append(output)
        return output

    def ping(self) -> None:
        self.ping_calls += 1
        return None

    def record_roundtrip(self, payload: Payload) -> Payload:
        self.record_calls += 1
        root, detached = inspect_record_payload(payload)
        try:
            assert_record_detached(detached)
        finally:
            root.close()
            detached.close()
        output = build_record_payload()
        self.outputs.append(output)
        return output

    def close(self) -> None:
        for value in (
            self.borrowed_view,
            self.borrowed_detached,
            self.borrowed_payload,
            *self.outputs,
        ):
            if value is not None:
                try:
                    value.close()
                except Exception:
                    pass
