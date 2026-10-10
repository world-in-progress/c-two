"""One persistent client; never shorten the native default 60-second grace."""
import json
import os
import sys
import time

import c_two as cc
from idle_pool_contract import Echo


def calls(address: str) -> None:
    for index in range(20):
        value = f'echo-{index}'
        with cc.connect(Echo, name='idle_pool', address=address, timeout=5) as resource:
            assert resource.echo(value) == value


print(json.dumps({'pid': os.getpid()}), flush=True)
for line in sys.stdin:
    command = line.strip()
    if command == 'quit':
        break
    assert command in ('calls', 'idle'), command
    started = time.monotonic()
    if command == 'idle':
        time.sleep(62)
    calls(sys.argv[1])
    print(json.dumps({'done': command, 'elapsed': time.monotonic() - started}), flush=True)
