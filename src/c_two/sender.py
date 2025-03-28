import sys
import zmq
import pyarrow as pa

ipc_addr = sys.argv[1]
context = zmq.Context()
socket = context.socket(zmq.PAIR)
socket.bind(ipc_addr)

data = pa.record_batch([
    pa.array([1, 2, 3, 4], type=pa.int8()),
    pa.array(["a", "b", "c", "d"])
], names=["int_col", "str_col"])

# 将数据序列化为 IPC 格式
sink = pa.BufferOutputStream()
with pa.ipc.new_stream(sink, data.schema) as writer:
    writer.write_batch(data)
buf = sink.getvalue()

# 通过 PAIR 套接字发送数据（零拷贝）
socket.send(buf.to_pybytes(), copy=False)

print("发送端：数据已发送")
socket.close()
context.term()