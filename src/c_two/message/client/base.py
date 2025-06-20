import subprocess

class BaseClient:
    def __init__(self, server_address: str):
        self.server_address = server_address
    
    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    def terminate(self):
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        raise NotImplementedError('This method should be implemented by subclasses.')
    
    @staticmethod
    def shutdown_by_process(process: subprocess.Popen, timeout: float = 1.0) -> bool:
        """
        Shutdown the CRM service by terminating the process.
        
        Note:
        This method can only be used if the CRM server process is started with a subprocess.
        It will attempt to gracefully terminate the process, and if that fails, it will forcefully
        kill the process after a timeout.
        """
        raise NotImplementedError('This method should be implemented by subclasses.')