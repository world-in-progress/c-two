"""Start NativeRelay + CRM server for full-chain benchmarking."""
import signal, time, sys, os, pickle
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import c_two as cc
from c_two._native import NativeRelay
from c_two.rpc_v2.registry import _ProcessRegistry
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello

port = int(sys.argv[1]) if len(sys.argv) > 1 else 19985
ipc_addr = f'ipc-v3://bench_full_{os.getpid()}'

_ProcessRegistry.reset()
cc.set_address(ipc_addr)
cc.register(IHello, Hello(), name='hello')

relay = NativeRelay(f'127.0.0.1:{port}')
relay.start()
time.sleep(0.1)
relay.register_upstream('hello', ipc_addr)

# Print the pickle payload for external use
payload = pickle.dumps(('Benchmark',))
print(f'RELAY+CRM READY on port {port}', flush=True)
print(f'IPC={ipc_addr}', flush=True)
print(f'PAYLOAD_HEX={payload.hex()}', flush=True)

def stop(sig, frame):
    relay.stop()
    cc.shutdown()
    _ProcessRegistry.reset()
    sys.exit(0)

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)

while True:
    time.sleep(1)
