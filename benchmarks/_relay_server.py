"""Start NativeRelay for external benchmarking. Runs until killed."""
import signal, time, sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from c_two._native import NativeRelay

port = int(sys.argv[1]) if len(sys.argv) > 1 else 19985
relay = NativeRelay(f'127.0.0.1:{port}')
relay.start()
print(f'RELAY READY on port {port}', flush=True)

def stop(sig, frame):
    relay.stop()
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)

while True:
    time.sleep(1)
