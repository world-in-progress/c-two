"""Stress tests for dynamic fork-route registration during active streams and holds."""

from __future__ import annotations

from collections.abc import Iterator
import json
import os
import struct
import threading
import time
import uuid

import pytest

import c_two as cc
from c_two import error
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

from tests.fixtures.stream_python import BLOB_SPEC, blob_bytes


_BLOB_COMPILED = CompiledSpec.compile(json.dumps(BLOB_SPEC, separators=(',', ':')).encode())


def _pack_frame(tag: int, step: int, payload_bytes: int) -> Payload:
    header = struct.pack('<II', tag, step)
    body = header + (bytes([(tag + step) & 0xFF]) * max(0, payload_bytes - len(header)))
    builder = Builder.create(_BLOB_COMPILED)
    try:
        builder.entry_begin(0, 1).value_bytes(body)
        plan = builder.freeze()
    finally:
        builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def _unpack_frame(payload: Payload) -> tuple[int, int, int]:
    raw = blob_bytes(payload)
    tag, step = struct.unpack_from('<II', raw, 0)
    return tag, step, len(raw)


def _wait_until(predicate, *, timeout: float = 5.0, interval: float = 0.01) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return bool(predicate())


@cc.crm(namespace='test.dynamic-fork-routes', version='0.1.0')
class ForkableSim:
    @cc.read
    @cc.transfer(output=BLOB_SPEC)
    def snapshot(self) -> Payload: ...

    @cc.write
    def apply_delta(self, delta: int) -> int: ...

    @cc.stream
    @cc.read
    @cc.transfer(output=BLOB_SPEC)
    def subscribe(self) -> Iterator[Payload]: ...

    @cc.on_shutdown
    def stop(self) -> None: ...


class ForkableSimResource:
    def __init__(
        self,
        tag: int,
        *,
        payload_bytes: int = 32 * 1024,
        step_interval: float = 0.01,
        gate_second_stream_step: bool = False,
    ) -> None:
        self._tag = tag
        self._payload_bytes = payload_bytes
        self._step_interval = step_interval
        self._cond = threading.Condition()
        self._step = 0
        self._delta_sum = 0
        self._stopped = False
        self.shutdown_called = threading.Event()
        self._gate_second_step = gate_second_stream_step
        self.gated_step_entered = threading.Event()
        self.release_gated_step = threading.Event()
        self._thread = threading.Thread(target=self._advance_loop, daemon=True)
        self._thread.start()

    def _advance_loop(self) -> None:
        while True:
            with self._cond:
                if self._stopped:
                    return
                self._step += 1
                self._cond.notify_all()
            time.sleep(self._step_interval)

    def snapshot(self) -> Payload:
        with self._cond:
            step = self._step
        return _pack_frame(self._tag, step, self._payload_bytes)

    def apply_delta(self, delta: int) -> int:
        with self._cond:
            self._delta_sum += delta
            return self._delta_sum

    def subscribe(self) -> Iterator[Payload]:
        last_step = -1
        yielded = 0
        while True:
            if self._gate_second_step and yielded == 1:
                self.gated_step_entered.set()
                if not self.release_gated_step.wait(timeout=10.0):
                    raise AssertionError('gated branch stream step was not released')
            with self._cond:
                while not self._stopped and self._step <= last_step:
                    self._cond.wait(timeout=0.2)
                if self._stopped:
                    return
                step = self._step
            last_step = step
            yielded += 1
            yield _pack_frame(self._tag, step, self._payload_bytes)

    def stop(self) -> None:
        self.release_gated_step.set()
        with self._cond:
            self._stopped = True
            self._cond.notify_all()
        self._thread.join(timeout=5.0)
        self.shutdown_called.set()


