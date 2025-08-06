import os
import enum
import json
import mmap
import logging
import tempfile
import threading
from pathlib import Path
from watchdog.observers import Observer
from watchdog.events import FileSystemEventHandler

from ..util.wait import wait
from ..base import BaseServer
from ..event import Event, EventTag, EventQueue

logger = logging.getLogger(__name__)

MAXIMUM_WAIT_TIMEOUT = 1

class MemoryEventHandler(FileSystemEventHandler):
    def __init__(self, file_pattern: str, file_extension: str = '.mem'):
        self.file_received = False
        self.file_pattern = file_pattern
        self.file_extension = file_extension
        self.condition = threading.Condition()
    
    def on_moved(self, event):
        if not event.is_directory:
            dest_filename = os.path.basename(event.dest_path)
            if dest_filename.startswith(self.file_pattern) and dest_filename.endswith(self.file_extension):
                with self.condition:
                    self.file_received = True
                    self.condition.notify_all() # wake up all waiting threads

class MemoryServerState(enum.Enum):
    STOPPED = 'stopped'
    RUNNING = 'running'

class MemoryServer(BaseServer):

    def __init__(self, bind_address, event_queue: EventQueue | None = None):
        super().__init__(bind_address, event_queue)
        
        self._shutdown_event = threading.Event()
        self._server_started = threading.Event()
        
        self.region_id = bind_address.replace('memory://', '')
        
        # Get temp directory from environment variable or use default
        memory_temp_dir = os.getenv('MEMORY_TEMP_DIR', None)
        if memory_temp_dir:
            base_temp_dir = Path(memory_temp_dir)
            base_temp_dir.mkdir(exist_ok=True, parents=True)
        else:
            base_temp_dir = Path(tempfile.gettempdir())
        self.temp_dir = base_temp_dir / f'{self.region_id}'
        
        self.control_file = self.temp_dir / f'cc_memory_server_{self.region_id}.ctrl'
        
        # Pre-cleanup the temp directory
        self._cleanup_temp_dir()
        
        # Create the temp directory if it doesn't exist
        self.temp_dir.mkdir(exist_ok=True, parents=True)
    
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
        # Write the server information to the temporary control file
        server_info = {
            'server_pid': os.getpid(),
            'bind_address': self.bind_address,
            'temp_dir': str(self.temp_dir),
            'status': state.value
        }
        
        temp_control_file = self.temp_dir / f'cc_memory_server_{self.region_id}.temp'
        with open(temp_control_file, 'w') as f:
            json.dump(server_info, f, indent=4)
        
        # Rename the temporary control file to the final name
        if self.control_file.exists():
            self.control_file.unlink(missing_ok=True)
        temp_control_file.rename(self.control_file)
        
        logger.debug(f'Created or Updated control file at {self.control_file}')
    
    def _poll_memory_events(self) -> Event | None:
        event_dir = self.temp_dir
        event_pattern = f'cc_event_req_{self.region_id}_'
        
        try:
            request_files = []
            for filename in os.listdir(str(event_dir)):
                if filename.startswith(event_pattern) and filename.endswith('.mem'):
                    try:
                        file_path = event_dir / filename
                        stat = file_path.stat()
                        request_files.append((file_path, stat.st_mtime, filename))
                    except FileNotFoundError:
                        # File was deleted while checking, skip it
                        continue

            # If no request files, return None
            if not request_files:
                return None
            
            # Process the earliest file
            request_files.sort(key=lambda x: x[1]) # sort by modification time
            file_path, _, filename = request_files[0]
            
            # Parse request information from the file name
            request_id = filename.replace(event_pattern, '').replace('.mem', '')
            
            try:
                # Read event data from the file
                with open(file_path, 'r+b') as f:
                    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                        data_bytes = mm.read()
                        event = Event.deserialize(data_bytes)
                        event.request_id = request_id
                
                # Clean up the file after processing
                file_path.unlink(missing_ok=True)
                return event
            
            except FileNotFoundError:
                # File was deleted while reading, return None
                return None
            
            except Exception as e:
                # Delete the corrupted file, return None
                file_path.unlink(missing_ok=True)
                return None
            
        except Exception as e:
            logger.error(f'Error polling memory events: {e}')
            
        return None
    
    def _serve(self):
        self._server_started.set()
        
        # Setup file system watcher
        event_handler = MemoryEventHandler(f'cc_event_req_{self.region_id}_', '.mem')
        observer = Observer()
        observer.schedule(event_handler, str(self.temp_dir), recursive=False)
        observer.start()
        
        # Start the service loop
        try:
            while True:
                # Check if server is closed unexpectedly
                if not self.temp_dir.exists():
                    logger.error(f'Temporary directory {self.temp_dir} does not exist. Shutting down server.')
                    self._shutdown_event.set()
                    self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_SERVER))
                    break
                
                # Check if the shutdown event is set
                if self._shutdown_event.is_set():
                    self.event_queue.put(Event(EventTag.SHUTDOWN_FROM_SERVER))
                    break
                
                # Try to poll for memory events
                event = self._poll_memory_events()
                if event:
                    self.event_queue.put(event)
                    if event.tag == EventTag.SHUTDOWN_FROM_CLIENT:
                        break
                else:
                    # Wait for new event file creation notification
                    with event_handler.condition:
                        if event_handler.condition.wait(MAXIMUM_WAIT_TIMEOUT):
                            if event_handler.file_received:
                                    event_handler.file_received = False
                                    continue  # file was received, continue to poll again
        finally:
            observer.stop()
            observer.join()

    def start(self):
        server_thread = threading.Thread(target=self._serve)
        server_thread.daemon = True
        server_thread.start()
        
        # Wait for the server to start
        wait(
            self._server_started.wait(),
            self._server_started.is_set,
            MAXIMUM_WAIT_TIMEOUT
        )
        
        # Create the control file
        self._create_control_file()
        
    def _create_response_file(self, event: Event):
        event_dir = self.temp_dir
        
        # Create the response paths
        temp_filename = f'cc_event_resp_{self.region_id}_{event.request_id}.temp'
        final_filename = f'cc_event_resp_{self.region_id}_{event.request_id}.mem'
        temp_path = event_dir / temp_filename
        final_path = event_dir / final_filename
        
        # Ensure response file does not exist
        temp_path.unlink(missing_ok=True)
        final_path.unlink(missing_ok=True)

        # Serialize the event to bytes and write to the response file
        data_bytes = event.serialize()
        data_length = len(data_bytes)
        
        # Write to a temporary file
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            
            # Memory map and write data
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = data_bytes
                mm.flush()

        # Rename the response file to a permanent name
        temp_path.rename(final_path)

    def reply(self, event: Event):
        self._create_response_file(event)
    
    def shutdown(self):
        self._create_control_file(MemoryServerState.STOPPED)
        self._shutdown_event.set()
    
    def destroy(self):
        self._cleanup_temp_dir()

    def cancel_all_calls(self):
        pass