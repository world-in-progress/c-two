from __future__ import annotations
import pickle
import inspect
import logging
from abc import ABCMeta
from functools import wraps
from pydantic import BaseModel, create_model
from dataclasses import dataclass, is_dataclass
from typing import get_type_hints, get_args, get_origin, Any, Callable, TypeVar, Type

import struct as _struct

from .. import error


def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = _struct.pack('>Q', length)
    return prefix + message_bytes

T = TypeVar('T')
logger = logging.getLogger(__name__)

# Global Caches ###################################################################

_TRANSFERABLE_MAP: dict[str, Transferable] = {}
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

            # Register the class except for classes in 'Default' module
            if cls.__module__ != 'Default':
                # Register the class in the global transferable map
                full_name = register_transferable(cls)
                
                # Register the class in the global transferable information list
                serialize_func = attrs['serialize']
                serialize_sig = list(inspect.signature(serialize_func).parameters.values())
                serialize_param_map = {}
                for param in serialize_sig:
                    serialize_param_map[param.name] = param.annotation
                _TRANSFERABLE_INFOS.append({
                    'name': full_name,
                    'module': cls.__module__,
                    'param_map': serialize_param_map
                })

                logger.debug(f'Registered transferable: {full_name}')
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

# Default Transferable Creation Factory ###########################################

def _default_serialize_func(*args):
    """Default serialization function for output transferables."""
    try:
        if len(args) == 1:
            return pickle.dumps(args[0])
        else:
            return pickle.dumps(args)
    except Exception as e:
        logger.error(f'Failed to serialize output data: {e}')
        raise

def _default_deserialize_func(data: memoryview | None):
    # b'' is used as a wire sentinel for None results (see crm_to_com:
    # serialized_result = b'' when result is None).  Must check both
    # None *and* empty bytes so the sentinel round-trips correctly,
    # while still allowing legitimate empty-bytes payloads when the
    # serializer actually produces them (pickle.dumps(b'') is non-empty).
    if data is None or len(data) == 0:
        return None
    return pickle.loads(data)

def _is_single_bytes_param(func, filtered_params, type_hints) -> bool:
    """Check if a function takes exactly one parameter of type bytes."""
    if len(filtered_params) != 1:
        return False
    param_type = type_hints.get(filtered_params[0])
    return param_type is bytes


def _is_bytes_return(func) -> bool:
    """Check if a function has return type bytes."""
    type_hints = get_type_hints(func)
    return type_hints.get('return') is bytes


def create_default_transferable(func, is_input: bool):
    """
    Factory function to dynamically create a Transferable class for a given function.

    This factory analyzes the function signature and creates a transferable class
    that uses pickle-based serialization as a fallback.

    Args:
        func: The function to create a transferable for
        is_input: True for input parameters, False for return values

    Returns:
        A dynamically created Transferable class
    """
    if is_input:
        # Create dynamic class name for input
        class_name = f'Default{func.__name__.title()}InputTransferable'
        
        # Create transferable for function input parameters
        sig = inspect.signature(func)
        param_names = list(sig.parameters.keys())
        type_hints = get_type_hints(func)
        
        # Filter out 'self' or 'cls' if they are the first parameter
        filtered_params = []
        for i, name in enumerate(param_names):
            if i == 0 and name in ('self', 'cls'):
                continue
            filtered_params.append(name)

        # Fast path: single bytes parameter — skip pickle entirely.
        # The deserializer returns the data as-is (including memoryview) to
        # enable zero-copy reads when the transport provides memoryview.
        if _is_single_bytes_param(func, filtered_params, type_hints):
            class DynamicInputTransferable(Transferable):
                __module__ = 'Default'

                def serialize(data) -> bytes:
                    if isinstance(data, (bytes, memoryview)):
                        return data
                    raise TypeError(f"bytes fast path: expected bytes or memoryview, got {type(data).__name__}")

                def deserialize(data: memoryview | bytes):
                    if data is None:
                        return None
                    return data

        else:
            # Define the dynamic transferable class for input.
            # pickle.loads() accepts memoryview since Python 3.8, so we
            # can safely mark the default pickle-based class as
            # memoryview-aware to avoid unnecessary bytes() conversion.
            class DynamicInputTransferable(Transferable):
                __module__ = 'Default'  # mark as default
                
                def serialize(*args) -> bytes:
                    """Default serialization: pickle args directly as tuple."""
                    try:
                        if len(args) == 1:
                            return pickle.dumps(args[0])
                        return pickle.dumps(args)
                    except Exception as e:
                        logger.error(f'Failed to serialize input data: {e}')
                        raise
                
                def deserialize(data: memoryview | bytes):
                    """Default deserialization: unpickle and return value or tuple."""
                    try:
                        if data is None or len(data) == 0:
                            return None
                        unpickled = pickle.loads(data)
                        return unpickled
                    except Exception as e:
                        raise ValueError(f"Failed to deserialize input data for {func.__name__}: {e}")
        
        # Set the dynamic class properties
        DynamicInputTransferable._is_input = True
        DynamicInputTransferable._original_func = func
        DynamicInputTransferable.__name__ = class_name
        DynamicInputTransferable._type_hints = type_hints
        DynamicInputTransferable.__qualname__ = class_name
        DynamicInputTransferable._param_names = filtered_params
        
        return DynamicInputTransferable
    
    else:
        # Create dynamic class name for output
        class_name = f'Default{func.__name__.title()}OutputTransferable'
        
        # Create transferable for function return value
        type_hints = get_type_hints(func)
        return_type = type_hints.get('return')

        # Fast path: bytes return type — skip pickle entirely.
        # The serializer passes memoryview through to enable zero-copy
        # SHM→SHM writes in transports that support it (ipc buddy).
        if return_type is bytes:
            def _bytes_output_serialize(val):
                if isinstance(val, (bytes, memoryview)):
                    return val
                raise TypeError(f"bytes fast path: expected bytes or memoryview, got {type(val).__name__}")
            serialize_func = staticmethod(_bytes_output_serialize)
            deserialize_func = staticmethod(lambda data: data if isinstance(data, (bytes, memoryview)) else pickle.loads(data) if data is not None else None)
        else:
            serialize_func = staticmethod(_default_serialize_func)
            deserialize_func = staticmethod(_default_deserialize_func)

        # Define the dynamic transferable class for output
        attrs = {
            '__module__': 'Default',
            'serialize': serialize_func,
            'deserialize': deserialize_func,
        }
        if return_type is bytes:
            pass  # already True above
        DynamicOutputTransferable = type(
            class_name,
            (Transferable,),
            attrs,
        )
        
        # Set the dynamic class properties
        DynamicOutputTransferable._is_input = False
        DynamicOutputTransferable.__name__ = class_name
        DynamicOutputTransferable._original_func = func
        DynamicOutputTransferable.__qualname__ = class_name
        DynamicOutputTransferable._return_type = return_type
        
        return DynamicOutputTransferable

