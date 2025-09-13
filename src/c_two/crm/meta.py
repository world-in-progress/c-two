from inspect import isfunction
from typing import TypeVar, cast
from ..rpc.transferable import auto_transfer

T = TypeVar('T')

class ICRMMeta(type):
    """
    ICRMMeta
    --
    A metaclass for ICRM (Interface of Core Resource Model) classes that automatically sets a 'direction' attribute on new classes.
    Direction rules:
    - If a class explicitly defines its own 'direction', that value is preserved
    - If a class inherits from an ICRM class with direction '->', it gets assigned the direction '<-', meaning it is an implementation of the ICRM  (aka CRM or IICRM)  
    - Otherwise, it defaults to direction '->' (becoming a base ICRM class)
    
    This metaclass helps distinguish between ICRM interfaces ('->' direction) and their implementations (CRM, '<-' direction).
    """
    
    def __new__(mcs, name, bases, attrs, **kwargs):
        
        if 'direction' in attrs:
            return super().__new__(mcs, name, bases, attrs, **kwargs)
        
        has_icrm_parent = False
        for base in bases:
            if hasattr(base, 'direction') and base.direction == '->':
                attrs['direction'] = '<-'
                has_icrm_parent = True
                break
        
        if not has_icrm_parent:
            attrs['direction'] = '->'
        
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        return cls

def icrm(cls: T, *, namespace: str = 'cc', version: str = '0.1.0') -> T:
    """
    Interface of Core Resource Model (ICRM) decorator
    --
    Convert a regular class to an ICRM class with direction '->'.
    
    This function transforms the given class by applying the ICRMMeta metaclass,
    making it a proper ICRM that other classes can implement.  
    Additionally, it decorates all member functions of the class with @auto_transfer,
    so that their input parameters and return values can be automatically transferred between Component and CRM.
    
    Returns:
        A class that has all the attributes of the original class plus a static
        'connect' method that creates and returns instances connected to a remote service.
    """
    # Validate namespace and version
    if not namespace:
        raise ValueError('Namespace of ICRM cannot be empty.')
    if not version:
        raise ValueError('Version of ICRM cannot be empty (version example: "1.0.0").')
    if not isinstance(version, str) or not version.count('.') == 2:
        raise ValueError('Version must be a string in the format "major.minor.patch".')
    
    decorated_methods = {}
    for name, value in cls.__dict__.items():
        if isfunction(value) and name not in ('__dict__', '__weakref__', '__module__', '__qualname__', '__init__', '__tag__'):
            decorated_methods[name] = auto_transfer(cls, '->', value)
    
    # Define a new class with ICRMMeta metaclass that inherits from the original class
    class_name = cls.__name__
    bases = (cls,)
    
    # Create the new class
    new_cls = ICRMMeta(class_name, bases, {
        **{k: v for k, v in cls.__dict__.items() 
        if k not in decorated_methods and k not in ('__dict__', '__weakref__')},
        **decorated_methods
    })
    
    # Copy over type hints explicitly to help type checkers
    try:
        new_cls.__annotations__ = getattr(cls, '__annotations__', {})
    except (AttributeError, TypeError):
        pass
    
    # Copy docstring, module name etc.
    for attr in ['__doc__', '__module__', '__qualname__']:
        try:
            setattr(new_cls, attr, getattr(cls, attr))
        except (AttributeError, TypeError):
            pass
    
    # Add static tag attributes
    setattr(new_cls, '__tag__', f'{namespace}/{class_name}/{version}')

    return new_cls

def crm(cls: T) -> T:
    """
    Implementation of ICRM (IICRM) decorator
    --
    A decorator for classes that implement an ICRM interface.
    
    This decorator ensures that:
    1. The decorated class has a 'terminate' method
    2. The decorated class properly implements all methods defined in its ICRM parent class
    3. All implemented methods are automatically decorated with @auto_transfer
    
    Note: The name 'iicrm' is used because 'crm' is already used as a sub-module name in c-two.
    """
    # Validate that the class has a terminate method
    if not hasattr(cls, 'terminate'):
        raise TypeError(f'Class {cls.__name__} does not have a "terminate" method, which is required for CRM functionality.')
    
    # Get the base class decorated with @icrm
    base_class = None
    for base in cls.__bases__:
        if isinstance(base, ICRMMeta) and getattr(base, 'direction', None) == '->':
            base_class = base
            break
    
    if base_class is None:
        raise TypeError(f'{cls.__name__} must inherit from a class decorated with @icrm.')
    
    # Check for unimplemented methods
    for name, value in base_class.__dict__.items():
        if isfunction(value) and name not in cls.__dict__:
            raise NotImplementedError(f"Method '{name}' from base class '{base_class.__name__}' must be implemented in '{cls.__name__}'.")
    
    # Decorate implemented methods with @auto_transfer
    for name, value in cls.__dict__.items():
        if isfunction(value) and name in base_class.__dict__:
            # Ensure the module is set as the base class module
            # So that the auto_transfer decorator can find the correct module
            value.__module__ = base_class.__module__
            setattr(cls, name, auto_transfer(cls, '<-', value))

    return cls