from inspect import isfunction
from typing import TypeVar, cast, Protocol
from ..message.transferable import auto_transfer, transfer

class HasConnect(Protocol):
    @staticmethod
    def connect(address: str) -> any:
        ...


T = TypeVar('T')
ConnecttableT = TypeVar('ConnecttableT', bound=HasConnect)

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
    
def icrm(cls: T) -> T:
    """
    Interface of Core Resource Model (ICRM) decorator
    --
    Convert a regular class to an ICRM class with direction '->'.
    
    This function transforms the given class by applying the ICRMMeta metaclass,
    making it a proper ICRM that other classes can implement.  
    Additionally, it decorates all member functions of the class with @auto_transfer,
    so that they can be automatically transferred between Component and CRM.
    
    Returns:
        A class that has all the attributes of the original class plus a static
        'connect' method that creates and returns instances connected to a remote service.
    """
    
    decorated_methods = {}
    for name, value in cls.__dict__.items():
        if isfunction(value) and name not in ('__dict__', '__weakref__', '__module__', '__qualname__', '__init__'):
            decorated_methods[name] = auto_transfer(value)
    
    # Define a new class with ICRMMeta metaclass that inherits from the original class
    class_name = cls.__name__
    bases = (cls,)
    
    # Create the new class
    NewClass = ICRMMeta(class_name, bases, {
        **{k: v for k, v in cls.__dict__.items() 
           if k not in decorated_methods and k not in ('__dict__', '__weakref__')},
        **decorated_methods
    })
    
    # Copy over type hints explicitly to help type checkers
    try:
        NewClass.__annotations__ = getattr(cls, '__annotations__', {})
    except (AttributeError, TypeError):
        pass
    
    # Copy docstring, module name etc.
    for attr in ['__doc__', '__module__', '__qualname__']:
        try:
            setattr(NewClass, attr, getattr(cls, attr))
        except (AttributeError, TypeError):
            pass

    return cast(T, NewClass)

def iicrm(cls):
    """
    Implementation of ICRM (IICRM) decorator
    --
    A decorator for classes that implement an ICRM interface.
    
    This decorator ensures that:
    1. The decorated class properly implements all methods defined in its ICRM parent class
    2. All implemented methods are automatically decorated with @auto_transfer
    
    Note: The name 'iicrm' is used because 'crm' is already used as a sub-module name in c-two.
    """
    
    # Get the base class decorated with @icrm
    base_class = None
    for base in cls.__bases__:
        if isinstance(base, ICRMMeta) and getattr(base, 'direction', None) == '->':
            base_class = base
            break
    
    if base_class is None:
        raise TypeError(f"{cls.__name__} must inherit from a class decorated with @icrm.")
    
    # Check for unimplemented methods
    for name, value in base_class.__dict__.items():
        if isfunction(value) and name not in cls.__dict__:
            raise NotImplementedError(f"Method '{name}' from base class '{base_class.__name__}' must be implemented in '{cls.__name__}'.")
    
    # Decorate implemented methods with @auto_transfer
    for name, value in cls.__dict__.items():
        if isfunction(value) and name in base_class.__dict__:
            setattr(cls, name, auto_transfer(value))
    
    return cls
