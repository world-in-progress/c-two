from inspect import isfunction
from typing import TypeVar, Type
from ..rpc.transferable import auto_transfer

ICRM = TypeVar('ICRM')

class ICRMMeta(type):
    """
    ICRMMeta
    --
    A metaclass for ICRM (Interface of Core Resource Model) classes that automatically sets a 'direction' attribute on new classes.
    Direction rules: 
    - An ICRM class (decorated with @icrm) has direction '->' as default, indicating its instance can transfer data from Component to CRM through a Client instance.
    - When an ICRM class is used at CRM server side, it is significant to set the direction of its instance to '<-', indicating its instance can transfer data from CRM to Component.
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        attrs['direction'] = '->'
        
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        return cls

def icrm(*, namespace: str = 'cc', version: str = '0.1.0'):
    """
    Interface of Core Resource Model (ICRM) decorator
    --
    Convert a regular class to an ICRM class.
    
    This function transforms the given class by applying the ICRMMeta metaclass,
    making it a proper ICRM that Server and Client can follow and interact with.
    Additionally, it decorates all member functions of the class with @auto_transfer,
    so that their input parameters and return values can be automatically transferred between Component and CRM.
    
    Returns:
        A class that has all the attributes of the original class plus a static
        'connect' method that creates and returns instances connected to a remote service.
    """
    def icrm_wrapper(cls: Type[ICRM]) -> Type[ICRM]:
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
                decorated_methods[name] = auto_transfer(value)
        
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
    return icrm_wrapper