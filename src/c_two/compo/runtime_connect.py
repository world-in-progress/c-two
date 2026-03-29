import inspect
import logging
import threading
from functools import wraps
from contextlib import contextmanager
from typing import ContextManager, Generator, TypeVar, overload

from ..transport.registry import connect as _transport_connect
from ..transport.registry import close as _transport_close

_local = threading.local()

ICRM = TypeVar('ICRM')

logger = logging.getLogger(__name__)


@overload
def connect_crm(address: str, icrm_class: type[ICRM], *, crm_name: str = '') -> ContextManager[ICRM]:
    ...


@contextmanager
def connect_crm(
    address: str,
    icrm_class: type[ICRM] = None,
    *,
    crm_name: str = '',
    **kwargs,
) -> Generator[ICRM, None, None]:
    """Context manager to connect to a CRM server.

    Args:
        address: The server address string (e.g., 'ipc-v3://region')
        icrm_class: The ICRM class to connect as
        crm_name: The CRM routing name (empty string uses server default)
        **kwargs: Reserved for future use
    """
    if icrm_class is None:
        raise ValueError('icrm_class is required for connect_crm')

    proxy = _transport_connect(icrm_class, name=crm_name, address=address)
    old_proxy = getattr(_local, 'current_proxy', None)
    _local.current_proxy = proxy
    try:
        yield proxy
    finally:
        if old_proxy is not None:
            _local.current_proxy = old_proxy
        elif hasattr(_local, 'current_proxy'):
            delattr(_local, 'current_proxy')
        _transport_close(proxy)


def get_current_client():
    """Get the current proxy from the thread-local storage."""
    return getattr(_local, 'current_proxy', None)


def connect(func=None, icrm_class=None) -> callable:
    """Decorator: inject a connected ICRM proxy as the first argument.

    Supports both @connect and @connect() usage patterns.
    If icrm_class is None, it is inferred from the first parameter's
    type annotation.

    The decorated function accepts additional kwargs:
    - crm_address: str — server address
    - crm_name: str — CRM routing name (default: 'default')
    - crm_connection: pre-existing proxy (for testing)
    """
    def create_wrapper(func):
        nonlocal icrm_class
        if icrm_class is None:
            signature = inspect.signature(func)
            params = list(signature.parameters.values())
            if params and params[0].annotation != inspect.Parameter.empty:
                icrm_class = params[0].annotation
            else:
                raise ValueError(
                    f"Could not infer ICRM class from {func.__name__}'s first parameter. "
                    "Either provide icrm_class explicitly or add type annotations."
                )

        @wraps(func)
        def wrapper(
            *args,
            crm_address: str = '',
            crm_name: str = '',
            crm_connection=None,
        ):
            # Priority: runtime context > provided connection > new from address
            current = get_current_client()
            if current is not None:
                proxy = current
                close_proxy = False
            elif crm_connection is not None:
                proxy = crm_connection
                close_proxy = False
            elif crm_address:
                proxy = _transport_connect(
                    icrm_class, name=crm_name, address=crm_address,
                )
                close_proxy = True
            else:
                raise ValueError(
                    "No client available: use 'with connect_crm()', "
                    "or provide 'crm_address' or 'crm_connection'"
                )

            try:
                return func(proxy, *args)
            finally:
                if close_proxy:
                    _transport_close(proxy)

        return wrapper

    if func is not None:
        return create_wrapper(func)

    if icrm_class is not None and not isinstance(icrm_class, type) and callable(icrm_class):
        return create_wrapper(icrm_class)

    return create_wrapper