# Transferable-related interfaces #################################################

def register_transferable(transferable: Transferable) -> str:
    full_name = f'{transferable.__module__}.{transferable.__name__}' if transferable.__module__ else transferable.__name__
    _TRANSFERABLE_MAP[full_name] = transferable
    return full_name

def get_transferable(full_name: str) -> Transferable | None:
    return None if full_name not in _TRANSFERABLE_MAP else _TRANSFERABLE_MAP[full_name]

# Transferable-related decorators #################################################

def transferable(cls: Type[T]) -> Type[T]:
    """
    A decorator to make a class automatically inherit from Transferable.
    """
    # Dynamically create a new class that inherits from both the original class and Transferable
    new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
    new_cls.__module__ = cls.__module__  # preserve the original module
    new_cls.__qualname__ = cls.__qualname__  # preserve the original qualified name
    return new_cls

def transfer(input: Transferable | None = None, output: Transferable | None = None) -> callable:

    def decorator(func: callable) -> callable:
        method_name = func.__name__
        
        def com_to_crm(*args: any) -> any:
            # Get transferable
            input_transferable = input.serialize if input else None
            output_transferable = output.deserialize if output else None

            try:
                if len(args) < 1:
                    raise ValueError('Instance method requires self, but only get one argument.')
                
                # Parse input args
                icrm = args[0]
                client = icrm.client
                request = args[1:] if len(args) > 1 else None

                # Thread fast path — skip all serialization/deserialization
                if getattr(client, 'supports_direct_call', False):
                    return client.call_direct(method_name, request or ())
                
                # Standard cross-process path
                stage = 'serialize_input'
                serialized_args = input_transferable(*request) if (request is not None and input_transferable is not None) else None
                
                # Call CRM
                stage = 'call_crm'
                result_bytes = client.call(method_name, serialized_args)
                
                # Deserialize output
                stage = 'deserialize_output'
                if not output_transferable:
                    return None
                return output_transferable(result_bytes)
            
            except error.CCBaseError:
                raise
            except Exception as e:
                if stage == 'serialize_input':
                    raise error.CompoSerializeInput(str(e)) from e
                elif stage == 'call_crm':
                    raise error.CompoCRMCalling(str(e)) from e
                else:
                    raise error.CompoDeserializeOutput(str(e)) from e
        
        def crm_to_com(*args: any) -> tuple[any, any]:
            # Get transferable
            input_transferable = input.deserialize if input else None
            output_transferable = output.serialize if output else None

            err = None
            result = None
            stage = 'deserialize_input'

            try:
                if len(args) < 1:
                    raise ValueError('Instance method requires self, but only get one argument.')
                
                # Parse input args
                iicrm = args[0]
                crm = iicrm.crm
                request = args[1] if len(args) > 1 else None
                
                # Deserialize input args if input_transferable is provided
                # And if deserialized_args is not a tuple, convert it to a tuple
                if request is not None and input_transferable is not None:
                    deserialized_args = input_transferable(request)
                else:
                    deserialized_args = tuple()
                if not isinstance(deserialized_args, tuple):
                    deserialized_args = (deserialized_args,)

                # Call CRM method with the same name
                crm_method = getattr(crm, method_name, None)
                if crm_method is None:
                    raise ValueError(f'Method "{method_name}" not found on CRM class.')

                stage = 'execute_function'
                result = crm_method(*deserialized_args)
                err = None
                
            except Exception as e:
                result = None
                if stage == 'deserialize_input':
                    err = error.CRMDeserializeInput(str(e))
                elif stage == 'execute_function':
                    err = error.CRMExecuteFunction(str(e))
                else:
                    err = error.CRMSerializeOutput(str(e))

            # Create a serialized response based on the serialized_error and serialized_result
            serialized_error: bytes = error.CCError.serialize(err)
            serialized_result = b''
            if output_transferable is not None and result is not None:
                # Unpack tuple arguments or pass single argument based on result type
                serialized_result = (
                    output_transferable(*result) if isinstance(result, tuple)
                    else output_transferable(result)
                )
            return (serialized_error, serialized_result)
        
        @wraps(func)
        def transfer_wrapper(*args: any) -> any:
            if not args:
                raise ValueError('No arguments provided to determine direction.')
            
            icrm = args[0]
            if not hasattr(icrm, 'direction'):
                raise AttributeError('The ICRM instance does not have a "direction" attribute.')
            
            if icrm.direction == '->':
                return com_to_crm(*args)
            elif icrm.direction == '<-':
                return crm_to_com(*args)
            else:
                raise ValueError(f'Invalid direction value: {icrm.direction}. Expected "->" or "<-".')
        
        return transfer_wrapper
    
    return decorator

