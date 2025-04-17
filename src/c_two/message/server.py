import zmq
import struct
import threading

class Server:
    def __init__(self, server_address: str, crm_instance: any):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
        self.crm = crm_instance
        self.running = True
        
        self._termination_event = threading.Event()
        self._server_thread = threading.Thread(target=self._run, daemon=True)
    
    def start(self):
        self._server_thread.name = 'CRM Server'
        self._server_thread.start()
    
    def stop(self):
        self._cleanup(f'Cleaning up CRM Server...')
        self._termination_event.set()
    
    def wait_for_termination(self, check_interval: float = 0.1):
        while not self._termination_event.is_set():
            try:
                threading.Event().wait(check_interval)
            except KeyboardInterrupt:
                print('\nKeyboardInterrupt received.\nStopping CRM server...', flush=True)
                self._cleanup(f'Cleaning up...')
                self._termination_event.set()
    
    def _run(self):
        try:
            self.socket.setsockopt(zmq.RCVTIMEO, 1000)
            while self.running:
                try:
                    full_request = self.socket.recv()
                    
                    # Check for PING message
                    if full_request == b'PING':
                        self.socket.send(b'PONG')
                        continue
                    
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
                except zmq.ContextTerminated:
                    break
            
        except Exception as e:
            self._cleanup(f'Error in CRM Server: {e}')
        
    def _stop(self):
        self.running = False
        if hasattr(self, '_server_thread') and self._server_thread.is_alive():
            self._server_thread.join()
        self.socket.close()
        self.context.term()
    
    def _cleanup(self, message: str = ''):
        print(message, flush=True)
        try:
            self._stop()
            if hasattr(self.crm, 'terminate') and self.crm.terminate:
                self.crm.terminate()
        except Exception as e:
            print(f'Error during termination: {e}')
            
# Helpers ##################################################

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
