import pytest
import c_two.error as error
from c_two.error import (
    ERROR_Code, CCBaseError, CCError,
    ResourceDeserializeInput, ResourceInputFromBuffer,
    ResourceSerializeOutput, ResourceExecuteFunction,
    ClientSerializeInput, ClientDeserializeOutput, ClientOutputFromBuffer,
    ClientCallResource,
    ResourceAlreadyRegistered, RouteStale, WriteConflict,
    ResourceClosed, ResourceRemoved, ContractMismatch, IdentityMismatch,
    RouteCatalogCompacted, RouteWatchUnavailable, ProtocolViolation, FallbackDenied,
    CallDeadlineExceeded, UnsupportedCallMode, CallCapacityExceeded,
)


class TestERRORCode:
    def test_all_values_exist(self):
        assert ERROR_Code.ERROR_UNKNOWN == 0
        assert ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING == 1
        assert ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING == 2
        assert ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING == 3
        assert ERROR_Code.ERROR_AT_RESOURCE_INPUT_FROM_BUFFER == 4
        assert ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING == 5
        assert ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING == 6
        assert ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE == 7
        assert ERROR_Code.ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER == 8
        assert ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED == 703
        assert ERROR_Code.ERROR_ROUTE_STALE == 704
        assert ERROR_Code.ERROR_WRITE_CONFLICT == 706
        assert ERROR_Code.ERROR_RESOURCE_CLOSED == 707
        assert ERROR_Code.ERROR_RESOURCE_REMOVED == 708
        assert ERROR_Code.ERROR_CONTRACT_MISMATCH == 709
        assert ERROR_Code.ERROR_IDENTITY_MISMATCH == 710
        assert ERROR_Code.ERROR_ROUTE_CATALOG_COMPACTED == 711
        assert ERROR_Code.ERROR_ROUTE_WATCH_UNAVAILABLE == 712
        assert ERROR_Code.ERROR_PROTOCOL_VIOLATION == 713
        assert ERROR_Code.ERROR_FALLBACK_DENIED == 714
        assert ERROR_Code.ERROR_CALL_DEADLINE_EXCEEDED == 715
        assert ERROR_Code.ERROR_UNSUPPORTED_CALL_MODE == 716
        assert ERROR_Code.ERROR_CALL_CAPACITY_EXCEEDED == 717

    def test_has_exactly_26_members(self):
        assert len(ERROR_Code) == 26

    def test_values_are_unique(self):
        values = [e.value for e in ERROR_Code]
        assert len(values) == len(set(values))

    def test_is_int_enum(self):
        assert isinstance(ERROR_Code.ERROR_UNKNOWN, int)


class TestCCError:
    def test_default_creation(self):
        err = CCError()
        assert err.code == ERROR_Code.ERROR_UNKNOWN
        assert err.message == 'Error occurred when using C-Two.'

    def test_custom_creation(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='something broke')
        assert err.code == ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING
        assert err.message == 'something broke'

    def test_details_creation(self):
        err = CCError(
            code=ERROR_Code.ERROR_RESOURCE_UNAVAILABLE,
            message='upstream failed',
            details={'route': 'grid', 'cause_kind': 'TransportIo'},
        )
        assert err.details == {'route': 'grid', 'cause_kind': 'TransportIo'}

    def test_str_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='oops')
        assert str(err) == 'ERROR_AT_RESOURCE_FUNCTION_EXECUTING: oops'

    def test_str_format_default(self):
        err = CCError()
        assert str(err) == 'ERROR_UNKNOWN: Error occurred when using C-Two.'

    def test_repr_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='oops')
        assert repr(err) == 'CCError(code=3, message=oops, details={})'

    def test_repr_format_default(self):
        err = CCError()
        assert repr(err) == 'CCError(code=0, message=Error occurred when using C-Two., details={})'

    def test_is_exception(self):
        err = CCError()
        assert isinstance(err, Exception)
        assert isinstance(err, CCBaseError)

    def test_can_be_raised_and_caught(self):
        with pytest.raises(CCError):
            raise CCError(message='fail')


