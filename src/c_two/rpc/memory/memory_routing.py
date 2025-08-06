import os
import json
import uuid
import mmap
import asyncio
import logging
import tempfile
from pathlib import Path

from ... import error

logger = logging.getLogger(__name__)

async def memory_routing(server_address: str, event_bytes: bytes, timeout: float = -1.0) -> bytes:
    """Relay event bytes to memory server and get response bytes asynchronously."""

    request_id = uuid.uuid4()
    region_id = server_address.replace('memory://', '')
        
    # Get temp directory from environment variable or use default
    memory_temp_dir = os.getenv('MEMORY_TEMP_DIR', None)
    if memory_temp_dir:
        base_temp_dir = Path(memory_temp_dir)
        base_temp_dir.mkdir(exist_ok=True, parents=True)
    else:
        base_temp_dir = Path(tempfile.gettempdir())
    temp_dir = base_temp_dir / f'{region_id}'
    
    control_file = temp_dir / f'cc_memory_server_{region_id}.ctrl'
    
    loop = asyncio.get_event_loop()
    
    # --- 1. Check memory server asynchronously ---
    def check_server_status():
        if not control_file.is_file():
            raise error.CompoClientError(f'Memory server control file {control_file} does not exist.')
        try:
            with open(control_file, 'r') as f:
                server_info = json.load(f)
        except Exception as e:
            raise error.CompoClientError(f'Error reading memory server control file {control_file}: {e}')
        
        if server_info.get('status') != 'running':
            raise error.CompoClientError(f'Memory server at {server_address} is not running.')

    await loop.run_in_executor(None, check_server_status)

    # --- 2. Send event to memory server asynchronously ---
    temp_filename = f'cc_event_req_{region_id}_{request_id}.temp'
    final_filename = f'cc_event_req_{region_id}_{request_id}.mem'
    response_filename = f'cc_event_resp_{region_id}_{request_id}.mem'
    temp_path = temp_dir / temp_filename
    final_path = temp_dir / final_filename
    response_path = temp_dir / response_filename

    # Ensure response file does not exist
    for path in [temp_path, final_path, response_path]:
        path.unlink(missing_ok=True)

    # Write the event bytes to a temporary file
    data_length = len(event_bytes)
    
    def write_request_file():
        try:
            temp_dir.mkdir(parents=True, exist_ok=True)
            with open(temp_path, 'w+b') as f:
                f.truncate(data_length)
                
                # Memory map and write data
                with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                    mm[:data_length] = event_bytes
                    mm.flush()
            
            # Rename the request file to a permanent name
            temp_path.rename(final_path)
            
        except Exception as e:
            raise error.CompoClientError(f'Failed to write request file: {e}')
    
    await loop.run_in_executor(None, write_request_file)

    # --- 3. Poll for response asynchronously ---
    start_time = loop.time()
    poll_interval = 0.001  # start with 1ms
    max_interval = 0.1     # max 100ms
    try:
        while True:
            if response_path.exists():
                def read_response():
                    try:
                        with open(response_path, 'r+b') as f:
                            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                                return mm.read()
                    except Exception as e:
                        raise error.CompoClientError(f'Failed to read response file: {e}')
                
                response_data = await loop.run_in_executor(None, read_response)
                return response_data
            
            if timeout > 0 and (loop.time() - start_time) > timeout:
                raise asyncio.TimeoutError('Waiting for response timed out.')
                
            await asyncio.sleep(poll_interval)
            # Exponential backoff for polling interval
            poll_interval = min(poll_interval * 1.5, max_interval)
            
    finally:
        # --- 4. Clean up response file ---
        response_path.unlink(missing_ok=True)