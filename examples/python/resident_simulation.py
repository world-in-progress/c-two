"""Resident simulation with background stepping, read-parallel access, streaming, and dynamic branching.

Run from a source checkout:
    C2_RELAY_ANCHOR_ADDRESS= uv run python examples/python/resident_simulation.py

The script uses two independent ``cc.Runtime`` instances over direct IPC so it
runs self-contained on Unix and Windows without an external relay. On Unix it
creates a private endpoint directory and removes it only after clear shutdown.
"""

from __future__ import annotations

from collections.abc import Iterator
import os
from pathlib import Path
import shutil
import tempfile
import threading
import time
from typing import Dict, List, Optional, Tuple

import c_two as cc


@cc.crm(namespace='science.resident_sim', version='0.1.0')
class ResidentSimulationContract:
    @cc.read
    def snapshot(self) -> Dict[str, object]: ...

    @cc.write
    def apply_intervention(self, delta: float, label: str) -> int: ...

    @cc.stream
    @cc.read
    def subscribe(self, max_frames: int) -> Iterator[Dict[str, object]]: ...

    @cc.on_shutdown
    def stop(self) -> None: ...


class ResidentSimulation:
    """Stateful simulation advancing in a background thread.

    Contract methods stay short and non-blocking:
    - ``snapshot`` reads the latest published frame under ``@cc.read``.
    - ``apply_intervention`` queues a parameter change for the next step.
    - ``subscribe`` yields published frames through ``@cc.stream``.
    - ``stop`` runs via ``@cc.on_shutdown`` to halt stepping cleanly.
    """

    def __init__(
        self,
        name: str,
        *,
        initial_step: int = 0,
        initial_level: float = 1.0,
        initial_pending: Optional[List[Tuple[float, str]]] = None,
        initial_labels: Optional[List[str]] = None,
        step_interval: float = 0.03,
    ) -> None:
        self._name = name
        self._step_interval = step_interval
        self._cond = threading.Condition()
        self._step = initial_step
        self._level = initial_level
        self._pending: List[Tuple[float, str]] = list(initial_pending or ())
        self._applied_labels: List[str] = list(initial_labels or ())
        self._stopped = False
        self._thread = threading.Thread(
            target=self._run_loop,
            name=f'sim-{name}',
            daemon=True,
        )
        self._thread.start()

    def _run_loop(self) -> None:
        while True:
            with self._cond:
                if self._stopped:
                    return
                if self._pending:
                    for delta, label in self._pending:
                        self._level = round(self._level + delta, 4)
                        self._applied_labels.append(label)
                    self._pending.clear()
                self._step += 1
                self._level = round(self._level + 0.1, 4)
                self._cond.notify_all()
            time.sleep(self._step_interval)

    def _current_frame_locked(self) -> Dict[str, object]:
        return {
            'name': self._name,
            'step': self._step,
            'level': self._level,
            'interventions': list(self._applied_labels),
        }

    def snapshot(self) -> Dict[str, object]:
        with self._cond:
            return self._current_frame_locked()

    def apply_intervention(self, delta: float, label: str) -> int:
        with self._cond:
            self._pending.append((delta, label))
            return self._step

    def subscribe(self, max_frames: int) -> Iterator[Dict[str, object]]:
        last_step = -1
        delivered = 0
        while delivered < max_frames:
            with self._cond:
                while not self._stopped and self._step <= last_step:
                    self._cond.wait(timeout=0.5)
                if self._stopped:
                    return
                frame = self._current_frame_locked()
                last_step = int(frame['step'])
            delivered += 1
            yield frame

    def fork_state(self, branch_name: str, *, delta: float, label: str) -> ResidentSimulation:
        """Clone committed state and queued interventions before starting the branch thread."""
        with self._cond:
            pending = list(self._pending)
            pending.append((delta, label))
            return ResidentSimulation(
                branch_name,
                initial_step=self._step,
                initial_level=self._level,
                initial_pending=pending,
                initial_labels=list(self._applied_labels),
                step_interval=self._step_interval,
            )

    def stop(self) -> None:
        with self._cond:
            self._stopped = True
            self._cond.notify_all()
        self._thread.join(timeout=5.0)


def _shutdown_is_clear(outcome: Dict[str, object]) -> bool:
    return (
        outcome.get('completed') is True
        and not any(
            outcome.get(key)
            for key in (
                'runtime_barrier_error',
                'route_close_error',
                'ipc_client_close_error',
                'relay_errors',
            )
        )
    )


