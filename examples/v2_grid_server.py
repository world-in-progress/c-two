"""Grid CRM server — standalone IPC process.

Registers the Grid CRM over IPC v3.  A separate relay process
(``v2_relay_standalone.py``) can then bridge HTTP traffic to this server.

Usage (3-terminal workflow):

    # Terminal 1 — start the Grid CRM
    uv run python examples/v2_grid_server.py

    # Terminal 2 — start the HTTP relay (connects to Grid's IPC)
    uv run python examples/v2_relay_standalone.py

    # Terminal 3 — send HTTP requests
    uv run python examples/v2_http_client.py
"""
import os, sys, signal, threading

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from icrm import IGrid
from crm import Grid

BIND_ADDRESS = 'ipc-v3://v2_grid_standalone'


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
    print('[Grid Server] Waiting for relay / clients… (Ctrl-C to stop)\n')

    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    cc.unregister('grid')
    cc.shutdown()
    print('\n[Grid Server] Shut down.')


if __name__ == '__main__':
    main()
