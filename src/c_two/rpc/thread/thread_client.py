import uuid
import logging

from ... import error
from . import get_server
from ..base import BaseClient
from ..event import Event, EventTag
from ..util.encoding import add_length_prefix, parse_message

logger = logging.getLogger(__name__)

class ThreadClient(BaseClient):
    def __init__(self, server_address):
        super().__init__(server_address)
        self.thread_id = server_address.replace('thread://', '')
        
    def _create_method_event(self, method_name: str, data: bytes | None = None) -> Event:
        """Create an Event for the given method."""
        try:
            request_id = str(uuid.uuid4())
            serialized_data = b'' if data is None else data
            serialized_method_name = method_name.encode('utf-8')
            combined_request = add_length_prefix(serialized_method_name) + add_length_prefix(serialized_data)
            event = Event(tag=EventTag.CRM_CALL, data=combined_request, request_id=request_id)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')

        return event

    def call(self, method_name: str, data: bytes | None = None) -> Event:
        """Make a synchronous call to the server."""
        # Get the target server
        server = get_server(self.thread_id)
        if not server:
            raise error.CompoClientError(f'Thread server {self.thread_id} not found.')
        
        # Create the request event
        event = self._create_method_event(method_name, data)
        
        try:
            # Send the event to the server
            server.put_request(event)
            
            # Wait for the response
            response = server.get_response(event.request_id)
            if not response:
                raise error.CompoClientError(f'No response received for request {event.request_id}.')

            if response.tag != EventTag.CRM_REPLY:
                raise error.CompoClientError(f'Unexpected response tag: {response.tag}')
        
            # Deserialize error
            sub_responses = parse_message(response.data)
            if len(sub_responses) != 2:
                raise error.CompoDeserializeOutput(f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}')
            
            err  = error.CCError.deserialize(sub_responses[0])
            if err:
                raise err
            
            return sub_responses[1]
                
        except Exception as e:
            raise error.CompoClientError(f'Thread call failed: {e}')

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw event bytes to the server."""
        try:
            event = Event.deserialize(event_bytes)
            if not event.request_id:
                event.request_id = str(uuid.uuid4())
            
            server = get_server(self.thread_id)
            if not server:
                raise error.CompoClientError(f'Thread server {self.thread_id} not found')
            
            server.put_request(event)
            response = server.get_response(event.request_id)
            
            if not response:
                raise error.CompoClientError(f'No response received for request {event.request_id}.')
            
            return response.serialize()
            
        except Exception as e:
            raise error.CompoClientError(f'Thread relay failed: {e}')
    
    def terminate(self):
        # For thread client, no persistent connection to clean up
        pass

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping a thread server to check if it's alive."""
        thread_id = server_address.replace('thread://', '')
        server = get_server(thread_id)
        
        if not server:
            return False
        
        try:
            # Create a ping event
            ping_event = Event(tag=EventTag.PING, request_id=str(uuid.uuid4()))
            server.put_request(ping_event)
            
            # Wait for pong response
            response = server.get_response(ping_event.request_id, timeout)
            return response is not None and response.tag == EventTag.PONG
            
        except Exception:
            return False

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        """Shutdown a thread server."""
        thread_id = server_address.replace('thread://', '')
        server = get_server(thread_id)
        
        if not server:
            return True
        try:
            # Send shutdown event
            shutdown_event = Event(tag=EventTag.SHUTDOWN_FROM_CLIENT, request_id=str(uuid.uuid4()))
            server.put_request(shutdown_event)
            
            # Wait for acknowledgment
            response = server.get_response(shutdown_event.request_id, timeout=timeout)
            return response is not None and response.tag == EventTag.SHUTDOWN_ACK
            
        except Exception:
            return False