import sys
import zmq
import pyarrow as pa

ipc_addr = sys.argv[1]

context = zmq.Context()
socket = context.socket(zmq.PAIR)
socket.connect(ipc_addr)

msg = socket.recv(copy=False)
buf = pa.py_buffer(msg)
with pa.ipc.open_stream(buf) as reader:
    schema = reader.schema
    batches = [b for b in reader]

received_data = batches[0]
print("接收端：收到数据")
print(f"Schema: {schema}")
print(f"Data: {received_data}")

socket.close()
context.term()