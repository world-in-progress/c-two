import requests

from ... import error
from ..base import BaseClient
from ..event.msg_type import MsgType
from ..util.wire import (
    encode_call, decode,
    PING_BYTES, SHUTDOWN_CLIENT_BYTES,
)

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
        
        try:
            wire_bytes = encode_call(method_name, data)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}') from e
        
        try:
            response = self.session.post(
                self.server_address, 
                data=wire_bytes,
                headers={'Content-Type': 'application/octet-stream'}
            )
            
            if response.status_code != 200:
                raise error.CompoClientError(f'HTTP error {response.status_code}')
                
            full_response = response.content
            
        except requests.RequestException as e:
            raise error.CompoClientError(f'HTTP request failed: {e}') from e
        
        env = decode(full_response)
        if env.msg_type != MsgType.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')

        if env.error:
            err = error.CCError.deserialize(env.error)
            if err:
                raise err
        
        return env.payload if env.payload is not None else b''
    
    def relay(self, event_bytes: bytes) -> bytes:
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
            raise error.CompoClientError(f'HTTP request failed: {e}') from e
    
    def terminate(self):
        """Close the HTTP session."""
        self.session.close()
    
    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping the HTTP server to check if it's alive."""
        try:
            response = requests.post(
                server_address, 
                data=PING_BYTES,
                headers={'Content-Type': 'application/octet-stream'},
                timeout=timeout
            )
            
            if response.status_code == 200:
                return decode(response.content).msg_type == MsgType.PONG
            return False
            
        except requests.RequestException:
            return False
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        """Send a shutdown command to the HTTP server."""
        try:
            if timeout <= 0.0:
                timeout = None
            response = requests.post(
                server_address, 
                data=SHUTDOWN_CLIENT_BYTES,
                headers={'Content-Type': 'application/octet-stream'},
                timeout=timeout
            )
            
            if response.status_code == 200:
                return decode(response.content).msg_type == MsgType.SHUTDOWN_ACK
            return False
            
        except requests.RequestException:
            return False