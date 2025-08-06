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
from ..event import Event, EventTag
from ..util.encoding import add_length_prefix, parse_message

logger = logging.getLogger(__name__)

class MemoryClient(BaseClient):
    def __init__(self, server_address: str):
        super().__init__(server_address)
        
        self.server_info: dict = {}
        self.region_id = server_address.replace('memory://', '')
        
        # Get temp directory from environment variable or use default
        memory_temp_dir = os.getenv('MEMORY_TEMP_DIR', None)
        if memory_temp_dir:
            base_temp_dir = Path(memory_temp_dir)
            base_temp_dir.mkdir(exist_ok=True, parents=True)
        else:
            base_temp_dir = Path(tempfile.gettempdir())
        self.temp_dir = base_temp_dir / f'{self.region_id}'
        
        self.control_file = self.temp_dir / f'cc_memory_server_{self.region_id}.ctrl'

    def _create_method_event(self, method_name: str, data: bytes | None = None) -> Event:
        """Create a Event for the given method."""
        try:
            request_id = str(uuid.uuid4())
            serialized_data = b'' if data is None else data
            serialized_method_name = method_name.encode('utf-8')
            combined_request = add_length_prefix(serialized_method_name) + add_length_prefix(serialized_data)
            event = Event(tag=EventTag.CRM_CALL, data=combined_request, request_id=request_id)
        except Exception as e:
            raise error.CompoSerializeInput(f'Error occurred when serializing request: {e}')

        return event

    def _create_request_file(self, event: Event):
        """Create a request file for the given Event."""
        # Create the response paths
        temp_filename = f'cc_event_req_{self.region_id}_{event.request_id}.temp'
        final_filename = f'cc_event_req_{self.region_id}_{event.request_id}.mem'
        temp_path = self.temp_dir / temp_filename
        final_path = self.temp_dir / final_filename
        
        # Ensure response file does not exist
        temp_path.unlink(missing_ok=True)
        final_path.unlink(missing_ok=True)

        # Serialize the event to bytes
        data_bytes = event.serialize()
        data_length = len(data_bytes)
        
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            
            # Memory map and write data
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = data_bytes
                mm.flush()
        
        # Rename the request file to a permanent name
        temp_path.rename(final_path)
    
    def _wait_for_response(self, request_id: str, timeout: float = -1.0) -> Event:
        event_dir = self.temp_dir
        response_filename = f'cc_event_resp_{self.region_id}_{request_id}.mem'
        response_path = event_dir / response_filename
        
        start_time = time.time()
        poll_interval = 0.001  # start with 1ms
        max_interval = 0.1     # max 100ms
        
        while True:
            # Check if server is closed unexpectedly
            if not self.temp_dir.exists():
                raise error.CompoClientError(f'Memory server at {self.server_address} is not running or has been closed unexpectedly.')
            
            if response_path.exists():
                try:
                    with open(response_path, 'r+b') as f:
                        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                            response_data = mm.read()
                    response_path.unlink(missing_ok=True)

                    # Deserialize Event
                    event = Event.deserialize(response_data)
                    return event
                except Exception as e:
                    raise error.CompoClientError(f'Failed to read response file: {e}')
            
            if timeout > 0 and (time.time() - start_time) > timeout:
                raise error.CompoClientError(f'Response timeout for request {request_id}')
                
            time.sleep(poll_interval)
            
            # Exponential backoff for polling interval
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

        # Create an event for the method call
        event = self._create_method_event(method_name, data)
        request_id = event.request_id
        
        # Create request file
        self._create_request_file(event)

        # Wait for response
        event = self._wait_for_response(request_id)
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
    
    def terminate(self):
        pass
    
    def relay(self, event_bytes: bytes) -> bytes:
        request_id = str(uuid.uuid4())
        
        # Create the response paths
        temp_filename = f'cc_event_req_{self.region_id}_{request_id}.temp'
        final_filename = f'cc_event_req_{self.region_id}_{request_id}.mem'
        temp_path = self.temp_dir / temp_filename
        final_path = self.temp_dir / final_filename
        
        # Ensure response file does not exist
        temp_path.unlink(missing_ok=True)
        final_path.unlink(missing_ok=True)

        # Write the event bytes to a temporary file
        data_length = len(event_bytes)
        
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            
            # Memory map and write data
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = event_bytes
                mm.flush()
        
        # Rename the request file to a permanent name
        temp_path.rename(final_path)

        # Wait for response
        event_dir = self.temp_dir
        response_filename = f'cc_event_resp_{self.region_id}_{request_id}.mem'
        response_path = event_dir / response_filename
        
        while True:
            if response_path.exists():
                with open(response_path, 'r+b') as f:
                    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                        response_data = mm.read()
                response_path.unlink(missing_ok=True)

                # Return Event bytes
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
            
            # Check if the server is running
            if not client.connect():
                return True # server is not runnning, return True
            
            # Create shutdown event
            temp_dir = client.temp_dir
            request_id = str(uuid.uuid4())
            shutdown_event = Event(tag=EventTag.SHUTDOWN_FROM_CLIENT, request_id=request_id)
            
            # Create request file
            client._create_request_file(shutdown_event)
            
            # Wait for shutdown acknowledgment
            response_filename = f'cc_event_resp_{client.region_id}_{request_id}.mem'
            response_path = temp_dir / response_filename
            
            start_time = time.time()
            poll_interval = 0.001  # start with 1ms
            max_interval = 0.1     # max 100ms
            
            while True:
                # Check if server is closed unexpectedly
                if not client.temp_dir.exists():
                    return True # server is not running or has been closed

                if response_path.exists():
                    try:
                        with open(response_path, 'r+b') as f:
                            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                                response_data = mm.read()
                        response_path.unlink(missing_ok=True)

                        # Deserialize Event
                        event = Event.deserialize(response_data)
                        
                        # Check the shutdown acknowledgment
                        if event.tag == EventTag.SHUTDOWN_ACK:
                            return True
                        else:
                            raise error.CompoClientError(f'Unexpected event tag: {event.tag}. Expected: {EventTag.SHUTDOWN_ACK}')
                        
                    except Exception as e:
                        raise error.CompoClientError(f'Failed to read response file: {e}')
                
                if timeout > 0 and (time.time() - start_time) > timeout:
                    raise error.CompoClientError(f'Response timeout for request {request_id}')
                    
                time.sleep(poll_interval)
                
                # Exponential backoff for polling interval
                poll_interval = min(poll_interval * 1.5, max_interval)

        except Exception as e:
            logger.error(str(e))
            return False