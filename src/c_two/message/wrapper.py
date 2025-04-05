import struct
import inspect
import pyarrow as pa
from functools import wraps
from abc import ABCMeta, abstractmethod
from typing import get_type_hints, get_origin, get_args
from .globals import Code, BASE_RESPONSE

class WrapperMeta(ABCMeta):
    """
    WrapperMeta
    --
    A metaclass for AbstractWrapper that:
    1. Automatically converts 'serialize' and 'deserialize' methods to static methods
    2. Registers the wrapper class in the global wrapper map
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        # Static
        if 'serialize' in attrs and not isinstance(attrs['serialize'], staticmethod):
            attrs['serialize'] = staticmethod(attrs['serialize'])
        if 'deserialize' in attrs and not isinstance(attrs['deserialize'], staticmethod):
            attrs['deserialize'] = staticmethod(attrs['deserialize'])
        
        # Register
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        if name != 'AbstractWrapper' and hasattr(cls, 'serialize') and hasattr(cls, 'deserialize'):
            _WRAPPER_MAP[name] = cls
            # print(f"Auto-registered wrapper class: {name}")
        return cls

class AbstractWrapper(metaclass=WrapperMeta):
    """
    AbstractWrapper
    --
    A wrapper class specification.  
    Any wrapper must have following methods:
    - serialize: convert runtime `args` to `bytes` message
    - deserialize: convert `bytes` message to runtime `args`
    """
    @staticmethod
    @abstractmethod
    def serialize(self, *args: any) -> bytes:
        ...
        
    @staticmethod
    @abstractmethod
    def deserialize(self, bytes: any) -> any:
        ...

class Wrapper:
    """
    Wrapper is a class to:  
    - serialize: convert runtime `args` to `bytes` message
    - deserialize: convert `bytes` message to runtime `args`
    """
    def __init__(self, serialize: callable, deserialize: callable):
        self.serialize = serialize
        self.deserialize = deserialize

_WRAPPER_MAP: dict[str, Wrapper] = {}

def create_wrapper(wrapper_name: str, serialize: callable, deserialize: callable):
    name = wrapper_name
    _WRAPPER_MAP[name] = Wrapper(serialize, deserialize)

def register_wrapper(wrapper: AbstractWrapper):
    name = wrapper.__name__
    _WRAPPER_MAP[name] = wrapper

def get_wrapper(wrapper_name: str) -> Wrapper | None:
    name = wrapper_name
    return None if name not in _WRAPPER_MAP else _WRAPPER_MAP[name]

# Register base wrappers #####################################################

# Wrapper for BaseResponse
class BaseResponse(AbstractWrapper):
    def serialize(code: Code, message: str) -> bytes:
        schema = pa.schema([
            pa.field('code', pa.int8()),
            pa.field('message', pa.string()),
        ])
        
        data = {
            'code': code.value,
            'message': message
        }
        
        table = pa.Table.from_pylist([data], schema)
        return serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> dict[str, int | str]:
        row = deserialize_to_rows(arrow_bytes)[0]
        return {
            'code': Code(row['code']),
            'message': row['message']
        }

# Wrapper decorator for CRM ##################################################

def transfer(input: str | None = None, output: str | None = None) -> callable:
    
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def transfer_wrapper(*args: any) -> any:
            def com_to_crm(*args: any) -> any:
                method_name = func.__name__
                
                # Get wrapper
                if input is not None and input not in _WRAPPER_MAP:
                    raise ValueError(f'No wrapper defined for method: {input}')
                if output is not None and output not in _WRAPPER_MAP:
                    raise ValueError(f'No wrapper defined for method: {output}')
                input_wrapper = None if input is None else _WRAPPER_MAP[input].serialize
                output_wrapper = None if output is None else _WRAPPER_MAP[output].deserialize
                
                # Convert input and run method
                if len(args) < 1:
                    raise ValueError("Instance method requires self, but only get one argument.")
                
                obj = args[0]
                request = args[1:] if len(args) > 1 else None
                
                try:
                    args_converted = input_wrapper(*request) if (request is not None and input_wrapper is not None) else tuple()
                    result_bytes = obj.client.call(method_name, args_converted)
                    return None if output_wrapper is None else output_wrapper(result_bytes)
                except Exception as e:
                    error_context = "input serialization at Component side" if "args_converted" not in locals() else \
                                    "CRM function call" if "result_bytes" not in locals() else \
                                    "output deserialization at Component side"
                    print(f"Error during {error_context}: {e}")
                    raise e
            
            @wraps(func)
            def crm_to_com(*args: any) -> tuple[any, any]:
                
                # Get wrapper
                if input is not None and input not in _WRAPPER_MAP:
                    raise ValueError(f'No wrapper defined for method: {input}')
                if output is not None and output not in _WRAPPER_MAP:
                    raise ValueError(f'No wrapper defined for method: {output}')
                input_wrapper = None if input is None else _WRAPPER_MAP[input].deserialize
                output_wrapper = None if output is None else _WRAPPER_MAP[output].serialize
                
                # Convert input and run method
                try:
                    if len(args) < 1:
                        raise ValueError("Instance method requires self, but only get one argument.")
                    
                    obj = args[0]
                    request = args[1] if len(args) > 1 else None
                    
                    args_converted = input_wrapper(request) if (request is not None and input_wrapper is not None) else tuple()
                    result = func(obj, *args_converted)
                    
                    code = Code.SUCCESS
                    message = 'Processing succeeded'
                    
                except Exception as e:
                    error_context = "input deserialization at CRM side" if "args_converted" not in locals() else \
                                    "CRM function execution" if "result" not in locals() else \
                                    "output serialization at CRM side"
                    print(f"Error during {error_context}: {e}")
                    result = None
                    code = Code.ERROR_INVALID
                    message = f'Error occurred: {e}'
                
                serialized_response: str = get_wrapper(BASE_RESPONSE).serialize(code, message)
                serialized_result: str = b'' if (output_wrapper is None or result is None) else output_wrapper(result)
                combined_response = _add_length_prefix(serialized_response) + _add_length_prefix(serialized_result)
                return result, combined_response
            
            if not args:
                raise ValueError("No arguments provided to determine direction.")
            
            obj = args[0]
            if not hasattr(obj, 'direction'):
                raise AttributeError("The object does not have a 'direction' attribute.")
            
            if obj.direction == '->':
                return com_to_crm(*args)
            elif obj.direction == '<-':
                return crm_to_com(*args)
            else:
                raise ValueError(f"Invalid direction value: {obj.direction}. Expected '->' or '<-'.")
        
        return transfer_wrapper
    
    return decorator

def auto_transfer(func = None) -> callable:
    
    def create_wrapper(func: callable) -> callable:
        # Get all registered wrappers and make parameter maps for all wrappers
        wrapper_infos = []
        wrappers =  _WRAPPER_MAP.values()
        for index, wrapper in enumerate(wrappers):
            serialize_func = wrapper.serialize
            serialize_sig = list(inspect.signature(serialize_func).parameters.values())
            serialize_param_map = {}
            for param in serialize_sig:
                serialize_param_map[param.name] = param.annotation
            wrapper_infos.append({
                'name': list(_WRAPPER_MAP.keys())[index],
                'param_map': serialize_param_map
            })
        
        # Skip 'self' and get parameters
        sig = inspect.signature(func)
        parameters = list(sig.parameters.values())
        if parameters and parameters[0].name == 'self':
            parameters = parameters[1:]
        
        # Try to find a matching wrapper for input parameters
        input_wrapper_name = None 
        if parameters:
            input_param_map = {}
            for param in parameters:
                input_param_map[param.name] = param.annotation
            
            for wrapper_info in wrapper_infos:
                if wrapper_info['param_map'] == input_param_map:
                    input_wrapper_name = wrapper_info['name']
                    break
            if input_wrapper_name is None:
                raise ValueError(f"No matching wrapper found for input parameters: {input_param_map}")

        # Get type hints for the function
        type_hints = get_type_hints(func)
        
        # Try to find a matching wrapper for output parameters
        output_wrapper_name = None
        if 'return' in type_hints:
            return_type = type_hints['return']
            return_name = return_type.__name__
                
            if return_name in _WRAPPER_MAP:
                output_wrapper_name = return_name
            else:
                return_param_map = {}
                if return_name == 'tuple':
                    return_args = get_args(return_type)
                    for i, arg in enumerate(return_args):
                        return_param_map[f'param{i}'] = arg
                    
                    for wrapper_info in wrapper_infos:
                        if return_param_map and len(wrapper_info['param_map']) == len(return_param_map):
                            # Check hit or not
                            hit = True
                            for i in range (len(return_param_map)):
                                if list(wrapper_info['param_map'].values())[i] != list(return_param_map.values())[i]:
                                    hit = False
                                    break
                            if hit:
                                output_wrapper_name = wrapper_info['name']
                                break
                   
                else:
                    for wrapper_info in wrapper_infos:
                        
                        wrapper_param_list = list(wrapper_info['param_map'].values())
                        if len(wrapper_param_list) == 1 and wrapper_param_list[0] == return_type:
                            output_wrapper_name = wrapper_info['name']
                            break
            
            if output_wrapper_name is None:
                raise ValueError(f"No matching wrapper found for output parameters: {return_type}")
        
        @wraps(func)
        def wrapped_func(*args: any) -> any:
            decorated = transfer(input=input_wrapper_name, output=output_wrapper_name)(func)
            return decorated(*args)
        return wrapped_func
    
    if func is None:
        # @auto_transfer() syntax - return a decorator
        return create_wrapper
    else:
        # @auto_transfer syntax - apply decorator directly to the function
        return create_wrapper(func)

# Helper ##################################################

def serialize_from_table(table: pa.Table) -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    binary_data = sink.getvalue().to_pybytes()
    return binary_data

def deserialize_to_table(serialized_data: bytes) -> pa.Table:
    buffer = pa.py_buffer(serialized_data)
    with pa.ipc.open_stream(buffer) as reader:
        table = reader.read_all()
    return table

def deserialize_to_rows(serialized_data: bytes) -> dict:
    buffer = pa.py_buffer(serialized_data)

    with pa.ipc.open_stream(buffer) as reader:
        table = reader.read_all()

    return table.to_pylist()

def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes
