import zmq
import queue
import logging
import threading
from ..base import BaseServer
from ..event import Event, EventQueue
from ..event.envelope import Envelope
from ..event.msg_type import MsgType
from ..util.wire import decode
from ..util.encoding import event_to_wire_bytes

logger = logging.getLogger(__name__)

MAXIMUM_WAIT_TIMEOUT = 0.1

class ZmqServer(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)

        self.context = None
        self.socket = None
        self.shutdown_event = threading.Event()
        self._reply_queue: queue.Queue[bytes] = queue.Queue()
        self._serve_done = threading.Event()
        self._serve_done.set()
    
    def _serve(self):
        self._serve_done.clear()
        try:
            while True:
                if self.shutdown_event.is_set():
                    self.event_queue.put(Envelope(msg_type=MsgType.SHUTDOWN_SERVER))
                    return

                envelope = self._poll(timeout=MAXIMUM_WAIT_TIMEOUT)
                if envelope is None:
                    continue

                self.event_queue.put(envelope)

                # REP socket requires exactly one send after each recv
                reply_data = self._wait_for_reply()
                if reply_data is not None:
                    try:
                        self.socket.send(reply_data)
                    except Exception as e:
                        if not self.shutdown_event.is_set():
                            logger.error(f'Failed to send ZMQ reply: {e}')

                if envelope.msg_type == MsgType.SHUTDOWN_CLIENT:
                    return
        finally:
            self._serve_done.set()

    def _wait_for_reply(self) -> bytes | None:
        """Block until a reply is available or shutdown is signaled."""
        while not self.shutdown_event.is_set():
            try:
                return self._reply_queue.get(timeout=MAXIMUM_WAIT_TIMEOUT)
            except queue.Empty:
                continue
        # Drain one last time in case the reply arrived during shutdown
        try:
            return self._reply_queue.get_nowait()
        except queue.Empty:
            return None

    def start(self):
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(self.bind_address)

        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()

    def _poll(self, timeout: float = 0.0) -> Envelope | None:
        if not self.socket or not self.context:
            return None
        
        timeout = int(timeout * 1000) if timeout > 0 else 100
        
        try:
            poller = zmq.Poller()
            poller.register(self.socket, zmq.POLLIN)
            
            socks = dict(poller.poll(timeout))
            if self.socket in socks:
                full_request = self.socket.recv(zmq.NOBLOCK)
                return decode(full_request)
            
        except zmq.error.Again:
            return None
        
        except Exception as e:
            logger.error(f'ZMQ error occurred: {e}')
            return None

    def reply(self, event: Event):
        """Thread-safe: queues the reply for the server thread to send."""
        self._reply_queue.put(event_to_wire_bytes(event))
    
    def shutdown(self):
        self.shutdown_event.set()
    
    def destroy(self):
        self._serve_done.wait(timeout=5.0)
        try:
            if self.socket and not self.socket.closed:
                self.socket.setsockopt(zmq.LINGER, 0)
                self.socket.close()
        except Exception:
            pass
        try:
            if self.context and not self.context.closed:
                self.context.term()
        except Exception:
            pass
    
    def cancel_all_calls(self):
        # Reply serialization is handled by the reply queue; nothing to cancel here
        pass