import enum
from inspect import isfunction
from typing import TypeVar, Type, Callable
from ..rpc.transferable import auto_transfer
from ..rpc.util.wire import preregister_methods

_F = TypeVar('_F', bound=Callable)
ICRM = TypeVar('ICRM')
_METHOD_ACCESS_ATTR = '__cc_method_access__'
_SHUTDOWN_ATTR = '__cc_on_shutdown__'

@enum.unique
class MethodAccess(enum.Enum):
    READ = 'read'
    WRITE = 'write'

def _set_method_access(func: _F, access: MethodAccess) -> _F:
    if not callable(func):
        raise TypeError('Method access decorators can only be applied to callables.')

    setattr(func, _METHOD_ACCESS_ATTR, access)
    return func

def read(func: _F) -> _F:
    return _set_method_access(func, MethodAccess.READ)

def write(func: _F) -> _F:
    return _set_method_access(func, MethodAccess.WRITE)

def get_method_access(func: Callable) -> MethodAccess:
    access = getattr(func, _METHOD_ACCESS_ATTR, MethodAccess.WRITE)
    if isinstance(access, MethodAccess):
        return access
    return MethodAccess(access)


def on_shutdown(func: _F) -> _F:
    """Mark an ICRM method as the shutdown callback.

    The decorated method is called (with no arguments) when the CRM is
    unregistered or the process exits.  At most one method per ICRM may
    carry this decorator.  It is **not** added to the RPC dispatch table.
    """
    if not callable(func):
        raise TypeError('@cc.on_shutdown can only be applied to callables.')
    setattr(func, _SHUTDOWN_ATTR, True)
    return func


def get_shutdown_method(cls: type) -> str | None:
    """Discover the ``@cc.on_shutdown``-decorated method name in *cls*.

    Returns ``None`` if no shutdown method exists.
    Raises ``ValueError`` if more than one method is decorated.
    """
    found: str | None = None
    for name in dir(cls):
        if name.startswith('_'):
            continue
        obj = getattr(cls, name, None)
        if callable(obj) and getattr(obj, _SHUTDOWN_ATTR, False):
            if found is not None:
                raise ValueError(
                    f'ICRM {cls.__name__} has multiple @on_shutdown methods: '
                    f'{found!r} and {name!r}',
                )
            found = name
    return found

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
                if getattr(value, _SHUTDOWN_ATTR, False):
                    continue  # @cc.on_shutdown methods are not RPC-callable
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

        # Pre-encode method names for fast wire encoding
        preregister_methods(decorated_methods.keys())

        return new_cls
    return icrm_wrapper
