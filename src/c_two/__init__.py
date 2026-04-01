from importlib.metadata import version
__version__ = version('c-two')

from . import compo
from . import error
from .compo import runtime
from .crm.meta import icrm, read, write, on_shutdown
from .crm.transferable import transferable
from .transport.registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
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
