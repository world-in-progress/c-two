import inspect
import threading
from functools import wraps
from typing import TypeVar, cast
from contextlib import contextmanager
from ..message.client import Client

_local = threading.local()

T = TypeVar('T')

@contextmanager
def connect_crm(address: str, icrm_class: T = None):
    """
    Context manager to connect to a CRM server.  
    
    This function creates a new client and sets it as the current client in thread-local storage.  
    When the context manager exits, it restores the old client or deletes the current client if it was the only one.
    Also, it terminates the client connection to the server.
    
    Args:
        address (str): The address of the CRM server to connect to.
        
    Returns:
        Client: The connected client instance, or an ICRM instance if icrm_class is provided
    """
    try:
        icrm = None
        client = Client(address)
        old_client = getattr(_local, 'current_client', None)
        _local.current_client = client
        if icrm_class is not None:
            icrm = icrm_class()
            icrm.client = client
            
        yield client if icrm is None else cast(T, icrm)
    except Exception as e:
        print(f"An error occurred when connecting to CRM: {e}")
    finally:
        if old_client is not None:
            _local.current_client = old_client
        else:
            delattr(_local, 'current_client')
            client.terminate()

def get_current_client():
    """
    Get the current client from the thread-local storage.
    """
    client = getattr(_local, 'current_client', None)
    return client

def connect(func = None, icrm_class = None) -> callable:
    """
    Insert a CRM client into ICRM instance and transfer the instance to a Component function. 
    
    Component function
    --
    Component function can be reused by different agnostic resources sharing the same `ICRM Specification`.  
    Common Component function is expected to have inputs:  
    - `ICRM connected to CRM`
    - `Component args`  
    The `ICRM connected to CRM` is an ICRM instance having a specific CRM client.
    This `ICRM` will direct the Component to a CRM Server and make it calculate with specified resource behind the CRM.
    
    Component-related runtime
    --
    The Component-related runtime does not know which `CRM client` the Component's ICRM needs until a specified request coming.  
    Therefore, when the runtime tries to invoke a Component function, it will call from the following arguments:  
    - `Component args`
    - `[CRM address (provided by request)]`
    - `[CRM client (provided for test)]`
    
    instead of expected inputs of Component function.
    
    connect
    --    
    This decorator handles three scenarios in priority order:
    - Use current_client from runtime context if available (with statement)
    - Use provided crm_connection if available
    - Create new client from crm_address if provided
    
    Supports both @connect and @connect() usage patterns.
    
    If icrm_class is None, it will be inferred from the first parameter type annotation of the decorated function.
    
    Args:
        func (callable, optional): The function to decorate
        icrm_class (type, optional): The ICRM class to use
    """
    def create_wrapper(func):
        
        # Get the icrm_class from the first parameter's type annotation if not provided
        nonlocal icrm_class
        if icrm_class is None:
            signature = inspect.signature(func)
            params = list(signature.parameters.values())
            if params and params[0].annotation != inspect.Parameter.empty:
                inferred_icrm_class = params[0].annotation
            else:
                raise ValueError(f"Could not infer ICRM class from {func.__name__}'s first parameter. "
                               "Either provide icrm_class explicitly or add type annotations.")
            icrm_class = inferred_icrm_class
    
        @wraps(func)
        def wrapper(*args: any, crm_address: str = '', crm_connection: Client | None = None):
            # Check for client in runtime context first (highest priority)
            current_client = get_current_client()
            if current_client is not None:
                # Use client from runtime context (from with statement)
                client = current_client
                close_client = False
            elif crm_connection is not None:
                # Use provided connection
                client = crm_connection
                close_client = False
            elif crm_address != '':
                # Create new client from address
                client = Client(crm_address)
                close_client = True
            else:
                raise ValueError("No client available: Either use 'with connect_crm()', or provide 'crm_address' or 'crm_connection'")
            
            icrm = icrm_class()
            icrm.client = client
            
            try:
                return func(icrm, *args)
            finally:
                if close_client:
                    icrm.client.terminate()
        return wrapper
    
    # Handle both @connect and @connect() usage patterns
    if func is not None:
        return create_wrapper(func)
    
    # Handle @connect(icrm_class=ICRM) or @connect(ICRM) case
    if icrm_class is not None and not isinstance(icrm_class, type) and callable(icrm_class):
        # In case someone does @connect(some_func)
        return create_wrapper(icrm_class)
    
    # Handle @connect() case - returns a decorator
    return create_wrapper
