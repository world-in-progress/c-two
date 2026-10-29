"""Start an empty public Runtime in a caller-owned endpoint directory.

Run from a source checkout: uv run python examples/python/independent_runtime.py
The Unix directory is removed only after the native close outcome is clear.
If cleanup is incomplete or reports an error, the directory is retained.
"""

import os
from pathlib import Path
import shutil
import tempfile

import c_two as cc


def shutdown_is_clear(outcome: dict) -> bool:
    return (
        outcome.get('completed') is True
        and not any(outcome.get(key) for key in (
            'runtime_barrier_error',
            'route_close_error',
            'ipc_client_close_error',
            'relay_errors',
        ))
        and all(
            route.get('active_drained') is True and not route.get('close_error')
            for route in outcome.get('route_outcomes', ())
        )
    )


def main() -> None:
    runtime = cc.Runtime()
    # mkdtemp creates a new private final directory. Never delete a shared root.
    root = Path(tempfile.mkdtemp(prefix='c-two-preflight-')) if os.name != 'nt' else None
    start_error = None
    try:
        if root is not None:
            runtime.set_local_endpoint(root=str(root))
        runtime.set_server(startup_timeout=30.0)
        runtime.start()  # Existing native readiness and IPC PING/PONG; no CRM.
        print(f'Ready at {runtime.server_address()}')
        print(f'Credential: {runtime.server_credential()}')
    except BaseException as exc:
        start_error = exc

    try:
        outcome = runtime.shutdown(timeout=30.0)
    except BaseException as shutdown_error:
        # The start failure remains available as the shutdown error's cause.
        if start_error is not None:
            raise shutdown_error from start_error
        raise

    if not shutdown_is_clear(outcome):
        cleanup_error = RuntimeError(
            f'Native cleanup is unconfirmed; retained endpoint directory '
            f'{root}: {outcome!r}'
        )
        if start_error is not None:
            raise cleanup_error from start_error
        raise cleanup_error

    if root is not None:
        try:
            shutil.rmtree(root)
        except OSError as remove_error:
            if start_error is not None:
                raise remove_error from start_error
            raise
    if start_error is not None:
        raise start_error


if __name__ == '__main__':
    main()
