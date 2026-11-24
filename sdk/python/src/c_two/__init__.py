"""Resource RPC for applications: declare contracts, register resources and connect clients.

Supervisors use ``endpoint`` for local endpoint administration; operators use
``observe`` for diagnostics, and authors use
``contract`` for descriptor and code-generation tools.
"""
from importlib.metadata import version as _version

__version__ = _version('c-two')

from . import error
from ._call_context import AdmissionContext, AdmissionDecision, CallContext, current_call_context
from .crm.meta import crm, read, write, stream, topic, on_shutdown
from .crm.transferable import hold, recover_call, transfer, with_call_id, HeldResult
from .crm.stream import Stream, StreamCloseReceipt
from .transport.client.proxy import CallOutcome, query_call, with_call_options
from .transport.server.scheduler import ConcurrencyConfig
from .transport.topic import Delivery, Publisher, Registration, Subscription, TopicCloseReceipt
from .transport.registry import (
    Runtime, register, connect, close, unregister, serve, shutdown,
    server_address, set_relay_anchor, set_server, set_client,
)

from . import endpoint, observe, contract

__all__ = [
    'crm',
    'read',
    'write',
    'stream',
    'topic',
    'Stream',
    'StreamCloseReceipt',
    'transfer',
    'on_shutdown',
    'hold',
    'with_call_id',
    'recover_call',
    'query_call',
    'CallOutcome',
    'AdmissionContext',
    'AdmissionDecision',
    'CallContext',
    'current_call_context',
    'HeldResult',
    'Delivery',
    'Publisher',
    'Registration',
    'Subscription',
    'TopicCloseReceipt',
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
]
