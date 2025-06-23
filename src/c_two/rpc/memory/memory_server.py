import os
import enum
import json
import logging
import tempfile
import threading
from pathlib import Path

from ..base import BaseServer
from ..event import Event, EventTag, EventQueue

logger = logging.getLogger(__name__)

class MemoryServerState(enum.Enum):
    STOPPED = 'stopped'
    RUNNING = 'running'

class MemoryServer(BaseServer):

    def __init__(self, bind_address, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)
        
        self.shutdown_event = threading.Event()
        self.region_id = bind_address.replace('memory://', '')
        self.temp_dir = Path(tempfile.gettempdir()) / f'{self.region_id}'
        self.control_file = self.temp_dir / f'cc_memory_server_{self.region_id}.ctrl'
        
        
        # Pre-cleanup the temp directory
        self._cleanup_temp_dir()
        
        # Create the temp directory if it doesn't exist
        self.temp_dir.mkdir(exist_ok=True, parents=True)
        
        # Create the control file
        self._create_control_file()
    
    def _cleanup_temp_dir(self):
        try:
            if self.temp_dir and self.temp_dir.exists():
                for child in self.temp_dir.iterdir():
                    if child.is_file():
                        child.unlink()
                self.temp_dir.rmdir()
        except Exception as e:
            logger.error(f'Failed to clean up temp dir {self.temp_dir}: {e}')

    def _create_control_file(self, state: MemoryServerState = MemoryServerState.RUNNING):
        
        # Write the server information to the control file
        server_info = {
            'server_pid': os.getpid(),
            'bind_address': self.bind_address,
            'temp_dir': str(self.temp_dir),
            'status': state.value
        }
        
        with open(self.control_file, 'w') as f:
            json.dump(server_info, f, indent=4)
        
        logger.debug(f'Created or Updated control file at {self.control_file}')
    
    def _poll_memory_events(self) -> Event | None:
        event_dir = self.temp_dir
        event_pattern = f'cc_event_req_{self.region_id}_'
        
        try:
            for filename in os.listdir(str(event_dir)):
                if filename.startswith(event_pattern) and filename.endswith('.mem'):
                    file_path = event_dir / filename

                    # Parse request information from the file name
                    request_id = filename.replace(event_pattern, '').replace('.mem', '')
                    
                    # Create a Event from the request file content
                    with open(file_path, 'rb') as f:
                        data_bytes = f.read()
                        event = Event.deserialize(data_bytes)
                        event.request_id = request_id
                        
                    # Clean up the file after processing
                    file_path.unlink(missing_ok=True)
                    return event
        except FileNotFoundError:
            logger.warning(f'No memory event files found in {event_dir}')
            
        return None
    
    def _serve(self):
        while True:
            # Check if the shutdown event is set
            if self.shutdown_event.is_set():
                self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_SERVER))
                break
            
            # Pool for memory events
            event = self._poll_memory_events()
            if event:
                self.event_queue.put(event)
                if event.tag == EventTag.SHUTDOWN_FROM_CLIENT:
                    break
    
    def start(self):
        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()
    
    def _create_response_file(self, event: Event):
        event_dir = self.temp_dir
        
        # Create the response file with a temporary name
        response_filename = f'_temp_cc_event_resp_{self.region_id}_{event.request_id}.mem'
        response_path = event_dir / response_filename

        # Serialize the event to bytes and write to the response file
        with open(response_path, 'wb') as f:
            f.write(event.serialize())
        
        # Rename the response file to a permanent name
        final_response_filename = f'cc_event_resp_{self.region_id}_{event.request_id}.mem'
        final_response_path = event_dir / final_response_filename
        response_path.rename(final_response_path)
    
    def reply(self, event: Event):
        self._create_response_file(event)
    
    def shutdown(self):
        self._create_control_file(MemoryServerState.STOPPED)
        self.shutdown_event.set()
    
    def destroy(self):
        self._cleanup_temp_dir()

    def cancel_all_calls(self):
        pass