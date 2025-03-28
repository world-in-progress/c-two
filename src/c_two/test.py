import zmq
import subprocess
import sys
import time
import os

# "tcp://127.0.0.1:5555"
def sender():
    context = zmq.Context()
    socket = context.socket(zmq.REQ)  # 主进程使用 REQ
    socket.setsockopt(zmq.LINGER, 0)  # 避免阻塞
    ipc_path = "ipc:///tmp/zmq_test"
    # 清理旧的 IPC 文件
    if os.path.exists("/tmp/zmq_test"):
        os.remove("/tmp/zmq_test")
    socket.bind(ipc_path)
    
    # 等待子进程连接
    time.sleep(0.1)
    
    # 发送消息
    request = b"Hello from main process"
    socket.send(request)
    print("Main sent:", request)
    
    # 接收子进程的回复
    response = socket.recv()
    print("Main received:", response)

if __name__ == '__main__':
    proc = subprocess.Popen([sys.executable, "sub.py"])
    sender()
    proc.wait()