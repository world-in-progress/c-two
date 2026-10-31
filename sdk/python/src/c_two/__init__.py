"""Resource RPC for applications: declare contracts, register resources and connect clients.

Supervisors use ``endpoint`` for local endpoint administration; operators use
``observe`` for diagnostics, and authors use
``contract`` for descriptor and code-generation tools.
"""
from importlib.metadata import version as _version

__version__ = _version('c-two')

from . import error
from .crm.meta import crm, read, write, on_shutdown
from .crm.transferable import hold, transfer, HeldResult
from .transport.client.proxy import with_call_options
from .transport.server.scheduler import ConcurrencyConfig
from .transport.registry import (
    Runtime, register, connect, close, unregister, serve, shutdown,
    server_address, set_relay_anchor, set_server, set_client,
    set_transport_policy, set_call_execution_limits,
)

from . import endpoint, observe, contract

__all__ = [
    'crm',
    'read',
    'write',
    'transfer',
    'on_shutdown',
    'hold',
    'HeldResult',
    'register',
    'connect',
    'close',
    'unregister',
    'serve',
    'shutdown',
    'server_address',
    'with_call_options',
    'set_relay_anchor',
    'set_server',
    'set_client',
    'Runtime',
    'ConcurrencyConfig',
    'error',
    'endpoint',
    'observe',
    'contract',
    '__version__',
    'set_transport_policy',
    'set_call_execution_limits',
]
