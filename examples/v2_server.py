"""SOTA API — server process.

Registers CRMs and keeps the process alive so remote clients can connect
via IPC.  Press Ctrl-C to shut down.

Run:
    uv run python examples/v2_server.py

Then in another terminal:
    uv run python examples/v2_client.py
"""
import os, sys, signal, threading
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid
from crm import Grid

# Fixed address so the client knows where to connect.
BIND_ADDRESS = 'ipc-v3://v2_grid'


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
    cc.register(IGrid, grid, bind_address=BIND_ADDRESS)
    print(f'Grid CRM registered at {cc.server_address()}')
    print('Waiting for clients… (Ctrl-C to stop)\n')

    # Block until interrupted
    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    # Cleanup
    cc.unregister(IGrid)
    cc.shutdown()
    print('\nServer shut down.')


if __name__ == '__main__':
    main()
