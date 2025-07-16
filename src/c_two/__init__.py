from dotenv import load_dotenv
load_dotenv()

from . import rpc
from . import mcp
from . import crm
from . import compo
from . import error
from .crm.meta import icrm, iicrm
from .rpc.transferable import transfer, auto_transfer, Transferable, transferable

__version__ = '0.2.2'