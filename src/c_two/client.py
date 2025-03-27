import subprocess
import pickle

class CRMClient:
    def __init__(self, process: subprocess.Popen):
        self.process = process
    
    def _send_request(self, request_type: str, data: any):
        """Send a request to the CRM service and get the response."""
        request = pickle.dumps((request_type, data))
        self.process.stdin.write(request + b'\n')
        self.process.stdin.flush()
        
        response = self.process.stdout.readline()
        status, result = pickle.loads(response)
        if status == "ERROR":
            raise RuntimeError(f"CRM error: {result}")
        return result
    
    def create(self, *args, **kwargs):
        """Create a CRM instance with arbitrary arguments."""
        self._send_request("CREATE", (args, kwargs))
    
    def call(self, method_name: str, *args, **kwargs):
        self._send_request("CALL", (method_name, (args, kwargs)))
    
    def stop(self):
        self._send_request("STOP", None)
        self.process.wait()