from __future__ import annotations
from enum import Enum, unique
from dataclasses import dataclass
import logging
from ... import error
from ..util.encoding import add_length_prefix, parse_message

logger = logging.getLogger(__name__)

class CompletionType(Enum):
    OP_REQUEST = 'op_request'       # request for a queue operation
    OP_TIMEOUT = 'op_timeout'       # queue operation timed out
    OP_COMPLETE = 'op_complete'     # queue operation completed successfully

@unique
class EventTag(Enum):
    PING = 'ping'
    PONG = 'pong'
    EMPTY = 'empty'
    CRM_CALL = 'crm_call'
    CRM_REPLY = 'crm_reply'
    SHUTDOWN_ACK = 'shutdown_ack'
    SHUTDOWN_FROM_SERVER = 'shutdown_from_server'
    SHUTDOWN_FROM_CLIENT = 'shutdown_from_client'

@dataclass
class Event:
    tag: EventTag
    data: bytes | None = None
    completion_type: CompletionType = CompletionType.OP_REQUEST
    request_id: str | None = None

    def serialize(self) -> bytes:
        try:
            tag_bytes = self.tag.value.encode('utf-8')
            data_bytes = b'' if self.data is None else self.data
            return add_length_prefix(tag_bytes) + add_length_prefix(data_bytes)
        except Exception as e:
            raise error.EventSerializeError(f'Error occurred when serialize event: {e}')
    
    @staticmethod
    def deserialize(content: bytes) -> Event:
        try:
            if not content:
                raise ValueError('Event content is empty')

            tag_bytes, data = parse_message(content)
            return Event(tag=EventTag(tag_bytes.tobytes().decode('utf-8')), data=data)

        except Exception as e:
            raise error.EventDeserializeError(f'Error occurred when deserialize event: {e}')