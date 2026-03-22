from __future__ import annotations
from dataclasses import dataclass
from enum import Enum, unique
from .msg_type import MsgType


@unique
class CompletionType(Enum):
    OP_REQUEST = 'op_request'
    OP_TIMEOUT = 'op_timeout'
    OP_COMPLETE = 'op_complete'


@dataclass
class Envelope:
    """In-process message envelope — never serialized to the wire.

    Carries decoded control metadata alongside a zero-copy reference
    (memoryview) to the payload bytes.  Replaces ``Event`` as the object
    that flows through ``EnvelopeQueue`` and the server scheduler.
    """
    msg_type: MsgType
    request_id: str | None = None
    completion_type: CompletionType = CompletionType.OP_REQUEST

    # CRM_CALL fields
    method_name: str | None = None

    # Payload: raw serialized args (CRM_CALL) or result (CRM_REPLY).
    # Kept as memoryview whenever possible for zero-copy.
    payload: bytes | memoryview | None = None

    # CRM_REPLY error part (raw CCError bytes, length from wire header).
    error: bytes | memoryview | None = None
