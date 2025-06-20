import zmq
import logging
from ..event import Event
from .base_server import BaseServer

# Logging Configuration ###########################################################
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

class TcpServer(BaseServer):
    def __init__(self, bind_address: str):
        super().__call__(bind_address)
        
        self.context = None
        self.socket = None
    
    def start(self):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(self.bind_address)

    def pool(self, timeout: float = 0.0) -> Event | None:
        if not self.socket or not self.context:
            return None
        
        timeout = int(timeout * 1000) if timeout > 0 else 100
        
        try:
            poller = zmq.Poller()
            poller.register(self.socket, zmq.POLLIN)
            
            socks = dict(poller.poll(timeout))
            if self.socket in socks:
                full_request = self.socket.recv(zmq.NOBLOCK)
                return Event.deserialize(full_request)
            
        except zmq.error.Again:
            return None
        
        except Exception as e:
            logger.error(f'ZMQ error occurred: {e}')
            return None

    def reply(self, event: Event):
        self.socket.send(event.serialize())
    
    def shutdown(self):
        pass
    
    def destroy(self):
        self.socket.close()
        self.context.term()
    
    def cancel_all_calls(self):
        try:
            self.socket.setsockopt(zmq.LINGER, 0)
            self.socket.close()
            
        except Exception as e:
            logger.error(f'Error cancelling ZMQ connections: {e}')