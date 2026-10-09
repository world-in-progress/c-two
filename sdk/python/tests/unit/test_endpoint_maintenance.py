"""Unit coverage for the native local-endpoint maintenance projection.

These tests exercise the real public native objects: the same
``_native.PyEndpointSweep`` / ``_native.PyEndpointCredential`` symbols the
Python facade wraps. Nothing is monkeypatched into a fake, because the
properties under test (the process maintenance lease, strict credential
parsing, honest result statuses) are owned by Rust and would not be proven by
a stub.

Platform-dependent behaviour is asserted in ``cfg`` branches rather than
skipped, so the Windows kernel-managed and managed-v2 rejection paths keep
executable coverage instead of disappearing from the suite.
"""
from __future__ import annotations

import json
import os
import sys
import threading
import time
import uuid

import pytest

import c_two as cc
from c_two import _native
from c_two.transport import endpoint as endpoint_module

# This address is never bound. Every maintenance constructor selects only this
# fresh slot, including calls directly to PyO3; shared-root history cannot be reaped.
SWEEP_ADDRESSES = [f'ipc://sweep-test-{uuid.uuid4().hex}']

MAX_CREDENTIAL_BYTES = 32 * 1024
#: The native gate's published hard ceilings. The tests own these expectations
#: so the Python facade cannot silently mirror, weaken, or dominate them.
MAX_SWEEP_ENTRIES = 4096
MAX_SWEEP_MS = 1000

IS_WINDOWS = sys.platform == 'win32'

#: A well-formed managed-v2 credential document. Managed-v2 is a Unix
#: filesystem namespace, so this document is only meaningful on POSIX.
def _unix_document(address: str) -> str:
    root = f'/tmp/c2-{os.geteuid():x}' if not IS_WINDOWS else '/tmp/c2-unix'
    namespace = cc.local_endpoint_context(root=root).namespace_id if not IS_WINDOWS else '0' * 64
    return json.dumps({
        'schemaVersion': 3, 'address': address, 'protocol': 'managed-v2', 'platform': 'unix',
        'incarnation': '00112233445566778899aabbccddeeff',
        'device': 1, 'inode': 2, 'changedSecs': 3, 'changedNanos': 4,
        'unixRoot': root, 'namespaceId': namespace,
    }, separators=(',', ':'))


MANAGED_V2_DOCUMENT = _unix_document('ipc://unit-managed-endpoint')


@pytest.fixture(scope='module', autouse=True)
def native_namespace():
    """A real isolated bind prepares the namespace that read-only sweeps open."""
    session = _native.RuntimeSession(
        server_id=f'sweep-prepare-{uuid.uuid4().hex}',
        use_process_relay_anchor=False,
    )
    bridge = session.ensure_server_bridge()
    try:
        bridge.start()
    finally:
        outcome = session.shutdown(timeout_seconds=5.0)
        assert outcome['completed'], outcome
    # The native listener removes only its owned socket; its legacy rendezvous
    # lock follows native retention policy. No scoped sweep selects that slot.
    yield


def _native_document(address: str = 'ipc://unit-native-endpoint') -> str:
    if not IS_WINDOWS:
        return _unix_document(address)
    return json.dumps({'schemaVersion': 1, 'address': address, 'protocol': 'named-pipe', 'platform': 'windows'}, separators=(',', ':'))


def _native_credential(address: str = 'ipc://unit-native-endpoint') -> cc.EndpointCredential:
    return cc.EndpointCredential.from_json(_native_document(address))


def test_credential_is_an_opaque_native_wrapper() -> None:
    # The Python type carries no field table; it only wraps the native value.
    credential = _native_credential()
    assert isinstance(credential._native, _native.PyEndpointCredential)
    assert credential.address == 'ipc://unit-native-endpoint'
    assert credential.protocol == ('named-pipe' if IS_WINDOWS else 'managed-v2')
    assert credential.platform == ('windows' if IS_WINDOWS else 'unix')


def test_credential_round_trips_through_the_one_rust_codec() -> None:
    document = _native_document()
    credential = cc.EndpointCredential.from_json(document)
    # `to_json` is produced by the Rust encoder; re-parsing it must yield the
    # same metadata, proving the value never passes through a Python field
    # table on the way out.
    reparsed = cc.EndpointCredential.from_json(credential.to_json())
    assert reparsed.address == credential.address
    assert reparsed.protocol == credential.protocol
    assert reparsed.to_json() == credential.to_json()