class TestCCErrorSerialization:
    def test_round_trip(self):
        original = CCError(
            code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING,
            message='test msg',
            details={'route': 'grid'},
        )
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.code == original.code
        assert restored.message == original.message
        assert restored.details == {'route': 'grid'}

    def test_serialize_none_returns_empty_bytes(self):
        assert CCError.serialize(None) == b''

    def test_deserialize_empty_memoryview_returns_none(self):
        assert CCError.deserialize(memoryview(b'')) is None

    def test_message_with_colons_preserved(self):
        original = CCError(code=ERROR_Code.ERROR_UNKNOWN, message='host:port:extra')
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.message == 'host:port:extra'

    def test_serialize_produces_expected_bytes(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='hello')
        assert CCError.serialize(err) == (
            b'C2E1{"version":1,"code":3,"name":"ResourceFunctionExecuting",'
            b'"message":"hello","details":{}}'
        )

    def test_round_trip_default_error(self):
        original = CCError()
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == original.message

    def test_unknown_numeric_code_deserializes_to_unknown_with_context(self):
        restored = CCError.deserialize(memoryview(
            b'C2E1{"version":1,"code":9999,"name":"FutureRouteError",'
            b'"message":"low-level relay failure","details":{"route":"grid"}}'
        ))
        assert restored is not None
        assert type(restored) is CCError
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == "Unknown error code 9999 (FutureRouteError): low-level relay failure"
        assert restored.details["route"] == "grid"
        assert restored.details["unknown_code"] == "9999"
        assert restored.details["unknown_name"] == "FutureRouteError"

    @pytest.mark.parametrize(
        ("payload", "expected_fragment"),
        [
            (b"abc:not a number", "Malformed error payload"),
            (b"3", "Malformed error payload"),
            (b"\xff", "Malformed error payload"),
            (b"703:grid exists", "Malformed error payload"),
        ],
    )
    def test_malformed_payload_deserializes_to_unknown(self, payload, expected_fragment):
        restored = CCError.deserialize(memoryview(payload))
        assert restored is not None
        assert type(restored) is CCError
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert expected_fragment in restored.message


SUBCLASS_PARAMS = [
    (ResourceDeserializeInput,   ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING,    'deserializing input at resource'),
    (ResourceInputFromBuffer,    ERROR_Code.ERROR_AT_RESOURCE_INPUT_FROM_BUFFER,      'constructing resource input from buffer'),
    (ResourceSerializeOutput,    ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING,     'serializing output at resource'),
    (ResourceExecuteFunction,    ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING,     'executing function at resource'),
    (ClientSerializeInput,   ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING,    'serializing input at client'),
    (ClientDeserializeOutput,ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING, 'deserializing output at client'),
    (ClientOutputFromBuffer,  ERROR_Code.ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER,  'constructing client output from buffer'),
    (ClientCallResource,       ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE,          'calling resource from client'),
]

ROUTE_CATALOG_SUBCLASS_PARAMS = [
    (RouteStale, ERROR_Code.ERROR_ROUTE_STALE),
    (ResourceClosed, ERROR_Code.ERROR_RESOURCE_CLOSED),
    (ResourceRemoved, ERROR_Code.ERROR_RESOURCE_REMOVED),
    (ContractMismatch, ERROR_Code.ERROR_CONTRACT_MISMATCH),
    (IdentityMismatch, ERROR_Code.ERROR_IDENTITY_MISMATCH),
    (RouteCatalogCompacted, ERROR_Code.ERROR_ROUTE_CATALOG_COMPACTED),
    (RouteWatchUnavailable, ERROR_Code.ERROR_ROUTE_WATCH_UNAVAILABLE),
    (ProtocolViolation, ERROR_Code.ERROR_PROTOCOL_VIOLATION),
    (FallbackDenied, ERROR_Code.ERROR_FALLBACK_DENIED),
]

