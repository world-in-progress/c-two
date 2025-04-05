import sys
import zmq
import signal
import struct

class Server:
    def __init__(self, server_address: str, crm_instance: any):
        
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
        self.crm = crm_instance
        self.running = True
        
        signal.signal(signal.SIGINT, self._signal_handler)
        signal.signal(signal.SIGTERM, self._signal_handler)
    
    def terminate(self):
        self.running = False
        self.socket.close()
        self.context.term()
    
    def run(self):
        try:
            self.socket.setsockopt(zmq.RCVTIMEO, 1000)
            while self.running:
                
                try:
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
                
                except zmq.error.Again:
                    continue
            
        except KeyboardInterrupt:
            self._cleanup('Shutting down CRM Server...')
        except Exception as e:
            self._cleanup(f'Error in CRM Server: {e}')
    
    def _signal_handler(self, sig, frame):
        self._cleanup(f'Shutting down CRM Server via signal {sig}...')
    
    def _cleanup(self, message: str = ''):
        print(message, flush=True)
        try:
            if hasattr(self.crm, 'terminate') and self.crm.terminate:
                self.crm.terminate()
            self.terminate()
            sys.exit(0)
        except Exception as e:
            print(f'Error during termination: {e}')
            sys.exit(1)
            
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
