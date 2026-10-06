"""Thin Python projection of the native local-endpoint maintenance API.

Every decision that matters is made in Rust. This module owns no credential
field table, derives no OS path, reassembles no native endpoint, and keeps no
independent sweep state. It only names the native entry points and presents
their honest result dictionaries.

The three operations map onto the `c2-core` / `c2-local` lifecycle surface:

``inspect_endpoint(address, *, endpoint_protocol=None)``
    Observe one logical endpoint. ``None`` resolves the configured process
    endpoint policy through the Rust resolver; an explicit
    ``'legacy-v1'`` / ``'managed-v2'`` value is parsed by the Rust enum.
    ``KernelManaged`` (Windows named pipes) and ``NotApplicable`` describe
    which layer owns endpoint lifetime and are never reported as ``alive``.

``reap_endpoint(address, credential)``
    Remove the exact endpoint object a credential names. The endpoint is
    derived with the protocol the credential itself records, so a managed
    credential is not misread as stale because the process default happens to
    be legacy-v1. The native identity check decides the outcome.

``sweep_endpoints(protocol, *, max_entries=None, max_ms=None)``
    Open one bounded, explicitly driven maintenance sweep. ``protocol`` is
    required and is never guessed. ``max_entries`` / ``max_ms`` are optional
    native budget overrides; ``None`` keeps the native ``SweepBudget`` default
    stored inside the Rust sweep. At most one sweep may be active per process;
    that lease is owned by Rust.

A sweep holds exactly one native iterator. Iterate it with ``next_batch()``
and always ``close()`` it (or use it as a context manager) so the native
iterator and the process lease are released deterministically::

    with sweep_endpoints('legacy-v1') as sweep:
        while True:
            batch = sweep.next_batch()
            if batch['round_complete'] or batch['round_interrupted']:
                break
"""
from __future__ import annotations

from typing import Any

__all__ = [
    'EndpointCredential',
    'EndpointSweep',
    'inspect_endpoint',
    'reap_endpoint',
    'sweep_endpoints',
]

# Statuses that describe which layer owns endpoint lifetime rather than a live
# instance. They are honest observations, not proofs of liveness.
NON_LIVENESS_STATUSES = frozenset({'not-applicable'})


def _native() -> Any:
    from c_two import _native

    return _native


class EndpointCredential:
    """Opaque native endpoint credential.

    The value is produced only by the native layer: ``inspect_endpoint``
    returns the encoded document, and ``EndpointCredential.from_json`` parses
    a document with the one Rust credential codec. There is no Python
    constructor that assembles fields, because a credential is a native
    identity record and not a Python-owned data structure.

    ``to_json()`` is a description, not a secret and not an authorization
    token. It records a logical address, protocol, and platform; it is never
    evidence that the endpoint exists or is alive.
    """

    __slots__ = ('_native',)

    def __init__(self, native: Any) -> None:
        # Wraps an already-parsed native credential. Callers use `from_json`
        # or receive one from `inspect_endpoint`.
        self._native = native

    @classmethod
    def from_json(cls, document: str) -> 'EndpointCredential':
        """Parse a strict JSON credential document with the Rust codec."""
        if not isinstance(document, str):
            raise TypeError('credential document must be a str')
        return cls(_native().PyEndpointCredential.from_json(document))

    def to_json(self) -> str:
        """Encode this credential as strict JSON."""
        return self._native.to_json()

    @property
    def address(self) -> str:
        """The logical IPC address this credential describes (metadata)."""
        return self._native.address

    @property
    def protocol(self) -> str:
        """The endpoint protocol this credential records (metadata)."""
        return self._native.protocol

    @property
    def platform(self) -> str:
        """The OS namespace this credential lives in (metadata)."""
        return self._native.platform

    def __repr__(self) -> str:
        return f'EndpointCredential(address={self.address!r}, protocol={self.protocol!r})'


def inspect_endpoint(
    address: str,
    *,
    endpoint_protocol: str | None = None,
) -> dict[str, Any]:
    """Inspect one logical local endpoint without creating ownership metadata.

    Args:
        address: Logical IPC address, for example ``'ipc://my_server'``.
        endpoint_protocol: ``'legacy-v1'`` or ``'managed-v2'``. ``None``
            resolves the configured process policy through the Rust resolver.

    Returns:
        A result dictionary with ``status``, ``credential``, ``reason``,
        ``io_kind``, ``raw_os_error``, and ``retryable``. ``status`` is one of
        ``'absent'``, ``'present'``, ``'not-applicable'``, ``'unverified'``,
        ``or 'io-error'``. Only ``'present'`` carries an
        :class:`EndpointCredential`, and it still only means the endpoint
        object was observed.

    Raises:
        TypeError: ``address`` is not a string.
        ValueError: the address is not a valid ``ipc://`` address, the
            protocol is unknown, or the configured process protocol could not
            be resolved.
    """
    if not isinstance(address, str):
        raise TypeError('address must be a str')
    result = dict(
        _native().inspect_endpoint_endpoint(address, endpoint_protocol=endpoint_protocol)
    )
    _wrap_credential(result)
    return result


