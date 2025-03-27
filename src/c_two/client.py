import subprocess
import pickle
import sys

class CRMClient:
    def __init__(self):
        self.process: subprocess.Popen = sys.modules['__main__'].__crm_process__
        if self.process.poll() is not None:
            raise RuntimeError("CRM process is not running")

    def _send_request(self, request_type: str, data):
        """Send a request to the CRM service and get the response synchronously."""
        if self.process.poll() is not None:
            raise RuntimeError("CRM process has terminated unexpectedly")
        
        # Send request
        request = pickle.dumps((request_type, data))
        self.process.stdin.write(request + b'\n')
        self.process.stdin.flush()
        
        # Wait for response synchronously
        response = self.process.stdout.readline()
        if not response:  # EOF or process closed stdout
            raise RuntimeError("CRM process closed communication unexpectedly")
        
        status, result = pickle.loads(response)
        if status == "ERROR":
            raise RuntimeError(f"CRM error: {result}")
        return result

    def create(self, *args, **kwargs):
        """Create a CRM instance with arbitrary arguments."""
        self._send_request("CREATE", (args, kwargs))

    def call(self, method_name: str, *args, **kwargs):
        """Call a method on the CRM instance."""
        return self._send_request("CALL", (method_name, (args, kwargs)))

    def stop(self):
        """Stop the CRM process."""
        self._send_request("STOP", None)
        self.process.wait()
        self.process.stdin.close()
        self.process.stdout.close()
        self.process.stderr.close()
