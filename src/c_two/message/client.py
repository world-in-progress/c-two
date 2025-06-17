import os
import sys
import zmq
import signal
import subprocess
from . import error
from .util.encoding import add_length_prefix, parse_message

class Client:
    def __init__(self, server_address: str):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REQ)
        
        self.socket.connect(server_address)
        self.server_address = server_address
    
    def terminate(self):
        self.socket.close()
        self.context.term()

    def _send_request(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a request to the CRM service and get the response synchronously."""
        # Serialize
        try:
            serialized_meta = method_name.encode('utf-8')
            serialized_data = b'' if data is None else data
            combined_request = add_length_prefix(serialized_meta) + add_length_prefix(serialized_data)
        except Exception as e:
            raise error.CRMSerializeOutput(f'Error occurred when serializing request: {e}')
        
        # Send and get response
        self.socket.send(combined_request)
        full_response = self.socket.recv()
        
        # Deserialize error
        sub_responses = parse_message(full_response)
        if len(sub_responses) != 2:
            raise error.CompoDeserializeOutput(f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}')

        err = error.CCError.deserialize(sub_responses[0])
        if err:
            raise err
        
        return sub_responses[1]

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Call a method on the CRM instance."""
        try:
            return self._send_request(method_name, data)
        except Exception as e:
            raise e

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping the CRM service to check if it's alive."""
        
        context = zmq.Context()
        socket = context.socket(zmq.REQ)
        socket.setsockopt(zmq.LINGER, 0)
        socket.setsockopt(zmq.RCVTIMEO, int(timeout * 1000))
        socket.connect(server_address)
        
        try:
            socket.send(b'PING')
            
            response = socket.recv()
            if response == b'PONG':
                return True
            return False
        except zmq.ZMQError:
            return False
        finally:
            socket.close()
            context.term()
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5, process: subprocess.Popen | None = None) -> bool:
        """Send a shutdown command to the CRM service."""
        
        context = zmq.Context()
        socket = context.socket(zmq.REQ)
        socket.setsockopt(zmq.LINGER, 0)
        socket.setsockopt(zmq.RCVTIMEO, int(timeout * 1000))
        socket.connect(server_address)
        
        try:
            socket.send(b'SHUTDOWN')
            response = socket.recv()
            if response == b'SHUTDOWN_ACK':
                if process:
                    if sys.platform != 'win32':
                        # Unix-specific: terminate the process group
                        try:
                            os.killpg(os.getpgid(process.pid), signal.SIGINT)
                        except (AttributeError, ProcessLookupError):
                            process.terminate()
                    else:
                        # Windows-specific: send Ctrl+C signal and then terminate
                        try:
                            process.send_signal(signal.CTRL_C_EVENT)
                        except (AttributeError, ProcessLookupError):
                            process.terminate()

                    try:
                        process.wait()
                    except subprocess.TimeoutExpired:
                        if sys.platform != 'win32':
                            try:
                                os.killpg(os.getpgid(process.pid), signal.SIGKILL)
                            except (AttributeError, ProcessLookupError):
                                process.kill()
                        else:
                            process.kill()
                return True
        except zmq.ZMQError:
            return False
        finally:
            socket.close()
            context.term()