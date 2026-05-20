import enum
import unicodedata
from inspect import isfunction
from typing import TypeVar, Type, Callable
from .transferable import auto_transfer

_F = TypeVar('_F', bound=Callable)
CRM = TypeVar('CRM')
_METHOD_ACCESS_ATTR = '__cc_method_access__'
_SHUTDOWN_ATTR = '__cc_on_shutdown__'
_CRM_TAG_MAX_BYTES = 255

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
    """Mark a CRM method as the shutdown callback.

    The decorated method is called (with no arguments) when the resource
    is unregistered or the process exits.  At most one method per CRM may
    carry this decorator.  It is **not** added to the RPC dispatch table.
    """
    if not callable(func):
        raise TypeError('@cc.on_shutdown can only be applied to callables.')
    setattr(func, _SHUTDOWN_ATTR, True)
    return func


def get_shutdown_method(cls: type) -> str | None:
    """Discover the ``@cc.on_shutdown``-decorated method name in *cls*.

    Returns ``None`` if no shutdown method exists.
    Raises ``ValueError`` if more than one method is decorated, or if the
    decorated method is private (name starts with ``_``).
    """
    found: str | None = None
    for name in dir(cls):
        obj = getattr(cls, name, None)
        if not callable(obj) or not getattr(obj, _SHUTDOWN_ATTR, False):
            continue
        if name.startswith('_'):
            raise ValueError(
                f'@on_shutdown cannot be applied to private method '
                f'{cls.__name__}.{name!r} — use a public method name',
            )
        if found is not None:
            raise ValueError(
                f'CRM {cls.__name__} has multiple @on_shutdown methods: '
                f'{found!r} and {name!r}',
            )
        found = name
    return found


def _validate_crm_tag_field(label: str, value: str) -> None:
    if not isinstance(value, str):
        raise ValueError(f'{label} of CRM must be a string.')
    if not value:
        raise ValueError(f'{label} of CRM cannot be empty.')
    if len(value.encode('utf-8')) > _CRM_TAG_MAX_BYTES:
        raise ValueError(
            f'{label} of CRM cannot exceed {_CRM_TAG_MAX_BYTES} bytes.',
        )
    if value.strip() != value:
        raise ValueError(
            f'{label} of CRM cannot contain leading or trailing whitespace.',
        )
    if any(unicodedata.category(ch) == 'Cc' for ch in value):
        raise ValueError(f'{label} of CRM cannot contain control characters.')
    if '/' in value or '\\' in value:
        raise ValueError(f'{label} of CRM cannot contain path or tag separators.')

class CRMMeta(type):
    """Metaclass for CRM (Core Resource Model) contract classes.

    Sets a ``direction`` attribute on every CRM class.  Direction rules:
      - ``'->'`` default: the CRM instance transfers data *from* client
        *to* the resource server via ``auto_transfer``.
      - ``'<-'``: server-side instance, flipped by the resource runtime
        to indicate the reverse transfer direction.
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        attrs['direction'] = '->'

        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        return cls


def _crm_enter(self):
    """Context-manager entry — returns the CRM instance itself."""
    return self


def _crm_exit(self, exc_type, exc_val, exc_tb):
    """Context-manager exit — closes the underlying proxy via the registry.

    Imports ``close`` lazily to avoid a circular import between the CRM
    layer and the transport layer.
    """
    from ..transport.registry import close as _close
    _close(self)
    return False


def crm(*, namespace: str = 'cc', version: str = '0.1.0'):
    """CRM (Core Resource Model) decorator.

    Declares a CRM contract — the interface that resource implementations
    expose to clients over RPC.  Apply to a plain class whose method
    bodies are typically ``...``; the decorator wires up auto-transfer on
    every public method, tags the class with ``namespace/ClassName/version``,
    and adds context-manager support so ``cc.connect`` can be used with a
    ``with`` statement.
    """
    def crm_wrapper(cls: Type[CRM]) -> Type[CRM]:
        _validate_crm_tag_field('Namespace', namespace)
        _validate_crm_tag_field('CRM name', cls.__name__)
        _validate_crm_tag_field('Version', version)
        if version.count('.') != 2:
            raise ValueError('Version must be a string in the format "major.minor.patch".')

        decorated_methods = {}
        for name, value in cls.__dict__.items():
            if isfunction(value) and name not in ('__dict__', '__weakref__', '__module__', '__qualname__', '__init__', '__tag__'):
                if getattr(value, _SHUTDOWN_ATTR, False):
                    continue  # @cc.on_shutdown methods are not RPC-callable
                payload_abi_context = {
                    'crm_namespace': namespace,
                    'crm_name': cls.__name__,
                    'crm_version': version,
                    'method_name': name,
                }
                transfer_config = getattr(value, '__cc_transfer__', None)
                if transfer_config:
                    decorated_methods[name] = auto_transfer(
                        value,
                        payload_abi_context=payload_abi_context,
                        **transfer_config,
                    )
                else:
                    decorated_methods[name] = auto_transfer(
                        value,
                        payload_abi_context=payload_abi_context,
                    )

        # Define a new class with CRMMeta metaclass that inherits from the original class
        class_name = cls.__name__
        bases = (cls,)

        # Create the new class with injected context-manager methods.
        new_cls = CRMMeta(class_name, bases, {
            **{k: v for k, v in cls.__dict__.items()
            if k not in decorated_methods and k not in ('__dict__', '__weakref__')},
            **decorated_methods,
            '__enter__': _crm_enter,
            '__exit__': _crm_exit,
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
        setattr(new_cls, '__cc_namespace__', namespace)
        setattr(new_cls, '__cc_name__', class_name)
        setattr(new_cls, '__cc_version__', version)

        return new_cls
    return crm_wrapper
