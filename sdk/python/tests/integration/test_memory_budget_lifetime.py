"""Public held-response lifetime across direct IPC shutdown and reassembly tiers."""
from __future__ import annotations

import gc
import json
import math
import threading
import time
import uuid
from contextlib import suppress

import pytest

import c_two as cc
from c_two.error import ClientCallResource
from c_two.transport.client.util import ping
from c_two.transport.registry import _ProcessRegistry
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload, PayloadError


@pytest.fixture(autouse=True)
def collect_unreachable_owners():
    # Failed constructor tracebacks can form cycles holding an old session.
    # Collect unreachable owners from earlier tests; live producers remain
    # observable and no native counters or observations are reset here.
    gc.collect()


CHUNK_SIZE = 16 * 1024
REASSEMBLY_SEGMENT_SIZE = 1024 * 1024
BLOB = bytes(range(251)) * 800 + b'\x00\xffX'
BLOB_SPEC = {
    'schema': 'fastdb.payload.v1',
    'profile': 'record.v1',
    'entries': [
        {
            'id': 'blob',
            'cardinality': 'one',
            'type': {'kind': 'bytes', 'nullable': False},
        },
    ],
    'components': [],
}


@cc.crm(namespace='test.memory-budget-lifetime', version='0.1.0')
class LargeOutput:
    @cc.transfer(output=BLOB_SPEC)
    def produce(self) -> Payload:
        ...


class LargeOutputResource:
    def __init__(self, payload: Payload) -> None:
        self.payload = payload

    def produce(self) -> Payload:
        return self.payload


class BlockedLargeOutputResource(LargeOutputResource):
    def __init__(
        self, payload: Payload, entered: threading.Event, proceed: threading.Event,
    ) -> None:
        super().__init__(payload)
        self.entered = entered
        self.proceed = proceed

    def produce(self) -> Payload:
        self.entered.set()
        if not self.proceed.wait(timeout=20):
            raise RuntimeError('timed out waiting to finish the resource callback')
        return self.payload


def _build_payload() -> Payload:
    """Use FastDB's Builder/BuildPlan path used by portable runtime fixtures."""
    spec = CompiledSpec.compile(json.dumps(BLOB_SPEC, separators=(',', ':')).encode())
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_bytes(BLOB)
        plan = builder.freeze()
    finally:
        builder.close()
        spec.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def _wait_ready(address: str) -> None:
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if ping(address, timeout=0.2):
            return
        time.sleep(0.02)
    pytest.fail(f'direct IPC server did not answer ping: {address}')


def _outgoing_cells(stats: dict) -> dict:
    outgoing = stats['runtime_outgoing']
    assert outgoing is not None
    assert outgoing['role'] == 'runtime_outgoing'
    assert outgoing['state'] == 'active'
    return outgoing['cells']


def _retired_outgoing_cells(stats: dict) -> dict:
    assert stats['runtime_outgoing'] is None
    outgoing = [
        scope for scope in stats['retired']
        if scope['role'] == 'runtime_outgoing'
    ]
    assert len(outgoing) == 1, stats['retired']
    assert outgoing[0]['state'] == 'retired'
    return outgoing[0]['cells']


