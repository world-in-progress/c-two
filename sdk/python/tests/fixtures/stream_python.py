"""Small server-stream contracts shared by Python runtime tests."""

from __future__ import annotations

import json
import threading
from collections.abc import Iterator

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload


BLOB_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "bytes", "nullable": False},
        },
    ],
    "components": [],
}


@cc.crm(namespace="test.python-server-stream", version="0.1.0")
class StreamValues:
    @cc.stream
    @cc.read
    def steps(self, count: int) -> Iterator[int]: ...

    @cc.stream
    def gated(self) -> Iterator[int]: ...

    @cc.stream
    def failed_step(self) -> Iterator[int]: ...

    @cc.stream
    def failed_cleanup(self) -> Iterator[int]: ...

    @cc.read
    def ping(self) -> str: ...


@cc.crm(namespace="test.python-server-stream-objects", version="0.1.0")
class StreamObjects:
    @cc.stream
    def steps(self) -> Iterator[object]: ...


@cc.crm(namespace="test.python-server-stream-payload", version="0.1.0")
class StreamPayloads:
    @cc.stream
    @cc.transfer(input=BLOB_SPEC, output=BLOB_SPEC)
    def steps(self, payload: Payload) -> Iterator[Payload]: ...


def blob_payload(data: bytes) -> Payload:
    spec = CompiledSpec.compile(json.dumps(BLOB_SPEC, separators=(",", ":")).encode())
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_bytes(data)
        plan = builder.freeze()
    finally:
        builder.close()
        spec.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def blob_bytes(payload: Payload) -> bytes:
    with payload.entry_view(0) as values:
        with values.at(0) as value:
            with value.acquire() as acquired:
                return bytes(acquired.bytes())


class ValuesResource:
    def __init__(self) -> None:
        self.events: list[str] = []
        self.step_entered = threading.Event()
        self.release_step = threading.Event()
        self.cleanup_entered = threading.Event()
        self.release_cleanup = threading.Event()

    def steps(self, count: int) -> Iterator[int]:
        self.events.append("factory")
        try:
            for index in range(count):
                self.events.append(f"yield:{index}")
                yield index
        finally:
            self.events.append("finally")

    def gated(self) -> Iterator[int]:
        self.events.append("factory")
        try:
            self.events.append("yield:0")
            yield 0
            self.step_entered.set()
            if not self.release_step.wait(timeout=10):
                raise AssertionError("test did not release the current stream step")
            self.events.append("yield:1")
            yield 1
        finally:
            self.cleanup_entered.set()
            if not self.release_cleanup.wait(timeout=10):
                raise AssertionError("test did not release stream cleanup")
            self.events.append("finally")

    def failed_step(self) -> Iterator[int]:
        try:
            yield 0
            raise ValueError("step failed after first item")
        finally:
            self.events.append("step-finally")

    def failed_cleanup(self) -> Iterator[int]:
        try:
            yield 0
        finally:
            self.events.append("cleanup-start")
            raise ValueError("cleanup failed")

    def ping(self) -> str:
        self.events.append("ping")
        return "pong"
