import sys
import pickle
import importlib

def run_crm_service(impl_module: str, impl_class: str):
    """CRM service running in a separate process."""
    module = importlib.import_module(impl_module)
    crm_class = getattr(module, impl_class)
    crm = None
    
    while True:
        request = sys.stdin.buffer.readline()
        if not request:
            break
        
        try:
            request_type, data = pickle.loads(request.strip())
            
            if request_type == 'STOP':
                break
            
            if request_type == "CREATE":
                args, kwargs = data
                crm = crm_class(*args, **kwargs)
                response = ("SUCCESS", None)
            
            elif request_type == "CALL":
                if crm is None:
                    response = ("ERROR", "CRM not initialized")
                else:
                    method_name, args, kwargs = data
                    method = getattr(crm, method_name)
                    result = method(*args, **kwargs)
                    response = ("SUCCESS", result)
            
            else:
                response = ("ERROR", "Unknown request type")
            
        except Exception as e:
            response = ("ERROR", str(e))
        
        sys.stdout.buffer.write(pickle.dumps(response))
        sys.stdout.buffer.flush()
        
if __name__ == "__main__":
    impl_module = sys.argv[1]
    impl_class = sys.argv[2]
    run_crm_service(impl_module, impl_class)
