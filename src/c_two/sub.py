import zmq
import sys

def receiver():
    context = zmq.Context()
    socket = context.socket(zmq.REP)  # 子进程使用 REP
    socket.connect("ipc:///tmp/zmq_test")
    
    # 接收主进程的消息
    request = socket.recv()
    print("Subprocess received:", request)
    
    # 发送回复
    response = b"Hi from subprocess"
    socket.send(response)
    print("Subprocess sent:", response)
    
    sys.exit(0)  # 确保子进程退出

receiver()