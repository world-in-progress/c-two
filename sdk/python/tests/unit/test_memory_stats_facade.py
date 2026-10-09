"""Read-only transport memory-stats facade (``cc.memory_stats``).

The counters are Rust-owned: the Python surface only projects the native
snapshot and must never keep its own counters. The three budget cells are
C-Two-owned IPC backing/live-reassembly accounting, not process RSS.

The retirement tests here prove the owner-lifetime observation contract: a
retirement record stores only weak views of budget accounting and lease
metadata, so it stays reportable exactly while a real owner — an old proxy's
native client or tracker handle, an in-flight response, an outstanding hold
or charge guard — keeps that metadata alive. Zero counters never prune a
record (a supported producer may still publish), records detach when their
last owner is gone, repeated empty session swaps never accumulate, and the
public surface exposes no way to create charges or synthetic holds.
"""
from __future__ import annotations

import json
import time

import pytest

from c_two.config.settings import settings
from c_two.mem import (
    MemoryCellStats,
    MemoryLimits,
    MemoryScopeStats,
    MemoryStats,
    RetiredScopeStats,
)
from c_two.transport.registry import _ProcessRegistry

import c_two as cc

try:  # fastdb4py is optional for every non-portable test in this file.
    from fastdb4py.payload import Payload
except ImportError:  # pragma: no cover - lean environments without FastDB
    Payload = None  # type: ignore[assignment]


@pytest.fixture(autouse=True)
def _clean_registry():
    """Start and end each facade test from the process-level singleton.

    The public facade reads ``_ProcessRegistry``, so a session left installed
    by another test file must never leak into these assertions.
    """
    _ProcessRegistry.reset()
    settings.shm_threshold = None
    yield
    settings.shm_threshold = None
    _ProcessRegistry.reset()


SPEC = {
    'schema': 'fastdb.payload.v1',
    'profile': 'record.v1',
    'entries': [
        {
            'id': 'value',
            'cardinality': 'one',
            'type': {'kind': 'u8', 'nullable': False},
        },
    ],
    'components': [],
}


def _build_payload(value: int = 7):
    from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec

    spec = CompiledSpec.compile(json.dumps(SPEC, separators=(',', ':')).encode())
    builder = Builder.create(spec)
    value_builder = builder.entry_begin(0, 1)
    value_builder.value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


def _track_retained_lease(session, *, bytes_: int = 64):
    return session.lease_tracker().track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        storage='inline',
        bytes=bytes_,
    )


def test_memory_stats_is_observable_without_freezing_an_unused_runtime() -> None:
    first = cc.memory_stats()
    second = cc.memory_stats()

    assert isinstance(first, dict)
    assert set(first) >= {'runtime_outgoing', 'server', 'retired', 'holds', 'budget_cells_note'}
    # Observing an unused Runtime must not create or freeze a client domain.
    assert first['runtime_outgoing'] is None
    assert second['runtime_outgoing'] is None
    assert first['server'] is None
    # A fresh process has no retired domains; a shared test process may still
    # report owner-backed retired domains from earlier tests, but the shape is
    # always an explicit list with scope labels.
    assert isinstance(first['retired'], list)
    assert all(
        {'role', 'state', 'limits', 'cells'} <= set(scope)
        for scope in first['retired']
    )
    # The retained-buffer counters stay Rust-owned and are composed, not
    # duplicated, into the snapshot.
    assert set(first['holds']) >= {'active_holds', 'total_held_bytes'}
    assert 'not process RSS' in first['budget_cells_note']


def test_memory_stats_typed_shape_is_declared_for_the_public_facade() -> None:
    # The typed facade is annotation-only, so this asserts the declared keys
    # stay aligned with the documented native shape.
    assert set(MemoryLimits.__annotations__) == {
        'shm_backing_bytes',
        'file_backing_bytes',
        'live_reassembly_bytes',
    }
    assert set(MemoryCellStats.__annotations__) == {
        'limit_bytes',
        'used_bytes',
        'peak_bytes',
        'rejected_allocations',
        'rejected_bytes',
    }
    assert set(MemoryScopeStats.__annotations__) == {'role', 'state', 'limits', 'cells'}
    assert set(RetiredScopeStats.__annotations__) == {'role', 'state', 'limits', 'cells'}
    assert set(MemoryStats.__annotations__) == {
        'runtime_outgoing',
        'server',
        'retired',
        'holds',
        'budget_cells_note',
    }


