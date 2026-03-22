import zmq

from ... import error
from ..base import BaseClient
from ..event.msg_type import MsgType
from ..util.wire import (
    encode_call, decode,
    PING_BYTES, SHUTDOWN_CLIENT_BYTES,
)

class ZmqClient(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)
        
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REQ)
        
        self.socket.connect(server_address)
        self.server_address = server_address

    def _send_request(self, method_name: str, data: bytes | None = None) -> bytes:
        """Send a request to the CRM service and get the response synchronously."""
        try:
            wire_bytes = encode_call(method_name, data)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}') from e
        
        self.socket.send(wire_bytes)
        full_response = self.socket.recv()
        
        env = decode(full_response)
        if env.msg_type != MsgType.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')
        
        if env.error:
            err = error.CCError.deserialize(env.error)
            if err:
                raise err
        
        return env.payload if env.payload is not None else b''
    
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
        self.socket.send(event_bytes)
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
            socket.send(PING_BYTES)
            response = socket.recv()
            return decode(response).msg_type == MsgType.PONG
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
            socket.send(SHUTDOWN_CLIENT_BYTES)
            response = socket.recv()
            return decode(response).msg_type == MsgType.SHUTDOWN_ACK
        except zmq.ZMQError:
            return False
        finally:
            socket.close()
            context.term()