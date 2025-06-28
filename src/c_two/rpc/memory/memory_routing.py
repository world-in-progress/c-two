import json
import uuid
import mmap
import asyncio
import logging
import tempfile
from pathlib import Path

from ... import error

logger = logging.getLogger(__name__)

async def memory_routing(server_address: str, event_bytes: bytes, timeout: float = 10.0) -> bytes:
    """Relay event bytes to memory server and get response bytes asynchronously."""

    request_id = uuid.uuid4()
    region_id = server_address.replace('memory://', '')
    temp_dir = Path(tempfile.gettempdir()) / f'{region_id}'
    control_file = temp_dir / f'cc_memory_server_{region_id}.ctrl'
    
    loop = asyncio.get_event_loop()
    
    # --- 1. Check memory server asynchronously ---
    def check_server_status():
        if not control_file.is_file():
            raise error.CompoClientError(f'Memory server control file {control_file} does not exist.')
        with open(control_file, 'r') as f:
            server_info = json.load(f)
        if server_info.get('status') != 'running':
            raise error.CompoClientError(f'Memory server at {server_address} is not running.')

    try:
        await loop.run_in_executor(None, check_server_status)
    except Exception as e:
        raise e

    # --- 2. Send event to memory server asynchronously ---
    temp_filename = f'cc_event_req_{region_id}_{request_id}.temp'
    final_filename = f'cc_event_req_{region_id}_{request_id}.mem'
    temp_path = temp_dir / temp_filename
    final_path = temp_dir / final_filename

    # Ensure response file does not exist
    temp_path.unlink(missing_ok=True)
    final_path.unlink(missing_ok=True)

    # Write the event bytes to a temporary file
    data_length = len(event_bytes)
    
    def write_request_file():
        with open(temp_path, 'w+b') as f:
            f.truncate(data_length)
            
            # Memory map and write data
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_WRITE) as mm:
                mm[:data_length] = event_bytes
                mm.flush()
        
        # Rename the request file to a permanent name
        temp_path.rename(final_path)
    
    await loop.run_in_executor(None, write_request_file)

    # --- 3. Polling for response asynchronously ---
    response_filename = f'cc_event_resp_{region_id}_{request_id}.mem'
    response_path = temp_dir / response_filename
    start_time = asyncio.get_event_loop().time()
    try:
        while True:
            if response_path.exists():
                def read_response():
                    with open(response_path, 'r+b') as f:
                        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mm:
                            return mm.read()
                
                response_data = await loop.run_in_executor(None, read_response)
                return response_data
            
            if (loop.time() - start_time) > timeout:
                raise asyncio.TimeoutError('Waiting for response timed out.')
                
            await asyncio.sleep(0.01)
            
    finally:
        # --- 4. Clean up response file ---
        response_path.unlink(missing_ok=True)