from importlib.metadata import version
__version__ = version('c-two')

from . import compo
from . import error
from .compo import runtime
from .crm.meta import icrm, read, write, on_shutdown
from .crm.transferable import transferable
from .crm.transferable import transfer, hold, HeldResult
from .transport.registry import (
    set_ipc_address,
    set_config,
    set_server,
    set_client,
    set_shm_threshold,
    set_server_ipc_config,
    set_client_ipc_config,
    set_relay,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
    hold_stats,
)

def _load_banner(name: str) -> str:
    """Load a pre-generated banner from package resources.

    Returns empty string if the file does not exist.
    """
    from importlib.resources import files
    resource = files("c_two").joinpath(name)
    try:
        return resource.read_text(encoding="utf-8")
    except FileNotFoundError:
        return ""


LOGO_UNICODE = _load_banner("banner_unicode.txt")
