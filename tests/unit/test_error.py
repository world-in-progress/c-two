import pytest
import c_two.error as error
from c_two.error import (
    ERROR_Code, CCBaseError, CCError,
    CRMDeserializeInput, CRMSerializeOutput, CRMExecuteFunction,
    CompoSerializeInput, CompoDeserializeOutput, CompoCRMCalling,
)


class TestERRORCode:
    def test_all_values_exist(self):
        assert ERROR_Code.ERROR_UNKNOWN == 0
        assert ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING == 1
        assert ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING == 2
        assert ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING == 3
        assert ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING == 5
        assert ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING == 6
        assert ERROR_Code.ERROR_AT_COMPO_CRM_CALLING == 7

    def test_has_exactly_7_members(self):
        assert len(ERROR_Code) == 7

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
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message='something broke')
        assert err.code == ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING
        assert err.message == 'something broke'

    def test_str_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message='oops')
        assert str(err) == 'ERROR_AT_CRM_FUNCTION_EXECUTING: oops'

    def test_str_format_default(self):
        err = CCError()
        assert str(err) == 'ERROR_UNKNOWN: Error occurred when using C-Two.'

    def test_repr_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message='oops')
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
        original = CCError(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message='test msg')
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
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message='hello')
        assert CCError.serialize(err) == b'3:hello'

    def test_round_trip_default_error(self):
        original = CCError()
        data = CCError.serialize(original)
        restored = CCError.deserialize(memoryview(data))
        assert restored is not None
        assert restored.code == ERROR_Code.ERROR_UNKNOWN
        assert restored.message == original.message


SUBCLASS_PARAMS = [
    (CRMDeserializeInput,   ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING,    'deserializing input at CRM'),
    (CRMSerializeOutput,    ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING,     'serializing output at CRM'),
    (CRMExecuteFunction,    ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING,     'executing function at CRM'),
    (CompoSerializeInput,   ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING,    'serializing input at Compo'),
    (CompoDeserializeOutput,ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING, 'deserializing output at Compo'),
    (CompoCRMCalling,       ERROR_Code.ERROR_AT_COMPO_CRM_CALLING,          'calling CRM from Compo'),
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
    error.CRMDeserializeInput,
    error.CRMSerializeOutput,
    error.CRMExecuteFunction,
    error.CompoSerializeInput,
    error.CompoDeserializeOutput,
    error.CompoCRMCalling,
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
