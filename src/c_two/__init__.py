from .server import Server
from .client import Client
from functools import wraps
from .component_server import ComponentServer
from .wrapper import Wrapper, register_wrapper, get_wrapper, transfer, serialize_from_table, deserialize_to_rows

def connect(icrm_class: any) -> callable:
    """
    Insert a CRM client into ICRM instance and transfer ICRM instance to a Component function. 
    
    Component function
    --
    Component function can be reused by different agnostic resources sharing the same `ICRM`.  
    Common Component function is expected to have inputs:  
    - `ICRM connected to CRM`
    - `Component args`  
    The `ICRM connected to CRM` is an ICRM instance having a specific CRM client.
    This `ICRM` will direct the Component to a CRM Server and make it calculate with specified resource behind the CRM.
    
    Component server
    --
    A Component server does not know which `CRM client` the Component's ICRM needs until a specified request coming.  
    Therefore, when the server tries to invoke a Component function, it will call from the following arguments:  
    - `Component args`
    - `[CRM address (provided by request)]`
    - `[CRM client (provided for test)]`
    
    instead of expected inputs of Component function.
    
    connect
    --    
    This decorater prefers instaciating a `CRM client` from `CRM address` and 
    injecting it to an ICRM instance. Then it will transmit the ICRM to the decorated Component function.  
    If the `CRM address` is not provided, this decorater will try to transmit given `CRM connection` to Component function directly.
    """
    def decorator(func: callable) -> callable:
        
        @wraps(func)
        def wrapper(*args: any, crm_address: str = '', crm_connection: Client | None = None) -> tuple[any, any]:
            if crm_address == '' and icrm is None:
                raise ValueError("Either 'crm_address' or 'icrm_instance' must be provided.")
            
            icrm = icrm_class()
            icrm.direction = '->'
            if crm_address != '':
                icrm.client = Client(crm_address)
            else:
                icrm.client = crm_connection
            
            result = func(icrm, *args)
            icrm.client.terminate()
            return result
        
        return wrapper
    
    return decorator
