import os
import sys
import zmq
import struct
import signal
import subprocess
from . import context
from .transferable import get_transferable

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
        serialized_meta = method_name.encode('utf-8')
        serialized_data = b'' if data is None else data
        combinned_request = _add_length_prefix(serialized_meta) + _add_length_prefix(serialized_data)
        
        # Send and get response
        self.socket.send(combinned_request)
        full_response = self.socket.recv()
        
        sub_responses = _parse_message(full_response)
        if len(sub_responses) != 2:
            raise ValueError("Expected exactly 2 sub-messages (response and result)")
        
        response = get_transferable(context.BASE_RESPONSE).deserialize(sub_responses[0])
        if response['code'] == context.Code.ERROR_INVALID:
            raise RuntimeError(f'Failed to make CRM process: {response["message"]}')
        
        return sub_responses[1]

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Call a method on the CRM instance."""
        try:
            return self._send_request(method_name, data)
        except Exception as e:
            raise RuntimeError(f'Failed to call CRM: {e}')
    
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

# Helper ##################################################
def _add_length_prefix(message_bytes: bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes

def _parse_message(full_message: bytes) -> list[memoryview]:
    buffer = memoryview(full_message)
    messages = []
    offset = 0
    
    while offset < len(buffer):
        if offset + 8 > len(buffer):
            raise ValueError("Incomplete length prefix at end of message")
        
        length = struct.unpack('>Q', buffer[offset:offset + 8])[0]
        offset += 8
        
        if offset + length > len(buffer):
            raise ValueError(f"Message length {length} exceeds remaining buffer size at offset {offset}")
        
        message = buffer[offset:offset + length]
        messages.append(message)
        offset += length
    
    if offset != len(buffer):
        raise ValueError(f"Extra bytes remaining after parsing: {len(buffer) - offset}")
    
    return messages