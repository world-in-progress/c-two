"""SOTA API — server process.

Registers CRMs and keeps the process alive so remote clients can connect
via IPC.  Press Ctrl-C to shut down.

Run:
    uv run python examples/v2_server.py

Then in another terminal:
    uv run python examples/v2_client.py
"""
import os, sys, signal, threading

# C2_IPC_ADDRESS must be set BEFORE importing c_two, because the pydantic
# settings singleton is instantiated at import time and won't pick up env
# vars that are set afterwards.
BIND_ADDRESS = 'ipc-v3://v2_grid'
os.environ['C2_IPC_ADDRESS'] = BIND_ADDRESS

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid
from crm import Grid


def main():
    # Init the Grid CRM (same as old server.py)
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    grid = Grid(epsg, bounds, first_size, subdivide_rules)

    # Register — one line replaces ServerConfig + Server + start()
    cc.register(IGrid, grid, name='grid')
    print(f'Grid CRM registered at {cc.server_address()}')
    print('Waiting for clients… (Ctrl-C to stop)\n')

    # Block until interrupted
    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    # Cleanup
    cc.unregister('grid')
    cc.shutdown()
    print('\nServer shut down.')


if __name__ == '__main__':
    main()