def test_credential_from_json_rejects_a_non_string_document() -> None:
    with pytest.raises(TypeError):
        cc.EndpointCredential.from_json(b'{}')  # type: ignore[arg-type]


@pytest.mark.parametrize(
    ('document', 'fragment'),
    [
        ('{', 'malformed-json'),
        ('', 'malformed-json'),
        # Unknown fields are rejected instead of ignored.
        (
            json.dumps(
                {
                    'schemaVersion': 1,
                    'address': 'ipc://x',
                    'protocol': 'legacy-v1',
                    'platform': 'unix',
                    'device': 1,
                    'inode': 2,
                    'changedSecs': 3,
                    'changedNanos': 4,
                    'path': '/tmp/forged',
                },
                separators=(',', ':'),
            ),
            'malformed-json',
        ),
        # An unimplemented schema version is never upgraded to a UUID record.
        (
            json.dumps(
                {
                    'schemaVersion': 9,
                    'address': 'ipc://x',
                    'protocol': 'legacy-v1',
                    'platform': 'unix',
                    'device': 1,
                    'inode': 2,
                    'changedSecs': 3,
                    'changedNanos': 4,
                },
                separators=(',', ':'),
            ),
            'unsupported-schema-version',
        ),
        # v1 may never be presented as a managed-v2 incarnation credential.
        (
            json.dumps(
                {
                    'schemaVersion': 1,
                    'address': 'ipc://x',
                    'protocol': 'managed-v2',
                    'platform': 'unix',
                    'device': 1,
                    'inode': 2,
                    'changedSecs': 3,
                    'changedNanos': 4,
                },
                separators=(',', ':'),
            ),
            # Unix rejects schema v1 for its native backend; Windows rejects
            # the Unix platform metadata before checking schema identity.
            'unsupported-platform' if IS_WINDOWS else 'invalid-value',
        ),
    ],
)
def test_credential_from_json_rejects_invalid_documents(
    document: str, fragment: str
) -> None:
    with pytest.raises(ValueError) as excinfo:
        cc.EndpointCredential.from_json(document)
    assert fragment in str(excinfo.value)


def test_credential_from_json_enforces_the_byte_limit() -> None:
    oversized = ' ' * (MAX_CREDENTIAL_BYTES + 1)
    with pytest.raises(ValueError) as excinfo:
        cc.EndpointCredential.from_json(oversized)
    assert 'too-large' in str(excinfo.value)


def test_managed_v2_credential_is_rejected_on_windows() -> None:
    # A managed-v2 endpoint is a Unix filesystem namespace. The Windows branch
    # must reject it rather than fabricate a kernel-managed pipe credential.
    if IS_WINDOWS:
        with pytest.raises(ValueError) as excinfo:
            cc.EndpointCredential.from_json(MANAGED_V2_DOCUMENT)
        assert 'unsupported-platform' in str(excinfo.value) or 'invalid' in str(
            excinfo.value
        )
    else:
        credential = cc.EndpointCredential.from_json(MANAGED_V2_DOCUMENT)
        assert credential.protocol == 'managed-v2'
        assert credential.to_json() == MANAGED_V2_DOCUMENT


def test_windows_kernel_managed_credential_parses_without_unix_fields() -> None:
    # The mirror of the test above: a kernel-managed pipe record has no inode,
    # so it must be accepted only on Windows and rejected on Unix.
    if IS_WINDOWS:
        credential = cc.EndpointCredential.from_json(_native_document())
        assert credential.platform == 'windows'
    else:
        with pytest.raises(ValueError):
            cc.EndpointCredential.from_json(
                json.dumps(
                    {
                        'schemaVersion': 1,
                        'address': 'ipc://unit-windows-pipe',
                        'protocol': 'legacy-v1',
                        'platform': 'windows',
                    },
                    separators=(',', ':'),
                )
            )


def test_inspect_reports_absent_for_an_unbound_endpoint() -> None:
    result = cc.inspect_endpoint(
        'ipc://unit-absent-endpoint'
    )
    if IS_WINDOWS:
        # A named pipe is a kernel namespace: the honest observation is that
        # the platform owns endpoint lifetime, never that a live instance is
        # present or absent.
        assert result['status'] == 'not-applicable'
        assert result['reason'] == 'kernel-managed'
    else:
        assert result['status'] == 'absent'
    assert result['credential'] is None
    assert result['retryable'] is False
    # The IPv4 shape is stable: every branchable field is always present.
    assert set(result) == {
        'status',
        'credential',
        'reason',
        'io_kind',
        'raw_os_error',
        'retryable',
    }


