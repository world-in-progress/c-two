from importlib import import_module
from importlib.metadata import version
__version__ = version('c-two')

from . import error
from .config import BaseIPCOverrides, ClientIPCOverrides, ServerIPCOverrides
from .crm.bridge import ResourceBridge, bridge
from .crm.descriptor import (
    contract_descriptor_diagnostics,
    export_contract_payload_abi_artifacts,
    export_contract_descriptor,
)
from .crm.infer import infer_crm_from_resource
from .crm.meta import crm, read, write, on_shutdown
from .crm.transferable import transferable
from .crm.transferable import transfer, hold, HeldResult
from .transport.server.scheduler import ConcurrencyConfig, ConcurrencyMode
from .transport.registry import (
    set_transport_policy,
    set_server,
    set_client,
    set_relay_anchor,
    register,
    connect,
    close,
    unregister,
    server_address,
    server_id,
    shutdown,
    serve,
    hold_stats,
)

__all__ = [
    '__version__',
    'error',
    'fastdb',
    'BaseIPCOverrides',
    'ClientIPCOverrides',
    'ServerIPCOverrides',
    'ResourceBridge',
    'bridge',
    'contract_descriptor_diagnostics',
    'export_contract_payload_abi_artifacts',
    'export_contract_descriptor',
    'infer_crm_from_resource',
    'crm',
    'read',
    'write',
    'on_shutdown',
    'transferable',
    'transfer',
    'hold',
    'HeldResult',
    'ConcurrencyConfig',
    'ConcurrencyMode',
    'set_transport_policy',
    'set_server',
    'set_client',
    'set_relay_anchor',
    'register',
    'connect',
    'close',
    'unregister',
    'server_address',
    'server_id',
    'shutdown',
    'serve',
    'hold_stats',
]


def __getattr__(name: str):
    if name == 'fastdb':
        module = import_module(f'{__name__}.fastdb')
        globals()[name] = module
        return module
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
