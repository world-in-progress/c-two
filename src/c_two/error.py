from __future__ import annotations
from enum import IntEnum, unique

@unique
class ERROR_Code(IntEnum):
    ERROR_UNKNOWN                           = 0
    ERROR_AT_CRM_INPUT_DESERIALIZING        = 1
    ERROR_AT_CRM_OUTPUT_SERIALIZING         = 2
    ERROR_AT_CRM_FUNCTION_EXECUTING         = 3
    ERROR_AT_COMPO_INPUT_SERIALIZING        = 5
    ERROR_AT_COMPO_OUTPUT_DESERIALIZING     = 6
    ERROR_AT_COMPO_CRM_CALLING              = 7
    ERROR_RESOURCE_NOT_FOUND                = 701
    ERROR_RESOURCE_UNAVAILABLE              = 702
    ERROR_REGISTRY_UNAVAILABLE              = 705

class CCBaseError(Exception):
    """Base class for all C-Two-related errors."""

class CCError(CCBaseError):
    """
    General error class for C-Two.
    
    Parameters:
        code (ERROR_Code): The error code representing the type of error.
        message (str | None): Optional custom error message. Defaults to a generic message.
    """
    
    code: ERROR_Code
    message: str | None

    def __init__(self, code: ERROR_Code = ERROR_Code.ERROR_UNKNOWN, message: str | None = None):
        self.code = code
        self.message = message or 'Error occurred when using C-Two.'

    def __str__(self):
        return f'{self.code.name}: {self.message}'
    
    def __repr__(self):
        return f'CCError(code={self.code}, message={self.message})'

    @staticmethod
    def serialize(err: 'CCError' | None) -> bytes:
        """
        Serialize the error to bytes.
        
        Returns:
            bytes: Serialized error data.
        """
        if err is None:
            return b''

        return f'{err.code.value}:{err.message}'.encode('utf-8')
    
    @staticmethod
    def deserialize(data: memoryview) -> 'CCError' | None:
        """
        Deserialize bytes to an error object.
        
        Args:
            data (memoryview): Serialized error data.

        Returns:
            CCError: Deserialized error object.
        """
        if not data:
            return None

        parts = data.tobytes().decode('utf-8').split(':', 1)
        code_value = int(parts[0])
        message = parts[1] if len(parts) > 1 else None
        
        code = ERROR_Code(code_value)
        subclass = _CODE_TO_CLASS.get(code, CCError)
        obj = Exception.__new__(subclass)
        obj.code = code
        obj.message = message or 'Error occurred when using C-Two.'
        return obj

class CRMDeserializeInput(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when deserializing input at CRM' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING, message=message)

class CRMSerializeOutput(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when serializing output at CRM' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING, message=message)

class CRMExecuteFunction(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when executing function at CRM' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING, message=message)

class CompoSerializeInput(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when serializing input at Compo' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING, message=message)
        
class CompoDeserializeOutput(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when deserializing output at Compo' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING, message=message)
        
class CompoCRMCalling(CCError):
    def __init__(self, message: str | None = None):
        message = 'Error occurred when calling CRM from Compo' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_COMPO_CRM_CALLING, message=message)

class ResourceNotFound(CCError):
    """Raised when a named resource cannot be resolved by any relay."""
    ERROR_CODE = 701

    def __init__(self, message: str | None = None):
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_NOT_FOUND, message=message or 'Resource not found')

class ResourceUnavailable(CCError):
    """Raised when a resource exists but is not reachable."""
    ERROR_CODE = 702

    def __init__(self, message: str | None = None, detail: str | None = None):
        msg = message or 'Resource unavailable'
        if detail:
            msg = f'{msg}: {detail}'
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_UNAVAILABLE, message=msg)

class RegistryUnavailable(CCError):
    """Raised when no relay is available for name resolution."""
    ERROR_CODE = 705

    def __init__(self, message: str | None = None):
        super().__init__(code=ERROR_Code.ERROR_REGISTRY_UNAVAILABLE, message=message or 'Registry unavailable')

_CODE_TO_CLASS: dict[int, type] = {
    ERROR_Code.ERROR_AT_CRM_INPUT_DESERIALIZING:    CRMDeserializeInput,
    ERROR_Code.ERROR_AT_CRM_OUTPUT_SERIALIZING:     CRMSerializeOutput,
    ERROR_Code.ERROR_AT_CRM_FUNCTION_EXECUTING:     CRMExecuteFunction,
    ERROR_Code.ERROR_AT_COMPO_INPUT_SERIALIZING:    CompoSerializeInput,
    ERROR_Code.ERROR_AT_COMPO_OUTPUT_DESERIALIZING: CompoDeserializeOutput,
    ERROR_Code.ERROR_AT_COMPO_CRM_CALLING:          CompoCRMCalling,
    ERROR_Code.ERROR_RESOURCE_NOT_FOUND:            ResourceNotFound,
    ERROR_Code.ERROR_RESOURCE_UNAVAILABLE:          ResourceUnavailable,
    ERROR_Code.ERROR_REGISTRY_UNAVAILABLE:          RegistryUnavailable,
}