def test_retired_observation_surface_has_no_track_or_reserve_escape() -> None:
    """The public observation surface is read-only and opaque.

    The retired-observation wrapper handed between sessions must not expose
    any way to create charges (``reserve``) or synthetic holds (``track``),
    and it offers no lifecycle surface at all: the observation lifetime is
    decided by real owners inside Rust. The native module never exposes the
    mutable budget or observer Rust types at all.
    """
    from c_two import _native

    session = _native.RuntimeSession()
    observation = session.retire_memory_observation()

    surface = {name for name in dir(observation) if not name.startswith('_')}
    assert surface == set(), f'observation must be opaque, saw {surface!r}'
    for forbidden in (
        'track',
        'track_retained',
        'reserve',
        'push_scope',
        'push_tracker',
        'prune',
        'prune_detached',
        'prune_confirmed_drained',
        'mark_close_confirmed',
        'close_confirmed',
        'scope_reports',
        'lease_stats',
        'sweep_retained_snapshots',
    ):
        assert forbidden not in surface, f'observation must not expose {forbidden!r}'

    for scope in session.memory_stats()['retired']:
        assert isinstance(scope, dict)

    # No mutable Rust accounting handle is projected to Python at all.
    assert not hasattr(_native, 'MemoryBudget')
    assert not hasattr(_native, 'BudgetObserver')


def test_native_retirement_keeps_retained_lease_metadata_across_sessions() -> None:
    """The session handoff keeps Rust-owned lease metadata observable.

    This is the deterministic native-level half of the public proof: the
    replacement session reports a retained lease that belongs to the retired
    session for exactly as long as a real owner holds it, and the record
    detaches once the last owner (the hold guard and the retired session
    itself) is gone.
    """
    from c_two import _native

    first = _native.RuntimeSession()
    lease = _track_retained_lease(first)
    observation = first.retire_memory_observation()

    replacement = _native.RuntimeSession()
    replacement.adopt_retired_memory_observation(observation)

    assert replacement.hold_stats()['active_holds'] == 1
    assert replacement.memory_stats()['holds']['active_holds'] == 1
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 64
    sweeps = replacement.sweep_hold_leases(0.0)
    assert [item['route_name'] for item in sweeps] == ['grid']

    # A hold is a real owner: releasing it does not detach the record while
    # the retired session's own tracker handle still exists.
    lease.release()
    assert replacement.hold_stats()['active_holds'] == 0
    assert replacement.sweep_hold_leases(0.0) == []

    # The last owner goes away (the retired session wrapper drops, exactly
    # like the end of a public shutdown); the record detaches.
    del first
    drained = replacement.memory_stats()
    assert drained['holds']['active_holds'] == 0
    assert drained['retired'] == []


def test_zero_retirement_keeps_a_late_hold_visible_while_a_producer_owns_the_tracker() -> None:
    """Initially zero is not quiescent.

    The retirement is captured while the old session's counters are zero and
    an in-flight response has not published its held result yet. The old
    session itself goes away — exactly like a public shutdown dropping the
    retired session — but the producer handle it handed out (what an old
    proxy's ``lease_tracker`` or an in-flight response holds) keeps the
    tracker alive, so the late hold must remain visible through the
    replacement once it lands, without relying on timing.
    """
    from c_two import _native

    first = _native.RuntimeSession()
    producer_tracker = first.lease_tracker()
    # Counters are zero and no hold exists at capture time.
    observation = first.retire_memory_observation()

    replacement = _native.RuntimeSession()
    replacement.adopt_retired_memory_observation(observation)
    assert replacement.hold_stats()['active_holds'] == 0

    # The retired session wrapper drops, then repeated snapshots must not
    # discard the producer-owned record even though it still reports zero.
    del first
    for _ in range(3):
        stats = replacement.memory_stats()
        assert stats['holds']['active_holds'] == 0

    # The in-flight response / old proxy publishes its held result after all
    # of that, through the same native tracker handle production code uses.
    lease = producer_tracker.track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        storage='inline',
        bytes=128,
    )
    assert replacement.hold_stats()['active_holds'] == 1, (
        'a late held result published through an old producer handle '
        'must stay observable'
    )
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 128

    lease.release()
    assert replacement.hold_stats()['active_holds'] == 0
    del producer_tracker
    assert replacement.memory_stats()['retired'] == []


