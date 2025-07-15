import threading
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .thread_server import ThreadServer
    
# Global registry for thread servers
_THREAD_SERVERS: dict[str, 'ThreadServer'] = {}
_REGISTRY_LOCK = threading.RLock()

def register_server(thread_id: str, server: 'ThreadServer') -> None:
    """Register a thread server in the global registry."""
    with _REGISTRY_LOCK:
        _THREAD_SERVERS[thread_id] = server

def unregister_server(thread_id: str) -> None:
    """Unregister a thread server from the global registry."""
    with _REGISTRY_LOCK:
        _THREAD_SERVERS.pop(thread_id, None)

def get_server(thread_id: str) -> 'ThreadServer | None':
    """Get a thread server from the global registry."""
    with _REGISTRY_LOCK:
        return _THREAD_SERVERS.get(thread_id, None)

def list_servers() -> dict[str, 'ThreadServer']:
    """List all registered thread servers."""
    with _REGISTRY_LOCK:
        return _THREAD_SERVERS.copy()

from .thread_server import ThreadServer
from .thread_client import ThreadClient