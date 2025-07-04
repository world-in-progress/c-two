from __future__ import annotations
import pickle
import inspect
import logging
from abc import ABCMeta
from functools import wraps
from pydantic import BaseModel, create_model
from dataclasses import dataclass, is_dataclass
from typing import get_type_hints, get_args, get_origin, Any, Callable

from .. import error
from .util.encoding import add_length_prefix

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
    return pickle.loads(data) if data else None

def defaultTransferableFactory(func, is_input: bool):
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
        
        # Define the dynamic transferable class for input
        class DynamicInputTransferable(Transferable):
            __module__ = 'Default'  # mark as default
            
            def serialize(*args) -> bytes:
                """
                Default serialization for function input parameters using pickle
                """
                try:
                    # Create a mapping of parameter names to arguments
                    args_dict = {}
                    for i, arg in enumerate(args):
                        if i < len(filtered_params):
                            args_dict[filtered_params[i]] = arg
                        else:
                            args_dict[f'extra_arg_{i}'] = arg
                    
                    args_dict['__serialized__'] = True
                    return pickle.dumps(args_dict)
                except Exception:
                    # Fallback to direct args serialization
                    return pickle.dumps(args)
            
            def deserialize(data: memoryview | bytes):
                """
                Default deserialization for function input parameters using pickle
                Returns either the original dict mapping or tuple of args.
                """
                try:
                    # Handle None case
                    if not data:
                        return None
                    
                    unpickled = pickle.loads(data)
                    
                    if isinstance(unpickled, dict):
                        if '__serialized__' not in unpickled:
                            return unpickled

                        # If it's a dict (from serialize method), convert back to tuple
                        del unpickled['__serialized__']
                        
                        # Preserve parameter order
                        ordered_args = []
                        for param_name in filtered_params:
                            if param_name in unpickled:
                                ordered_args.append(unpickled[param_name])
                        # Add any extra arguments
                        for key, value in unpickled.items():
                            if key.startswith('extra_arg_'):
                                ordered_args.append(value)
                        
                        return tuple(ordered_args) if len(ordered_args) != 1 else ordered_args[0]
                    else:
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
        return_type = type_hints['return']
        serialize_func = _default_serialize_func
        deserialize_func = _default_deserialize_func

        # Define the dynamic transferable class for output
        DynamicOutputTransferable = type(
            class_name,
            (Transferable,),
            {
                '__module__': 'Default',  # mark as default
                'serialize': staticmethod(serialize_func),
                'deserialize': staticmethod(deserialize_func)
            }
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

def get_transferable(transferable_name: str) -> Transferable | None:
    name = transferable_name
    return None if name not in _TRANSFERABLE_MAP else _TRANSFERABLE_MAP[name]

# Transferable-related decorators #################################################

def transferable(cls: type) -> type:
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
        
        def com_to_crm(*args: any) -> any:
            method_name = func.__name__
            
            # Get transferable
            input_transferable = input.serialize if input else None
            output_transferable = output.deserialize if output else None

            # Convert input and run method
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')
            
            obj = args[0]
            request = args[1:] if len(args) > 1 else None
            
            try:
                args_converted = input_transferable(*request) if (request is not None and input_transferable is not None) else None
                result_bytes = obj.client.call(method_name, args_converted)
                return None if not output_transferable else output_transferable(result_bytes)
            except Exception as e:
                if 'args_converted' not in locals():
                    raise error.CompoSerializeInput(str(e))
                elif 'result_bytes' not in locals():
                    raise error.CompoCRMCalling(str(e))
                else:
                    raise error.CompoDeserializeOutput(str(e))
        
        def crm_to_com(*args: any) -> tuple[any, any]:
            
            # Get transferable
            input_transferable = input.deserialize if input else None
            output_transferable = output.serialize if output else None

            # Convert input and run method
            try:
                if len(args) < 1:
                    raise ValueError('Instance method requires self, but only get one argument.')
                
                obj = args[0]
                request = args[1] if len(args) > 1 else None
                
                args_converted = input_transferable(request) if (request is not None and input_transferable is not None) else tuple()
                # If args_converted is not a tuple, convert it to a tuple
                if not isinstance(args_converted, tuple):
                    args_converted = (args_converted,)
                
                result = func(obj, *args_converted)
                err = None
                
            except Exception as e:
                result = None
                if 'args_converted' not in locals():
                    err = error.CRMDeserializeInput(str(e))
                elif 'result' not in locals():
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
            combined_response = add_length_prefix(serialized_error) + add_length_prefix(serialized_result)
            return combined_response
        
        @wraps(func)
        def transfer_wrapper(*args: any) -> any:
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

def auto_transfer(direction: str, func = None) -> callable:
    def create_wrapper(func: callable) -> callable:
        # --- Input Matching using Pydantic Model Comparison ---
        input_model = _create_pydantic_model_from_func_sig(func)
        input_transferable: Transferable | None = None

        if input_model is not None:
            # Get fields from the generated Pydantic model
            input_model_fields = getattr(input_model, 'model_fields', {})

            is_empty_input = not bool(input_model_fields)
            if not is_empty_input:
                for info in _TRANSFERABLE_INFOS:
                    # Safe matching, ensure module names match
                    if info.get('module') != func.__module__:
                        continue
                    
                    registered_param_map = info.get('param_map', {})
                    is_empty_registered = not bool(registered_param_map)

                    # Match if field names and types align
                    if not is_empty_input and not is_empty_registered:
                        # Compare names first
                        if set(input_model_fields.keys()) == set(registered_param_map.keys()):
                            # Compare types
                            match = True
                            for name, field_info in input_model_fields.items():
                                # Get the type annotation from Pydantic's FieldInfo
                                pydantic_type = field_info.annotation
                                registered_type = registered_param_map.get(name)
                                # Simple type comparison (TODO: might need refinement for complex types like generics)
                                if pydantic_type != registered_type:
                                    match = False
                                    break
                            if match:
                                input_transferable = get_transferable(info['name'])
                                break

            # If no matching transferable found, create a default one (not registered and only used in this function)
            if input_transferable is None and not is_empty_input:
                input_transferable = defaultTransferableFactory(func, is_input=True)

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
                    output_transferable = defaultTransferableFactory(func, is_input=False)

        # --- Wrapping ---
        transfer_decorator = transfer(input=input_transferable, output=output_transferable)
        wrapped_func = transfer_decorator(func)
            
        if direction == '->':
            return wrapped_func
        elif direction == '<-':
            func.__wrapped__ = wrapped_func
            return func
        else:
            raise ValueError(f'Invalid direction value: {direction}. Expected "->" or "<-".')

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