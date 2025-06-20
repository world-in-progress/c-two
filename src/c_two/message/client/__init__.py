import os
import sys
import signal
import subprocess

from .base import BaseClient
from .tcp_client import TcpClient

def _get_client_class(server_address: str):
    """Determine the client class based on the server address."""
    if server_address.startswith(('tcp://', 'ipc://')):
        return TcpClient
    else:
        # TODO: Handle other protocols if needed
        pass

class Client(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)

        # Check server_address use TCP or IPC protocol
        client_class = _get_client_class(server_address)
        if client_class:
            self._client = client_class(server_address)
        else:
            # TODO: Handle other protocols if needed
            pass

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        return self._client.call(method_name, data)

    def terminate(self):
        self._client.terminate()
    
    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        return _get_client_class(server_address).ping(server_address, timeout)

    @staticmethod
    def shutdown(server_address, timeout = 0.5):
        return _get_client_class(server_address).shutdown(server_address, timeout)
    
    @staticmethod
    def shutdown_by_process(process: subprocess.Popen, timeout: float = 1.0) -> bool:
        if not process:
            return True
            
        if process.poll() is not None:
            return True  # Process is already terminated
        
        if sys.platform != 'win32':
            # Unix-specific: terminate the process group
            try:
                os.killpg(os.getpgid(process.pid), signal.SIGINT)
            except (AttributeError, ProcessLookupError):
                process.terminate()
        else:
            # Windows-specific: temporarily ignore CTRL_C in parent process
            original_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
            try:
                process.send_signal(signal.CTRL_C_EVENT)
            except (AttributeError, ProcessLookupError):
                process.terminate()
                
        # Wait for the process to terminate
        try:
            process.wait(timeout=timeout)
            return True
        
        except KeyboardInterrupt:
            if process.poll() is not None:
                return True
        
        except subprocess.TimeoutExpired:
            print(f'Timeout expired while waiting for process {process.pid} to terminate. Forcing shutdown...')
            if sys.platform != 'win32':
                try:
                    os.killpg(os.getpgid(process.pid), signal.SIGKILL)
                except (AttributeError, ProcessLookupError):
                    process.kill()
            else:
                process.kill()
            return False
        
        finally:
            if sys.platform == 'win32':
                # Restore original signal handler
                signal.signal(signal.SIGINT, original_handler)