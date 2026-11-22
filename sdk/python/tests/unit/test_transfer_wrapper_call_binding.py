"""Call-binding behavior of the transfer and stream wrappers.

The exact-positional fast path must be observably identical to the
``inspect.Signature.bind`` fallback: same argument tuples, same defaults,
same exception types and messages.
"""
from __future__ import annotations

import inspect
import pickle

import pytest

import c_two as cc
from c_two import error
from c_two.crm.payload_plan import (
    python_pickle_input_binding,
    python_pickle_output_binding,
)
from c_two.crm.stream import _build_stream_wrapper
from c_two.crm.transferable import _build_transfer_wrapper


class RecordingClient:
    supports_direct_call = False

    def __init__(self):
        self.calls: list[tuple[str, object]] = []

    def call(self, method_name: str, payload: object) -> object:
        self.calls.append((method_name, payload))
        return payload


class FakeNativeStream:
    def cancel(self) -> None:
        pass


class DirectStreamClient:
    supports_direct_call = True

    def __init__(self):
        self.opened: list[tuple[str, tuple]] = []

    def open_direct_stream(self, method_name: str, values: tuple, *, output):
        self.opened.append((method_name, values))
        return FakeNativeStream()


def build_transfer_wrapper(func):
    return _build_transfer_wrapper(
        func,
        input=python_pickle_input_binding(func),
        output=python_pickle_output_binding(func),
    )


def forbid_signature_bind(monkeypatch) -> None:
    def forbidden(*args, **kwargs):
        raise AssertionError("bind must not run on the exact positional fast path")

    monkeypatch.setattr(inspect.Signature, "bind", forbidden)


# ---------------------------------------------------------------------------
# Client ("->") fast path
# ---------------------------------------------------------------------------

def test_exact_positional_call_skips_signature_bind(monkeypatch):
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()
    forbid_signature_bind(monkeypatch)

    assert wrapped(contract, 1, 2) == (1, 2)
    assert len(client.calls) == 1
    method_name, payload = client.calls[0]
    assert method_name == "compute"
    assert pickle.loads(payload) == (1, 2)


def test_zero_argument_method_takes_the_fast_path(monkeypatch):
    def ping(self):
        ...

    wrapped = build_transfer_wrapper(ping)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()
    forbid_signature_bind(monkeypatch)

    assert wrapped(contract) is None
    method_name, payload = client.calls[0]
    assert method_name == "ping"
    assert payload is None


def test_hold_through_bound_method_takes_the_fast_path(monkeypatch):
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)
    client = RecordingClient()
    crm = type("Crm", (), {"direction": "->", "client": client, "compute": wrapped})()
    forbid_signature_bind(monkeypatch)

    held = cc.hold(crm.compute)(1, 2)
    try:
        assert isinstance(held, cc.HeldResult)
        assert held.value == (1, 2)
    finally:
        held.release()
    assert len(client.calls) == 1


def test_hold_kwarg_alone_still_takes_the_fast_path(monkeypatch):
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()
    forbid_signature_bind(monkeypatch)

    held = wrapped(contract, 1, 2, _c2_buffer="hold")
    try:
        assert held.value == (1, 2)
    finally:
        held.release()


# ---------------------------------------------------------------------------
# Client ("->") fallback paths keep bind semantics
# ---------------------------------------------------------------------------

def test_missing_default_is_applied_by_the_bind_fallback():
    def scale(self, value, factor=10):
        ...

    wrapped = build_transfer_wrapper(scale)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()

    assert wrapped(contract, 5) == (5, 10)
    assert pickle.loads(client.calls[0][1]) == (5, 10)


def test_keyword_argument_binds_through_the_fallback():
    def scale(self, value, factor=10):
        ...

    wrapped = build_transfer_wrapper(scale)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()

    assert wrapped(contract, 5, factor=2) == (5, 2)


def test_too_many_positional_arguments_keeps_the_bind_error():
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)
    contract = type("Contract", (), {"direction": "->", "client": RecordingClient()})()

    with pytest.raises(TypeError, match="too many positional arguments"):
        wrapped(contract, 1, 2, 3)


def test_missing_required_argument_keeps_the_bind_error():
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)
    contract = type("Contract", (), {"direction": "->", "client": RecordingClient()})()

    with pytest.raises(TypeError, match="missing a required argument: 'a'"):
        wrapped(contract)


def test_keyword_only_argument_is_rejected_with_the_original_message():
    def kwonly(self, a, *, flag):
        ...

    wrapped = build_transfer_wrapper(kwonly)
    contract = type("Contract", (), {"direction": "->", "client": RecordingClient()})()

    with pytest.raises(
        TypeError,
        match="kwonly cannot transport keyword-only arguments: flag",
    ):
        wrapped(contract, 1, flag=2)


def test_defaulted_keyword_only_parameter_is_still_rejected():
    def kwonly(self, a, *, flag=3):
        ...

    wrapped = build_transfer_wrapper(kwonly)
    contract = type("Contract", (), {"direction": "->", "client": RecordingClient()})()

    with pytest.raises(TypeError, match="keyword-only arguments: flag"):
        wrapped(contract, 1)


