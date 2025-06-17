import sys
import zmq
import logging
import threading
from . import error
from .transferable import get_transferable
from .util.encoding import add_length_prefix, parse_message

# Logging Configuration ###########################################################
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

class Server:
    def __init__(self, server_address: str, crm_instance: object, name: str = '', timeout: int = 0):
        if name == '':
            name = crm_instance.__class__.__name__
        
        self.name = name
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
        self.crm = crm_instance
        
        self._idle_timeout = timeout
        self._termination_event = threading.Event()
        self._server_thread = threading.Thread(target=self._run, daemon=True)
    
    def start(self):
        self._server_thread.name = self.name
        self._server_thread.start()
    
    def stop(self):
        self._cleanup(f'Cleaning up CRM Server "{self.name}"...')
        self._termination_event.set()
    
    def wait_for_termination(self, check_interval: float = 0.1):
        if sys.platform != 'win32':
            while not self._termination_event.is_set():
                try:
                    threading.Event().wait(check_interval)
                except KeyboardInterrupt:
                    logger.info(f'\nKeyboardInterrupt received.')
                    self._termination_event.set()
                    self._cleanup(f'Stopping CRM Server "{self.name}"...')
                    logger.info(f'CRM Server "{self.name}" stopped.')

        else:
            while not self._termination_event.is_set():
                pass
            self._termination_event.set()
            self._cleanup(f'Stopping CRM Server "{self.name}"...')
            logger.info(f'CRM Server "{self.name}" stopped.')
            
    def _run(self):
        timeout_ms = int(self._idle_timeout * 1000) if self._idle_timeout > 0 else 1000
        self.socket.setsockopt(zmq.RCVTIMEO, timeout_ms)
        while not self._termination_event.is_set():
            try:
                full_request = self.socket.recv()
                
                # Check for PING message
                if full_request == b'PING':
                    self.socket.send(b'PONG')
                    continue
                
                # Check for SHUTDOWN message
                if full_request == b'SHUTDOWN':
                    self.socket.send(b'SHUTDOWN_ACK')
                    self._termination_event.set()
                    break
                
                sub_messages = parse_message(full_request)
                if len(sub_messages) != 2:
                    raise error.CompoDeserializeOutput(f'Expected exactly 2 sub-messages (error and result), got {len(sub_messages)}')

                # Get method name
                method_name = sub_messages[0].tobytes().decode('utf-8')
                
                # Get arguments
                args_bytes = sub_messages[1]
                
                # Call method wrapped from CRM instance method
                method = getattr(self.crm, method_name)
                response = method.__wrapped__(self.crm, args_bytes)
                
                # Send response
                self.socket.send(response)
            
            except zmq.error.Again:
                if self._idle_timeout > 0:
                    logger.info(f'Server "{self.name}" idle timeout ({self._idle_timeout}s) exceeded. Terminating server...')
                    self._termination_event.set()
                    break
                continue
            
            except zmq.ContextTerminated:
                break
            
            except Exception as e:
                # Log the error and send create an error response
                message = f'Error occurred when CRM Server "{self.name}" tried to process the request "{method_name}":\n{e}'
                logger.error(f'Error occurred when CRM Server "{self.name}" tried to process the request "{method_name}":\n{e}')

                # Create a serialized response based on the serialized_error and serialized_result
                serialized_error: bytes = error.CCError.serialize(error.CRMServerError(message))
                combined_response = add_length_prefix(serialized_error) + add_length_prefix(b'')

                # Send response
                self.socket.send(combined_response)
            
    def _stop(self):
        if hasattr(self, '_server_thread') and self._server_thread.is_alive():
            self._server_thread.join()
            
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