@pytest.mark.timeout(60)
@pytest.mark.parametrize('tier', ['file', 'buddy'])
def test_held_portable_reassembly_budget_survives_public_shutdown(tier: str) -> None:
    """A held checked owner keeps full reassembly capacity until release."""
    cc.shutdown()
    route = f'memory-budget-{tier}-{uuid.uuid4().hex[:12]}'
    server_overrides = {
        'pool_segment_size': REASSEMBLY_SEGMENT_SIZE,
        'reassembly_segment_size': REASSEMBLY_SEGMENT_SIZE,
        'max_pool_segments': 1,
        'reassembly_max_segments': 1,
        'pool_prewarm_segments': 0,
        'pool_min_retained_segments': 0,
        'chunk_size': CHUNK_SIZE,
        'max_total_chunks': 64,
        'max_reassembly_bytes': REASSEMBLY_SEGMENT_SIZE,
        'shm_backing_budget_bytes': 0,
        'file_backing_budget_bytes': 2 * REASSEMBLY_SEGMENT_SIZE,
        'live_reassembly_budget_bytes': REASSEMBLY_SEGMENT_SIZE,
    }
    client_overrides = {
        **server_overrides,
        'shm_backing_budget_bytes': (
            0 if tier == 'file' else 4 * REASSEMBLY_SEGMENT_SIZE
        ),
        'file_backing_budget_bytes': (
            2 * REASSEMBLY_SEGMENT_SIZE if tier == 'file' else 0
        ),
        'pool_decay_seconds': 0.0,
    }
    payload: Payload | None = None
    proxy = None
    held = None
    checked = None
    sequence = None
    try:
        cc.set_server(ipc_overrides=server_overrides)
        cc.set_client(ipc_overrides=client_overrides)
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)

        proxy = cc.connect(LargeOutput, name=route, address=address)
        assert proxy.client._mode == 'ipc'  # noqa: SLF001
        baseline = _outgoing_cells(cc.memory_stats())
        baseline_reassembly = baseline['reassembly']['used_bytes']
        baseline_file = baseline['file']['used_bytes']
        baseline_shm = baseline['shm']['used_bytes']

        held = cc.hold(proxy.produce)()
        retained = held.value
        assert isinstance(retained, Payload)
        sequence = retained.entry_view(0)
        checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        # The response is chunked because the server cannot allocate SHM.
        # The first reply chunk sets the reassembly capacity; a short final
        # chunk trims only logical length, never the reservation.
        logical_len = len(held.unsafe_buffer)
        assert logical_len > CHUNK_SIZE
        assert logical_len % CHUNK_SIZE != 0
        capacity = math.ceil(logical_len / CHUNK_SIZE) * CHUNK_SIZE
        active = _outgoing_cells(cc.memory_stats())
        assert active['reassembly']['used_bytes'] - baseline_reassembly == capacity
        if tier == 'file':
            assert active['file']['used_bytes'] - baseline_file == capacity
            assert active['shm']['used_bytes'] == baseline_shm
        else:
            # A 1 MiB reassembly segment distinguishes buddy backing from a
            # dedicated mapping sized only for this roughly 200 KiB payload.
            assert active['shm']['used_bytes'] - baseline_shm >= REASSEMBLY_SEGMENT_SIZE
            assert active['file']['used_bytes'] == baseline_file

        cc.shutdown()
        retired = _retired_outgoing_cells(cc.memory_stats())
        assert retired['reassembly']['used_bytes'] - baseline_reassembly == capacity
        if tier == 'file':
            assert retired['file']['used_bytes'] - baseline_file == capacity
            assert retired['shm']['used_bytes'] == baseline_shm
        else:
            assert retired['shm']['used_bytes'] - baseline_shm >= REASSEMBLY_SEGMENT_SIZE
            assert retired['file']['used_bytes'] == baseline_file
        assert cc.hold_stats()['active_holds'] >= 1
        assert cc.memory_stats()['holds']['active_holds'] >= 1
        # Keep the proxy/client alive while the held FastDB checked view is
        # read after shutdown; its owner and transport backing must still live.
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        deadline = time.monotonic() + 5.0
        while True:
            after_release = cc.memory_stats()
            retired_after = [
                scope for scope in after_release['retired']
                if scope['role'] == 'runtime_outgoing'
            ]
            assert len(retired_after) <= 1
            # A fully drained observer may be pruned. If it remains, every
            # cell must return to its pre-call usage while proxy stays alive.
            cells = retired_after[0]['cells'] if retired_after else None
            drained = (
                after_release['holds']['active_holds'] == 0
                and (
                    cells is None
                    or (
                        cells['reassembly']['used_bytes'] == baseline_reassembly
                        and cells['file']['used_bytes'] == baseline_file
                        and cells['shm']['used_bytes'] == baseline_shm
                    )
                )
            )
            if drained or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
        assert drained, after_release
        assert cc.hold_stats()['active_holds'] == 0
        # All assertions above occur before cc.close(proxy).
        assert proxy is not None
    finally:
        if held is not None:
            with suppress(Exception):
                held.release()
        if checked is not None:
            with suppress(Exception):
                checked.close()
        if sequence is not None:
            with suppress(Exception):
                sequence.close()
        if proxy is not None:
            with suppress(Exception):
                cc.close(proxy)
        cc.shutdown()
        if payload is not None:
            payload.close()


