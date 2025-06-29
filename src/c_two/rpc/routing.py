from .memory.memory_routing import memory_routing

async def routing(server_address: str, event_bytes: bytes, timeout: float = 10.0) -> bytes:
    """Relay event bytes to server and get response bytes asynchronously."""
    if server_address.startswith(('tcp://', 'ipc://')):
        return # TODO: Implement ZMQ routing
    elif server_address.startswith('http://'):
        return # TODO: Implement HTTP routing
    elif server_address.startswith('memory://'):
        return await memory_routing(server_address, event_bytes, timeout)
    else:
        # TODO: Handle other protocols if needed
        raise ValueError(f'Unsupported protocol in server_address: {server_address}')