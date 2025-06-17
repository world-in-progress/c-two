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