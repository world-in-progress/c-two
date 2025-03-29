import struct
from functools import wraps
import google.protobuf.message as message
from proto.common import base_pb2 as base

class Wrapper:
    """
    Wrapper is a class to:  
    - forward: convert runtime data to ProtoBuf data
    - inverse: convert ProtoBuf data to runtime data
    """
    def __init__(self, forward: callable, inverse: callable):
        self.forward = forward
        self.inverse = inverse

WRAPPER_MAP: dict[str, Wrapper] = {}

def register_wrapper(proto_type: message.Message, serialize: callable, deserialize: callable):
    name = proto_type.DESCRIPTOR.full_name
    WRAPPER_MAP[name] = Wrapper(serialize, deserialize)

def get_wrapper(proto_type: message.Message) -> Wrapper | None:
    name = proto_type.DESCRIPTOR.full_name
    return None if name not in WRAPPER_MAP else WRAPPER_MAP[name]

# Register base wrappers #####################################################

# Wrapper for BaseResponse
def _forward_base_res(code: base.Code, message: str, _: None = None) -> base.BaseResponse:
    proto = base.BaseResponse()
    proto.code = code
    proto.message = message
    return proto

def _inverse_base_res(proto: base.BaseResponse) -> dict[str, base.Code | str]:
    return {
        'code': proto.code,
        'message': proto.message
    }

register_wrapper(base.BaseResponse, _forward_base_res, _inverse_base_res)

# Wrapper decorator for CRM ##################################################

def cc_wrapper(input_proto:message.Message | None = None, output_proto: message.Message | None = None, static: bool = False) -> callable:
    
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def wrapper(*args: any) -> tuple[any, any]:
            input_proto_name = None if input_proto is None else input_proto.DESCRIPTOR.full_name
            output_proto_name = None if output_proto is None else output_proto.DESCRIPTOR.full_name
            
            # Get converter
            if input_proto_name is not None and input_proto_name not in WRAPPER_MAP:
                raise ValueError(f'No converter defined for method: {input_proto_name}')
            if output_proto_name is not None and output_proto_name not in WRAPPER_MAP:
                raise ValueError(f'No converter defined for method: {output_proto_name}')
            input_wrapper = None if input_proto_name is None else WRAPPER_MAP[input_proto_name].inverse
            output_wrapper = None if output_proto_name is None else WRAPPER_MAP[output_proto_name].forward
            
            # Convert input and run method
            try:
                if static:
                    args_converted = input_wrapper(args[0]) if (args and input_wrapper is not None) else tuple()
                    result = func(*args_converted)
                else:
                    if len(args) < 1:
                        raise ValueError("Instance method requires self, but only get one argument.")
                    obj = args[0]
                    request = args[1] if len(args) > 1 else None
                    args_converted = input_wrapper(request) if (request is not None and input_wrapper is not None) else tuple()
                    result = func(obj, *args_converted)
                    
                code = base.SUCCESS
                _message = 'Processing successed'
                
            except Exception as e:
                result = None
                code = base.ERROR_INVALID
                _message = f'Error happened: {e}'
            
            serialized_resposne: str = get_wrapper(base.BaseResponse).forward(code, _message).SerializeToString()
            serialized_result: str = b'' if output_wrapper is None else output_wrapper(result).SerializeToString()
            combined_resposne = _add_length_prefix(serialized_resposne) + _add_length_prefix(serialized_result)
            return result, combined_resposne
        
        return wrapper
    
    return decorator

# Helper ##################################################
def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes
