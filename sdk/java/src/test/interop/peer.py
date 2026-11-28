"""Python consumer and author of the Java interoperability contract."""

from collections.abc import Iterator
import hashlib
import importlib.metadata
import json
from pathlib import Path
import sys
import time

import c_two as cc
import c_two.contract
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload


FIXTURES = Path(__file__).resolve().parent
ECHO_SPEC = (FIXTURES / "echo.json").read_bytes()
NUMBER_SPEC = (FIXTURES / "number.json").read_bytes()
ROUTE = "java-interop"
ECHO_FIELDS = ("跨语言", 231, 4_000_000_007, -2.625)
ECHO_BYTES = bytes(range(256)) * 4096


@cc.crm(namespace="test.interop", version="0.1.0")
class Interop:
    __cc_rpc_methods__ = ["ping", "echo", "apply", "scan"]

    @cc.read
    def ping(self) -> None: ...

    @cc.read
    @cc.transfer(input=json.loads(ECHO_SPEC), output=json.loads(ECHO_SPEC))
    def echo(self, payload: Payload) -> Payload: ...

    @cc.write
    @cc.transfer(input=json.loads(NUMBER_SPEC), output=json.loads(NUMBER_SPEC))
    def apply(self, payload: Payload) -> Payload: ...

    @cc.stream
    @cc.transfer(output=json.loads(NUMBER_SPEC))
    def scan(self) -> Iterator[Payload]: ...

    @cc.topic
    @cc.transfer(output=json.loads(NUMBER_SPEC))
    def events(self) -> Payload: ...


def build(spec_json, fields):
    with CompiledSpec.compile(spec_json) as spec, Builder.create(spec) as builder:
        for index, (kind, value) in enumerate(fields):
            builder.entry_begin(index, 1)
            getattr(builder, f"value_{kind}")(value)
        with builder.freeze() as plan:
            return plan.execute(BuildPolicy.ALLOW_STAGING).payload


def read(payload, kind, index=0):
    with payload.entry_view(index) as entry, entry.at(0) as value:
        if kind in ("str", "bytes"):
            with value.acquire() as access:
                return getattr(access, kind)()
        return getattr(value, f"get_{kind}")()


def number(value):
    return build(NUMBER_SPEC, [("u32", value)])


def verify_echo(payload):
    assert tuple(read(payload, kind, index) for index, kind in enumerate(
        ("str", "u8", "u32", "f64")
    )) == ECHO_FIELDS
    data = read(payload, "bytes", 4)
    assert len(data) == len(ECHO_BYTES)
    assert hashlib.sha256(data).digest() == hashlib.sha256(ECHO_BYTES).digest()


def admit(context):
    if context.metadata.get("interop") == "deny":
        return cc.AdmissionDecision.deny("fixture denial", details={"reason": "interop"})
    return None


class Resource:
    def __init__(self):
        self.pings = 0
        self.applies = 0
        self.publisher = None

    def ping(self):
        self.pings += 1
        if self.pings == 2:
            with number(20) as payload:
                assert self.publisher.publish(payload) == 2

    def echo(self, payload):
        verify_echo(payload)
        return payload

    def apply(self, payload):
        self.applies += 1
        return number(read(payload, "u32") + self.applies)

    def scan(self):
        for value in (31, 32, 33):
            with number(value) as payload:
                yield payload


def host(server_id, stop):
    runtime = cc.Runtime()
    runtime.set_server(server_id=server_id, admission_hook=admit)
    resource = Resource()
    with number(10) as initial:
        registration = runtime.register(Interop, resource, name=ROUTE, initial_topics={"events": initial})
    resource.publisher = registration.publisher("events")
    print(f"READY {runtime.server_address()}", flush=True)
    deadline = time.monotonic() + 120
    while not Path(stop).exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("Host did not receive its shutdown signal")
        time.sleep(0.025)
    assert runtime.shutdown(timeout=20)["completed"]
    assert (resource.pings, resource.applies) == (2, 1)
    print("PASS host shutdown", flush=True)


def client(endpoint, generated):
    sys.path.insert(0, generated)
    from c_two_contract import ContractClient

    runtime = cc.Runtime()
    proxy = runtime.connect(Interop, name=ROUTE, address=endpoint, timeout=20)
    timed = cc.with_call_options(proxy, timeout=20)
    client = ContractClient.from_proxy(timed)
    assert client.method_0_ping() is None
    echo = zip(("str", "u8", "u32", "f64", "bytes"), (*ECHO_FIELDS, ECHO_BYTES))
    with build(ECHO_SPEC, echo) as request, client.method_1_echo(request) as response:
        verify_echo(response)

    with number(41) as request:
        for _ in range(2):
            with client.method_2_apply(request, call_id="apply-once") as response:
                assert read(response, "u32") == 42
    outcome = client.query_call("apply-once")
    assert outcome.status == "succeeded" and outcome.result_available
    with client.recover_method_2_apply("apply-once") as response:
        assert read(response, "u32") == 42

    with client.method_3_scan() as stream:
        for expected in (31, 32, 33):
            with next(stream) as item:
                assert read(item, "u32") == expected
        assert next(stream, None) is None

    subscription = client.subscribe_topic_0_events()
    try:
        for seq, expected in ((1, 10), (2, 20)):
            if seq == 2:
                assert client.method_0_ping() is None
            delivery = next(subscription)
            with delivery.value as item:
                assert (delivery.seq, delivery.dropped, read(item, "u32")) == (seq, 0, expected)
    finally:
        assert subscription.close(timeout=20)["completed"]

    denied = ContractClient.from_proxy(cc.with_call_options(timed, metadata={"interop": "deny"}))
    try:
        denied.method_0_ping()
    except cc.error.CCError as error:
        assert error.code == 724 and error.transport_phase == "pre_dispatch"
        assert error.details["reason"] == "interop"
    else:
        raise AssertionError("metadata must deny the call before dispatch")
    assert runtime.shutdown(timeout=20)["completed"]
    print("PASS client operations", flush=True)


def codegen_check(descriptor_path, java_tree):
    descriptor = Path(descriptor_path).read_bytes()
    generated = cc.contract.compile_artifacts(descriptor, target="java")
    root = Path(java_tree)
    expected = {file.relative_to(root).as_posix(): file.read_bytes()
                for file in root.rglob("*") if file.is_file()}
    assert {item.relative_path: item.bytes for item in generated.artifacts} == expected
    print("PASS Python and c3 Java artifacts match byte for byte", flush=True)


if __name__ == "__main__":
    mode, *args = sys.argv[1:]
    if mode == "describe":
        assert importlib.metadata.version("fastdb4py") == "0.4.0"
        Path(args[0]).write_text(cc.contract.export_descriptor(Interop), encoding="utf-8")
    elif mode == "codegen-check":
        codegen_check(*args)
    elif mode == "host":
        host(*args)
    elif mode == "client":
        client(*args)
    else:
        raise ValueError(f"unknown peer mode {mode!r}")
