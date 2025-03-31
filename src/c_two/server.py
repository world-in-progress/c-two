import zmq
import struct

class Server:
    def __init__(self, server_address: str, crm_instance: any):
        
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
        self.crm = crm_instance
    
    def terminate(self):
        self.socket.close()
        self.context.term()
    
    def run(self):
        try:
            while True:
                full_request = self.socket.recv()
                sub_messages = _parse_message(full_request)
                if len(sub_messages) != 2:
                    raise ValueError("Expected exactly 2 sub-messages (meta and data)")
                
                # Get method name 
                method_name = sub_messages[0].tobytes().decode('utf-8')
                
                # Get arguments
                args_bytes = sub_messages[1]
                
                # Call method wrapped from CRM instance method
                method = getattr(self.crm, method_name)
                _, response = method(args_bytes)
                
                # Send response
                self.socket.send(response)
        except KeyboardInterrupt:
            print('Shutting down CRM Server...')

# Helper ##################################################

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