CALL_ERROR_SUBCLASS_PARAMS = [
    (CallDeadlineExceeded, ERROR_Code.ERROR_CALL_DEADLINE_EXCEEDED),
    (UnsupportedCallMode, ERROR_Code.ERROR_UNSUPPORTED_CALL_MODE),
    (CallCapacityExceeded, ERROR_Code.ERROR_CALL_CAPACITY_EXCEEDED),
]


class TestErrorSubclasses:
    @pytest.mark.parametrize("cls,expected_code,desc_fragment", SUBCLASS_PARAMS)
    def test_correct_error_code(self, cls, expected_code, desc_fragment):
        err = cls('detail')
        assert err.code == expected_code

    @pytest.mark.parametrize("cls,expected_code,desc_fragment", SUBCLASS_PARAMS)
    def test_custom_message_included(self, cls, expected_code, desc_fragment):
        err = cls('detail')
        assert 'detail' in err.message
        assert desc_fragment in err.message

    @pytest.mark.parametrize("cls,expected_code,desc_fragment", SUBCLASS_PARAMS)
    def test_default_no_message(self, cls, expected_code, desc_fragment):
        err = cls()
        assert err.code == expected_code
        # With no message the ternary yields '', which is falsy so __init__
        # falls through to the default CCError message.
        assert isinstance(err.message, str)

    @pytest.mark.parametrize("cls,expected_code,desc_fragment", SUBCLASS_PARAMS)
    def test_is_cc_error(self, cls, expected_code, desc_fragment):
        err = cls()
        assert isinstance(err, CCError)
        assert isinstance(err, CCBaseError)
        assert isinstance(err, Exception)


class TestRouteCatalogErrorSubclasses:
    @pytest.mark.parametrize("cls,expected_code", ROUTE_CATALOG_SUBCLASS_PARAMS)
    def test_correct_error_code(self, cls, expected_code):
        err = cls('detail', details={'route': 'grid'})
        assert err.code == expected_code
        assert err.message == 'detail'
        assert err.details == {'route': 'grid'}


class TestCallErrorSubclasses:
    @pytest.mark.parametrize("cls,expected_code", CALL_ERROR_SUBCLASS_PARAMS)
    def test_correct_error_code_message_and_details(self, cls, expected_code):
        err = cls('bounded call failed', details={'route': 'grid'})
        assert err.code == expected_code
        assert err.message == 'bounded call failed'
        assert err.details == {'route': 'grid'}

    @pytest.mark.parametrize("cls,expected_code", CALL_ERROR_SUBCLASS_PARAMS)
    def test_default_message(self, cls, expected_code):
        err = cls()
        assert err.code == expected_code
        assert isinstance(err.message, str)
        assert err.message != ''

    @pytest.mark.parametrize("cls,expected_code", CALL_ERROR_SUBCLASS_PARAMS)
    def test_is_cc_error(self, cls, expected_code):
        err = cls()
        assert isinstance(err, CCError)
        assert isinstance(err, CCBaseError)
        assert isinstance(err, Exception)


class TestCallDeadlineExceededTransportPhase:
    def test_phase_property_reads_pre_dispatch_detail(self):
        err = CallDeadlineExceeded('late', details={'transport_phase': 'pre_dispatch'})
        assert err.transport_phase == 'pre_dispatch'

    def test_phase_property_reads_dispatch_uncertain_detail(self):
        err = CallDeadlineExceeded('late', details={'transport_phase': 'dispatch_uncertain'})
        assert err.transport_phase == 'dispatch_uncertain'

    def test_phase_property_is_none_without_detail(self):
        err = CallDeadlineExceeded('late')
        assert err.transport_phase is None

    def test_phase_property_is_read_only(self):
        err = CallDeadlineExceeded('late', details={'transport_phase': 'pre_dispatch'})
        with pytest.raises(AttributeError):
            err.transport_phase = 'dispatch_uncertain'
        assert err.transport_phase == 'pre_dispatch'

    def test_phase_property_does_not_rewrite_received_details(self):
        details = {'transport_phase': 'dispatch_uncertain', 'route': 'grid'}
        err = CallDeadlineExceeded('late', details=details)
        assert err.transport_phase == 'dispatch_uncertain'
        assert err.details == {'transport_phase': 'dispatch_uncertain', 'route': 'grid'}

    def test_phase_not_stored_as_instance_attribute_copy(self):
        err = CallDeadlineExceeded('late', details={'transport_phase': 'pre_dispatch'})
        assert 'transport_phase' not in vars(err)

    def test_wire_carries_phase_only_once_as_detail(self):
        err = CallDeadlineExceeded('late', details={'transport_phase': 'pre_dispatch'})
        wire = CCError.serialize(err)
        assert wire.count(b'transport_phase') == 1
        restored = CCError.deserialize(memoryview(wire))
        assert isinstance(restored, CallDeadlineExceeded)
        assert restored.transport_phase == 'pre_dispatch'
        assert restored.details == {'transport_phase': 'pre_dispatch'}

    def test_wire_omits_phase_when_detail_absent(self):
        err = CallDeadlineExceeded('late')
        wire = CCError.serialize(err)
        assert b'transport_phase' not in wire
        restored = CCError.deserialize(memoryview(wire))
        assert isinstance(restored, CallDeadlineExceeded)
        assert restored.transport_phase is None


