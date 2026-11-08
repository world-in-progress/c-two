from c_two import _native


def test_native_error_registry_exposes_canonical_codes():
    registry = _native.error_registry()
    assert registry["Unknown"] == 0
    assert registry["ResourceNotFound"] == 701
    assert registry["ResourceInputFromBuffer"] == 4
    assert registry["ClientOutputFromBuffer"] == 8
    assert registry["ResourceUnavailable"] == 702
    assert registry["ResourceAlreadyRegistered"] == 703
    assert registry["RegistryUnavailable"] == 705
    assert registry["ResourceClosed"] == 707
    assert registry["ResourceRemoved"] == 708
    assert registry["ContractMismatch"] == 709
    assert registry["IdentityMismatch"] == 710
    retired_names = {"".join(parts) for parts in (
        ("Route", "Stale"), ("Write", "Conflict"), ("Route", "Catalog", "Compacted"),
    )}
    assert not retired_names & registry.keys()
    assert registry["RouteWatchUnavailable"] == 712
    assert registry["ProtocolViolation"] == 713
    assert registry["RouteConflict"] == 719
    assert registry["FallbackDenied"] == 714


def test_retired_codes_encode_as_unknown():
    for code in (704, 706, 711):
        wire = _native.encode_error_wire(code, "retired", {})
        assert _native.decode_error_wire_parts(memoryview(wire)) == (0, "retired", {})


def test_native_decode_error_wire_parts_known_code():
    decoded = _native.decode_error_wire_parts(memoryview(
        b'C2E1{"version":1,"code":703,"name":"ResourceAlreadyRegistered",'
        b'"message":"grid exists","details":{"route":"grid"}}'
    ))
    assert decoded == (703, "grid exists", {"route": "grid"})


def test_native_route_conflict_wire_preserves_registrations():
    details = {"route": "grid", "dispatch_phase": "pre_dispatch", "registrations": "[]"}
    wire = _native.encode_error_wire(719, "conflict", details)
    assert _native.decode_error_wire_parts(memoryview(wire)) == (719, "conflict", details)


def test_native_decode_error_wire_parts_unknown_code_degrades():
    decoded = _native.decode_error_wire_parts(memoryview(
        b'C2E1{"version":1,"code":9999,"name":"FutureError",'
        b'"message":"relay exploded","details":{"route":"grid"}}'
    ))
    assert decoded == (
        0,
        "Unknown error code 9999 (FutureError): relay exploded",
        {"route": "grid", "unknown_code": "9999", "unknown_name": "FutureError"},
    )


def test_native_decode_error_wire_parts_empty_bytes_returns_none():
    assert _native.decode_error_wire_parts(memoryview(b"")) is None


def test_native_decode_error_wire_parts_rejects_malformed_payloads():
    for payload in (b"abc:not a number", b"3", b"\xff", b"703:grid exists"):
        try:
            _native.decode_error_wire_parts(memoryview(payload))
        except ValueError as exc:
            assert "C2 error wire" in str(exc)
        else:
            raise AssertionError(f"expected ValueError for {payload!r}")


def test_native_encode_error_wire_matches_canonical_wire_format():
    assert _native.encode_error_wire(703, "grid exists", {"route": "grid"}) == (
        b'C2E1{"version":1,"code":703,"name":"ResourceAlreadyRegistered",'
        b'"message":"grid exists","details":{"route":"grid"}}'
    )


def test_native_error_ffi_does_not_export_legacy_codec_names():
    suffix = "legacy"
    assert not hasattr(_native, f"encode_error_{suffix}")
    assert not hasattr(_native, f"decode_error_{suffix}")
