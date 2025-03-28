from functools import wraps
from proto.common import base_pb2 as base

# Message wrapper for communication based on ProtoBuf ########################

class Wrapper:
    """
    Wrapper is a class to make runtime data serialized to ProtoBuf data, or make ProtoBuf data deserialized to runtime data.
    """
    def __init__(self, serialize: callable, deserialize: callable):
        self.serialize = serialize
        self.deserialize = deserialize

# Wrapper map ################################################################

WRAPPER_MAP: dict[str, Wrapper] = {}

def Register_Wrapper(name: str, serialize: callable, deserialize: callable):
    WRAPPER_MAP[name] = Wrapper(serialize, deserialize)

def Get_Wrapper(name: str) -> Wrapper | None:
    return None if name not in WRAPPER_MAP else WRAPPER_MAP[name]

# proto: BaseResponse
def serialize_base_res(status: base.Status, message: str, _: any) -> base.BaseResponse:
    proto = base.BaseResponse()
    proto.status = status
    proto.message = message
    return proto

def deserialize_base_res(proto: base.BaseResponse) -> tuple[base.Status, str]:
    status = proto.status
    message = proto.message
    return status, message

# Register BaseResposne
Register_Wrapper(base.BaseResponse.DESCRIPTOR.full_name, serialize_base_res, deserialize_base_res)

# Wrapper decorator for CRM ##################################################

def cc_wrapper(input_schema: str = '', output_schema: str = '', static: bool = False) -> callable:
    
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def wrapper(*args: any) -> tuple[any, any]:
            # Get converter
            if input_schema != '' and input_schema not in WRAPPER_MAP:
                raise ValueError(f'No converter defined for method: {input_schema}')
            if output_schema != '' and output_schema not in WRAPPER_MAP:
                raise ValueError(f'No converter defined for method: {output_schema}')
            input_wrapper = WRAPPER_MAP[input_schema].deserialize if input_schema != '' else None
            output_wrapper = WRAPPER_MAP[output_schema].serialize if output_schema != '' else WRAPPER_MAP[base.BaseResponse.DESCRIPTOR.full_name].serialize
            
            # Convert input and run method
            try:
                if static:
                    args_converted = input_wrapper(args[0]) if args else tuple()
                    result = func(*args_converted)
                else:
                    if len(args) < 1:
                        raise ValueError("Instance method requires self, but only get one argument.")
                    obj = args[0]
                    request = args[1] if len(args) > 1 else None
                    args_converted = input_wrapper(request) if request is not None else tuple()
                    result = func(obj, *args_converted)
                    
                status = base.SUCCESS
                message = 'Processing successed'
                
            except Exception as e:
                result = None
                status = base.ERROR_INVALID
                message = f'Error happened: {e}'
            return result, output_wrapper(status, message, result)
        
        return wrapper
    
    return decorator