class TestNewCallErrorCodec:
    @pytest.mark.parametrize("cls,expected_code", CALL_ERROR_SUBCLASS_PARAMS)
    def test_canonical_native_codec_round_trip(self, cls, expected_code):
        original = cls('budget exhausted', details={'route': 'grid', 'stage': 'call_deadline'})
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert isinstance(restored, cls)
        assert restored.code == expected_code
        assert restored.message == 'budget exhausted'
        assert restored.details == {'route': 'grid', 'stage': 'call_deadline'}

    def test_deadline_error_round_trip_keeps_phase_and_details(self):
        original = CallDeadlineExceeded(
            'deadline elapsed before a definitive outcome',
            details={'transport_phase': 'dispatch_uncertain', 'fallback_eligible': 'false'},
        )
        wire = CCError.serialize(original)
        assert wire.startswith(b'C2E1{"version":1,"code":715,"name":"CallDeadlineExceeded"')
        restored = CCError.deserialize(memoryview(wire))
        assert isinstance(restored, CallDeadlineExceeded)
        assert restored.message == 'deadline elapsed before a definitive outcome'
        assert restored.details == {
            'transport_phase': 'dispatch_uncertain',
            'fallback_eligible': 'false',
        }
        assert restored.transport_phase == 'dispatch_uncertain'

    def test_unsupported_call_mode_round_trip(self):
        original = UnsupportedCallMode('finite deadline does not support same-process sync')
        restored = CCError.deserialize(memoryview(CCError.serialize(original)))
        assert isinstance(restored, UnsupportedCallMode)
        assert restored.code == ERROR_Code.ERROR_UNSUPPORTED_CALL_MODE
        assert restored.message == 'finite deadline does not support same-process sync'
        assert restored.details == {}

    def test_call_capacity_exceeded_round_trip(self):
        original = CallCapacityExceeded('continuation byte capacity exhausted')
        restored = CCError.deserialize(memoryview(CCError.serialize(original)))
        assert isinstance(restored, CallCapacityExceeded)
        assert restored.code == ERROR_Code.ERROR_CALL_CAPACITY_EXCEEDED
        assert restored.message == 'continuation byte capacity exhausted'
        assert restored.details == {}


ALL_SUBCLASSES = [
    error.ResourceDeserializeInput,
    error.ResourceInputFromBuffer,
    error.ResourceSerializeOutput,
    error.ResourceExecuteFunction,
    error.ClientSerializeInput,
    error.ClientDeserializeOutput,
    error.ClientOutputFromBuffer,
    error.ClientCallResource,
    error.RouteStale,
    error.ResourceClosed,
    error.ResourceRemoved,
    error.ContractMismatch,
    error.IdentityMismatch,
    error.RouteCatalogCompacted,
    error.RouteWatchUnavailable,
    error.ProtocolViolation,
    error.FallbackDenied,
    error.CallDeadlineExceeded,
    error.UnsupportedCallMode,
    error.CallCapacityExceeded,
]


