import pytest
import c_two.error as error
from c_two.error import (
    ERROR_Code, CCBaseError, CCError,
    ResourceDeserializeInput, ResourceSerializeOutput, ResourceExecuteFunction,
    ClientSerializeInput, ClientDeserializeOutput, ClientCallResource,
    ResourceAlreadyRegistered, StaleResource, WriteConflict,
)


class TestERRORCode:
    def test_all_values_exist(self):
        assert ERROR_Code.ERROR_UNKNOWN == 0
        assert ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING == 1
        assert ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING == 2
        assert ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING == 3
        assert ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING == 5
        assert ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING == 6
        assert ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE == 7
        assert ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED == 703
        assert ERROR_Code.ERROR_STALE_RESOURCE == 704
        assert ERROR_Code.ERROR_WRITE_CONFLICT == 706

    def test_has_exactly_13_members(self):
        assert len(ERROR_Code) == 13

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

    def test_str_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='oops')
        assert str(err) == 'ERROR_AT_RESOURCE_FUNCTION_EXECUTING: oops'

    def test_str_format_default(self):
        err = CCError()
        assert str(err) == 'ERROR_UNKNOWN: Error occurred when using C-Two.'

    def test_repr_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='oops')
        assert repr(err) == 'CCError(code=3, message=oops)'

    def test_repr_format_default(self):
        err = CCError()
        assert repr(err) == 'CCError(code=0, message=Error occurred when using C-Two.)'

    def test_is_exception(self):
        err = CCError()
        assert isinstance(err, Exception)
        assert isinstance(err, CCBaseError)

    def test_can_be_raised_and_caught(self):
        with pytest.raises(CCError):
            raise CCError(message='fail')


class TestCCErrorSerialization:
    def test_round_trip(self):
        original = CCError(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message='test msg')
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.code == original.code
        assert restored.message == original.message

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
        assert CCError.serialize(err) == b'3:hello'

    def test_round_trip_default_error(self):
        original = CCError()
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == original.message

    def test_unknown_numeric_code_deserializes_to_unknown_with_context(self):
        restored = CCError.deserialize(memoryview(b"9999:low-level relay failure"))
        assert restored is not None
        assert type(restored) is CCError
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == "Unknown error code 9999: low-level relay failure"

    @pytest.mark.parametrize(
        ("payload", "expected_fragment"),
        [
            (b"abc:not a number", "Malformed error payload"),
            (b"3", "Malformed error payload"),
            (b"\xff", "Malformed error payload"),
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
    (ResourceSerializeOutput,    ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING,     'serializing output at resource'),
    (ResourceExecuteFunction,    ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING,     'executing function at resource'),
    (ClientSerializeInput,   ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING,    'serializing input at client'),
    (ClientDeserializeOutput,ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING, 'deserializing output at client'),
    (ClientCallResource,       ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE,          'calling resource from client'),
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


ALL_SUBCLASSES = [
    error.ResourceDeserializeInput,
    error.ResourceSerializeOutput,
    error.ResourceExecuteFunction,
    error.ClientSerializeInput,
    error.ClientDeserializeOutput,
    error.ClientCallResource,
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

    def test_future_mesh_errors_round_trip(self):
        stale = StaleResource("grid stale")
        conflict = WriteConflict("grid write conflict")

        stale_result = CCError.deserialize(memoryview(CCError.serialize(stale)))
        conflict_result = CCError.deserialize(memoryview(CCError.serialize(conflict)))

        assert isinstance(stale_result, StaleResource)
        assert stale_result.code == ERROR_Code.ERROR_STALE_RESOURCE
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
        result = CCError.deserialize(memoryview(b'0:some message'))
        assert type(result) is CCError
        assert result.code == ERROR_Code.ERROR_UNKNOWN
        assert result.message == 'some message'


class TestNativeErrorRegistryParity:
    def test_python_error_codes_match_native_registry(self):
        from c_two import _native

        native = _native.error_registry()
        python_codes = {code.name.removeprefix("ERROR_"): code.value for code in ERROR_Code}
        expected_python_names = {
            "UNKNOWN": "Unknown",
            "AT_RESOURCE_INPUT_DESERIALIZING": "ResourceInputDeserializing",
            "AT_RESOURCE_OUTPUT_SERIALIZING": "ResourceOutputSerializing",
            "AT_RESOURCE_FUNCTION_EXECUTING": "ResourceFunctionExecuting",
            "AT_CLIENT_INPUT_SERIALIZING": "ClientInputSerializing",
            "AT_CLIENT_OUTPUT_DESERIALIZING": "ClientOutputDeserializing",
            "AT_CLIENT_CALLING_RESOURCE": "ClientCallingResource",
            "RESOURCE_NOT_FOUND": "ResourceNotFound",
            "RESOURCE_UNAVAILABLE": "ResourceUnavailable",
            "RESOURCE_ALREADY_REGISTERED": "ResourceAlreadyRegistered",
            "STALE_RESOURCE": "StaleResource",
            "REGISTRY_UNAVAILABLE": "RegistryUnavailable",
            "WRITE_CONFLICT": "WriteConflict",
        }
        assert set(expected_python_names.values()) == set(native)
        for python_name, native_name in expected_python_names.items():
            assert python_codes[python_name] == native[native_name]