def test_var_positional_signature_always_uses_the_bind_fallback():
    def collect(self, a, *rest):
        ...

    wrapped = build_transfer_wrapper(collect)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()

    # BoundArguments.args flattens VAR_POSITIONAL values into the tuple the
    # client serializes; the fast path is disabled for such signatures.
    assert wrapped(contract, 1, 2, 3) == (1, 2, 3)


def test_positional_only_parameters_take_the_fast_path(monkeypatch):
    def compute(self, a, b, /):
        ...

    wrapped = build_transfer_wrapper(compute)
    client = RecordingClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()
    forbid_signature_bind(monkeypatch)

    assert wrapped(contract, 1, 2) == (1, 2)


# ---------------------------------------------------------------------------
# Resource ("<-" ) direction keeps its hooks
# ---------------------------------------------------------------------------

def test_resource_direction_decodes_input_and_serializes_output():
    def compute(self, a, b):
        ...

    wrapped = build_transfer_wrapper(compute)

    class Resource:
        def compute(self, a, b):
            return a + b

    contract = type(
        "Contract", (), {"direction": "<-", "resource": Resource()}
    )()

    payload = pickle.dumps((1, 2))
    result = wrapped(contract, payload)
    assert isinstance(result, bytes)
    assert pickle.loads(result) == 3


def test_resource_direction_rejects_borrowed_without_scoped_owner():
    def compute(self, a):
        ...

    wrapped = build_transfer_wrapper(compute)
    contract = type(
        "Contract",
        (),
        {"direction": "<-", "resource": type("Resource", (), {"compute": lambda self, a: a})()},
    )()

    payload = pickle.dumps((1,))
    with pytest.raises(error.SerializationError, match="borrowed input requires"):
        wrapped(contract, payload, _c2_input_buffer_mode="borrowed")


# ---------------------------------------------------------------------------
# Stream wrappers
# ---------------------------------------------------------------------------

def build_stream_wrapper(func):
    return _build_stream_wrapper(
        func,
        input_binding=python_pickle_input_binding(func),
        output_binding=python_pickle_output_binding(func),
    )


def test_stream_exact_positional_call_skips_signature_bind(monkeypatch):
    def items(self, n, offset):
        ...

    wrapped = build_stream_wrapper(items)
    client = DirectStreamClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()
    forbid_signature_bind(monkeypatch)

    stream = wrapped(contract, 5, 0)
    assert isinstance(stream, cc.Stream)
    assert client.opened == [("items", (5, 0))]


def test_stream_missing_default_is_applied_by_the_bind_fallback():
    def items(self, n, offset=0):
        ...

    wrapped = build_stream_wrapper(items)
    client = DirectStreamClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()

    wrapped(contract, 5)
    assert client.opened == [("items", (5, 0))]


def test_stream_keyword_call_binds_through_the_fallback():
    def items(self, n, offset=0):
        ...

    wrapped = build_stream_wrapper(items)
    client = DirectStreamClient()
    contract = type("Contract", (), {"direction": "->", "client": client})()

    wrapped(contract, 5, offset=2)
    assert client.opened == [("items", (5, 2))]


def test_stream_argument_count_error_keeps_the_bind_error():
    def items(self, n, offset):
        ...

    wrapped = build_stream_wrapper(items)
    contract = type("Contract", (), {"direction": "->", "client": DirectStreamClient()})()

    with pytest.raises(TypeError, match="missing a required argument: 'offset'"):
        wrapped(contract, 5)


def test_stream_keyword_only_argument_is_rejected_with_the_original_message():
    def items(self, n, *, flag):
        ...

    wrapped = build_stream_wrapper(items)
    contract = type("Contract", (), {"direction": "->", "client": DirectStreamClient()})()

    with pytest.raises(
        TypeError,
        match="stream methods cannot transport keyword-only arguments",
    ):
        wrapped(contract, 5, flag=1)


def test_stream_resource_direction_uses_the_frozen_arity():
    from collections.abc import Iterator
    from c_two.crm.payload_plan import DEFAULT_PICKLE_PROTOCOL

    def items(self, n, offset=0) -> Iterator[int]:
        ...

    wrapped = build_stream_wrapper(items)

    class Resource:
        def items(self, n, offset=0):
            yield from range(n + offset)

    contract = type("Contract", (), {"direction": "<-", "resource": Resource()})()

    payload = pickle.dumps((2, 1), protocol=DEFAULT_PICKLE_PROTOCOL)
    resource_stream = wrapped(contract, payload)
    assert resource_stream.step() == (True, pickle.dumps(0, protocol=DEFAULT_PICKLE_PROTOCOL))
    assert resource_stream.step() == (True, pickle.dumps(1, protocol=DEFAULT_PICKLE_PROTOCOL))
    assert resource_stream.step() == (True, pickle.dumps(2, protocol=DEFAULT_PICKLE_PROTOCOL))
    assert resource_stream.step() == (False, None)
