import struct
import pyarrow as pa
from functools import wraps
from .globals import Code, BASE_RESPONSE

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

def register_wrapper(wrapper_name: str, serialize: callable, deserialize: callable):
    name = wrapper_name
    _WRAPPER_MAP[name] = Wrapper(serialize, deserialize)

def get_wrapper(wrapper_name: str) -> Wrapper | None:
    name = wrapper_name
    return None if name not in _WRAPPER_MAP else _WRAPPER_MAP[name]

# Register base wrappers #####################################################

# Wrapper for BaseResponse
def _forward_base_res(code: int, message: str) -> bytes:
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

def _inverse_base_res(arrow_bytes: bytes) -> dict[str, int | str]:
    row = deserialize_to_rows(arrow_bytes)[0]
    return {
        'code': row['code'],
        'message': row['message']
    }

register_wrapper(BASE_RESPONSE, _forward_base_res, _inverse_base_res)

# Wrapper decorator for CRM ##################################################

def transfer(input_name: str | None = None, output_name: str | None = None) -> callable:
    
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def transfer_wrapper(*args: any) -> any:
            def com_to_crm(*args: any) -> any:
                method_name = func.__name__
                input_proto_name = None if input_name is None else input_name
                output_proto_name = None if output_name is None else output_name
                
                # Get converter
                if input_proto_name is not None and input_proto_name not in _WRAPPER_MAP:
                    raise ValueError(f'No converter defined for method: {input_proto_name}')
                if output_proto_name is not None and output_proto_name not in _WRAPPER_MAP:
                    raise ValueError(f'No converter defined for method: {output_proto_name}')
                input_wrapper = None if input_proto_name is None else _WRAPPER_MAP[input_proto_name].serialize
                output_wrapper = None if output_proto_name is None else _WRAPPER_MAP[output_proto_name].deserialize
                
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
                input_proto_name = None if input_name is None else input_name
                output_proto_name = None if output_name is None else output_name
                
                # Get converter
                if input_proto_name is not None and input_proto_name not in _WRAPPER_MAP:
                    raise ValueError(f'No converter defined for method: {input_proto_name}')
                if output_proto_name is not None and output_proto_name not in _WRAPPER_MAP:
                    raise ValueError(f'No converter defined for method: {output_proto_name}')
                input_wrapper = None if input_proto_name is None else _WRAPPER_MAP[input_proto_name].deserialize
                output_wrapper = None if output_proto_name is None else _WRAPPER_MAP[output_proto_name].serialize
                
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