@pytest.mark.timeout(60)
def test_early_zero_retirement_survives_an_inflight_response() -> None:
    """A weak zero capture follows late allocation without premature shutdown.

    An independent native observer adopts the real session's handoff before
    the blocked callback produces a response. Public shutdown is tested later:
    its pending drain retains the session and Python hooks until completion.
    """
    from c_two import _native

    cc.shutdown()
    route = f'memory-retire-race-{uuid.uuid4().hex[:12]}'
    entered = threading.Event()
    proceed = threading.Event()
    call_outcome: dict[str, object] = {}
    call_thread = None
    proxy = None
    payload = None
    held = None
    checked = None
    old_session = None
    observer = _native.RuntimeSession(use_process_relay_anchor=False)
    hooks: list[str] = []

    @cc.crm(namespace='test.memory-budget-retire', version='0.1.0')
    class ObservedLargeOutput:
        @cc.transfer(output=BLOB_SPEC)
        def produce(self) -> Payload:
            ...

        @cc.on_shutdown
        def stopped(self) -> None:
            ...

    class ObservedBlockedResource(BlockedLargeOutputResource):
        def stopped(self) -> None:
            hooks.append('stopped')

    try:
        overrides = {
            'pool_prewarm_segments': 0,
            'shm_backing_budget_bytes': 0,
            'file_backing_budget_bytes': 2 * REASSEMBLY_SEGMENT_SIZE,
            'live_reassembly_budget_bytes': REASSEMBLY_SEGMENT_SIZE,
            'chunk_size': CHUNK_SIZE,
        }
        cc.set_server(ipc_overrides=overrides)
        cc.set_client(ipc_overrides=overrides)
        payload = _build_payload()
        resource = ObservedBlockedResource(payload, entered, proceed)
        cc.register(ObservedLargeOutput, resource, name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(ObservedLargeOutput, name=route, address=address)
        old_session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001
        initial = _outgoing_cells(cc.memory_stats())
        assert all(initial[kind]['used_bytes'] == 0 for kind in ('shm', 'file', 'reassembly'))
        assert cc.hold_stats()['active_holds'] == 0

        def call_held() -> None:
            try:
                call_outcome['held'] = cc.hold(proxy.produce)()
            except BaseException as exc:
                call_outcome['error'] = exc

        call_thread = threading.Thread(target=call_held, daemon=True)
        call_thread.start()
        assert entered.wait(timeout=5), 'real resource callback never started'
        observation = old_session.retire_memory_observation()
        observer.adopt_retired_memory_observation(observation)
        # Re-adoption must not double-count the same domains or lease tracker.
        observer.adopt_retired_memory_observation(observation)
        del observation
        for _ in range(3):
            early = observer.memory_stats()
            retired = _retired_outgoing_cells(early)
            assert all(retired[kind]['used_bytes'] == 0 for kind in ('shm', 'file', 'reassembly'))
            assert early['holds']['active_holds'] == 0
        assert not proceed.is_set()

        proceed.set()
        call_thread.join(timeout=10)
        assert not call_thread.is_alive(), 'held response did not finish'
        assert 'error' not in call_outcome, call_outcome
        held = call_outcome.pop('held')
        with held.value.entry_view(0) as sequence:
            checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        capacity = math.ceil(len(held.unsafe_buffer) / CHUNK_SIZE) * CHUNK_SIZE
        late = observer.memory_stats()
        retired = _retired_outgoing_cells(late)
        current = _outgoing_cells(cc.memory_stats())
        for kind in ('shm', 'file', 'reassembly'):
            assert retired[kind] == current[kind]
        assert retired['shm']['used_bytes'] == 0
        assert retired['file']['used_bytes'] == capacity
        assert retired['reassembly']['used_bytes'] == capacity
        assert late['holds']['active_holds'] == cc.hold_stats()['active_holds'] == 1

        # A second real callback blocks drain while the first held owner lives.
        entered.clear()
        proceed.clear()
        call_thread = threading.Thread(target=call_held, daemon=True)
        call_thread.start()
        assert entered.wait(timeout=5)
        bridge = _ProcessRegistry.get()._server  # noqa: SLF001
        pending = cc.shutdown(timeout=0.05)
        assert pending['completed'] is False, pending
        assert _ProcessRegistry.get()._runtime_session is old_session  # noqa: SLF001
        assert _ProcessRegistry.get()._server is bridge  # noqa: SLF001
        assert route in _ProcessRegistry.get().names
        assert hooks == []
        assert not proceed.is_set()
        proceed.set()
        call_thread.join(timeout=10)
        assert not call_thread.is_alive()
        assert isinstance(call_outcome.get('error'), ClientCallResource), call_outcome
        assert 'held' not in call_outcome
        completed = cc.shutdown(timeout=10)
        assert completed['completed'] is True, completed
        assert hooks == ['stopped']
        assert _ProcessRegistry.get()._runtime_session is not old_session  # noqa: SLF001
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        after_shutdown = observer.memory_stats()
        retired = _retired_outgoing_cells(after_shutdown)
        assert retired['file']['used_bytes'] == capacity
        assert retired['reassembly']['used_bytes'] == capacity
        assert after_shutdown['holds']['active_holds'] == 1

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        checked.close()
        checked = None
        held = None
        for _ in range(3):
            released = observer.memory_stats()
            cells = _retired_outgoing_cells(released)
            assert all(cells[kind]['used_bytes'] == 0 for kind in ('shm', 'file', 'reassembly'))
            assert released['holds']['active_holds'] == 0
        # Zero stays observable while old_session/proxy are real producers;
        # the weak bundle disappears only after their final release.
        cc.close(proxy)
        proxy = None
        old_session = None
        bridge = None
        call_thread = None
        call_outcome.clear()
        gc.collect()
        assert observer.memory_stats()['retired'] == []
    finally:
        proceed.set()
        if call_thread is not None:
            call_thread.join(timeout=10)
        extra_held = call_outcome.get('held')
        if extra_held is not None:
            extra_held.release()
        if held is not None:
            held.release()
        if checked is not None:
            checked.close()
        if proxy is not None:
            cc.close(proxy)
        cc.shutdown(timeout=10)
        observer.shutdown(timeout_seconds=5)
        if payload is not None:
            payload.close()


@pytest.mark.timeout(60)
def test_delivered_response_tracks_hold_after_confirmed_shutdown(monkeypatch) -> None:
    """A delivered response may be wrapped after the transport closes.

    Block the real FastDB output decoder, which runs after the native call has
    delivered its response but before ``response.track_retained``. Repeated
    public snapshots after confirmed shutdown must not discard the old lease
    tracker while this SDK call can still publish a held result.
    """
    cc.shutdown()
    route = f'memory-late-hold-{uuid.uuid4().hex[:12]}'
    decode_entered = threading.Event()
    decode_proceed = threading.Event()
    shutdown_done = threading.Event()
    call_outcome: dict[str, object] = {}
    shutdown_outcome: dict[str, BaseException] = {}
    call_thread = None
    shutdown_thread = None
    proxy = None
    payload: Payload | None = None
    checked = None
    try:
        cc.set_server(ipc_overrides={
            'pool_prewarm_segments': 0,
            'shm_backing_budget_bytes': 0,
            'file_backing_budget_bytes': 2 * REASSEMBLY_SEGMENT_SIZE,
            'live_reassembly_budget_bytes': REASSEMBLY_SEGMENT_SIZE,
            'chunk_size': CHUNK_SIZE,
        })
        cc.set_client(ipc_overrides={
            'pool_prewarm_segments': 0,
            'shm_backing_budget_bytes': 0,
            'file_backing_budget_bytes': 2 * REASSEMBLY_SEGMENT_SIZE,
            'live_reassembly_budget_bytes': REASSEMBLY_SEGMENT_SIZE,
            'chunk_size': CHUNK_SIZE,
        })
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(LargeOutput, name=route, address=address)
        baseline = _outgoing_cells(cc.memory_stats())
        original_open_copy = Payload.open_copy

        def blocked_open_copy(cls, spec, source, options=None):  # noqa: ARG001
            decode_entered.set()
            if not decode_proceed.wait(timeout=20):
                raise RuntimeError('timed out waiting to decode delivered response')
            return original_open_copy(spec, source, options)

        monkeypatch.setattr(Payload, 'open_copy', classmethod(blocked_open_copy))

        def call_held() -> None:
            try:
                call_outcome['held'] = cc.hold(proxy.produce)()
            except BaseException as exc:
                call_outcome['error'] = exc

        def shut_down() -> None:
            try:
                cc.shutdown()
            except BaseException as exc:
                shutdown_outcome['error'] = exc
            finally:
                shutdown_done.set()

        call_thread = threading.Thread(target=call_held, daemon=True)
        call_thread.start()
        assert decode_entered.wait(timeout=10), 'real response never reached FastDB decoder'
        delivered = _outgoing_cells(cc.memory_stats())
        assert delivered['file']['used_bytes'] > baseline['file']['used_bytes']
        assert delivered['reassembly']['used_bytes'] > baseline['reassembly']['used_bytes']
        assert cc.hold_stats()['active_holds'] == 0

        shutdown_thread = threading.Thread(target=shut_down, daemon=True)
        shutdown_thread.start()
        assert shutdown_done.wait(timeout=10), 'public shutdown did not finish'
        shutdown_thread.join(timeout=1)
        assert not shutdown_thread.is_alive()
        assert not shutdown_outcome, shutdown_outcome
        assert not decode_proceed.is_set()

        for _ in range(3):
            stats = cc.memory_stats()
            retired = _retired_outgoing_cells(stats)
            assert retired['file']['used_bytes'] > baseline['file']['used_bytes']
            assert retired['reassembly']['used_bytes'] > baseline['reassembly']['used_bytes']
            assert stats['holds']['active_holds'] == 0
            assert cc.hold_stats()['active_holds'] == 0

        decode_proceed.set()
        call_thread.join(timeout=10)
        assert not call_thread.is_alive(), 'held response did not finish decoding'
        assert 'error' not in call_outcome, call_outcome
        held = call_outcome['held']
        retained = held.value
        with retained.entry_view(0) as sequence:
            checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        assert cc.hold_stats()['active_holds'] >= 1
        assert cc.memory_stats()['holds']['active_holds'] >= 1

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        assert cc.hold_stats()['active_holds'] == 0
        assert cc.memory_stats()['holds']['active_holds'] == 0
    finally:
        decode_proceed.set()
        if call_thread is not None:
            call_thread.join(timeout=10)
        if shutdown_thread is not None:
            shutdown_thread.join(timeout=10)
        held = call_outcome.get('held')
        if held is not None:
            with suppress(Exception):
                held.release()
        if checked is not None:
            with suppress(Exception):
                checked.close()
        if proxy is not None:
            with suppress(Exception):
                cc.close(proxy)
        if shutdown_thread is None or not shutdown_thread.is_alive():
            cc.shutdown()
        if payload is not None:
            payload.close()


@pytest.mark.timeout(60)
def test_failed_public_shutdown_retry_keeps_its_own_held_budget(monkeypatch) -> None:
    """A failed replacement cannot consume the current session's observer.

    This uses a real file-backed FastDB held response from the current native
    session. Only the next native RuntimeSession construction is refused; the
    same installed session then retries public shutdown successfully.
    """
    cc.shutdown()
    route = f'memory-retry-{uuid.uuid4().hex[:12]}'
    overrides = {
        'pool_prewarm_segments': 0,
        'shm_backing_budget_bytes': 0,
        'file_backing_budget_bytes': 2 * REASSEMBLY_SEGMENT_SIZE,
        'live_reassembly_budget_bytes': REASSEMBLY_SEGMENT_SIZE,
        'chunk_size': CHUNK_SIZE,
    }
    payload: Payload | None = None
    proxy = None
    held = None
    checked = None
    try:
        cc.set_server(ipc_overrides=overrides)
        cc.set_client(ipc_overrides=overrides)
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(LargeOutput, name=route, address=address)
        baseline = _outgoing_cells(cc.memory_stats())

        held = cc.hold(proxy.produce)()
        retained = held.value
        with retained.entry_view(0) as sequence:
            checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        live = _outgoing_cells(cc.memory_stats())
        logical_len = len(held.unsafe_buffer)
        capacity = math.ceil(logical_len / CHUNK_SIZE) * CHUNK_SIZE
        assert live['reassembly']['used_bytes'] - baseline['reassembly']['used_bytes'] == capacity
        assert live['file']['used_bytes'] - baseline['file']['used_bytes'] == capacity
        assert cc.hold_stats()['active_holds'] >= 1

        old_session = _ProcessRegistry.get()._runtime_session  # noqa: SLF001

        def refuse_replacement(_self, *_args, **_kwargs):
            raise RuntimeError('injected RuntimeSession construction failure')

        # The native class remains real; only this one replacement attempt is
        # refused after retire_memory_observation() captures the old session.
        with monkeypatch.context() as constructor_patch:
            constructor_patch.setattr(type(old_session), '__init__', refuse_replacement)
            with pytest.raises(RuntimeError, match='injected RuntimeSession'):
                cc.shutdown()

        assert _ProcessRegistry.get()._runtime_session is old_session  # noqa: SLF001
        failed = _outgoing_cells(cc.memory_stats())
        assert failed['reassembly']['used_bytes'] - baseline['reassembly']['used_bytes'] == capacity
        assert failed['file']['used_bytes'] - baseline['file']['used_bytes'] == capacity
        assert cc.hold_stats()['active_holds'] >= 1
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        cc.shutdown()
        retried = _retired_outgoing_cells(cc.memory_stats())
        assert retried['reassembly']['used_bytes'] - baseline['reassembly']['used_bytes'] == capacity
        assert retried['file']['used_bytes'] - baseline['file']['used_bytes'] == capacity
        assert cc.hold_stats()['active_holds'] >= 1
        assert cc.memory_stats()['holds']['active_holds'] >= 1
        # Both the proxy and its held checked owner remain referenced here.
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        assert cc.hold_stats()['active_holds'] == 0
        assert cc.memory_stats()['holds']['active_holds'] == 0
        # old_session and proxy deliberately remain alive. Their zero-valued
        # metadata must stay observable; all physical charges must be gone.
        released = _retired_outgoing_cells(cc.memory_stats())
        for kind in ('shm', 'file', 'reassembly'):
            assert released[kind]['used_bytes'] == baseline[kind]['used_bytes']
    finally:
        if held is not None:
            with suppress(Exception):
                held.release()
        if checked is not None:
            with suppress(Exception):
                checked.close()
        if proxy is not None:
            with suppress(Exception):
                cc.close(proxy)
        cc.shutdown()
        if payload is not None:
            payload.close()
