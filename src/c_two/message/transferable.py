import json
import struct
import inspect
from abc import ABCMeta
from functools import wraps
from typing import get_type_hints, get_args, get_origin, Any, Callable
from dataclasses import dataclass, is_dataclass
from pydantic import BaseModel, create_model
from .globals import Code, BASE_RESPONSE

# Global Caches ###################################################################

_TRANSFERABLE_MAP: dict[str, 'Transferable'] = {}
_TRANSFERABLE_INFOS: list[dict[str, dict[str, type] | str]] = []

# Definition of Transferable ######################################################

class TransferableMeta(ABCMeta):
    """
    TransferableMeta
    --
    A metaclass for Transferable that:
    1. Automatically converts 'serialize' and 'deserialize' methods to static methods
    2. Ensures all subclasses of Transferable are dataclasses
    3. Registers the transferable class in the global transferable map and transferable information list
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        # Static
        if 'serialize' in attrs and not isinstance(attrs['serialize'], staticmethod):
            attrs['serialize'] = staticmethod(attrs['serialize'])
        if 'deserialize' in attrs and not isinstance(attrs['deserialize'], staticmethod):
            attrs['deserialize'] = staticmethod(attrs['deserialize'])
        
        # Create the class
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        
        # Register
        if name != 'Transferable' and hasattr(cls, 'serialize') and hasattr(cls, 'deserialize'):
            if not is_dataclass(cls):
                cls = dataclass(cls)
                
            _TRANSFERABLE_MAP[name] = cls
            
            serialize_func = attrs['serialize']
            serialize_sig = list(inspect.signature(serialize_func).parameters.values())
            serialize_param_map = {}
            for param in serialize_sig:
                serialize_param_map[param.name] = param.annotation
            _TRANSFERABLE_INFOS.append({
                'name': name,
                'param_map': serialize_param_map
            })
        return cls

class Transferable(metaclass=TransferableMeta):
    """
    Transferable
    --
    A base specification for classes that can be transferred between `Component` and `CRM`.  
    Transferable classes are automatically converted to `dataclasses` and should implement the methods:
    - serialize: convert runtime `args` to `bytes` message
    - deserialize: convert `bytes` message to runtime `args`
    """
    def serialize(*args: any) -> bytes:
        """
        serialize is a static method, and the decorator @staticmethod is added in the metaclass.  
        No Need To Add @staticmethod Here.
        """
        ...

    def deserialize(bytes: any) -> any:
        """
        deserialize is a static method, and the decorator @staticmethod is added in the metaclass.  
        No Need To Add @staticmethod Here.
        """
        ...

# Transferable-related interfaces #################################################

def register_transferable(transferable: Transferable):
    name = transferable.__name__
    _TRANSFERABLE_MAP[name] = transferable

def get_transferable(transferable_name: str) -> Transferable | None:
    name = transferable_name
    return None if name not in _TRANSFERABLE_MAP else _TRANSFERABLE_MAP[name]

# Transferable-related decorators #################################################

def transferable(cls: type) -> type:
    """
    A decorator to make a class automatically inherit from Transferable.
    """
    # Dynamically create a new class that inherits from both the original class and Transferable
    return type(cls.__name__, (cls, Transferable), dict(cls.__dict__))

def transfer(input: str | None = None, output: str | None = None) -> callable:
    
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def transfer_wrapper(*args: any) -> any:
            def com_to_crm(*args: any) -> any:
                method_name = func.__name__
                
                # Get transferable
                if input is not None and input not in _TRANSFERABLE_MAP:
                    raise ValueError(f'No transferable defined for method: {input}')
                if output is not None and output not in _TRANSFERABLE_MAP:
                    raise ValueError(f'No transferable defined for method: {output}')
                input_transferable = None if input is None else _TRANSFERABLE_MAP[input].serialize
                output_transferable = None if output is None else _TRANSFERABLE_MAP[output].deserialize
                
                # Convert input and run method
                if len(args) < 1:
                    raise ValueError('Instance method requires self, but only get one argument.')
                
                obj = args[0]
                request = args[1:] if len(args) > 1 else None
                
                try:
                    args_converted = input_transferable(*request) if (request is not None and input_transferable is not None) else None
                    result_bytes = obj.client.call(method_name, args_converted)
                    return None if output_transferable is None else output_transferable(result_bytes)
                except Exception as e:
                    error_context = 'input serialization at Component side' if 'args_converted' not in locals() else \
                                    'CRM function call' if 'result_bytes' not in locals() else \
                                    'output deserialization at Component side'
                    print(f'Error during {error_context}: {e}')
                    raise e
            
            @wraps(func)
            def crm_to_com(*args: any) -> tuple[any, any]:
                
                # Get transferable
                if input is not None and input not in _TRANSFERABLE_MAP:
                    raise ValueError(f'No transferable defined for method: {input}')
                if output is not None and output not in _TRANSFERABLE_MAP:
                    raise ValueError(f'No transferable defined for method: {output}')
                input_transferable = None if input is None else _TRANSFERABLE_MAP[input].deserialize
                output_transferable = None if output is None else _TRANSFERABLE_MAP[output].serialize
                
                # Convert input and run method
                try:
                    if len(args) < 1:
                        raise ValueError('Instance method requires self, but only get one argument.')
                    
                    obj = args[0]
                    request = args[1] if len(args) > 1 else None
                    
                    args_converted = input_transferable(request) if (request is not None and input_transferable is not None) else tuple()
                    result = func(obj, *args_converted)
                    
                    code = Code.SUCCESS
                    message = 'Processing succeeded'
                    
                except Exception as e:
                    error_context = 'input deserialization at CRM side' if 'args_converted' not in locals() else \
                                    'CRM function execution' if 'result' not in locals() else \
                                    'output serialization at CRM side'
                    print(f'Error during {error_context}: {e}')
                    result = None
                    code = Code.ERROR_INVALID
                    message = f'Error occurred: {e}'
                
                # Create a serialized response based on the serialized_response and serialized_result
                serialized_response: str = get_transferable(BASE_RESPONSE).serialize(code, message)
                serialized_result = b''
                if output_transferable is not None and result is not None:
                    # Unpack tuple arguments or pass single argument based on result type
                    serialized_result = (
                        output_transferable(*result) if isinstance(result, tuple)
                        else output_transferable(result)
                    )
                combined_response = _add_length_prefix(serialized_response) + _add_length_prefix(serialized_result)
                return result, combined_response
            
            if not args:
                raise ValueError('No arguments provided to determine direction.')
            
            obj = args[0]
            if not hasattr(obj, 'direction'):
                raise AttributeError('The object does not have a "direction" attribute.')
            
            if obj.direction == '->':
                return com_to_crm(*args)
            elif obj.direction == '<-':
                return crm_to_com(*args)
            else:
                raise ValueError(f'Invalid direction value: {obj.direction}. Expected "->" or "<-".')
        
        return transfer_wrapper
    
    return decorator

def auto_transfer(func = None) -> callable:

    def create_wrapper(func: callable) -> callable:

        # --- Input Matching using Pydantic Model Comparison ---
        input_model = _create_pydantic_model_from_func_sig(func)
        input_transferable_name = None

        if input_model is not None:
            # Get fields from the generated Pydantic model
            input_model_fields = getattr(input_model, 'model_fields', {})

            is_empty_input = not bool(input_model_fields)

            for info in _TRANSFERABLE_INFOS:
                registered_param_map = info.get('param_map', {})
                is_empty_registered = not bool(registered_param_map)

                # Match if both are empty
                if is_empty_input and is_empty_registered:
                    input_transferable_name = info['name']
                    break

                # Match if field names and types align
                if not is_empty_input and not is_empty_registered:
                    # Compare names first
                    if set(input_model_fields.keys()) == set(registered_param_map.keys()):
                        # Compare types (more robustly)
                        match = True
                        for name, field_info in input_model_fields.items():
                            # Get the type annotation from Pydantic's FieldInfo
                            pydantic_type = field_info.annotation
                            registered_type = registered_param_map.get(name)
                            # Simple type comparison (might need refinement for complex types like generics)
                            if pydantic_type != registered_type:
                                match = False
                                break
                        if match:
                            input_transferable_name = info['name']
                            break

            # Error only if input fields were expected but no match found
            if input_transferable_name is None and not is_empty_input:
                 func_sig_str = ", ".join(f"{n}: {getattr(f.annotation, '__name__', repr(f.annotation))}" for n, f in input_model_fields.items())
                 available_sigs = "\n".join([f"  - {i['name']}: { {n: getattr(t, '__name__', repr(t)) for n, t in i.get('param_map', {}).items()} }" for i in _TRANSFERABLE_INFOS])
                 raise ValueError(
                     f"No matching transferable found for Pydantic-derived input signature ({func_sig_str}) of {func.__qualname__}.\n"
                     f"Available transferable input signatures:\n{available_sigs}"
                 )

        else:
             print(f"Warning: Could not create Pydantic model for input signature of {func.__qualname__}. Proceeding without specific input transferable.")
             input_transferable_name = None

        # --- Output Matching (compares types directly) ---
        type_hints = get_type_hints(func)
        output_transferable_name = None
        if 'return' in type_hints:
            return_type = type_hints['return']
            if return_type is None or return_type is type(None):
                 output_transferable_name = None
            else:
                return_type_name = getattr(return_type, '__name__', str(return_type))
                if return_type_name in _TRANSFERABLE_MAP:
                    output_transferable_name = return_type_name
                else:
                    origin = get_origin(return_type)
                    args = get_args(return_type)
                    expected_output_types = []
                    if origin is tuple:
                        expected_output_types = list(args)
                    elif return_type is not None and return_type is not type(None):
                        expected_output_types = [return_type]

                    if expected_output_types:
                        for info in _TRANSFERABLE_INFOS:
                            registered_param_map = info.get('param_map', {})
                            registered_output_types = list(registered_param_map.values())
                            if expected_output_types == registered_output_types:
                                output_transferable_name = info['name']
                                break
                if output_transferable_name is None and not (return_type is None or return_type is type(None)):
                    expected_types_str = [getattr(t, '__name__', repr(t)) for t in expected_output_types]
                    available_output_sigs = "\n".join([f"  - {i['name']}: {[getattr(t, '__name__', repr(t)) for t in i.get('param_map', {}).values()]}" for i in _TRANSFERABLE_INFOS])
                    raise ValueError(
                        f"No matching transferable found for output type(s) {expected_types_str} of {func.__qualname__}.\n"
                        f"Available transferable output signatures (types):\n{available_output_sigs}"
                    )
        else:
             output_transferable_name = None


        # --- Wrapping ---
        transfer_decorator = transfer(input=input_transferable_name, output=output_transferable_name)
        wrapped_func = transfer_decorator(func)

        @wraps(func)
        def final_wrapper(*args, **kwargs):
             return wrapped_func(*args, **kwargs)

        return final_wrapper

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
             raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)

# Helpers #########################################################################

def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes

def _create_pydantic_model_from_func_sig(func: Callable, model_name_suffix: str = "InputModel") -> type[BaseModel] | None:
    """
    Creates a Pydantic model representing the input signature of a function,
    skipping the first parameter if it's 'self' or 'cls'.
    Returns None if signature cannot be determined or on error.
    """
    try:
        signature = inspect.signature(func)
        type_hints = get_type_hints(func)

        fields = {}
        param_names = list(signature.parameters.keys())

        for i, name in enumerate(param_names):
            param = signature.parameters[name]
            # Skip 'self' or 'cls' only if it's the *first* parameter
            if i == 0 and name in ('self', 'cls'):
                continue
            
            annotation = type_hints.get(name, param.annotation)
            # Pydantic needs Ellipsis (...) for required fields without defaults
            default = ... if param.default is inspect.Parameter.empty else param.default
            # Use Any if no annotation found, Pydantic handles Any
            actual_annotation = annotation if annotation is not inspect.Parameter.empty else Any

            fields[name] = (actual_annotation, default)

        # Create the dynamic model
        model_name = f"{func.__qualname__.replace('.', '_')}_{model_name_suffix}"
        # Handle functions with no relevant parameters (e.g., only self)
        if not fields:
             # Return an empty model definition
             return create_model(model_name)

        return create_model(model_name, **fields)

    except ValueError: # Handle functions without signatures (like some built-ins)
        print(f"Warning: Could not get signature for {func.__qualname__}")
        return None
    except Exception as e:
        print(f"Error creating Pydantic model for {func.__qualname__}: {e}")
        return None

# Register base transferables #####################################################

class BaseResponse(Transferable):
    def serialize(code: Code, message: str) -> bytes:
        res = {
            'code': code.value,
            'message': message
        }
        return json.dumps(res).encode('utf-8')

    def deserialize(res_bytes: memoryview) -> dict[str, int | str]:
        res = json.loads(res_bytes.tobytes().decode('utf-8'))
        return {
            'code': Code(res['code']),
            'message': res['message']
        }
