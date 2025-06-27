import subprocess

class BaseClient:
    def __init__(self, server_address: str):
        self.server_address = server_address
    
    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def terminate(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def relay(self, event_bytes: bytes) -> bytes:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        raise NotImplementedError('This method should be implemented by subclasses.')