class TestSubclassDeserialization:
    @pytest.mark.parametrize("subclass", ALL_SUBCLASSES, ids=lambda c: c.__name__)
    def test_each_subclass_round_trip(self, subclass):
        original = subclass(message='test detail')
        data = CCError.serialize(original)
        result = CCError.deserialize(memoryview(data))
        assert isinstance(result, subclass)
        assert result.code == original.code
        assert 'test detail' in result.message

    def test_generic_ccerror_round_trip(self):
        original = CCError(ERROR_Code.ERROR_UNKNOWN, 'generic')
        data = CCError.serialize(original)
        result = CCError.deserialize(memoryview(data))
        assert type(result) is CCError
        assert result.code == ERROR_Code.ERROR_UNKNOWN
        assert result.message == 'generic'

    def test_resource_already_registered_round_trip(self):
        original = ResourceAlreadyRegistered("Route name already registered: 'grid'")
        data = CCError.serialize(original)
        result = CCError.deserialize(memoryview(data))
        assert isinstance(result, ResourceAlreadyRegistered)
        assert result.code == ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED
        assert result.message == "Route name already registered: 'grid'"

    def test_from_buffer_errors_round_trip_to_specific_subclasses(self):
        resource_err = ResourceInputFromBuffer("bad input view")
        restored_resource = CCError.deserialize(CCError.serialize(resource_err))
        assert isinstance(restored_resource, ResourceInputFromBuffer)
        assert restored_resource.code == ERROR_Code.ERROR_AT_RESOURCE_INPUT_FROM_BUFFER
        assert "constructing resource input from buffer" in restored_resource.message
        assert "bad input view" in restored_resource.message

        client_err = ClientOutputFromBuffer("bad output view")
        restored_client = CCError.deserialize(CCError.serialize(client_err))
        assert isinstance(restored_client, ClientOutputFromBuffer)
        assert restored_client.code == ERROR_Code.ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER
        assert "constructing client output from buffer" in restored_client.message
        assert "bad output view" in restored_client.message

    def test_future_mesh_errors_round_trip(self):
        stale = RouteStale("grid stale")
        conflict = WriteConflict("grid write conflict")

        stale_result = CCError.deserialize(memoryview(CCError.serialize(stale)))
        conflict_result = CCError.deserialize(memoryview(CCError.serialize(conflict)))

        assert isinstance(stale_result, RouteStale)
        assert stale_result.code == ERROR_Code.ERROR_ROUTE_STALE
        assert stale_result.message == "grid stale"
        assert isinstance(conflict_result, WriteConflict)
        assert conflict_result.code == ERROR_Code.ERROR_WRITE_CONFLICT
        assert conflict_result.message == "grid write conflict"

    @pytest.mark.parametrize("subclass", ALL_SUBCLASSES, ids=lambda c: c.__name__)
    def test_none_message_produces_description(self, subclass):
        err = subclass(message=None)
        assert err.message != ''


class TestErrorCodeToClass:
    def test_code_to_class_registry_complete(self):
        expected_codes = {code for code in ERROR_Code if code != ERROR_Code.ERROR_UNKNOWN}
        assert set(error._CODE_TO_CLASS.keys()) == expected_codes

    def test_unknown_code_deserializes_to_base(self):
        result = CCError.deserialize(memoryview(
            b'C2E1{"version":1,"code":0,"name":"Unknown","message":"some message","details":{}}'
        ))
        assert type(result) is CCError
        assert result.code == ERROR_Code.ERROR_UNKNOWN
        assert result.message == 'some message'