def test_inspect_rejects_an_invalid_address() -> None:
    with pytest.raises(ValueError):
        cc.inspect_endpoint('http://not-an-ipc-address')


def test_reap_reports_stale_target_for_a_foreign_address() -> None:
    credential = _native_credential('ipc://unit-credential-owner')
    result = cc.reap_endpoint('ipc://unit-credential-other', credential)
    assert result['status'] == 'stale-target'
    assert result['reason'] == 'credential-address-mismatch'
    assert result['retryable'] is False


def test_reap_never_removes_from_the_wrong_root() -> None:
    # A credential for another logical address must not probe this address's
    # namespace at all: the outcome is a mismatch, not `already-absent`.
    owner = _native_credential('ipc://unit-root-a')
    result = cc.reap_endpoint('ipc://unit-root-b', owner)
    assert result['status'] != 'already-absent'
    assert result['status'] == 'stale-target'


def test_reap_of_an_unbound_endpoint_is_platform_honest() -> None:
    credential = _native_credential('ipc://unit-unbound-endpoint')
    result = cc.reap_endpoint('ipc://unit-unbound-endpoint', credential)
    assert set(result) == {
        'status',
        'credential',
        'reason',
        'io_kind',
        'raw_os_error',
        'retryable',
    }
    if IS_WINDOWS:
        # A named pipe lives in a kernel namespace: there is no filesystem
        # entry to collect, and this is not evidence of a live instance.
        assert result['status'] == 'not-applicable'
        assert result['reason'] == 'no-filesystem-entry'
    else:
        assert result['status'] in {'already-absent', 'reaped'}


def test_reap_requires_an_endpoint_credential() -> None:
    with pytest.raises(TypeError):
        cc.reap_endpoint('ipc://unit-type-check', object())  # type: ignore[arg-type]


def test_reap_does_not_accept_a_decoded_credential_for_a_new_incarnation() -> None:
    # A decoded old credential must never authorize removal of a newer native
    # incarnation. The native gate compares the recorded incarnation with the
    # live record, so a stale document is a `stale-target` (or a platform
    # `not-applicable`), never a success against the new listener.
    if IS_WINDOWS:
        # Managed-v2 has no Windows namespace, so the document is rejected
        # before a credential exists. A kernel-managed record must still never
        # report a filesystem removal.
        with pytest.raises(ValueError) as excinfo:
            cc.EndpointCredential.from_json(MANAGED_V2_DOCUMENT)
        assert 'invalid' in str(excinfo.value) or 'unsupported-platform' in str(
            excinfo.value
        )
        credential = _native_credential('ipc://unit-windows-incarnation')
        result = cc.reap_endpoint('ipc://unit-windows-incarnation', credential)
        assert result['status'] == 'not-applicable'
        return
    @cc.crm(namespace='cc.test.endpoint.scope', version='0.1.0')
    class ScopeProbe:
        def echo(self, value: int) -> int:
            ...

    class ProbeResource:
        def echo(self, value: int) -> int:
            return value

    server_id = f'scope-incarnation-{uuid.uuid4().hex}'
    current = None
    address = None
    try:
        cc.set_server(server_id=server_id)
        cc.register(ScopeProbe, ProbeResource(), name='scope-probe')
        address = cc.server_address()
        first = cc.inspect_endpoint(address)
        assert first['status'] == 'present'
        old = cc.EndpointCredential.from_json(first['credential'].to_json())
        cc.shutdown()
        cc.set_server(server_id=server_id)
        cc.register(ScopeProbe, ProbeResource(), name='scope-probe')
        assert cc.server_address() == address
        second = cc.inspect_endpoint(address)
        assert second['status'] == 'present'
        current = second['credential']
        assert current.to_json() != old.to_json()
        result = cc.reap_endpoint(address, old)
        assert result['status'] in {'busy', 'stale-target'}
        after = cc.inspect_endpoint(address)
        assert after['status'] == 'present'
        assert after['credential'].to_json() == current.to_json()
    finally:
        cc.shutdown()
        if current is not None:
            assert cc.reap_endpoint(address, current)['status'] in {'already-absent', 'reaped'}


