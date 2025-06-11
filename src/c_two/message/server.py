import os
import zmq
import queue
import struct
import logging
import threading

# Logging Configuration ###########################################################
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

# TODO: Add auto-termination of server subprocess if max idle time is exceeded
class Server:
    def __init__(self, server_address: str, crm_instance: object):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
        self.crm = crm_instance
        self.running = True
        
        self._termination_event = threading.Event()
        self._server_thread = threading.Thread(target=self._run, daemon=True)
        self._shutdown_queue = queue.Queue()
        
        if os.name == 'nt': # Windows
            import win32api
            import win32con
            
            def windows_handler(dwCtrlType):
                if dwCtrlType in (win32con.CTRL_C_EVENT, win32con.CTRL_BREAK_EVENT, 
                                win32con.CTRL_CLOSE_EVENT, win32con.CTRL_SHUTDOWN_EVENT):
                    logger.info(f'\nReceived Windows shutdown signal: {dwCtrlType}')
                    self._shutdown_queue.put('SHUTDOWN')
                    self._termination_event.set()
                    return True
                return False
            
            win32api.SetConsoleCtrlHandler(windows_handler, True)
            
        else: # Unix-like systems
            import signal
            
            def signal_handler(signum, frame):
                logger.info(f'\nReceived signal: {signum}')
                self._shutdown_queue.put('SHUTDOWN')
                self._termination_event.set()
            
            signal.signal(signal.SIGINT, signal_handler)
            signal.signal(signal.SIGTERM, signal_handler)
    
    def start(self):
        self._server_thread.name = 'CRM Server'
        self._server_thread.start()
    
    def stop(self):
        self._cleanup(f'Cleaning up CRM Server...')
        self._termination_event.set()
    
    def wait_for_termination(self, check_interval: float = 0.1):
        try:
            while not self._termination_event.is_set():
                try:
                    shutdown_signal = self._shutdown_queue.get(timeout=check_interval)
                    if shutdown_signal == 'SHUTDOWN':
                        break
                except queue.Empty:
                    continue
        except Exception as e:
            logger.error(f'Error in wait_for_termination: {e}')
        finally:
            self._cleanup('Shutting down...')
    
    def _run(self):
        try:
            self.socket.setsockopt(zmq.RCVTIMEO, 1000)
            while self.running and not self._termination_event.is_set():
                try:
                    # Check for shutdown signal
                    try:
                        shutdown_signal = self._shutdown_queue.get_nowait()
                        if shutdown_signal == 'SHUTDOWN':
                            break
                    except queue.Empty:
                        pass
                    
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
            self._server_thread.join(timeout=5.0)
        self.socket.close()
        self.context.term()
    
    def _cleanup(self, message: str = ''):
        logger.info(message)
        try:
            self._stop()
            if hasattr(self.crm, 'terminate') and callable(self.crm.terminate):
                self.crm.terminate()
        except Exception as e:
            logger.error(f'Error during termination: {e}')

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