def test_old_proxy_tracker_publishes_a_hold_after_public_shutdown() -> None:
    """An old proxy remains a supported producer after ``cc.shutdown()``.

    Old HTTP and thread-local proxies legitimately stay usable after a public
    shutdown; their held results publish through the native tracker handle
    they hold (``proxy.lease_tracker`` → ``response.track_retained``). This
    reproduces that exact producer chain deterministically at the native
    level: the session goes away, the producer handle publishes, and public
    statistics must see it.
    """
    from c_two import _native

    session = _native.RuntimeSession()
    proxy_tracker = session.lease_tracker()
    observation = session.retire_memory_observation()

    # Public shutdown: the registry swaps in a replacement that adopts the
    # observation and then drops the old session wrapper.
    del session
    replacement = _native.RuntimeSession()
    replacement.adopt_retired_memory_observation(observation)

    for _ in range(3):
        stats = replacement.memory_stats()
        assert stats['holds']['active_holds'] == 0

    # The old proxy makes a new held call; its response tracks the retained
    # lease on the tracker the proxy holds, after shutdown and snapshots.
    lease = proxy_tracker.track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        storage='inline',
        bytes=64,
    )
    assert replacement.hold_stats()['active_holds'] == 1
    assert replacement.memory_stats()['holds']['active_holds'] == 1
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 64

    lease.release()
    assert replacement.hold_stats()['active_holds'] == 0
    # The proxy itself goes away; the observation record detaches with it.
    del proxy_tracker
    assert replacement.memory_stats()['retired'] == []
    assert replacement.sweep_hold_leases(0.0) == []


def test_failed_session_replacement_preserves_previous_and_own_observations() -> None:
    """A replacement that never materializes must not lose retired records.

    Retiring is a pure re-capture that consumes nothing, so a replacement
    construction that fails afterwards leaves the old session current and
    exactly as observable as before — and a later successful retry still
    carries the old session's own charges and holds, including scopes that
    only came into existence after the failed attempt.
    """
    from c_two import _native

    first = _native.RuntimeSession()
    lease = _track_retained_lease(first)
    handoff = first.retire_memory_observation()
    second = _native.RuntimeSession()
    second.adopt_retired_memory_observation(handoff)
    assert second.hold_stats()['active_holds'] == 1

    # The next swap captures again, but the replacement construction fails
    # and the captured wrapper is dropped without ever being adopted.
    dropped = second.retire_memory_observation()
    del dropped
    assert second.hold_stats()['active_holds'] == 1, (
        'a failed replacement must leave the previous observations intact'
    )
    assert second.memory_stats()['holds']['active_holds'] == 1

    # The session continues working and publishes another hold after the
    # failed attempt; the retry's fresh capture must include it.
    own_lease = _track_retained_lease(second, bytes_=96)
    retry_handoff = second.retire_memory_observation()
    third = _native.RuntimeSession()
    third.adopt_retired_memory_observation(retry_handoff)
    assert third.hold_stats()['active_holds'] == 2, (
        'a successful retry after a failed replacement must carry the old '
        'session\'s own holds'
    )
    assert third.memory_stats()['holds']['total_held_bytes'] == 64 + 96

    lease.release()
    own_lease.release()
    assert third.hold_stats()['active_holds'] == 0

    # The registry-level ordering: a session class whose next construction
    # fails (an invalid replacement policy) keeps the installed session and
    # its already-adopted observations untouched.
    class FailingSwapSession:
        client_config_frozen = False
        server_id = None
        server_id_override = None
        server_ipc_overrides = None
        client_ipc_overrides = None
        fail_next_construction = False

        def __init__(self, **_kwargs) -> None:
            if type(self).fail_next_construction:
                raise RuntimeError('invalid replacement policy')

        @property
        def call_execution_limits_overrides(self) -> dict[str, int]:
            # No explicit limits were accepted by this installed session.
            # Let the swap reach the deliberately failing constructor.
            return {}

        def retire_memory_observation(self):
            return _StubObservation()

        def adopt_retired_memory_observation(self, observation) -> None:  # noqa: ARG002
            pass

    class _StubObservation:
        pass

    registry = _ProcessRegistry()
    installed = FailingSwapSession()
    registry._runtime_session = installed  # noqa: SLF001
    registry._server = None  # noqa: SLF001

    FailingSwapSession.fail_next_construction = True
    try:
        settings.shm_threshold = 8192
        with pytest.raises(RuntimeError, match='invalid replacement policy'):
            registry.set_transport_policy(shm_threshold=8192)
    finally:
        FailingSwapSession.fail_next_construction = False
        settings.shm_threshold = None

    assert registry._runtime_session is installed  # noqa: SLF001