def test_reap_uses_the_recorded_context_not_the_process_default(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    credential = _native_credential('ipc://unit-recorded-context')
    # A malformed process root must not reinterpret the captured credential.
    monkeypatch.setenv('C2_IPC_ROOT', 'invalid-relative-root')
    result = cc.reap_endpoint(credential.address, credential)
    if IS_WINDOWS:
        assert result['status'] == 'not-applicable'
        assert result['reason'] == 'no-filesystem-entry'
    else:
        assert result['status'] in {'already-absent', 'stale-target', 'unverified'}


class TestSweepLease:
    """The process lease is owned by Rust and allows exactly one sweep."""

    def test_one_sweep_at_a_time(self) -> None:
        first = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        try:
            with pytest.raises(RuntimeError) as excinfo:
                cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
            assert 'already active' in str(excinfo.value)
        finally:
            first.close()

    def test_close_releases_the_lease(self) -> None:
        first = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        first.close()
        assert first.closed is True
        second = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        second.close()

    def test_close_is_idempotent(self) -> None:
        sweep = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        sweep.close()
        sweep.close()
        assert sweep.closed is True

    def test_context_manager_releases_the_lease(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            assert sweep.closed is False
        assert sweep.closed is True
        # The lease is free again after the context exits.
        cc.sweep_endpoints(addresses=SWEEP_ADDRESSES).close()

    def test_dropping_an_abandoned_sweep_releases_the_lease(self) -> None:
        abandoned = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        # Dropping without close is the abandoned path; the native Drop impl
        # must still release the process lease.
        del abandoned
        cc.sweep_endpoints(addresses=SWEEP_ADDRESSES).close()

    def test_lease_admits_exactly_one_sweep_under_concurrent_open(self) -> None:
        # The lease is one atomic compare-exchange, so two concurrent openers
        # cannot both win. The loser sees the same RuntimeError as the
        # sequential case, and the winner still owns one usable iterator.
        outcomes: list[object] = []

        def open_sweep() -> None:
            try:
                outcomes.append(cc.sweep_endpoints(addresses=SWEEP_ADDRESSES))
            except RuntimeError as error:
                outcomes.append(error)

        threads = [threading.Thread(target=open_sweep) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()

        sweeps = [item for item in outcomes if isinstance(item, cc.EndpointSweep)]
        refusals = [item for item in outcomes if isinstance(item, RuntimeError)]
        try:
            assert len(sweeps) == 1
            assert len(refusals) == 1
            assert 'already active' in str(refusals[0])
            assert sweeps[0].closed is False
            assert sweeps[0].next_batch()['batch'] == 1
        finally:
            for sweep in sweeps:
                sweep.close()

    def test_next_batch_after_close_is_rejected(self) -> None:
        sweep = cc.sweep_endpoints(addresses=SWEEP_ADDRESSES)
        sweep.close()
        with pytest.raises(ValueError):
            sweep.next_batch()


class TestSweepBudget:
    """Budgets are validated in Rust before any `Duration` arithmetic."""

    def test_zero_entries_is_rejected(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_entries=0)

    def test_entries_above_the_ceiling_are_rejected(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_entries=MAX_SWEEP_ENTRIES + 1)

    def test_zero_milliseconds_is_rejected(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_ms=0)

    def test_milliseconds_above_the_ceiling_are_rejected(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_ms=MAX_SWEEP_MS + 1)

    def test_non_integer_budgets_are_rejected(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(TypeError):
                sweep.next_batch(max_entries=1.5)  # type: ignore[arg-type]
            with pytest.raises(TypeError):
                sweep.next_batch(max_ms='10')  # type: ignore[arg-type]

    def test_boolean_budget_is_rejected_as_a_non_integer(self) -> None:
        # `bool` is an `int` subclass; a sweep budget is not a flag.
        with pytest.raises(TypeError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_entries=True)  # type: ignore[arg-type]

    def test_huge_budget_cannot_panic_or_overflow_native_duration(self) -> None:
        # Values far beyond `u64` milliseconds would overflow a `Duration`
        # multiplication in a naive implementation. The Rust gate bounds the
        # budget before any arithmetic, so this is a clean ValueError.
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_ms=10**19)
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_entries=10**19)

    @pytest.mark.parametrize('value', [2**32, 2**64, 2**70, 10**40])
    def test_out_of_range_budget_never_reaches_native_duration_arithmetic(
        self, value: int
    ) -> None:
        # Regression: a value larger than the native integer width used to be
        # accepted at open time and only surface later (or overflow). It must
        # be a clean ValueError at open time for both budget fields.
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_ms=value)
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_entries=value)

    def test_boundary_budgets_are_accepted(self) -> None:
        # The inclusive ceilings are legal; only values beyond them are not.
        with cc.sweep_endpoints(
            addresses=SWEEP_ADDRESSES, max_entries=MAX_SWEEP_ENTRIES, max_ms=MAX_SWEEP_MS
        ) as sweep:
            batch = sweep.next_batch(
                max_entries=MAX_SWEEP_ENTRIES, max_ms=MAX_SWEEP_MS
            )
        assert batch['entries_visited'] >= 0

    def test_batch_budget_is_bounded_even_when_open_allows_it(self) -> None:
        # Every batch re-enters the native gate, and the override may differ
        # from the budget validated at open time. There is no Python pre-check
        # to bypass, so the ValueError can only be the native rejection.
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_ms=2**64)
            with pytest.raises(ValueError):
                sweep.next_batch(max_entries=2**64)

    @pytest.mark.parametrize(
        'kwargs',
        [
            {'max_entries': 0},
            {'max_entries': MAX_SWEEP_ENTRIES + 1},
            {'max_ms': 0},
            {'max_ms': MAX_SWEEP_MS + 1},
        ],
    )
    def test_native_gate_rejects_out_of_range_values_without_the_python_precheck(
        self, kwargs: dict[str, int]
    ) -> None:
        # The Rust gate is now the only budget authority, so the old "bypass
        # the Python precheck" path is every path. A ValueError here proves the
        # native gate rejects the value, and a rejected budget must leave both
        # the iterator and the process lease intact.
        sweep = _native.PyEndpointSweep(addresses=SWEEP_ADDRESSES)
        try:
            with pytest.raises(ValueError):
                sweep.next_batch(**kwargs)
            assert sweep.closed is False
            assert sweep.next_batch()['batch'] == 1
        finally:
            sweep.close()

    @pytest.mark.parametrize('kwargs', [{'max_entries': 2**32}, {'max_ms': 2**64}])
    def test_native_gate_rejects_values_beyond_the_budget_integer_width(
        self, kwargs: dict[str, int]
    ) -> None:
        # Every integer is parsed by the native gate itself, so a value far
        # beyond the widest exact parse is still one clean ValueError instead
        # of an interpreter-level conversion error. The property under test is
        # that the rejection is a clean exception, never a panic or a clamp.
        sweep = _native.PyEndpointSweep(addresses=SWEEP_ADDRESSES)
        try:
            with pytest.raises(ValueError):
                sweep.next_batch(**kwargs)
            assert sweep.closed is False
        finally:
            sweep.close()

    def test_negative_budget_is_rejected(self) -> None:
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_entries=-1)

    def test_open_validates_every_budget_before_taking_the_process_lease(self) -> None:
        # Both dimensions are validated by the native constructor before it
        # takes the process lease or opens the iterator, so a rejected open
        # cannot leave the lease held or wedge the next sweep.
        with pytest.raises(ValueError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_entries=0)
        with pytest.raises(TypeError):
            cc.sweep_endpoints(addresses=SWEEP_ADDRESSES, max_ms=True)
        cc.sweep_endpoints(addresses=SWEEP_ADDRESSES).close()

    def test_rejected_batch_override_keeps_the_iterator_and_stored_default(
        self,
    ) -> None:
        # An explicit override is validated by the same native gate that
        # validated the stored default. Rejection must not close the sweep or
        # lose the default, so a bare next_batch still advances batch 1.
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            with pytest.raises(ValueError):
                sweep.next_batch(max_entries=MAX_SWEEP_ENTRIES + 1)
            with pytest.raises(TypeError):
                sweep.next_batch(max_ms='10')  # type: ignore[arg-type]
            assert sweep.closed is False
            assert sweep.next_batch()['batch'] == 1


