"""Keep one resource server alive until the test closes its stdin."""
import json
import os
import sys

import c_two as cc
from idle_pool_contract import Echo


class EchoResource:
    def echo(self, value: str) -> str:
        return value


cc.register(Echo, EchoResource(), name='idle_pool')
print(json.dumps({'pid': os.getpid(), 'address': cc.server_address()}), flush=True)
try:
    sys.stdin.read()
finally:
    cc.shutdown()
