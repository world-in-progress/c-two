"""Relay server — route table for resource discovery.

Start this first, then launch CRM processes that auto-register.

Run:
    uv run python examples/relay_mesh/relay.py
"""
import os, sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import logging

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s [relay] %(levelname)s %(message)s',
    datefmt='%H:%M:%S',
)

BIND = '0.0.0.0:8300'


def main():
    from c_two.relay import NativeRelay

    relay = NativeRelay(BIND)
    relay.start()

    print(f'Relay listening on http://{BIND}')
    print(f'  /_resolve/{{name}}  — name resolution')
    print(f'  /_register        — CRM registration')
    print(f'  /health           — health check')
    print('Press Ctrl-C to stop.\n')

    try:
        import signal
        signal.sigwait({signal.SIGINT, signal.SIGTERM})
    except KeyboardInterrupt:
        pass
    finally:
        relay.stop()
        print('\nRelay stopped.')


if __name__ == '__main__':
    main()