class TestNativeErrorRegistryParity:
    def test_python_error_codes_match_native_registry(self):
        from c_two import _native

        native = _native.error_registry()
        expected = {
            "ERROR_UNKNOWN": ("Unknown", 0),
            "ERROR_AT_RESOURCE_INPUT_DESERIALIZING": ("ResourceInputDeserializing", 1),
            "ERROR_AT_RESOURCE_OUTPUT_SERIALIZING": ("ResourceOutputSerializing", 2),
            "ERROR_AT_RESOURCE_FUNCTION_EXECUTING": ("ResourceFunctionExecuting", 3),
            "ERROR_AT_RESOURCE_INPUT_FROM_BUFFER": ("ResourceInputFromBuffer", 4),
            "ERROR_AT_CLIENT_INPUT_SERIALIZING": ("ClientInputSerializing", 5),
            "ERROR_AT_CLIENT_OUTPUT_DESERIALIZING": ("ClientOutputDeserializing", 6),
            "ERROR_AT_CLIENT_CALLING_RESOURCE": ("ClientCallingResource", 7),
            "ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER": ("ClientOutputFromBuffer", 8),
            "ERROR_RESOURCE_NOT_FOUND": ("ResourceNotFound", 701),
            "ERROR_RESOURCE_UNAVAILABLE": ("ResourceUnavailable", 702),
            "ERROR_RESOURCE_ALREADY_REGISTERED": ("ResourceAlreadyRegistered", 703),
            "ERROR_ROUTE_STALE": ("RouteStale", 704),
            "ERROR_REGISTRY_UNAVAILABLE": ("RegistryUnavailable", 705),
            "ERROR_WRITE_CONFLICT": ("WriteConflict", 706),
            "ERROR_RESOURCE_CLOSED": ("ResourceClosed", 707),
            "ERROR_RESOURCE_REMOVED": ("ResourceRemoved", 708),
            "ERROR_CONTRACT_MISMATCH": ("ContractMismatch", 709),
            "ERROR_IDENTITY_MISMATCH": ("IdentityMismatch", 710),
            "ERROR_ROUTE_CATALOG_COMPACTED": ("RouteCatalogCompacted", 711),
            "ERROR_ROUTE_WATCH_UNAVAILABLE": ("RouteWatchUnavailable", 712),
            "ERROR_PROTOCOL_VIOLATION": ("ProtocolViolation", 713),
            "ERROR_FALLBACK_DENIED": ("FallbackDenied", 714),
            "ERROR_CALL_DEADLINE_EXCEEDED": ("CallDeadlineExceeded", 715),
            "ERROR_UNSUPPORTED_CALL_MODE": ("UnsupportedCallMode", 716),
            "ERROR_CALL_CAPACITY_EXCEEDED": ("CallCapacityExceeded", 717),
        }

        assert set(ERROR_Code.__members__) == set(expected)
        for py_name, (native_name, value) in expected.items():
            assert native[native_name] == value
            assert ERROR_Code[py_name].value == value

    def test_native_registry_entries_are_unique_and_exact(self):
        from c_two import _native

        native = _native.error_registry()
        assert len(native) == 26
        names = list(native.keys())
        values = list(native.values())
        assert len(set(names)) == len(names)
        assert len(set(values)) == len(values)
        assert native["CallDeadlineExceeded"] == 715
        assert native["UnsupportedCallMode"] == 716
        assert native["CallCapacityExceeded"] == 717

    def test_serialize_uses_native_wire_encoder(self, monkeypatch):
        calls = []

        def fake_encode(code, message, details):
            calls.append((code, message, details))
            return b"C2E1from-native"

        monkeypatch.setattr(error._native, "encode_error_wire", fake_encode)

        err = CCError(ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, "boom")
        assert CCError.serialize(err) == b"C2E1from-native"
        assert calls == [(3, "boom", {})]

    def test_deserialize_uses_native_wire_decoder_without_python_tobytes(self, monkeypatch):
        class NoToBytes:
            def tobytes(self):
                raise AssertionError("deserialize must not call tobytes")

        calls = []

        def fake_decode(data):
            calls.append(data)
            return (703, "grid exists", {"route": "grid"})

        monkeypatch.setattr(error._native, "decode_error_wire_parts", fake_decode)

        result = CCError.deserialize(NoToBytes())
        assert isinstance(result, ResourceAlreadyRegistered)
        assert result.message == "grid exists"
        assert result.details == {"route": "grid"}
        assert len(calls) == 1
        assert isinstance(calls[0], NoToBytes)
