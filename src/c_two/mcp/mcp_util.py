import inspect
from mcp.server.fastmcp import FastMCP
from ..compo.runtime_connect import connect_crm

_MCP_FUNCTION_REGISTRY: dict = {}

def register_mcp_tool_from_compo_funcs(mcp: FastMCP, funcs: list[callable]):
    try:
        for func in [adapt_compo_func_for_mcp(func) for func in funcs]:
            mcp.add_tool(func)
        mcp.add_tool(flow)
    except Exception as e:
        print(f"Error registering component functions to MCP: {e}")
        raise e

def register_mcp_tools_from_compo_module(mcp: FastMCP, module):
    """
    Register all component functions from a module to the MCP server.
    
    This function dynamically imports the specified module and registers all its
    component functions to the provided MCP server instance.
    
    Args:
        mcp (FastMCP): The MCP server instance to register functions to
        module (str or module): The module or module name containing component functions
    """
    try:
        # Handle both module object and module name string
        if isinstance(module, str):
            imported_module = __import__(module, fromlist=[''])
        else:
            imported_module = module
            
        # Find callable functions that aren't private methods
        funcs = [
            getattr(imported_module, func_name) 
            for func_name in dir(imported_module) 
            if callable(getattr(imported_module, func_name)) 
            and not func_name.startswith('_')
            and inspect.isfunction(getattr(imported_module, func_name))
        ]
        
        # Register component functions
        register_mcp_tool_from_compo_funcs(mcp, funcs)
        
    except Exception as e:
        print(f"Error registering component module to MCP: {e}")
        raise e

def adapt_compo_func_for_mcp(func):
    """
    A decorator that adapts a component function to be compatible with MCP (Message Communication Protocol).
    This function modifies a given function to make it compatible with MCP by:
    1. Creating a wrapper function that preserves the original functionality
    2. Copying metadata (name, docstring, module, qualified name) from the original function
    3. Filtering annotations to exclude the first parameter (ICRM-related parameter)
    4. Registering the original function in the _MCP_FUNCTION_REGISTRY for later use
    Parameters:
        func (callable): The component function to adapt for MCP compatibility
    Returns:
        callable: An MCP-compatible version of the original function with adjusted metadata
    """
    
    # Create MCP-compatible function with the same name
    signature = inspect.signature(func)
    first_param_name = next(iter(signature.parameters))
    
    # Create a MCP compatible function
    def mcp_compatible_func(*args, **kwargs):
        return func(*args, **kwargs)
    
    # Update metadata to match original function (except parameter list)
    mcp_compatible_func.__name__ = func.__name__
    mcp_compatible_func.__doc__ = func.__doc__
    mcp_compatible_func.__module__ = func.__module__
    mcp_compatible_func.__qualname__ = func.__qualname__
    
    # Keep only the relevant annotations (skip first parameter)
    mcp_compatible_func.__annotations__ = {k: v for k, v in func.__annotations__.items() 
                                         if k != first_param_name and k != 'return'}
    if 'return' in func.__annotations__:
        mcp_compatible_func.__annotations__['return'] = func.__annotations__['return']
        
    # Register the function to FUNCTION_REGISTRY
    _MCP_FUNCTION_REGISTRY[func.__name__] = func
    return mcp_compatible_func

def flow(address: str, flow_func_code: str, flow_func_name: str) -> any:
    """Execute a LLM-provided flow function that orchestrates operations
    
    This is the primary entry point for complex operations. The other functions 
    SHOULD NOT be called directly. Instead,
    create a flow function that contains your processing logic, and pass it to this
    function for execution.
    
    Example usage:
    ```
    flow_code = '''python
    def my_flow_function():
        # Get grid information
        grids = get_grid_infos(1, [101, 102, 103])
        
        # Process grids and identify which to subdivide
        ids_to_subdivide = [g.global_id for g in grids if some_condition(g)]
        
        # Subdivide selected grids
        result = subdivide_grids([1] * len(ids_to_subdivide), ids_to_subdivide)
        
        return {"original_grids": grids, "subdivided_children": result}
    '''
    
    flow("flow_code", "my_flow_function")
    ```
    
    Args:
        address (str): The address of the CRM server to connect to
        flow_func_code (str): String containing the Python code with the flow function
        flow_func_name (str): Name of the function in the code to execute
        
    Returns:
        dict: Result of executing the flow function
    """
    namespace = _MCP_FUNCTION_REGISTRY.copy()
    try:
        exec(flow_func_code, namespace)
        flow_func = namespace.get(flow_func_name)
        if flow_func:
            try:
                with connect_crm(address):
                    return flow_func()
            except Exception as e:
                return {
                    'status': 'error',
                    'message': f'Error executing flow function: {str(e)}'
                }
        else:
            return {
                'status': 'error',
                'message': f'Flow function {flow_func_name} not found'
            }
    except Exception as e:
        return {
            'status': 'error',
            'message': f'Error executing flow function code: {str(e)}'
        }
