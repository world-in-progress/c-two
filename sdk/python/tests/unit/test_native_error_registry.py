from c_two import _native


def test_native_error_registry_exposes_canonical_codes():
    registry = _native.error_registry()
    assert registry["Unknown"] == 0
    assert registry["ResourceNotFound"] == 701
    assert registry["ResourceUnavailable"] == 702
    assert registry["ResourceAlreadyRegistered"] == 703
    assert registry["StaleResource"] == 704
    assert registry["RegistryUnavailable"] == 705
    assert registry["WriteConflict"] == 706


def test_native_decode_legacy_error_known_code():
    decoded = _native.decode_error_legacy(b"703:grid exists")
    assert decoded == {
        "code": 703,
        "name": "ResourceAlreadyRegistered",
        "message": "grid exists",
    }


def test_native_decode_legacy_error_unknown_code_degrades():
    decoded = _native.decode_error_legacy(b"9999:relay exploded")
    assert decoded == {
        "code": 0,
        "name": "Unknown",
        "message": "Unknown error code 9999: relay exploded",
    }


def test_native_decode_legacy_error_empty_bytes_returns_none():
    assert _native.decode_error_legacy(b"") is None


def test_native_encode_legacy_error_matches_python_wire_format():
    assert _native.encode_error_legacy(703, "grid exists") == b"703:grid exists"
