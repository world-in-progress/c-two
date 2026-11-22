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

import c_two.observe
from c_two.transport.client.util import ping
from c_two.transport.registry import _get_default_runtime, _reset_default_runtime
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload, PayloadError


@pytest.fixture(autouse=True)
def collect_unreachable_owners():
    _reset_default_runtime()
    yield
    # Failed constructor tracebacks can form cycles holding an old session.
    # Collect unreachable owners from earlier tests; live producers remain
    # observable and no native counters or observations are reset here.
    gc.collect()


CHUNK_SIZE = 128 * 1024
MIB = 1024 * 1024
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
    client = stats['client']
    assert client['frozen']
    return client


def _retired_outgoing_cells(stats: dict) -> dict:
    assert not stats['client']['frozen']
    roles = [role for role in stats['retired'] if role['role'] == 'client']
    assert len(roles) == 1, stats['retired']
    assert roles[0]['closed']
    return roles[0]


def _configure_file_reassembly() -> None:
    cc.set_server(memory_budget_bytes=64 * 1024)
    cc.set_client(memory_budget_bytes=128 * 1024, file_spill_budget_bytes=2 * MIB)


@pytest.mark.timeout(60)
@pytest.mark.parametrize('tier', ['file', 'buddy'])
def test_local_hold_outlives_reassembly_backing_and_public_shutdown(tier: str) -> None:
    """Reassembly is released before a local hold; checked views follow the hold."""
    cc.shutdown()
    route = f'memory-budget-{tier}-{uuid.uuid4().hex[:12]}'
    payload: Payload | None = None
    proxy = None
    held = None
    checked = None
    sequence = None
    try:
        cc.set_server(memory_budget_bytes=64 * 1024)
        cc.set_client(
            memory_budget_bytes=128 * 1024 if tier == 'file' else 16 * MIB,
            file_spill_budget_bytes=2 * MIB if tier == 'file' else 0,
        )
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)

        proxy = cc.connect(LargeOutput, name=route, address=address)
        assert proxy.client._mode == 'ipc'  # noqa: SLF001
        baseline = _outgoing_cells(cc.observe.memory_stats())
        baseline_memory = baseline['memory']['used_bytes']
        baseline_file = baseline['file_spill']['used_bytes']

        held = cc.hold(proxy.produce)()
        retained = held.value
        assert isinstance(retained, Payload)
        sequence = retained.entry_view(0)
        checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        # The response was reassembled, then copied and released before SDK decode.
        logical_len = len(held.unsafe_buffer)
        assert logical_len > CHUNK_SIZE
        assert logical_len % CHUNK_SIZE != 0
        capacity = math.ceil(logical_len / CHUNK_SIZE) * CHUNK_SIZE
        active_stats = cc.observe.memory_stats()
        active = _outgoing_cells(active_stats)
        assert active['file_spill']['used_bytes'] == baseline_file
        if tier == 'file':
            assert active['file_spill']['peak_bytes'] >= capacity
            assert active['memory']['used_bytes'] == baseline_memory
        else:
            assert active['memory']['peak_bytes'] >= capacity
            assert active['file_spill']['peak_bytes'] == 0
        assert set(active_stats['holds']) == {
            'active_holds', 'total_held_bytes', 'oldest_hold_seconds', 'by_direction',
        }
        assert active_stats['holds']['active_holds'] == 1
        assert active_stats['holds']['total_held_bytes'] == logical_len
        original = bytes(held.unsafe_buffer)
        # Reuse the transport storage while the first local copy stays alive.
        next_value = proxy.produce()
        next_value.close()
        assert bytes(held.unsafe_buffer) == original

        cc.shutdown()
        retired = _retired_outgoing_cells(cc.observe.memory_stats())
        assert retired['memory']['used_bytes'] == baseline_memory
        assert retired['file_spill']['used_bytes'] == baseline_file
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        cc.close(proxy)
        proxy = None
        deadline = time.monotonic() + 5.0
        while True:
            stats = cc.observe.memory_stats()
            if not stats['retired'] or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
        assert stats['retired'] == [], stats
        assert stats['holds']['active_holds'] == 1
        assert stats['holds']['total_held_bytes'] == logical_len
        assert bytes(held.unsafe_buffer) == original

        exported = memoryview(held.unsafe_buffer)
        try:
            with pytest.raises(cc.error.SerializationError) as failure:
                held.release()
            assert failure.value.code == cc.error.ErrorCode.ClientOutputFromBuffer
            assert isinstance(failure.value.__cause__, BufferError)
            assert held.value is retained
            with checked.acquire() as access:
                assert access.bytes() == BLOB
            assert cc.observe.memory_stats()['holds']['total_held_bytes'] == logical_len
        finally:
            exported.release()
        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        deadline = time.monotonic() + 5.0
        while True:
            after_release = cc.observe.memory_stats()
            retired_after = [
                scope for scope in after_release['retired']
                if scope['role'] == 'client'
            ]
            assert len(retired_after) <= 1
            # A fully drained observer may be pruned. If it remains, every
            # cell must return to its pre-call usage while proxy stays alive.
            cells = retired_after[0] if retired_after else None
            drained = (
                after_release['holds']['active_holds'] == 0
                and (
                    cells is None
                    or (
                        cells['memory']['used_bytes'] == baseline_memory
                        and cells['file_spill']['used_bytes'] == baseline_file
                    )
                )
            )
            if drained or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
        assert drained, after_release
        assert cc.observe.memory_stats()["holds"]['active_holds'] == 0
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
        _configure_file_reassembly()
        payload = _build_payload()
        resource = ObservedBlockedResource(payload, entered, proceed)
        cc.register(ObservedLargeOutput, resource, name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(ObservedLargeOutput, name=route, address=address)
        old_session = _get_default_runtime()._runtime_session  # noqa: SLF001
        initial = _outgoing_cells(cc.observe.memory_stats())
        assert all(initial[kind]['used_bytes'] == 0 for kind in ('memory', 'file_spill'))
        assert cc.observe.memory_stats()["holds"]['active_holds'] == 0

        def call_held() -> None:
            try:
                call_outcome['held'] = cc.hold(proxy.produce)()
            except BaseException as exc:
                call_outcome['error'] = exc

        call_thread = threading.Thread(target=call_held, daemon=True)
        call_thread.start()
        assert entered.wait(timeout=5), 'real resource callback never started'
        with pytest.raises(cc.error.CoreError, match='retirement must complete'):
            observer.inherit_retirement(old_session)
        assert not observer.memory_stats()['retired']
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
        assert cc.observe.memory_stats()['holds']['active_holds'] == 1

        # A second real callback blocks drain while the first held owner lives.
        entered.clear()
        proceed.clear()
        call_thread = threading.Thread(target=call_held, daemon=True)
        call_thread.start()
        assert entered.wait(timeout=5)
        bridge = _get_default_runtime()._server  # noqa: SLF001
        pending = cc.shutdown(timeout=0.05)
        assert pending['completed'] is False, pending
        assert _get_default_runtime()._runtime_session is old_session  # noqa: SLF001
        assert _get_default_runtime()._server is bridge  # noqa: SLF001
        assert route in _get_default_runtime().names
        assert hooks == []
        assert not proceed.is_set()
        # Shutdown's short observation budget does not end the dispatched
        # call or its response wait. The resource is still blocked here.
        assert call_thread.is_alive()
        assert call_outcome == {}
        proceed.set()
        call_thread.join(timeout=10)
        assert not call_thread.is_alive()
        assert 'error' not in call_outcome, call_outcome
        extra_held = call_outcome['held']
        with extra_held.value.entry_view(0) as sequence:
            extra_checked = sequence.at(0)
        with extra_checked.acquire() as access:
            assert access.bytes() == BLOB
        extra_checked.close()
        extra_held.release()
        call_outcome.pop('held')
        completed = cc.shutdown(timeout=10)
        assert completed['completed'] is True, completed
        observer.inherit_retirement(old_session)
        observer.inherit_retirement(old_session)
        assert hooks == ['stopped']
        assert _get_default_runtime()._runtime_session is not old_session  # noqa: SLF001
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        after_shutdown = observer.memory_stats()
        retired = _retired_outgoing_cells(after_shutdown)
        assert retired['file_spill']['used_bytes'] == 0
        assert retired['memory']['used_bytes'] == 0
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
            assert all(cells[kind]['used_bytes'] == 0 for kind in ('memory', 'file_spill'))
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
        # A failed assertion retains this frame in pytest's report. Release
        # real producers too, so one failure cannot pin another test's domain.
        call_outcome.clear()
        call_thread = proxy = old_session = bridge = held = checked = extra_held = None


@pytest.mark.timeout(60)
def test_delivered_response_tracks_hold_after_confirmed_shutdown(monkeypatch) -> None:
    """A delivered response may be wrapped after the transport closes.

    Block the real FastDB output decoder, which runs after the native call has
    delivered its response but before ``response.track_retained``. Repeated
    public snapshots after confirmed shutdown must not discard the old local-copy
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
        _configure_file_reassembly()
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(LargeOutput, name=route, address=address)
        baseline = _outgoing_cells(cc.observe.memory_stats())
        original_open_external = Payload.open_external

        def blocked_open_external(cls, spec, source, options=None):  # noqa: ARG001
            decode_entered.set()
            if not decode_proceed.wait(timeout=20):
                raise RuntimeError('timed out waiting to decode delivered response')
            return original_open_external(spec, source, options)

        monkeypatch.setattr(Payload, 'open_external', classmethod(blocked_open_external))

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
        delivered = _outgoing_cells(cc.observe.memory_stats())
        assert delivered['file_spill']['used_bytes'] == baseline['file_spill']['used_bytes']
        assert delivered['memory']['used_bytes'] == baseline['memory']['used_bytes']
        assert cc.observe.memory_stats()["holds"]['active_holds'] == 0

        shutdown_thread = threading.Thread(target=shut_down, daemon=True)
        shutdown_thread.start()
        assert shutdown_done.wait(timeout=10), 'public shutdown did not finish'
        shutdown_thread.join(timeout=1)
        assert not shutdown_thread.is_alive()
        assert not shutdown_outcome, shutdown_outcome
        assert not decode_proceed.is_set()

        for _ in range(3):
            stats = cc.observe.memory_stats()
            retired = _retired_outgoing_cells(stats)
            assert retired['file_spill']['used_bytes'] == baseline['file_spill']['used_bytes']
            assert retired['memory']['used_bytes'] == baseline['memory']['used_bytes']
            assert stats['holds']['active_holds'] == 0
            assert cc.observe.memory_stats()["holds"]['active_holds'] == 0

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
        assert cc.observe.memory_stats()["holds"]['active_holds'] >= 1
        assert cc.observe.memory_stats()['holds']['active_holds'] >= 1

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        assert cc.observe.memory_stats()["holds"]['active_holds'] == 0
        assert cc.observe.memory_stats()['holds']['active_holds'] == 0
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
def test_failed_public_shutdown_retry_keeps_its_own_local_hold(monkeypatch) -> None:
    """A failed replacement cannot consume the current session's observer.

    This uses a local FastDB held response received through file reassembly from the current native
    session. Only the next native RuntimeSession construction is refused; the
    same installed session then retries public shutdown successfully.
    """
    cc.shutdown()
    route = f'memory-retry-{uuid.uuid4().hex[:12]}'
    payload: Payload | None = None
    proxy = None
    held = None
    checked = None
    try:
        _configure_file_reassembly()
        payload = _build_payload()
        cc.register(LargeOutput, LargeOutputResource(payload), name=route)
        address = cc.server_address()
        assert address is not None
        _wait_ready(address)
        proxy = cc.connect(LargeOutput, name=route, address=address)
        baseline = _outgoing_cells(cc.observe.memory_stats())

        held = cc.hold(proxy.produce)()
        retained = held.value
        with retained.entry_view(0) as sequence:
            checked = sequence.at(0)
        with checked.acquire() as access:
            assert access.bytes() == BLOB
        live = _outgoing_cells(cc.observe.memory_stats())
        assert live['memory']['used_bytes'] == baseline['memory']['used_bytes']
        assert live['file_spill']['used_bytes'] == baseline['file_spill']['used_bytes']
        assert cc.observe.memory_stats()["holds"]['active_holds'] >= 1

        old_session = _get_default_runtime()._runtime_session  # noqa: SLF001

        def refuse_replacement(_self, *_args, **_kwargs):
            raise RuntimeError('injected RuntimeSession construction failure')

        # The native class remains real; only this one replacement attempt is
        # refused after native retirement reaches its terminal state.
        with monkeypatch.context() as constructor_patch:
            constructor_patch.setattr(type(old_session), '__init__', refuse_replacement)
            with pytest.raises(RuntimeError, match='injected RuntimeSession'):
                cc.shutdown()

        assert _get_default_runtime()._runtime_session is old_session  # noqa: SLF001
        failed = _outgoing_cells(cc.observe.memory_stats())
        assert failed['memory']['used_bytes'] == baseline['memory']['used_bytes']
        assert failed['file_spill']['used_bytes'] == baseline['file_spill']['used_bytes']
        assert cc.observe.memory_stats()["holds"]['active_holds'] >= 1
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        cc.shutdown()
        retried = _retired_outgoing_cells(cc.observe.memory_stats())
        assert retried['memory']['used_bytes'] == baseline['memory']['used_bytes']
        assert retried['file_spill']['used_bytes'] == baseline['file_spill']['used_bytes']
        assert cc.observe.memory_stats()["holds"]['active_holds'] >= 1
        assert cc.observe.memory_stats()['holds']['active_holds'] >= 1
        # Both the proxy and its held checked owner remain referenced here.
        with checked.acquire() as access:
            assert access.bytes() == BLOB

        held.release()
        with pytest.raises(PayloadError) as invalidated:
            checked.kind()
        assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        assert cc.observe.memory_stats()["holds"]['active_holds'] == 0
        assert cc.observe.memory_stats()['holds']['active_holds'] == 0
        # old_session and proxy deliberately remain alive. Their zero-valued
        # metadata must stay observable; all physical charges must be gone.
        released = _retired_outgoing_cells(cc.observe.memory_stats())
        for kind in ('memory', 'file_spill'):
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
