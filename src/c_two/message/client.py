import zmq
import uuid
import struct
from . import context
from .transferable import get_transferable

class Client:
    def __init__(self, server_address: str):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.DEALER)
        self.client_id = str(uuid.uuid4()).encode('utf-8')
        self.socket.setsockopt(zmq.IDENTITY, self.client_id)
        self.socket.connect(server_address)
        self.server_address = server_address
    
    def terminate(self):
        self.socket.close()
        self.context.term()

    def _send_request(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a request to the CRM service and get the response synchronously."""
        
        # Generate unique request ID for correlation
        request_id = str(uuid.uuid4()).encode('utf-8')
        
        # Serialize
        serialized_meta = method_name.encode('utf-8')
        serialized_data = b'' if data is None else data
        combinned_request = _add_length_prefix(serialized_meta) + _add_length_prefix(serialized_data)
        
        # Send request with ID for correlation
        self.socket.send_multipart([request_id, combinned_request])
        
        # Receive response and verify correlation ID
        response_parts = self.socket.recv_multipart()
        if len(response_parts) != 2:
            raise ValueError('Expected 2-part response (correlation_id, data)')
        
        received_id, full_response = response_parts
        if received_id != request_id:
            raise ValueError(f'Request/Response correlation mismatch: {received_id} != {request_id}')
        
        sub_responses = _parse_message(full_response)
        if len(sub_responses) != 2:
            raise ValueError('Expected exactly 2 sub-messages (response and result)')
        
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
        socket = context.socket(zmq.DEALER)
        socket.setsockopt(zmq.LINGER, 0)
        socket.setsockopt(zmq.RCVTIMEO, int(timeout * 1000))
        socket.connect(server_address)
        
        try:
            ping_id = str(uuid.uuid4()).encode('utf-8')
            socket.send_multipart([ping_id, b'PING'])
            response_parts = socket.recv_multipart()
            
            if len(response_parts) == 2 and response_parts[0] == ping_id and response_parts[1] == b'PONG':
                return True
            return False
            
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
