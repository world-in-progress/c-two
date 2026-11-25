from c_two import _native


def test_native_error_registry_exposes_canonical_codes():
    registry = _native.error_registry()
    assert registry["Unknown"][0] == 0
    assert registry["ResourceNotFound"][0] == 701
    assert registry["ResourceInputFromBuffer"][0] == 4
    assert registry["ClientOutputFromBuffer"][0] == 8
    assert registry["ResourceUnavailable"][0] == 702
    assert registry["ResourceAlreadyRegistered"][0] == 703
    assert registry["RegistryUnavailable"][0] == 705
    assert registry["ResourceClosed"][0] == 707
    assert registry["ResourceRemoved"][0] == 708
    assert registry["ContractMismatch"][0] == 709
    assert registry["IdentityMismatch"][0] == 710
    retired_names = {"".join(parts) for parts in (
        ("Route", "Stale"), ("Write", "Conflict"), ("Route", "Catalog", "Compacted"),
    )}
    assert not retired_names & registry.keys()
    assert registry["RouteWatchUnavailable"][0] == 712
    assert registry["ProtocolViolation"][0] == 713
    assert registry["RouteConflict"][0] == 719
    assert registry["SubscriptionLagged"][0] == 720
    assert registry["CallIdConflict"][0] == 721
    assert registry["CallResultUnavailable"][0] == 722
    assert registry["CallOutcomeUnknown"][0] == 723
    assert registry["AdmissionDenied"][0] == 724
    assert registry["FallbackDenied"][0] == 714


def test_registry_entries_carry_side_and_stage_classification():
    registry = _native.error_registry()
    assert registry["ResourceInputDeserializing"] == (1, "resource", "deserialize")
    assert registry["ResourceOutputSerializing"] == (2, "resource", "serialize")
    assert registry["ResourceFunctionExecuting"] == (3, "resource", "execute")
    assert registry["ResourceInputFromBuffer"] == (4, "resource", "from_buffer")
    assert registry["ClientInputSerializing"] == (5, "client", "serialize")
    assert registry["ClientOutputDeserializing"] == (6, "client", "deserialize")
    assert registry["ClientCallingResource"] == (7, "client", "call")
    assert registry["ClientOutputFromBuffer"] == (8, "client", "from_buffer")
    for name, value in registry.items():
        code, side, stage = value
        if int(code) > 8:
            assert side is None and stage is None, name


def test_registry_names_and_values_are_unique():
    registry = _native.error_registry()
    names = list(registry)
    values = [entry[0] for entry in registry.values()]
    assert len(set(names)) == len(names)
    assert len(set(values)) == len(values)
