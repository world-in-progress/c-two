import os
import zmq
import struct
import google.protobuf.message
import proto.common.base_pb2 as base

class CRMServer:
    def __init__(self, crm: object, server_address):
        self.crm = crm
        
        if os.path.exists("/tmp/zmq_test"):
            os.remove("/tmp/zmq_test")
        
        self.context = zmq.Context()
        self.socket = self.context.socket(zmq.REP)
        self.socket.bind(server_address)
        self.server_address = server_address
    
    def terminate(self):
        self.socket.close()
        self.context.term()
    
    def run_service(self):
        while True:
            full_request = self.socket.recv()
            sub_messages = _parse_message(full_request)
            if len(sub_messages) != 2:
                raise ValueError("Expected exactly 2 sub-messages (meta and data)")
            
            # Get request meta information
            request_meta: base.BaseRequestMetaInfo = base.BaseRequestMetaInfo()
            request_meta.ParseFromString(sub_messages[0])
            
            # Get requets arguments
            request_args_bytes = sub_messages[1]
            
            # Call CRM interface
            response: google.protobuf.message.Message
            method = getattr(self.crm, request_meta.method_name)
            _, response = method(request_args_bytes)
            
            # Send response
            self.socket.send(response)

# Helper ##################################################
    
def _add_length_prefix(message_bytes: bytes):
    length = len(message_bytes)
    prefix = struct.pack('>Q', length)
    return prefix + message_bytes

def _parse_message(full_message: bytes) -> list[memoryview]:
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