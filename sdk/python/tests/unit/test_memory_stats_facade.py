"""Native holds and retired role reports remain observable while owned."""
from __future__ import annotations

import json
import os
import time

import pytest

from c_two.transport.registry import _reset_default_runtime

import c_two as cc

import c_two.observe

try:  # fastdb4py is optional for every non-portable test in this file.
    from fastdb4py.payload import Payload
except ImportError:  # pragma: no cover - lean environments without FastDB
    Payload = None  # type: ignore[assignment]


@pytest.fixture(autouse=True)
def _clean_registry():
    """Start and end each facade test from the process-level singleton.

    The public facade reads the default ``Runtime``, so a session left installed
    by another test file must never leak into these assertions.
    """
    _reset_default_runtime()
    yield
    _reset_default_runtime()


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


def _track_retained_local_copy(session, *, bytes_: int = 64):
    return session.local_copy_tracker().track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        bytes=bytes_,
    )


def test_native_retirement_keeps_retained_local_copy_metadata_across_sessions() -> None:
    """The session handoff keeps Rust-owned local-copy metadata observable.

    This is the deterministic native-level half of the public proof: the
    replacement session reports a retained local copy that belongs to the retired
    session for exactly as long as a real owner holds it, and the record
    detaches once the last owner (the hold guard and the retired session
    itself) is gone.
    """
    from c_two import _native

    first = _native.RuntimeSession()
    local_copy = _track_retained_local_copy(first)
    assert first.begin_shutdown().observe()["completed"]

    replacement = _native.RuntimeSession()
    replacement.inherit_retirement(first)

    assert replacement.memory_stats()["holds"]['active_holds'] == 1
    assert replacement.memory_stats()['holds']['active_holds'] == 1
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 64

    # A hold is a real owner: releasing it does not detach the record while
    # the retired session's own tracker handle still exists.
    local_copy.release()
    assert replacement.memory_stats()["holds"]['active_holds'] == 0

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
    proxy's ``local_copy_tracker`` or an in-flight response holds) keeps the
    tracker alive, so the late hold must remain visible through the
    replacement once it lands, without relying on timing.
    """
    from c_two import _native

    first = _native.RuntimeSession()
    producer_tracker = first.local_copy_tracker()
    # Counters are zero and no hold exists at capture time.
    assert first.begin_shutdown().observe()["completed"]

    replacement = _native.RuntimeSession()
    replacement.inherit_retirement(first)
    assert replacement.memory_stats()["holds"]['active_holds'] == 0

    # The retired session wrapper drops, then repeated snapshots must not
    # discard the producer-owned record even though it still reports zero.
    del first
    for _ in range(3):
        stats = replacement.memory_stats()
        assert stats['holds']['active_holds'] == 0

    # The in-flight response / old proxy publishes its held result after all
    # of that, through the same native tracker handle production code uses.
    local_copy = producer_tracker.track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        bytes=128,
    )
    assert replacement.memory_stats()["holds"]['active_holds'] == 1, (
        'a late held result published through an old producer handle '
        'must stay observable'
    )
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 128

    local_copy.release()
    assert replacement.memory_stats()["holds"]['active_holds'] == 0
    del producer_tracker
    assert replacement.memory_stats()['retired'] == []


def test_old_proxy_tracker_publishes_a_hold_after_public_shutdown() -> None:
    """An old proxy remains a supported producer after ``cc.shutdown()``.

    Old HTTP and thread-local proxies legitimately stay usable after public
    shutdown; new held calls publish through the native local-copy tracker
    they captured (``proxy.local_copy_tracker`` -> ``response.track_retained``).
    This reproduces that producer chain deterministically at the native level:
    the session goes away, the producer publishes, and statistics must see it.
    """
    from c_two import _native

    session = _native.RuntimeSession()
    proxy_tracker = session.local_copy_tracker()
    assert session.begin_shutdown().observe()['completed']
    replacement = _native.RuntimeSession()
    replacement.inherit_retirement(session)
    del session

    for _ in range(3):
        stats = replacement.memory_stats()
        assert stats['holds']['active_holds'] == 0

    # The old proxy makes a new held call and tracks its retained local copy
    # through the captured tracker, after shutdown and repeated snapshots.
    local_copy = proxy_tracker.track_retained(
        route_name='grid',
        method_name='echo',
        direction='client_response',
        bytes=64,
    )
    assert replacement.memory_stats()["holds"]['active_holds'] == 1
    assert replacement.memory_stats()['holds']['active_holds'] == 1
    assert replacement.memory_stats()['holds']['total_held_bytes'] == 64

    local_copy.release()
    assert replacement.memory_stats()["holds"]['active_holds'] == 0
    # The proxy itself goes away; the observation record detaches with it.
    del proxy_tracker
    assert replacement.memory_stats()['retired'] == []


@pytest.mark.timeout(120)
@pytest.mark.parametrize("scope", ["default", "explicit"])
def test_repeated_shutdown_swaps_with_one_hold_do_not_grow_retired_records(scope, tmp_path) -> None:
    """One long-lived hold spans repeated ``api.shutdown()`` session swaps.

    The live old proxy keeps the retired session's Runtime, budget domain,
    and local-copy tracker alive, so its retired record stays observable — with
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

    _reset_default_runtime()
    api = cc if scope == 'default' else cc.Runtime()
    endpoint_api = cc.endpoint if scope == 'default' else api
    observe_api = cc.observe if scope == 'default' else api
    if os.name != 'nt':
        endpoint_api.set_local_endpoint(root=str(tmp_path))
    proxy = None
    try:
        api.register(HoldEcho, HoldEchoResource(), name='mem-stats-swap-hold')
        time.sleep(0.2)
        address = api.server_address()
        assert address is not None

        proxy = api.connect(HoldEcho, name='mem-stats-swap-hold', address=address)
        assert proxy.client._mode == 'ipc'  # noqa: SLF001
        held = cc.hold(proxy.echo)(_build_payload())
        try:
            before = observe_api.memory_stats()
            assert before['client']['frozen']
            assert before['holds']['active_holds'] >= 1

            for swap in range(3):
                api.shutdown()
                after = observe_api.memory_stats()
                assert after['client']['memory'] is None
                # The hold must survive every swap.
                assert observe_api.memory_stats()["holds"]['active_holds'] >= 1, (
                    'retained hold metadata must not disappear across api.shutdown()'
                )
                assert after['holds']['active_holds'] >= 1
                assert after['holds']['total_held_bytes'] >= 1
                # Exactly the old session's owner-backed record stays listed:
                # later empty sessions' trackers and scopes detach with their
                # session, so repeated swaps never accumulate records.
                outgoing = [
                    scope for scope in after['retired']
                    if scope['role'] == 'client'
                ]
                assert len(outgoing) == 1, (
                    f'swap {swap}: expected one owner-backed retired record, '
                    f'saw {after["retired"]!r}'
                )
                assert outgoing[0]['closed']
        finally:
            held.release()

        # The hold's owners are gone; the still-referenced old proxy keeps the
        # record alive but drained, then detaching it when the proxy drops.
        assert observe_api.memory_stats()["holds"]['active_holds'] == 0
        cc.close(proxy)
        proxy = None

        deadline = time.monotonic() + 5.0
        while True:
            drained = observe_api.memory_stats()
            retired = [
                scope for scope in drained['retired']
                if scope['role'] == 'client'
            ]
            if not retired or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
        assert not retired, drained
        assert drained['holds']['active_holds'] == 0
        assert drained['holds']['total_held_bytes'] == 0
    finally:
        if proxy is not None:
            cc.close(proxy)
            proxy = None
        api.shutdown()
        _reset_default_runtime()
