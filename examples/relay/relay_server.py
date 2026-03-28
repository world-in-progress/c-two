"""HTTP relay — standalone gateway process.

Bridges HTTP requests to CRM resource processes running on IPC v3.
The relay starts empty; CRM processes auto-register via
``C2_RELAY_ADDRESS`` when they call ``cc.register()``.

Usage (3-terminal workflow):

    # Terminal 1 — start the relay (empty, waits for registrations)
    uv run python examples/v2_relay/relay_server.py

    # Terminal 2 — start the Grid CRM (auto-registers with relay)
    C2_RELAY_ADDRESS=http://127.0.0.1:8080 uv run python examples/v2_relay/resource.py

    # Terminal 3 — send HTTP requests
    uv run python examples/v2_relay/http_client.py
"""
import os, sys, signal, threading

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

from c_two.transport.relay import RelayV2

HTTP_BIND = '127.0.0.1:8080'


def main():
    relay = RelayV2(bind=HTTP_BIND)
    relay.start(blocking=False)

    print(f'[Relay] HTTP listen  : {relay.url}')
    print(f'[Relay] Control endpoints:')
    print(f'  POST /_register    — register a CRM upstream')
    print(f'  POST /_unregister  — remove a CRM upstream')
    print(f'  GET  /_routes      — list registered routes')
    print(f'  GET  /health       — relay health check')
    print(f'[Relay] Data endpoints (after registration):')
    print(f'  POST /{{name}}/{{method}} — call a CRM method')
    print('[Relay] Waiting for CRM registrations… (Ctrl-C to stop)\n')

    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    relay.stop()
    print('\n[Relay] Shut down.')


if __name__ == '__main__':
    main()
