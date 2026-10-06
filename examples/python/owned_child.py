"""Launch a dedicated Python resource with a private owner control receiver.

The child explicitly calls cc.adopt_owner_stdin(), configures owner_bound with
cc.set_server(), registers resources and calls cc.serve(). Its ordinary stdout
and stderr remain inherited. This controller releases ownership on Enter and
waits for the process through the opaque native handle.
"""
from __future__ import annotations

import sys

import c_two as cc


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit('usage: owned_child.py RESOURCE_SCRIPT')
    keepalive, receiver = cc.owner_control_pair()
    child = cc.spawn_owned_child(receiver, sys.executable, [sys.argv[1]])
    try:
        print(f'owned resource started: {child.id}')
        input('Press Enter to release the owner. ')
    finally:
        keepalive.close()
        try:
            code = child.wait(timeout=30)
        except TimeoutError:
            child.kill()
            code = child.wait(timeout=10)
        finally:
            child.close()
    raise SystemExit(code)


if __name__ == '__main__':
    main()
