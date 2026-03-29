"""SOTA API — server + HTTP relay.

Registers CRMs via IPC and starts an HTTP relay so remote clients can
access them over HTTP.  Press Ctrl-C to shut down.

Run:
    uv run python examples/relay_server.py

Then in another terminal:
    uv run python examples/relay_client.py
"""
import os, sys, signal, threading

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../examples/')))

import c_two as cc
from c_two.transport.relay import Relay
from icrm import IGrid
from crm import Grid

IPC_ADDRESS = 'ipc://grid_relay'
HTTP_BIND = '127.0.0.1:8080'


def main():
    # ── Register CRM ──────────────────────────────────────────────
    cc.set_address(IPC_ADDRESS)

    epsg = 2326
    first_size = [64.0, 64.0]
    bounds = [808357.5, 824117.5, 838949.5, 843957.5]
    subdivide_rules = [
        [478, 310], [2, 2], [2, 2], [2, 2], [2, 2], [2, 2], [1, 1]
    ]
    grid = Grid(epsg, bounds, first_size, subdivide_rules)
    cc.register(IGrid, grid, name='grid')

    # ── Start HTTP relay ──────────────────────────────────────────
    relay = Relay(bind=HTTP_BIND, upstream=IPC_ADDRESS)
    relay.start(blocking=False)

    print(f'IPC  : {cc.server_address()}')
    print(f'HTTP : {relay.url}')
    print(f'  POST /grid/hello          — say hello')
    print(f'  POST /grid/get_grid_infos — query grid attributes')
    print(f'  GET  /health              — relay health check')
    print('Waiting for clients… (Ctrl-C to stop)\n')

    # ── Block until interrupted ───────────────────────────────────
    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    # ── Cleanup ───────────────────────────────────────────────────
    relay.stop()
    cc.unregister('grid')
    cc.shutdown()
    print('\nServer + relay shut down.')


if __name__ == '__main__':
    main()