def main() -> None:
    server = cc.Runtime()
    client = cc.Runtime()
    root: Optional[Path] = (
        Path(tempfile.mkdtemp(prefix='c-two-resident-sim-'))
        if os.name != 'nt'
        else None
    )
    primary_sim = ResidentSimulation('primary', step_interval=0.025)
    branch_sim: Optional[ResidentSimulation] = None
    primary_proxy = None
    stream_frames: List[Dict[str, object]] = []
    stream_errors: List[BaseException] = []
    run_error: Optional[BaseException] = None

    try:
        if root is not None:
            server.set_local_endpoint(root=str(root))
            client.set_local_endpoint(root=str(root))

        server.register(
            ResidentSimulationContract,
            primary_sim,
            name='sim-primary',
            concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=4),
        )
        address = server.server_address()
        assert address is not None
        print(f'Primary resident simulation ready at {address}')

        primary_proxy = client.connect(
            ResidentSimulationContract,
            name='sim-primary',
            address=address,
        )

        # 1. Start a live subscriber on the primary simulation.
        def consume_primary_stream() -> None:
            stream = primary_proxy.subscribe(6)
            try:
                for frame in stream:
                    stream_frames.append(frame)
            except BaseException as exc:
                stream_errors.append(exc)
            finally:
                receipt = stream.close(timeout=5.0)
                if not receipt['completed']:
                    stream_errors.append(RuntimeError(f'unclosed stream: {receipt!r}'))

        subscriber = threading.Thread(target=consume_primary_stream, daemon=True)
        subscriber.start()

        # 2. Read snapshots and apply an intervention while stepping continues.
        before = primary_proxy.snapshot()
        queued_at = primary_proxy.apply_intervention(-0.5, 'gate-open')
        print(f'Queued intervention at step {queued_at}; snapshot before={before}')

        # 3. Dynamically fork, register a temporary branch route, inspect, and unregister.
        branch_sim = primary_sim.fork_state('branch-a', delta=-1.2, label='alt-control')
        server.register(
            ResidentSimulationContract,
            branch_sim,
            name='sim-branch-a',
            concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=2),
        )
        with client.connect(
            ResidentSimulationContract,
            name='sim-branch-a',
            address=address,
        ) as branch_proxy:
            branch_snap = branch_proxy.snapshot()
            branch_stream = branch_proxy.subscribe(2)
            try:
                branch_frames = list(branch_stream)
            finally:
                branch_stream.close(timeout=5.0)
            print(
                f'Branch snapshot={branch_snap}; '
                f'collected {len(branch_frames)} branch frames'
            )

        server.unregister('sim-branch-a')
        branch_sim = None
        print(f'Unregistered sim-branch-a; active routes={server.names}')

        # 4. Wait for the primary subscriber to finish cleanly.
        subscriber.join(timeout=10.0)
        if subscriber.is_alive():
            raise RuntimeError('Primary subscriber did not complete in time')
        if stream_errors:
            raise stream_errors[0]

        after = primary_proxy.snapshot()
        print(
            f'Primary stream delivered {len(stream_frames)} frames; '
            f'final snapshot={after}'
        )
        holds = client.memory_stats()['holds']['active_holds']
        ops = client.call_execution_snapshot()['client']['used_operations']
        print(f'Client holds={holds}, used_operations={ops}')
    except BaseException as exc:
        run_error = exc
    finally:
        if primary_proxy is not None:
            cc.close(primary_proxy)
        if branch_sim is not None:
            branch_sim.stop()
        client_outcome = client.shutdown(timeout=10.0)
        server_outcome = server.shutdown(timeout=10.0)
        primary_sim.stop()
        cleanup_ok = _shutdown_is_clear(client_outcome) and _shutdown_is_clear(server_outcome)
        if not cleanup_ok:
            cleanup_error = RuntimeError(
                f'Native cleanup is unconfirmed; retained endpoint directory {root}: '
                f'client={client_outcome!r}, server={server_outcome!r}'
            )
            if run_error is not None:
                raise cleanup_error from run_error
            raise cleanup_error
        if root is not None:
            shutil.rmtree(root, ignore_errors=False)
        if run_error is not None:
            raise run_error


if __name__ == '__main__':
    main()
