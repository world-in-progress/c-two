import uuid
import logging

from ... import error
from . import get_server
from ..base import BaseClient
from ..event import Event, EventTag
from ..event.envelope import Envelope
from ..event.msg_type import MsgType
from ..util.encoding import parse_message, event_to_wire_bytes
from ..util.wire import decode
from .thread_server import DirectCallEvent

logger = logging.getLogger(__name__)

class ThreadClient(BaseClient):
    def __init__(self, server_address):
        super().__init__(server_address)
        self.thread_id = server_address.replace('thread://', '')
        
    def call_direct(self, method_name: str, args: tuple) -> any:
        """Direct call — pass Python objects without serialization (thread:// only)."""
        server = get_server(self.thread_id)
        if not server:
            raise error.CompoClientError(f'Thread server {self.thread_id} not found.')

        request_id = str(uuid.uuid4())
        direct_event = DirectCallEvent(request_id=request_id, method_name=method_name, args=args)

        try:
            server.put_request(direct_event)
            result, err = server.get_direct_response(request_id)
            if err is not None:
                raise err
            return result
        except error.CCBaseError:
            raise
        except Exception as e:
            raise error.CompoClientError(f'Thread direct call failed: {e}') from e

    def _extract_reply(self, response: Event) -> bytes:
        """Extract result bytes from a scheduler-produced reply Event."""
        if response.data_parts is not None and response.data is None:
            err_bytes = response.data_parts[0] if response.data_parts[0] else b''
            result_bytes = response.data_parts[1] if len(response.data_parts) > 1 else b''
        else:
            sub_responses = parse_message(response.get_data())
            if len(sub_responses) != 2:
                raise error.CompoDeserializeOutput(
                    f'Expected exactly 2 sub-messages (error and result), got {len(sub_responses)}')
            err_bytes = sub_responses[0]
            result_bytes = sub_responses[1]

        err = error.CCError.deserialize(err_bytes)
        if err:
            raise err
        return result_bytes

    def call(self, method_name: str, data: bytes | None = None) -> bytes:
        """Make a synchronous call to the server."""
        server = get_server(self.thread_id)
        if not server:
            raise error.CompoClientError(f'Thread server {self.thread_id} not found.')
        
        request_id = str(uuid.uuid4())
        envelope = Envelope(
            msg_type=MsgType.CRM_CALL,
            request_id=request_id,
            method_name=method_name,
            payload=data if data is not None else b'',
        )
        
        try:
            server.put_request(envelope)
            response = server.get_response(request_id)
            if not response:
                raise error.CompoClientError(f'No response received for request {request_id}.')
            if response.tag != EventTag.CRM_REPLY:
                raise error.CompoClientError(f'Unexpected response tag: {response.tag}')
            return self._extract_reply(response)
        except error.CCBaseError:
            raise
        except Exception as e:
            raise error.CompoClientError(f'Thread call failed: {e}') from e

    def relay(self, event_bytes: bytes) -> bytes:
        """Relay raw wire bytes to the server."""
        try:
            envelope = decode(event_bytes)
            if not envelope.request_id:
                envelope.request_id = str(uuid.uuid4())
            
            server = get_server(self.thread_id)
            if not server:
                raise error.CompoClientError(f'Thread server {self.thread_id} not found')
            
            server.put_request(envelope)
            response = server.get_response(envelope.request_id)
            
            if not response:
                raise error.CompoClientError(f'No response received for request {envelope.request_id}.')
            
            return event_to_wire_bytes(response)
            
        except error.CCBaseError:
            raise
        except Exception as e:
            raise error.CompoClientError(f'Thread relay failed: {e}') from e
    
    def terminate(self):
        pass

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        """Ping a thread server to check if it's alive."""
        thread_id = server_address.replace('thread://', '')
        server = get_server(thread_id)
        
        if not server:
            return False
        
        try:
            request_id = str(uuid.uuid4())
            envelope = Envelope(msg_type=MsgType.PING, request_id=request_id)
            server.put_request(envelope)
            response = server.get_response(request_id, timeout)
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
            request_id = str(uuid.uuid4())
            envelope = Envelope(msg_type=MsgType.SHUTDOWN_CLIENT, request_id=request_id)
            server.put_request(envelope)
            response = server.get_response(request_id, timeout=timeout)
            return response is not None and response.tag == EventTag.SHUTDOWN_ACK
        except Exception:
            return False