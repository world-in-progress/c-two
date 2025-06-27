import zmq

from ... import error
from ..base import BaseClient
from ..event import Event, EventTag
from ..util.encoding import add_length_prefix, parse_message

class ZmqClient(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)
        
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REQ)
        
        self.socket.connect(server_address)
        self.server_address = server_address

    def _send_request(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a request to the CRM service and get the response synchronously."""
        # Serialize
        try:
            serialized_method_name = method_name.encode('utf-8')
            serialized_data = b'' if data is None else data
            combined_request = add_length_prefix(serialized_method_name) + add_length_prefix(serialized_data)
            event = Event(tag=EventTag.CRM_CALL, data=combined_request)
        except Exception as e:
            raise error.CRMSerializeOutput(f'Error occurred when serializing request: {e}')
        
        # Send request event
        self.socket.send(event.serialize())
        
        # Wait for response
        full_response = self.socket.recv()
        
        # Deserialize Event
        event = Event.deserialize(full_response)
        if event.tag != EventTag.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected event tag: {event.tag}. Expected: {EventTag.CRM_REPLY}')
        
        # Deserialize error
        sub_responses = parse_message(event.data)
        if len(sub_responses) != 2:
            raise error.CompoDeserializeOutput(f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}')

        err = error.CCError.deserialize(sub_responses[0])
        if err:
            raise err
        
        return sub_responses[1]
    
    def terminate(self):
        self.socket.close()
        self.context.term()

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Call a method on the CRM instance."""
        try:
            return self._send_request(method_name, data)
        except Exception as e:
            raise e

    def relay(self, event_bytes: bytes) -> bytes:
        # Send and get response
        self.socket.send(event_bytes)
        
        # Wait for response
        full_response = self.socket.recv()
        
        return full_response

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping the CRM service to check if it's alive."""
        context = zmq.Context()
        socket = context.socket(zmq.REQ)
        socket.setsockopt(zmq.LINGER, 0)
        socket.setsockopt(zmq.RCVTIMEO, int(timeout * 1000))
        socket.connect(server_address)
        
        try:
            socket.send(Event(tag=EventTag.PING).serialize())
            
            response = socket.recv()
            if Event.deserialize(response).tag == EventTag.PONG:
                return True
            return False
        except zmq.ZMQError:
            return False
        finally:
            socket.close()
            context.term()
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        """Send a shutdown command to the CRM service."""
        context = zmq.Context()
        socket = context.socket(zmq.REQ)
        socket.setsockopt(zmq.LINGER, 0)
        socket.setsockopt(zmq.RCVTIMEO, int(timeout * 1000))
        socket.connect(server_address)
        
        try:
            socket.send(Event(tag=EventTag.SHUTDOWN_FROM_CLIENT).serialize())
            response = socket.recv()
            if Event.deserialize(response).tag == EventTag.SHUTDOWN_ACK:
                return True
        except zmq.ZMQError:
            return False
        finally:
            socket.close()
            context.term()