def _run_dynamic_fork_workload(
    *,
    server: cc.Runtime,
    client: cc.Runtime,
    primary_route: str,
    primary_address: str,
    branch_address: str,
    branch_cycles: int,
    expected_primary_path: str,
    expected_branch_path: str,
    verify_unregistered_relay_404: bool = False,
) -> None:
    primary_resource = ForkableSimResource(tag=1, payload_bytes=32 * 1024, step_interval=0.01)
    primary_proxy = None
    stop_readers = threading.Event()
    stream_steps: list[int] = []
    stream_errors: list[BaseException] = []
    snapshot_count = 0
    snapshot_errors: list[BaseException] = []
    first_item_seen = threading.Event()
    first_snapshot_seen = threading.Event()
    stream_thread: threading.Thread | None = None
    snapshot_thread: threading.Thread | None = None

    try:
        server.register(
            ForkableSim,
            primary_resource,
            name=primary_route,
            concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=4),
        )
        primary_proxy = client.connect(
            ForkableSim,
            name=primary_route,
            address=primary_address,
        )
        assert primary_proxy.client.observed_path == expected_primary_path

        def stream_worker() -> None:
            stream = None
            try:
                stream = cc.hold(primary_proxy.subscribe)()
                for held in stream:
                    try:
                        tag, step, size = _unpack_frame(held.value)
                        assert tag == 1
                        assert size == 32 * 1024
                        if stream_steps:
                            assert step > stream_steps[-1]
                        stream_steps.append(step)
                        first_item_seen.set()
                    finally:
                        held.release()
                    if stop_readers.is_set():
                        break
            except BaseException as exc:
                stream_errors.append(exc)
            finally:
                if stream is not None:
                    try:
                        receipt = stream.close(timeout=10.0)
                        if not receipt['completed']:
                            stream_errors.append(
                                RuntimeError(f'primary stream close incomplete: {receipt!r}')
                            )
                    except BaseException as exc:
                        stream_errors.append(exc)

        def snapshot_worker() -> None:
            nonlocal snapshot_count
            try:
                while not stop_readers.is_set():
                    with cc.hold(primary_proxy.snapshot)() as held:
                        tag, _, size = _unpack_frame(held.value)
                        assert tag == 1
                        assert size == 32 * 1024
                    snapshot_count += 1
                    first_snapshot_seen.set()
                    time.sleep(0.005)
            except BaseException as exc:
                snapshot_errors.append(exc)

        stream_thread = threading.Thread(target=stream_worker, daemon=True)
        snapshot_thread = threading.Thread(target=snapshot_worker, daemon=True)
        stream_thread.start()
        snapshot_thread.start()
        assert first_item_seen.wait(10.0), 'primary stream did not yield its first frame'
        assert first_snapshot_seen.wait(10.0), 'primary snapshot worker did not complete first read'

        steps_before_forks = len(stream_steps)
        snapshots_before_forks = snapshot_count

        # 1. Prove primary stream and held snapshots stay live while a branch unregister drains.
        gated_name = f'{primary_route}-gated-fork'
        gated_resource = ForkableSimResource(
            tag=99,
            payload_bytes=16 * 1024,
            step_interval=0.005,
            gate_second_stream_step=True,
        )
        gated_proxy = None
        gated_stream = None
        unregister_errors: list[BaseException] = []
        unregister_thread: threading.Thread | None = None
        next_item_thread: threading.Thread | None = None
        try:
            server.register(
                ForkableSim,
                gated_resource,
                name=gated_name,
                concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=2),
            )
            gated_proxy = client.connect(ForkableSim, name=gated_name, address=branch_address)
            assert gated_proxy.client.observed_path == expected_branch_path
            gated_stream = cc.hold(gated_proxy.subscribe)()
            first_branch = next(gated_stream)
            try:
                tag, _, size = _unpack_frame(first_branch.value)
                assert tag == 99
                assert size == 16 * 1024
            finally:
                first_branch.release()

            def pull_second_branch_item() -> None:
                try:
                    item = next(gated_stream)
                    item.release()
                except BaseException:
                    # Expected once unregister cancels the branch stream.
                    pass

            next_item_thread = threading.Thread(target=pull_second_branch_item, daemon=True)
            next_item_thread.start()
            assert gated_resource.gated_step_entered.wait(10.0), 'branch did not enter gated step'

            def run_unregister() -> None:
                try:
                    server.unregister(gated_name)
                except BaseException as exc:
                    unregister_errors.append(exc)

            unregister_thread = threading.Thread(target=run_unregister, daemon=True)
            unregister_thread.start()

            # While unregister() is waiting for gated_resource's running step to finish,
            # primary must deliver both a fresh stream frame and a fresh held snapshot.
            steps_during_drain = len(stream_steps)
            snaps_during_drain = snapshot_count
            assert _wait_until(
                lambda: len(stream_steps) > steps_during_drain and snapshot_count > snaps_during_drain,
                timeout=5.0,
            ), 'primary route stalled while branch unregister was draining'
        finally:
            gated_resource.release_gated_step.set()
            if unregister_thread is not None:
                unregister_thread.join(timeout=10.0)
                assert not unregister_thread.is_alive()
            if next_item_thread is not None:
                next_item_thread.join(timeout=10.0)
            if gated_stream is not None:
                receipt = gated_stream.close(timeout=10.0)
                assert receipt['completed'] is True
            if gated_proxy is not None:
                cc.close(gated_proxy)
            if gated_name in server.names:
                server.unregister(gated_name)
            gated_resource.stop()
        assert not unregister_errors, f'gated unregister errors: {unregister_errors!r}'
        assert gated_resource.shutdown_called.is_set()
        if verify_unregistered_relay_404:
            with pytest.raises(error.ResourceNotFound) as exc_info:
                client.connect(
                    ForkableSim,
                    name=gated_name,
                    address=branch_address,
                    timeout=3.0,
                )
            assert int(exc_info.value.code) == 701

        # 2. Repeated dynamic fork / invoke / stream / unregister churn.
        for index in range(branch_cycles):
            branch_name = f'{primary_route}-fork-{index}'
            branch_tag = 100 + index
            branch_resource = ForkableSimResource(
                tag=branch_tag,
                payload_bytes=16 * 1024,
                step_interval=0.005,
            )
            branch_proxy = None
            try:
                registered_name = server.register(
                    ForkableSim,
                    branch_resource,
                    name=branch_name,
                    concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=2),
                )
                assert registered_name == branch_name

                branch_proxy = client.connect(
                    ForkableSim,
                    name=branch_name,
                    address=branch_address,
                )
                assert branch_proxy.client.observed_path == expected_branch_path

                assert branch_proxy.apply_delta(index + 1) == index + 1
                with cc.hold(branch_proxy.snapshot)() as held:
                    tag, _, size = _unpack_frame(held.value)
                    assert tag == branch_tag
                    assert size == 16 * 1024

                branch_stream = cc.hold(branch_proxy.subscribe)()
                collected = 0
                try:
                    for held in branch_stream:
                        try:
                            tag, _, size = _unpack_frame(held.value)
                            assert tag == branch_tag
                            assert size == 16 * 1024
                            collected += 1
                        finally:
                            held.release()
                        if collected >= 2:
                            break
                finally:
                    receipt = branch_stream.close(timeout=10.0)
                    assert receipt['completed'] is True
                assert collected == 2
            finally:
                if branch_proxy is not None:
                    cc.close(branch_proxy)
                if branch_name in server.names:
                    server.unregister(branch_name)
                    assert branch_resource.shutdown_called.is_set()
                else:
                    branch_resource.stop()

            if verify_unregistered_relay_404 and index == 0:
                with pytest.raises(error.ResourceNotFound) as exc_info:
                    client.connect(
                        ForkableSim,
                        name=branch_name,
                        address=branch_address,
                        timeout=3.0,
                    )
                assert int(exc_info.value.code) == 701

            if index == branch_cycles // 2:
                assert primary_proxy.apply_delta(5) == 5

        assert len(stream_steps) > steps_before_forks, (
            'primary stream failed to advance during dynamic fork-route churn'
        )
        assert snapshot_count > snapshots_before_forks, (
            'primary snapshots failed to advance during dynamic fork-route churn'
        )

        stop_readers.set()
        stream_thread.join(timeout=10.0)
        snapshot_thread.join(timeout=10.0)
        assert not stream_thread.is_alive()
        assert not snapshot_thread.is_alive()
        assert not stream_errors, f'primary stream errors: {stream_errors!r}'
        assert not snapshot_errors, f'primary snapshot errors: {snapshot_errors!r}'

        assert server.names == [primary_route]
        assert _wait_until(
            lambda: (
                client.call_execution_snapshot()['client']['used_operations'] == 0
                and server.call_execution_snapshot()['server']['used_operations'] == 0
            ),
            timeout=5.0,
        ), (
            f'operations did not drain: '
            f'client={client.call_execution_snapshot()!r}, '
            f'server={server.call_execution_snapshot()!r}'
        )
        client_mem = client.memory_stats()
        holds = client_mem['holds']
        assert holds['active_holds'] == 0
        assert holds['total_held_bytes'] == 0
        if client_mem['client']['memory'] is not None:
            assert client_mem['client']['memory']['used_bytes'] == 0
    finally:
        stop_readers.set()
        primary_resource.stop()
        if stream_thread is not None and stream_thread.is_alive():
            stream_thread.join(timeout=5.0)
        if snapshot_thread is not None and snapshot_thread.is_alive():
            snapshot_thread.join(timeout=5.0)
        if primary_proxy is not None:
            cc.close(primary_proxy)


