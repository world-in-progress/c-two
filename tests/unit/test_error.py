import pytest
from c_two.error import (
    ERROR_Code, CCBaseError, CCError,
    CRMDeserializeInput, CRMSerializeOutput, CRMExecuteFunction, CRMServerError,
    CompoSerializeInput, CompoDeserializeOutput, CompoCRMCalling, CompoClientError,
    EventSerializeError, EventDeserializeError,
)


class TestERRORCode:
    def test_all_values_exist(self):
        assert ERROR_Code.ERROR_UNKNOWN == 0
        assert ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING == 1
        assert ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING == 2
        assert ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING == 3
        assert ERROR_Code.ERROR_AT_CRM_SERVER == 4
        assert ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING == 5
        assert ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING == 6
        assert ERROR_Code.ERROR_AT_COMPO_CRM_CALLING == 7
        assert ERROR_Code.ERROR_AT_COMPO_CLIENT == 8
        assert ERROR_Code.ERROR_AT_EVENT_SERIALIZING == 9
        assert ERROR_Code.ERROR_AT_EVENT_DESERIALIZING == 10

    def test_has_exactly_11_members(self):
        assert len(ERROR_Code) == 11

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
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_SERVER, message='something broke')
        assert err.code == ERROR_Code.ERROR_AT_CRM_SERVER
        assert err.message == 'something broke'

    def test_str_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_SERVER, message='oops')
        assert str(err) == 'ERROR_AT_CRM_SERVER: oops'

    def test_str_format_default(self):
        err = CCError()
        assert str(err) == 'ERROR_UNKNOWN: Error occurred when using C-Two.'

    def test_repr_format(self):
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_SERVER, message='oops')
        assert repr(err) == 'CCError(code=4, message=oops)'

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
        original = CCError(code=ERROR_Code.ERROR_AT_CRM_SERVER, message='test msg')
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
        err = CCError(code=ERROR_Code.ERROR_AT_CRM_SERVER, message='hello')
        assert CCError.serialize(err) == b'4:hello'

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
    (CRMServerError,        ERROR_Code.ERROR_AT_CRM_SERVER,                 'at CRM server'),
    (CompoSerializeInput,   ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING,    'serializing input at Compo'),
    (CompoDeserializeOutput,ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING, 'deserializing output at Compo'),
    (CompoCRMCalling,       ERROR_Code.ERROR_AT_COMPO_CRM_CALLING,          'calling CRM from Compo'),
    (CompoClientError,      ERROR_Code.ERROR_AT_COMPO_CLIENT,               'at Compo client'),
    (EventSerializeError,   ERROR_Code.ERROR_AT_EVENT_SERIALIZING,          'serializing event'),
    (EventDeserializeError, ERROR_Code.ERROR_AT_EVENT_DESERIALIZING,        'deserializing event'),
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
