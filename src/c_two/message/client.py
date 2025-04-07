import zmq
import struct
from . import globals
from .transferable import get_transferable

class Client:
    def __init__(self, server_address):
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
        
        response = get_transferable(globals.BASE_RESPONSE).deserialize(sub_responses[0])
        if response['code'] == globals.Code.ERROR_INVALID:
            raise RuntimeError(f'Failed to make CRM process: {response['message']}')
        
        return sub_responses[1]

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Call a method on the CRM instance."""
        try:
            return self._send_request(method_name, data)
        except Exception as e:
            raise RuntimeError(f'Failed to call CRM: {e}')

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