def _assert_retired_memory_drained(runtime: cc.Runtime) -> None:
    for retired in runtime.memory_stats()['retired']:
        if retired['memory'] is not None:
            assert retired['memory']['used_bytes'] == 0


def test_dynamic_fork_routes_direct_ipc(monkeypatch, tmp_path):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    server = cc.Runtime()
    client = cc.Runtime()
    if os.name != 'nt':
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    try:
        server.start()
        address = server.server_address()
        assert address is not None
        _run_dynamic_fork_workload(
            server=server,
            client=client,
            primary_route=f'ipc-primary-{uuid.uuid4().hex[:8]}',
            primary_address=address,
            branch_address=address,
            branch_cycles=12,
            expected_primary_path='direct_ipc',
            expected_branch_path='direct_ipc',
        )
    finally:
        assert client.shutdown(timeout=10.0)['completed'] is True
        assert server.shutdown(timeout=10.0)['completed'] is True
        _assert_retired_memory_drained(client)
        _assert_retired_memory_drained(server)


def test_dynamic_fork_routes_via_relay(start_c3_relay, monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    relay = start_c3_relay()
    server = cc.Runtime()
    client = cc.Runtime()
    server.set_relay_anchor(relay.url)
    client.set_relay_anchor(relay.url)
    primary_route = f'relay-primary-{uuid.uuid4().hex[:8]}'
    try:
        _run_dynamic_fork_workload(
            server=server,
            client=client,
            primary_route=primary_route,
            primary_address=relay.url,
            branch_address=relay.url,
            branch_cycles=8,
            expected_primary_path='explicit_relay',
            expected_branch_path='explicit_relay',
            verify_unregistered_relay_404=True,
        )
        # Also verify mid-run relay-aware name discovery for a dynamically registered branch.
        discovered_name = f'{primary_route}-discovered'
        discovered_resource = ForkableSimResource(tag=200, payload_bytes=8 * 1024, step_interval=0.01)
        discovered_proxy = None
        try:
            server.register(
                ForkableSim,
                discovered_resource,
                name=discovered_name,
                concurrency=cc.ConcurrencyConfig(mode='read_parallel', max_workers=2),
            )
            discovered_proxy = client.connect(ForkableSim, name=discovered_name)
            assert discovered_proxy.client.observed_path in (
                'relay_aware_local_ipc',
                'relay_aware_relay',
            )
            assert discovered_proxy.apply_delta(7) == 7
        finally:
            if discovered_proxy is not None:
                cc.close(discovered_proxy)
            if discovered_name in server.names:
                server.unregister(discovered_name)
                assert discovered_resource.shutdown_called.is_set()
            else:
                discovered_resource.stop()
    finally:
        assert client.shutdown(timeout=10.0)['completed'] is True
        assert server.shutdown(timeout=10.0)['completed'] is True
        _assert_retired_memory_drained(client)
        _assert_retired_memory_drained(server)


def test_late_low_level_server_shutdown_preserves_restarted_route(monkeypatch, tmp_path):
    """A late low-level Server.shutdown() observer must not pop or hook a restarted route."""
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    from c_two._native import RuntimeSession
    from c_two.transport.server.native import NativeServerBridge

    @cc.crm(namespace='test.low-level-restart', version='0.1.0')
    class EchoContract:
        @cc.read
        def label(self) -> str: ...

        @cc.on_shutdown
        def stop(self) -> None: ...

    class EchoResource:
        def __init__(self, value: str) -> None:
            self._value = value
            self.stopped = 0

        def label(self) -> str:
            return self._value

        def stop(self) -> None:
            self.stopped += 1

    session = RuntimeSession(use_process_relay_anchor=False)
    if os.name != 'nt':
        session.set_local_endpoint(root=str(tmp_path))
    bridge = NativeServerBridge(
        local_copy_tracker=session.local_copy_tracker(),
        runtime_session=session,
    )
    client = cc.Runtime()
    if os.name != 'nt':
        client.set_local_endpoint(root=str(tmp_path))
    first = EchoResource('host-a')
    second = EchoResource('host-b')
    proxy = None
    try:
        bridge.register_crm(EchoContract, first, name='shared-route')
        outcome_a = bridge.shutdown(timeout=5.0)
        assert outcome_a['completed'] is True
        assert first.stopped == 1

        # Restart a fresh Host B on the same session and reuse the exact route name.
        bridge.register_crm(EchoContract, second, name='shared-route')
        # A stale low-level shutdown observation from Host A's slot snapshot must not
        # remove Host B's slot or invoke Host B's shutdown hook.
        late_slots = bridge._take_shutdown_slots(outcome_a, expected_slots={})
        assert late_slots == []
        assert second.stopped == 0
        assert bridge.names == ['shared-route']

        address = session.server_address
        assert address is not None
        proxy = client.connect(EchoContract, name='shared-route', address=address)
        assert proxy.label() == 'host-b'
    finally:
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5.0)['completed'] is True
        outcome_b = bridge.shutdown(timeout=5.0)
        assert outcome_b['completed'] is True
        assert second.stopped == 1