def test_failed_native_replacement_retry_carries_own_holds_through_the_registry(monkeypatch) -> None:
    """Failed swap → old session continues → successful swap keeps own holds.

    The failed attempt must not consume the old session's own observation:
    after a failed replacement construction (invalid policy value) the old
    session stays installed with its hold observable, and the successful
    retry swaps in a replacement that still reports it.
    """
    from c_two import _native
    from c_two.transport import registry as registry_module

    first = _native.RuntimeSession()
    lease = _track_retained_lease(first)

    registry = _ProcessRegistry()
    registry._runtime_session = first  # noqa: SLF001
    registry._server = None  # noqa: SLF001

    calls = {'count': 0}
    original_kwargs = registry_module._runtime_session_kwargs_from_settings

    def invalid_then_valid_kwargs():
        calls['count'] += 1
        kwargs = dict(original_kwargs())
        if calls['count'] == 1:
            kwargs['server_id'] = 'bad/name'
        return kwargs

    monkeypatch.setattr(
        registry_module, '_runtime_session_kwargs_from_settings', invalid_then_valid_kwargs,
    )

    settings.shm_threshold = 8192
    try:
        # Preserve-identity swap forwards the old session's server identity,
        # so the invalid id fails exactly the first replacement construction.
        with pytest.raises(ValueError, match='server_id'):
            registry.set_transport_policy(shm_threshold=8192)
        assert registry._runtime_session is first  # noqa: SLF001
        assert registry._runtime_session.hold_stats()['active_holds'] == 1

        registry.set_transport_policy(shm_threshold=4096)
        replacement = registry._runtime_session  # noqa: SLF001
        assert replacement is not first
        assert replacement.hold_stats()['active_holds'] == 1, (
            'the successful retry must carry the old session\'s own holds'
        )
        assert replacement.memory_stats()['holds']['total_held_bytes'] == 64
    finally:
        settings.shm_threshold = None
        registry._runtime_session.shutdown(  # noqa: SLF001
            route_names=[], relay_anchor_address=None,
        )
        lease.release()
        _ProcessRegistry.reset()


@pytest.mark.timeout(120)
def test_repeated_shutdown_swaps_with_one_hold_do_not_grow_retired_records() -> None:
    """One long-lived hold spans repeated ``cc.shutdown()`` session swaps.

    The live old proxy keeps the retired session's Runtime, budget domain,
    and lease tracker alive, so its retired record stays observable — with
    the hold visible — through every swap. Each later empty session's own
    records detach when that session drops, so ``retired`` stays bounded at
    the one owner-backed row instead of accumulating, and once the hold, the
    proxy, and their owners are gone the observation detaches completely.
    """
    pytest.importorskip('fastdb4py.payload')
    # The CRM method annotation is resolved from module globals even though the
    # class is declared inside this test, so publish `Payload` for it.
    global Payload
    if Payload is None:  # pragma: no cover - import above already set it
        from fastdb4py.payload import Payload as _Payload

        Payload = _Payload

    @cc.crm(namespace='test.mem-stats', version='0.1.0')
    class HoldEcho:
        @cc.transfer(input=SPEC, output=SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    class HoldEchoResource:
        def echo(self, payload: Payload) -> Payload:
            return payload

    _ProcessRegistry.reset()
    settings.shm_threshold = 1
    proxy = None
    try:
        cc.register(HoldEcho, HoldEchoResource(), name='mem-stats-swap-hold')
        time.sleep(0.2)
        address = cc.server_address()
        assert address is not None

        proxy = cc.connect(HoldEcho, name='mem-stats-swap-hold', address=address)
        assert proxy.client._mode == 'ipc'  # noqa: SLF001
        held = cc.hold(proxy.echo)(_build_payload())
        try:
            before = cc.memory_stats()
            assert before['runtime_outgoing'] is not None
            assert before['holds']['active_holds'] >= 1

            for swap in range(3):
                cc.shutdown()
                after = cc.memory_stats()
                assert after['runtime_outgoing'] is None
                # The hold must survive every swap.
                assert cc.hold_stats()['active_holds'] >= 1, (
                    'retained hold metadata must not disappear across cc.shutdown()'
                )
                assert after['holds']['active_holds'] >= 1
                assert after['holds']['total_held_bytes'] >= 1
                # Exactly the old session's owner-backed record stays listed:
                # later empty sessions' trackers and scopes detach with their
                # session, so repeated swaps never accumulate records.
                outgoing = [
                    scope for scope in after['retired']
                    if scope['role'] == 'runtime_outgoing'
                ]
                assert len(outgoing) == 1, (
                    f'swap {swap}: expected one owner-backed retired record, '
                    f'saw {after["retired"]!r}'
                )
                assert outgoing[0]['state'] == 'retired'
        finally:
            held.release()

        # The hold's owners are gone; the still-referenced old proxy keeps the
        # record alive but drained, then detaching it when the proxy drops.
        assert cc.hold_stats()['active_holds'] == 0
        cc.close(proxy)
        proxy = None

        deadline = time.monotonic() + 5.0
        while True:
            drained = cc.memory_stats()
            retired = [
                scope for scope in drained['retired']
                if scope['role'] == 'runtime_outgoing'
            ]
            if not retired or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
        assert not retired, drained
        assert drained['holds']['active_holds'] == 0
        assert drained['holds']['total_held_bytes'] == 0
    finally:
        settings.shm_threshold = None
        if proxy is not None:
            cc.close(proxy)
            proxy = None
        cc.shutdown()
        _ProcessRegistry.reset()
