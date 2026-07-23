from importlib.metadata import version
__version__ = version('c-two')

from . import error
from .config import BaseIPCOverrides, ClientIPCOverrides, ServerIPCOverrides
from .codegen import (
    ContractArtifact,
    ContractArtifactSet,
    ContractCodegenError,
    ContractCodegenTarget,
    compile_contract_artifacts,
)
from .crm.bridge import ResourceBridge, bridge
from .crm.descriptor import (
    contract_descriptor_diagnostics,
    export_contract_descriptor,
    export_contract_release_ref,
)
from .crm.infer import infer_crm_from_resource
from .crm.meta import crm, read, write, on_shutdown
from .crm.transferable import hold, transfer, Held, HeldResult
from .transport.input_lifetime import InputLifetime
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
    'BaseIPCOverrides',
    'ClientIPCOverrides',
    'ServerIPCOverrides',
    'ContractArtifact',
    'ContractArtifactSet',
    'ContractCodegenError',
    'ContractCodegenTarget',
    'compile_contract_artifacts',
    'ResourceBridge',
    'bridge',
    'contract_descriptor_diagnostics',
    'export_contract_descriptor',
    'export_contract_release_ref',
    'infer_crm_from_resource',
    'crm',
    'read',
    'write',
    'on_shutdown',
    'hold',
    'transfer',
    'Held',
    'HeldResult',
    'InputLifetime',
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
