import requests

from ... import error
from ..base import BaseClient
from ..event import Event, EventTag
from ..util.encoding import add_length_prefix, parse_message

class HttpClient(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)
        
        self.session = requests.Session()
        self.server_address = server_address
    
    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Call a method on the CRM instance via HTTP, blocking like local function call."""
        return self._send_request(method_name, data)
    
    def _send_request(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a request to the CRM service and get the response synchronously."""
        
        # Serialize request
        try:
            serialized_method_name = method_name.encode('utf-8')
            serialized_data = b'' if data is None else data
            combined_request = add_length_prefix(serialized_method_name) + add_length_prefix(serialized_data)
            event = Event(tag=EventTag.CRM_CALL, data=combined_request)
        except Exception as e:
            raise error.CRMSerializeOutput(f'Error occurred when serializing request: {e}')
        
        # Send POST request with serialized event as body
        try:
            response = self.session.post(
                self.server_address, 
                data=event.serialize(),
                headers={'Content-Type': 'application/octet-stream'}
            )
            
            if response.status_code != 200:
                raise error.CompoClientError(f'HTTP error {response.status_code}')
                
            full_response = response.content
            
        except requests.RequestException as e:
            raise error.CompoClientError(f'HTTP request failed: {e}')
        
        # Deserialize Event
        event = Event.deserialize(full_response)
        if event.tag != EventTag.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected event tag: {event.tag}. Expected: {EventTag.CRM_REPLY}')

        # Deserialize error and result
        sub_responses = parse_message(event.data)
        if len(sub_responses) != 2:
            raise error.CompoDeserializeOutput(f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}')

        err = error.CCError.deserialize(sub_responses[0])
        if err:
            raise err
        
        return sub_responses[1]
    
    def relay(self, event_bytes: bytes) -> bytes:
        # Send POST request with serialized event as body
        try:
            response = self.session.post(
                self.server_address, 
                data=event_bytes,
                headers={'Content-Type': 'application/octet-stream'}
            )
            
            if response.status_code != 200:
                raise error.CompoClientError(f'HTTP error {response.status_code}')
                
            full_response = response.content
            return full_response
            
        except requests.RequestException as e:
            raise error.CompoClientError(f'HTTP request failed: {e}')
    
    def terminate(self):
        """Close the HTTP session."""
        self.session.close()
    
    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping the HTTP server to check if it's alive."""
        try:
            response = requests.post(
                server_address, 
                data=Event(tag=EventTag.PING).serialize(),
                headers={'Content-Type': 'application/octet-stream'},
                timeout=timeout
            )
            
            if response.status_code == 200:
                event = Event.deserialize(response.content)
                return event.tag == EventTag.PONG
            return False
            
        except requests.RequestException:
            return False
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        """Send a shutdown command to the HTTP server."""
        try:
            response = requests.post(
                server_address, 
                data=Event(tag=EventTag.SHUTDOWN_FROM_CLIENT).serialize(),
                headers={'Content-Type': 'application/octet-stream'},
                timeout=timeout
            )
            
            if response.status_code == 200:
                event = Event.deserialize(response.content)
                return event.tag == EventTag.SHUTDOWN_ACK
            return False
            
        except requests.RequestException:
            return False