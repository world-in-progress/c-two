"""Grid CRM server — standalone IPC process with relay auto-registration.

Registers the Grid CRM over IPC.  When ``C2_RELAY_ADDRESS`` is set,
``cc.register()`` automatically notifies the relay server, making the
Grid CRM accessible via HTTP.

Usage (3-terminal workflow):

    # Terminal 1 — start the relay
    uv run python examples/v2_relay/relay_server.py

    # Terminal 2 — start the Grid CRM (auto-registers with relay)
    C2_RELAY_ADDRESS=http://127.0.0.1:8080 uv run python examples/v2_relay/resource.py

    # Terminal 3 — send HTTP requests
    uv run python examples/v2_relay/http_client.py
"""
import os, sys, signal, threading

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../examples/')))

import logging
import c_two as cc
from grid.grid import Grid
from grid.igrid import IGrid

BIND_ADDRESS = 'ipc://v2_grid_standalone'


def main():
    cc.set_address(BIND_ADDRESS)

    # Init the Grid CRM
    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    grid = Grid(epsg, bounds, first_size, subdivide_rules)

    cc.register(IGrid, grid, name='grid')
    print(f'[Grid Server] CRM registered at {cc.server_address()}')
    print('[Grid Server] Waiting for clients… (Ctrl-C to stop)\n')
    
    cc.serve()


if __name__ == '__main__':
    main()
