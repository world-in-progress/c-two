"""Real child-process fixture for the OwnerBound Python facade.

The role is selected by the first positional argument, so the controller's
environment is never inherited as the child's role:

``service``
    A resource process. It explicitly adopts the owner control receiver that a
    trusted launcher wired to its stdin, then builds an ``owner_bound`` server
    with :func:`c_two.set_server`, registers one CRM, and calls
    :func:`c_two.serve`.

The Rust SDK owned_child example is the controller. It transfers its receiver
as this resource process's stdin and retains the keepalive privately.

The capability itself never crosses argv, the environment, or stdout: the
child receives it as stdin, and the controller keeps the keepalive private.
"""
from __future__ import annotations

import os
import sys

import c_two as cc

READY_MARKER = 'OWNER_BOUND_READY'
FINISHED_MARKER = 'OWNER_BOUND_FINISHED'
HOLD_MARKER = 'OWNER_BOUND_HOLDING'
RELEASED_MARKER = 'CONTROLLER_RELEASED'


@cc.crm(namespace='cc.test.owner_bound_fixture', version='0.1.0')
class OwnerBoundEcho:
    def ping(self) -> str:
        ...

    def echo(self, text: str) -> str:
        ...

    @cc.on_shutdown
    def cleanup(self) -> None:
        ...


class OwnerBoundEchoResource:
    def __init__(self) -> None:
        self._shutdown_calls = 0

    def ping(self) -> str:
        return 'pong'

    def echo(self, text: str) -> str:
        return text

    def cleanup(self) -> None:
        self._shutdown_calls += 1
        marker = os.environ.get('C2_OWNER_SHUTDOWN_MARKER')
        if marker:
            with open(marker, 'w', encoding='utf-8') as handle:
                handle.write(f'{self._shutdown_calls}')


def _service() -> int:
    marker = os.environ.get('C2_OWNER_SHUTDOWN_MARKER')
    if marker and os.path.exists(marker):
        os.unlink(marker)

    receiver = cc.adopt_owner_stdin()
    grace = float(os.environ.get('C2_OWNER_GRACE_SECONDS', '0.5'))
    cc.set_server(
        server_id=os.environ.get('C2_OWNER_SERVER_ID', 'owner-bound-fixture'),
        lifecycle=cc.LifecycleConfig.owner_bound(grace),
        owner_control=receiver,
    )
    cc.register(OwnerBoundEcho, OwnerBoundEchoResource(), name='owned-echo')

    print(f'{READY_MARKER} {cc.server_address()}', flush=True)
    # cc.serve blocks until the native host reaches a real terminal outcome.
    from contextlib import redirect_stdout
    with redirect_stdout(sys.stderr):
        cc.serve()
    calls = '0'
    if marker and os.path.exists(marker):
        calls = open(marker, encoding='utf-8').read().strip()
    print(f'{FINISHED_MARKER} shutdown_calls={calls}', flush=True)
    return 0


def main() -> int:
    action = sys.argv[1] if len(sys.argv) > 1 else os.environ.get('C2_OWNER_FIXTURE_ACTION', '')
    if action == 'service':
        return _service()
    raise SystemExit(f'unknown fixture role {action!r}')


if __name__ == '__main__':
    raise SystemExit(main())
