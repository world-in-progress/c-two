import grpc
import importlib
from concurrent import futures

class ComponentServer:
    
    @staticmethod
    def serve(component_service_server_module_names: list[str]):
        if not isinstance(component_service_server_module_names, list):
            raise TypeError("Expected a list of module names")
        if not all(isinstance(name, str) for name in component_service_server_module_names):
            raise ValueError("All module names must be strings")
        
        # Init Component Server
        server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
        
        # Register all Components
        for module_name in component_service_server_module_names:
            try:
                component = importlib.import_module(module_name)
                if not hasattr(component, 'Component') or not hasattr(component.Component, 'register_to_server'):
                    raise AttributeError(f"Module '{module_name}' does not have a valid 'Component' class with 'register_to_server' method")
                component.Component.register_to_server(server)
            except Exception as e:
                print(f"Error loading module '{module_name}': {e}")
                continue
        
        server.add_insecure_port('[::]:50051')
        print("gRPC server starting on port 50051...")
        server.start()
        
        try:
            server.wait_for_termination()
        except KeyboardInterrupt:
            print('Shutting down gRPC server...')
            server.stop(0)
    