def auto_transfer(func: callable | None = None) -> callable:
    def create_wrapper(func: callable) -> callable:
        # --- Input Matching ---
        # Priority 1: Direct Transferable lookup — if the method has exactly
        # one non-self parameter whose type is a registered Transferable,
        # use it directly (same strategy as the output matcher).
        input_model = _create_pydantic_model_from_func_sig(func)
        input_transferable: Transferable | None = None

        if input_model is not None:
            input_model_fields = getattr(input_model, 'model_fields', {})
            is_empty_input = not bool(input_model_fields)

            if not is_empty_input:
                # Direct lookup: single-param whose type is a registered
                # Transferable gets matched without fragile name/type
                # comparison against serialize() signatures.
                if len(input_model_fields) == 1:
                    field_info = next(iter(input_model_fields.values()))
                    param_type = field_info.annotation
                    param_module = getattr(param_type, '__module__', None)
                    param_name = getattr(param_type, '__name__', str(param_type))
                    full_name = f'{param_module}.{param_name}' if param_module else param_name
                    if full_name in _TRANSFERABLE_MAP:
                        input_transferable = get_transferable(full_name)

                # Priority 2: Field name+type comparison (legacy path).
                if input_transferable is None:
                    for info in _TRANSFERABLE_INFOS:
                        if info.get('module') != func.__module__:
                            continue
                        registered_param_map = info.get('param_map', {})
                        is_empty_registered = not bool(registered_param_map)
                        if not is_empty_input and not is_empty_registered:
                            if set(input_model_fields.keys()) == set(registered_param_map.keys()):
                                match = True
                                for name, fi in input_model_fields.items():
                                    pydantic_type = fi.annotation
                                    registered_type = registered_param_map.get(name)
                                    if pydantic_type != registered_type:
                                        match = False
                                        break
                                if match:
                                    input_transferable = get_transferable(info['name'])
                                    break

            # If no matching transferable found, create a default one (not registered and only used in this function)
            if input_transferable is None and not is_empty_input:
                input_transferable = create_default_transferable(func, is_input=True)

        # --- Output Matching (compares types directly) ---
        type_hints = get_type_hints(func)
        output_transferable: Transferable | None = None
        
        if 'return' in type_hints:
            return_type = type_hints['return']
            if return_type is None or return_type is type(None):
                 output_transferable = None
            else:
                return_type_name = getattr(return_type, '__name__', str(return_type))
                return_type_module = getattr(return_type, '__module__', None)
                return_type_full_name = f'{return_type_module}.{return_type_name}' if return_type_module else return_type_name
                
                if return_type_full_name in _TRANSFERABLE_MAP:
                    output_transferable = get_transferable(return_type_full_name)
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
                            if info.get('module') != func.__module__:
                                continue
                            registered_param_map = info.get('param_map', {})
                            registered_output_types = list(registered_param_map.values())
                            if expected_output_types == registered_output_types:
                                output_transferable = get_transferable(info['name'])
                                break
                
                # If no matching transferable found, create a default one (not registered and only used in this function)
                if output_transferable is None and not (return_type is None or return_type is type(None)):
                    output_transferable = create_default_transferable(func, is_input=False)

        # --- Wrapping ---
        transfer_decorator = transfer(input=input_transferable, output=output_transferable)
        wrapped_func = transfer_decorator(func)
        return wrapped_func

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
             raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)

# Helpers #########################################################################

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
        logger.error(f'Could not get signature for {func.__qualname__}')
        return None
    except Exception as e:
        logger.error(f'Error creating Pydantic model for {func.__qualname__}: {e}')
        return None