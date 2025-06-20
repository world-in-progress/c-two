from __future__ import annotations
from enum import Enum, unique
from dataclasses import dataclass
from .. import error
from .util.encoding import add_length_prefix, parse_message

@unique
class EventTag(Enum):
    PING = 'ping'
    PONG = 'pong'
    CRM_CALL = 'crm_call'
    CRM_REPLY = 'crm_reply'
    SHUTDOWN_ACK = 'shutdown_ack'
    SHUTDOWN_FROM_SERVER = 'shutdown_from_server'
    SHUTDOWN_FROM_CLIENT = 'shutdown_from_client'

@dataclass
class Event:
    tag: EventTag
    data: bytes | None
    
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

PingEvent = Event(tag=EventTag.PING, data=None).serialize()
ShutdownEvent = Event(tag=EventTag.SHUTDOWN_FROM_CLIENT, data=None).serialize()

def bvent(tag: EventTag, data: bytes | None = None) -> bytes:
    """
    Create a serialized event with the given tag and optional data.
    
    Parameters:
        tag (EventTag): The event tag.
        data (bytes | None): Optional data for the event.
        
    Returns:
        bytes: Serialized event data.
    """
    return Event(tag=tag, data=data).serialize()
