import os
import json
import time
import uuid
import mmap
import logging
import tempfile
from pathlib import Path

from ... import error
from ..base import BaseClient
from ..event.msg_type import MsgType
from ..util.wire import encode_call, decode, SHUTDOWN_CLIENT_BYTES

logger = logging.getLogger(__name__)

class MemoryClient(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)
        
        self.server_info: dict = {}
        self.region_id = server_address.replace('memory://', '')
        
        # Get temp directory from environment variable or use default
        memory_temp_path = os.getenv('MEMORY_TEMP_PATH', None)
        if memory_temp_path:
            base_temp_dir = Path(memory_temp_path)
            base_temp_dir.mkdir(exist_ok=True, parents=True)
        else:
            base_temp_dir = Path(tempfile.gettempdir())
        self.temp_dir = base_temp_dir / f'{self.region_id}'
        
        self.control_file = self.temp_dir / f'cc_memory_server_{self.region_id}.ctrl'

    def _create_request_file(self, request_id: str, wire_bytes: bytes):
        """Create a request file with wire format bytes."""
        temp_filename = f'cc_event_req_{self.region_id}_{request_id}.temp'
        final_filename = f'cc_event_req_{self.region_id}_{request_id}.mem'
        temp_path = self.temp_dir / temp_filename
        final_path = self.temp_dir / final_filename
        
        temp_path.unlink(missing_ok=True)
        final_path.unlink(missing_ok=True)

        data_length = len(wire_bytes)
        
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = wire_bytes
                mm.flush()
        
        temp_path.rename(final_path)
    
    def _wait_for_response(self, request_id: str, timeout: float = -1.0):
        event_dir = self.temp_dir
        response_filename = f'cc_event_resp_{self.region_id}_{request_id}.mem'
        response_path = event_dir / response_filename
        
        start_time = time.time()
        poll_interval = 0.001  # start with 1ms
        max_interval = 0.1     # max 100ms
        
        while True:
            if not self.temp_dir.exists():
                raise error.CompoClientError(f'Memory server at {self.server_address} is not running or has been closed unexpectedly.')
            
            if response_path.exists():
                try:
                    with open(response_path, 'r+b') as f:
                        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                            response_data = mm.read()
                    response_path.unlink(missing_ok=True)
                    return response_data
                except Exception as e:
                    raise error.CompoClientError(f'Failed to read response file: {e}') from e
            
            if timeout > 0 and (time.time() - start_time) > timeout:
                raise error.CompoClientError(f'Response timeout for request {request_id}')
                
            time.sleep(poll_interval)
            poll_interval = min(poll_interval * 1.5, max_interval)
    
    def connect(self) -> bool:
        try:
            with open(self.control_file, 'r') as f:
                self.server_info = json.load(f)
                
            if self.server_info.get('status', '') != 'running':
                return False
            else:
                return True
        except Exception:
            return False
        
    def call(self, method_name, data: bytes | None = None) -> bytes:
        if not self.connect():
            raise error.CompoClientError(f'Failed to connect to memory server at {self.server_address}')

        request_id = str(uuid.uuid4())
        try:
            wire_bytes = encode_call(method_name, data)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')
        
        self._create_request_file(request_id, wire_bytes)
        response_data = self._wait_for_response(request_id)

        env = decode(response_data)
        if env.msg_type != MsgType.CRM_REPLY:
            raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')
        
        if env.error:
            err = error.CCError.deserialize(env.error)
            if err:
                raise err

        return env.payload if env.payload is not None else b''
    
    def terminate(self):
        pass
    
    def relay(self, event_bytes: bytes) -> bytes:
        request_id = str(uuid.uuid4())
        
        temp_filename = f'cc_event_req_{self.region_id}_{request_id}.temp'
        final_filename = f'cc_event_req_{self.region_id}_{request_id}.mem'
        temp_path = self.temp_dir / temp_filename
        final_path = self.temp_dir / final_filename
        
        temp_path.unlink(missing_ok=True)
        final_path.unlink(missing_ok=True)

        data_length = len(event_bytes)
        
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = event_bytes
                mm.flush()
        
        temp_path.rename(final_path)

        event_dir = self.temp_dir
        response_filename = f'cc_event_resp_{self.region_id}_{request_id}.mem'
        response_path = event_dir / response_filename
        
        while True:
            if response_path.exists():
                with open(response_path, 'r+b') as f:
                    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                        response_data = mm.read()
                response_path.unlink(missing_ok=True)
                return response_data

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        try:
            client = MemoryClient(server_address)
            return client.connect()
        except Exception as e:
            logger.error(f'Failed to ping memory server at {server_address}: {e}')
            return False
    
    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5):
        """Send a shutdown command to the Memory server."""
        try:
            client = MemoryClient(server_address)
            
            if not client.connect():
                return True

            request_id = str(uuid.uuid4())
            client._create_request_file(request_id, SHUTDOWN_CLIENT_BYTES)
            
            response_filename = f'cc_event_resp_{client.region_id}_{request_id}.mem'
            response_path = client.temp_dir / response_filename
            
            start_time = time.time()
            poll_interval = 0.001
            max_interval = 0.1
            
            while True:
                if not client.temp_dir.exists():
                    return True

                if response_path.exists():
                    try:
                        with open(response_path, 'r+b') as f:
                            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                                response_data = mm.read()
                        response_path.unlink(missing_ok=True)

                        env = decode(response_data)
                        if env.msg_type == MsgType.SHUTDOWN_ACK:
                            return True
                        else:
                            raise error.CompoClientError(f'Unexpected response type: {env.msg_type}')
                        
                    except error.CCBaseError:
                        raise
                    except Exception as e:
                        raise error.CompoClientError(f'Failed to read response file: {e}') from e
                
                if timeout > 0 and (time.time() - start_time) > timeout:
                    raise error.CompoClientError(f'Response timeout for request {request_id}')
                    
                time.sleep(poll_interval)
                poll_interval = min(poll_interval * 1.5, max_interval)

        except Exception as e:
            logger.error(str(e))
            return False