import struct

def add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes

def parse_message(full_message: bytes) -> list[memoryview]:
    buffer = memoryview(full_message)
    messages = []
    offset = 0
    
    while offset < len(buffer):
        if offset + 8 > len(buffer):
            raise ValueError("Incomplete length prefix at end of message")
        
        length = struct.unpack('>Q', buffer[offset:offset + 8])[0]
        offset += 8
        
        if offset + length > len(buffer):
            raise ValueError(f"Message length {length} exceeds remaining buffer size at offset {offset}")
        
        message = buffer[offset:offset + length]
        messages.append(message)
        offset += length
    
    if offset != len(buffer):
        raise ValueError(f"Extra bytes remaining after parsing: {len(buffer) - offset}")
    
    return messages


def event_to_wire_bytes(event) -> bytes | bytearray:
    """Convert a legacy Event reply to compact wire format bytes.

    Bridge function for the transitional period while the scheduler still
    produces Event objects.  Will be removed once the scheduler emits
    Envelope directly.
    """
    from .wire import encode_reply, encode_signal
    from ..event.msg_type import MsgType
    from ..event import EventTag

    _tag_to_signal = {
        EventTag.PONG: MsgType.PONG,
        EventTag.SHUTDOWN_ACK: MsgType.SHUTDOWN_ACK,
        EventTag.PING: MsgType.PING,
        EventTag.SHUTDOWN_FROM_CLIENT: MsgType.SHUTDOWN_CLIENT,
        EventTag.SHUTDOWN_FROM_SERVER: MsgType.SHUTDOWN_SERVER,
    }
    signal_type = _tag_to_signal.get(event.tag)
    if signal_type is not None:
        return encode_signal(signal_type)

    # CRM_REPLY — extract error and result
    if event.data_parts is not None and event.data is None:
        err = event.data_parts[0] if event.data_parts[0] else b''
        res = event.data_parts[1] if len(event.data_parts) > 1 and event.data_parts[1] else b''
    else:
        data = event.get_data()
        if not data:
            return encode_reply(b'', b'')
        parts = parse_message(data)
        err = parts[0] if parts else b''
        res = parts[1] if len(parts) > 1 else b''
    return encode_reply(err, res)