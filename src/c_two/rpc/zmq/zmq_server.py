import zmq
import logging
import threading
from ..base import BaseServer
from ..event import Event, EventTag, EventQueue

logger = logging.getLogger(__name__)

MAXIMUM_WAIT_TIMEOUT = 0.1

class ZmqServer(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)

        self.context = None
        self.socket = None
        self.shutdown_event = threading.Event()
    
    def _serve(self):
        while True:
            # Check if the shutdown event is set
            if self.shutdown_event.is_set():
                self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_SERVER))
                break
            
            # Poll for events with a timeout
            event = self._poll(timeout=MAXIMUM_WAIT_TIMEOUT)
            if event is None:
                continue
            else:
                self.event_queue.put(event)
                if event.tag == EventTag.SHUTDOWN_FROM_CLIENT:
                    break
            
    def start(self):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(self.bind_address)

        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()

    def _poll(self, timeout: float = 0.0) -> Event | None:
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
        self.shutdown_event.set()
    
    def destroy(self):
        self.socket.close()
        self.context.term()
    
    def cancel_all_calls(self):
        try:
            self.socket.setsockopt(zmq.LINGER, 0)
            self.socket.close()
            
        except Exception as e:
            logger.error(f'Error cancelling ZMQ connections: {e}')