def reap_endpoint(address: str, credential: EndpointCredential) -> dict[str, Any]:
    """Reap the exact endpoint object named by ``credential``.

    The protocol used to derive the endpoint comes from the credential, not
    from the process default. A credential describing another endpoint is
    reported as ``'stale-target'`` and is never probed against this endpoint's
    namespace. A decoded credential never removes a newer native incarnation:
    the native identity check compares the recorded incarnation and reports
    ``'stale-target'`` instead.

    Returns:
        A result dictionary. Only ``'reaped'`` and ``'already-absent'`` are
        terminal successes. ``'busy'``, ``'stale-target'``, ``'unverified'``,
        ``'not-applicable'``, and ``'io-error'`` are reported as-is; a partial
        or unverifiable cleanup is never upgraded into a fabricated success.

    Raises:
        TypeError: ``credential`` is not an :class:`EndpointCredential` or
            ``address`` is not a string.
        ValueError: ``address`` is not a valid ``ipc://`` address.
    """
    if not isinstance(address, str):
        raise TypeError('address must be a str')
    if not isinstance(credential, EndpointCredential):
        raise TypeError('credential must be an EndpointCredential')
    return dict(
        _native().reap_endpoint_credential(address, credential._native)  # noqa: SLF001
    )


class EndpointSweep:
    """A bounded, explicitly driven maintenance sweep over one protocol.

    Exactly one native iterator backs this object for its whole life. The
    process-wide maintenance lease is Rust-owned: constructing a second sweep
    while one is open raises ``RuntimeError``, and releasing this one (through
    ``close()`` or garbage collection) frees that lease.

    Budgets are stored and validated natively by the Rust sweep; this facade
    only forwards values. ``closed`` and ``close()`` are native state
    projections, not Python-owned lifecycle flags.

    Do not construct this type directly; use :func:`sweep_endpoints`.
    """

    __slots__ = ('_native',)

    def __init__(self, native: Any) -> None:
        self._native = native

    @property
    def protocol(self) -> str:
        """The protocol namespace this sweep covers."""
        return self._native.protocol

    @property
    def closed(self) -> bool:
        """Whether the native iterator and process lease were released."""
        return self._native.closed

    def next_batch(
        self,
        *,
        max_entries: int | None = None,
        max_ms: int | None = None,
    ) -> dict[str, Any]:
        """Advance the native iterator by exactly one bounded batch.

        Each call consumes its own budget: nothing is carried into a
        directory-wide collection, and no budget is silently dropped. The
        counts are the real per-batch native counters.

        Args:
            max_entries: Override the sweep's stored native entry budget.
                ``None`` keeps the value validated when the sweep was opened.
            max_ms: Override the sweep's stored native wall-clock budget in
                milliseconds. ``None`` keeps the value validated at open time.
                This is a scheduling target, not a hard real-time guarantee
                for filesystem calls. Explicit values are validated by the
                native gate, which owns the range and ceiling.

        Returns:
            A batch dictionary with the per-batch counters plus
            ``round_complete``, ``round_interrupted``, and
            ``namespace_changed``. ``round_complete`` is true only when this
            batch reached directory EOF. A caller that only checks
            ``round_complete`` cannot mistake an interrupted round for full
            coverage, because ``round_interrupted`` stays set.
        """
        return dict(
            self._native.next_batch(max_entries=max_entries, max_ms=max_ms)
        )

    def close(self) -> None:
        """Release the native iterator and the process maintenance lease.

        Idempotent. Dropping the object without calling this releases them
        too, but that timing is not deterministic.
        """
        self._native.close()

    def __enter__(self) -> 'EndpointSweep':
        return self

    def __exit__(self, _exc_type: object, _exc: object, _traceback: object) -> bool:
        self.close()
        return False

    def __repr__(self) -> str:
        return f'EndpointSweep(protocol={self.protocol!r}, closed={self.closed})'


def sweep_endpoints(
    protocol: str,
    *,
    addresses: list[str] | None = None,
    max_entries: int | None = None,
    max_ms: int | None = None,
) -> EndpointSweep:
    """Open one bounded maintenance sweep over ``protocol``.

    ``protocol`` is required and is never guessed: a sweep must not silently
    cover the wrong namespace. The returned sweep must be closed.

    Args:
        protocol: ``'legacy-v1'`` or ``'managed-v2'``.
        addresses: Optional logical IPC addresses to select. ``None`` covers
            the whole protocol namespace; an empty list selects no slots.
            Rust derives and validates canonical socket and ownership names
            before opening the iterator or acquiring the maintenance lease.
        max_entries: Optional entry-budget override for every batch. ``None``
            keeps the native ``SweepBudget`` default stored by the Rust sweep.
        max_ms: Optional wall-clock budget override in milliseconds. ``None``
            keeps the native default. Explicit values are validated by the
            native gate before the process lease or iterator exists.

    Raises:
        RuntimeError: another sweep is already active in this process.
        TypeError: ``protocol`` is not a string, or a budget is not an int.
        ValueError: the protocol is unknown, or a budget is out of range.
    """
    native = _native()
    # The facade forwards the caller's values unchanged: the Rust sweep stores
    # the default budget and validates every explicit dimension before it
    # takes the process lease or opens the iterator.
    return EndpointSweep(
        native.PyEndpointSweep(protocol, addresses=addresses, max_entries=max_entries, max_ms=max_ms)
    )


def _wrap_credential(result: dict[str, Any]) -> None:
    """Replace a ``'present'`` credential document with a native credential.

    The document itself is not retained: it is only a transport form. The
    native credential object is the authority for a later ``reap_endpoint``.
    """
    document = result.get('credential')
    if isinstance(document, str):
        result['credential'] = EndpointCredential.from_json(document)