class TestSweepBatches:
    """Batch dictionaries carry the real native counters."""

    EXPECTED_KEYS = {
        'batch',
        'entries_visited',
        'endpoints_examined',
        'reaped',
        'already_absent',
        'busy',
        'stale_target',
        'unverified',
        'io_errors',
        'last_io_error',
        'not_applicable',
        'leases_retired',
        'round_complete',
        'round_interrupted',
        'namespace_changed',
    }

    def test_first_batch_reports_the_full_counter_set(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            batch = sweep.next_batch()
        assert set(batch) == self.EXPECTED_KEYS
        assert batch['batch'] == 1
        assert isinstance(batch['entries_visited'], int)
        assert isinstance(batch['round_complete'], bool)
        assert isinstance(batch['round_interrupted'], bool)
        assert isinstance(batch['namespace_changed'], bool)

    def test_batch_index_advances(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            first = sweep.next_batch()
            assert first['batch'] == 1
            if not (first['round_complete'] or first['round_interrupted']):
                second = sweep.next_batch()
                assert second['batch'] == 2
                assert second['entries_visited'] > 0

    def test_budget_is_consumed_per_batch_not_dropped(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            batch = sweep.next_batch(max_entries=1, max_ms=1)
        # A single-entry budget can never exceed one visited entry.
        assert batch['entries_visited'] <= 1

    def test_round_never_claims_completion_while_interrupted(self) -> None:
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            batch = sweep.next_batch()
        if batch['round_interrupted']:
            assert batch['round_complete'] is False

    def test_windows_sweep_reports_not_applicable(self) -> None:
        # The Windows branch must produce an executable observation, not a skip.
        with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
            batch = sweep.next_batch()
        if IS_WINDOWS:
            assert batch['not_applicable'] >= 1
            assert batch['round_complete'] is True
        else:
            # On Unix a fresh namespace has either entries or nothing; the
            # counters must still be non-negative integers.
            assert batch['entries_visited'] >= 0
            assert batch['not_applicable'] >= 0

    def test_repeated_sweeps_do_not_leak_the_lease(self) -> None:
        for _ in range(3):
            with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES) as sweep:
                sweep.next_batch(max_entries=1, max_ms=1)


def test_endpoint_module_keeps_no_python_field_tables() -> None:
    # The projection must stay thin: no Python-side allowed/forbidden key
    # tables and no path assembly. Guard the two shapes we removed.
    source = endpoint_module.__doc__ or ''
    assert 'Rust' in source
    for forbidden in ('allow', 'forbid', 'shutil', 'rmtree', 'os.path'):
        assert not hasattr(endpoint_module, forbidden)
    assert not hasattr(endpoint_module, 'CREDENTIAL_FIELDS')
    # No Python JSON codec either: both credential directions are Rust.
    assert not hasattr(endpoint_module, 'json')
    # Budget numbers and their validation belong to the native sweep: the
    # facade must not mirror ceilings/defaults or own a range-check helper.
    for native_owned in (
        'MAX_SWEEP_ENTRIES',
        'MAX_SWEEP_MS',
        'DEFAULT_MAX_ENTRIES',
        'DEFAULT_MAX_MS',
        '_bounded_int',
        'SweepBudget',
    ):
        assert not hasattr(endpoint_module, native_owned), native_owned
    # The facade wrapper carries the native object and no Python budget state.
    assert endpoint_module.EndpointSweep.__slots__ == ('_native',)


def test_top_level_exports_are_the_facade_objects() -> None:
    assert cc.inspect_endpoint is endpoint_module.inspect_endpoint
    assert cc.reap_endpoint is endpoint_module.reap_endpoint
    assert cc.sweep_endpoints is endpoint_module.sweep_endpoints
    assert cc.EndpointCredential is endpoint_module.EndpointCredential
    assert cc.EndpointSweep is endpoint_module.EndpointSweep
    for name in (
        'EndpointCredential',
        'EndpointSweep',
        'inspect_endpoint',
        'reap_endpoint',
        'sweep_endpoints',
    ):
        assert name in cc.__all__


def test_no_liveness_status_is_fabricated_for_kernel_managed_platforms() -> None:
    # `not-applicable` describes endpoint ownership, never a live instance.
    assert 'not-applicable' in endpoint_module.NON_LIVENESS_STATUSES
    assert 'present' not in endpoint_module.NON_LIVENESS_STATUSES


@pytest.mark.parametrize('addresses', [['http://wrong'], ['/tmp/forged.sock'], [SWEEP_ADDRESSES[0]] * 4097])
def test_sweep_address_scope_rejects_before_taking_the_lease(addresses: list[str]) -> None:
    with pytest.raises(ValueError):
        cc.sweep_endpoints(addresses=addresses)
    with cc.sweep_endpoints(addresses=SWEEP_ADDRESSES):
        pass


def test_empty_sweep_scope_never_inspects_endpoints() -> None:
    with cc.sweep_endpoints(addresses=[]) as sweep:
        for _ in range(10000):
            batch = sweep.next_batch(max_entries=MAX_SWEEP_ENTRIES)
            assert batch['endpoints_examined'] == 0
            assert batch['reaped'] == batch['leases_retired'] == 0
            if batch['round_complete'] or batch['round_interrupted']:
                break
        else:
            pytest.fail('finite namespace did not reach a terminal round')
