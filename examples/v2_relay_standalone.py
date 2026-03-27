"""HTTP relay — standalone process.

Bridges HTTP requests to a Grid CRM server running on IPC v3.
This process does NOT host any CRM — it only relays HTTP traffic.

Usage (3-terminal workflow):

    # Terminal 1 — start the Grid CRM
    uv run python examples/v2_grid_server.py

    # Terminal 2 — start this relay
    uv run python examples/v2_relay_standalone.py

    # Terminal 3 — send HTTP requests
    uv run python examples/v2_http_client.py
"""
import os, sys, signal, threading

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src/')))

from c_two.rpc_v2.relay import RelayV2

# Must match the address used by v2_grid_server.py
IPC_UPSTREAM = 'ipc-v3://v2_grid_standalone'
HTTP_BIND = '127.0.0.1:8080'


def main():
    relay = RelayV2(bind=HTTP_BIND, upstream=IPC_UPSTREAM)
    relay.start(blocking=False)

    print(f'[Relay] Upstream IPC : {IPC_UPSTREAM}')
    print(f'[Relay] HTTP listen  : {relay.url}')
    print(f'[Relay] Endpoints:')
    print(f'  POST /grid/hello              — say hello')
    print(f'  POST /grid/get_grid_infos     — query grid attributes')
    print(f'  POST /grid/subdivide_grids    — subdivide grids')
    print(f'  GET  /health                  — relay health check')
    print('[Relay] Ready. (Ctrl-C to stop)\n')

    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    relay.stop()
    print('\n[Relay] Shut down.')


if __name__ == '__main__':
    main()
