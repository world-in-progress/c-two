"""Portable streams between Python and Rust through one real c3 relay."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import uuid

import pytest

import c_two as cc
from c_two.crm.descriptor import build_portable_contract_descriptor
from c_two import error
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec
from tests.fixtures.process_control import readline_with_timeout
from tests.fixtures.stream_matrix import (
    Portable, REPOSITORY, RUST_SOURCE, RustHarness, SPEC, build_payload, inspect,
    process_environment,
)


ITEM_BYTES = 128 * 1024
SLOW_ITEMS = 512
MAX_RUN_AHEAD = 64


def _payload(text):
    spec = CompiledSpec.compile(json.dumps(SPEC).encode())
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_str(text)
        plan = builder.freeze()
    finally:
        builder.close()
        spec.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def _item(index):
    empty = _payload("")
    try:
        overhead = len(empty.binary_bytes())
    finally:
        empty.close()
    value = _payload("x" * (ITEM_BYTES - overhead - 12) + f"{index:012}")
    assert len(value.binary_bytes()) == ITEM_BYTES
    return value


def _inspect_item(value, index):
    assert len(value.binary_bytes()) == ITEM_BYTES
    with value.entry_view(0) as entry, entry.at(0) as item, item.acquire() as acquired:
        text = acquired.str()
    assert text.endswith(f"{index:012}")
    assert set(text[:-12]) == {"x"}


class PythonResource:
    def __init__(self, count, ending):
        self.count = count
        self.ending = ending
        self.steps = self.produced = self.cleanups = 0
        self.changed = threading.Condition()
        self.output = None

    def ping(self):
        return None

    def echo(self, payload):
        inspect(payload, False)
        return self

    def __iter__(self):
        return self

    def __next__(self):
        with self.changed:
            self.steps += 1
            self.changed.notify_all()
        if self.produced == self.count:
            if self.ending in ("step", "cleanup-step"):
                raise ValueError("relay cross step failed")
            raise StopIteration
        if self.output is not None:
            self.output.close()
        self.output = _item(self.produced)
        with self.changed:
            self.produced += 1
            self.changed.notify_all()
        return self.output

    def close(self):
        if self.output is not None:
            self.output.close()
            self.output = None
        with self.changed:
            self.cleanups += 1
            self.changed.notify_all()
        if self.ending.startswith("cleanup"):
            raise ValueError("relay cross cleanup failed")

    def stats(self):
        with self.changed:
            return {"steps": self.steps, "produced": self.produced, "cleanups": self.cleanups}

    def produced_three(self):
        with self.changed:
            assert self.changed.wait_for(lambda: self.produced >= 3, timeout=10)
        return self.stats()

    def quiet(self):
        # The client is parked at a test barrier. Wait for a full observation
        # interval with no resource step, rather than sleeping before sampling.
        deadline = time.monotonic() + 10
        with self.changed:
            while True:
                before = self.steps
                advanced = self.changed.wait_for(lambda: self.steps != before, timeout=1)
                assert self.steps <= MAX_RUN_AHEAD + 1
                if not advanced:
                    return self.stats()
                assert time.monotonic() < deadline, "resource did not become quiet"


class Process:
    def __init__(self, harness, args):
        self.process = subprocess.Popen(
            [str(harness.binary), *args], cwd=harness.root,
            env=process_environment(), stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1,
        )

    def read(self):
        line = readline_with_timeout(self.process.stdout, 15)
        assert line, self.process.stderr.read()
        return json.loads(line)

    def send(self, command):
        self.process.stdin.write(command + "\n")
        self.process.stdin.flush()

    def request(self, command):
        self.send(command)
        return self.read()

    def wait(self):
        self.process.wait(timeout=15)
        assert self.process.returncode == 0, self.process.stderr.read()

    def abort(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.communicate(timeout=10)


class Session:
    def __init__(self, direction, relay, harness):
        self.direction, self.relay, self.harness = direction, relay, harness
        self.runtime = self.proxy = self.source = self.stream = None
        self.resource = self.host = self.client = None

    def start(self, count, ending="success", behavior="materialized"):
        route = "relay-cross-" + uuid.uuid4().hex[:12]
        self.count, self.ending = count, ending
        self.runtime = cc.Runtime()
        if self.direction == "python-to-rust":
            address = "ipc://relay_cross_" + uuid.uuid4().hex[:12]
            self.host = Process(self.harness, [
                "relay-host", address, route, self.relay.url,
                "--items", str(count), "--ending", ending,
            ])
            assert self.host.read() == {"event": "ready", "address": address}
            self.runtime.set_call_execution_limits(
                payload_buffer_budget_bytes=(2 if behavior == "hold" else 3) * ITEM_BYTES,
            )
            self.proxy = self.runtime.connect(Portable, name=route, address=self.relay.url)
            assert self.proxy.client.observed_path == "explicit_relay"
            self.source = build_payload(False)
            method = cc.hold(self.proxy.echo) if behavior == "hold" else self.proxy.echo
            self.stream = method(self.source)
        else:
            self.runtime.set_relay_anchor(self.relay.url)
            self.resource = PythonResource(count, ending)
            self.runtime.register(Portable, self.resource, name=route)
            self.client = Process(self.harness, [
                "relay-client", self.relay.url, route, str(count), behavior,
            ])

    def resource_stats(self, command="stats"):
        if self.host is not None:
            return self.host.request(command)
        return getattr(self.resource, {"produced-three": "produced_three"}.get(command, command))()

    def _rust_done(self):
        result = self.client.read()
        assert result["event"] == "done" and result["path"] == "explicit_relay"
        self.client.wait()
        return result

    def _python_done(self, delivered, failure=None):
        receipt = self.stream.close()
        assert receipt["completed"]
        assert receipt["complete_items"] == delivered
        repeated = self.stream.close(timeout=0)
        assert repeated["completed"] and repeated["complete_items"] == delivered
        for terminal in (receipt["error"], repeated["error"]):
            if failure is None:
                assert terminal is None
            else:
                assert isinstance(terminal, error.ResourceExecuteFunction)
                assert str(terminal) == str(failure)
                assert terminal.details == failure.details
        assert self.runtime.call_execution_snapshot()["used_operations"] == 0
        stats = self.runtime.memory_stats()
        assert stats["payload_buffer"]["used_bytes"] == 0
        return {
            "delivered": delivered, "complete_items": receipt["complete_items"],
            "error": None if failure is None else {
                "code": 3, "message": str(failure), "details": failure.details,
            },
            "memory": stats,
        }

    def consume(self):
        if self.client is not None:
            return self._rust_done()
        delivered = 0
        failure = None
        try:
            for item in self.stream:
                try:
                    _inspect_item(item, delivered)
                finally:
                    item.close()
                delivered += 1
        except error.ResourceExecuteFunction as exc:
            failure = exc
        return self._python_done(delivered, failure)

    def held(self):
        if self.client is not None:
            full = self.client.read()
            assert full["event"] == "holds-full"
            self.resource_stats("produced-three")
            assert full["memory"]["payload_buffer"]["used_bytes"] == 2 * ITEM_BYTES
            self.client.send("release")
            return self._rust_done()
        first = second = third = None
        worker = None
        result = {}
        started = threading.Barrier(2)
        finished = threading.Event()

        def receive():
            started.wait(timeout=10)
            try:
                result["item"] = next(self.stream)
            except BaseException as exc:
                result["error"] = exc
            finally:
                finished.set()

        try:
            first, second = next(self.stream), next(self.stream)
            _inspect_item(first.value, 0)
            _inspect_item(second.value, 1)
            full = self.runtime.memory_stats()
            assert full["holds"]["active_holds"] == 2
            assert full["payload_buffer"]["used_bytes"] == 2 * ITEM_BYTES
            worker = threading.Thread(target=receive, daemon=True)
            worker.start()
            started.wait(timeout=10)
            self.resource_stats("produced-three")
            assert not finished.is_set(), result
            first.release()
            worker.join(10)
            assert not worker.is_alive()
            assert "error" not in result, result
            third = result["item"]
            _inspect_item(second.value, 1)
            _inspect_item(third.value, 2)
            after_one_release = self.runtime.memory_stats()
            assert after_one_release["holds"]["active_holds"] == 2
            assert after_one_release["payload_buffer"]["used_bytes"] == 2 * ITEM_BYTES
            second.release()
            third.release()
            assert list(self.stream) == []
            report = self._python_done(3)
            assert report["memory"]["holds"]["active_holds"] == 0
            report["holds"] = {"full": full, "after_one_release": after_one_release}
            return report
        finally:
            for item in (first, second, third, result.get("item")):
                if item is not None:
                    item.release()
            if worker is not None:
                worker.join(10)

    def slow(self):
        if self.client is not None:
            assert self.client.read() == {"event": "paused", "delivered": 1}
        else:
            item = next(self.stream)
            try:
                _inspect_item(item, 0)
            finally:
                item.close()
        paused = self.resource_stats("quiet")
        assert 1 <= paused["produced"] <= paused["steps"] <= MAX_RUN_AHEAD + 1
        assert paused["cleanups"] == 0
        assert paused["steps"] < SLOW_ITEMS // 4
        if self.client is not None:
            self.client.send("resume")
            report = self._rust_done()
        else:
            delivered = 1
            for item in self.stream:
                try:
                    _inspect_item(item, delivered)
                finally:
                    item.close()
                delivered += 1
            report = self._python_done(delivered)
        report["paused"] = {**paused, "delivered": 1}
        return report

    def close(self):
        if self.client is not None:
            self.client.abort()
        if self.stream is not None:
            self.stream.close(timeout=5)
        if self.source is not None:
            self.source.close()
        if self.proxy is not None:
            cc.close(self.proxy)
        if self.runtime is not None:
            assert self.runtime.shutdown(timeout=5)["completed"]
        if self.host is not None:
            try:
                self.host.send("stop")
                self.host.read()
                self.host.wait()
            finally:
                self.host.abort()


@pytest.fixture(scope="module")
def relay_cross_rust(tmp_path_factory):
    root = tmp_path_factory.mktemp("relay-stream-cross")
    (root / "src").mkdir()
    (root / "fixtures").mkdir()
    shutil.copyfile(RUST_SOURCE, root / "src/main.rs")
    for name, value in (
        ("stream.contract.json", build_portable_contract_descriptor(Portable)),
        ("stream-spec.json", SPEC),
    ):
        (root / "fixtures" / name).write_text(json.dumps(value), encoding="utf-8")
    # Match the CLI/native extension's immutable FastDB source input. Cargo
    # patches belong to the root manifest and are not inherited from sdk/rust.
    (root / "Cargo.toml").write_text(f'''[package]
name = "c-two-relay-stream-cross"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ path = {json.dumps(str(REPOSITORY / "sdk/rust"))} }}
fastdb = "=0.2.4"
serde_json = "1"

[patch.crates-io]
fastdb-sys = {{ git = "https://github.com/world-in-progress/fastdb", rev = "f82fa13adeae2b87a295bf984821c85c44b55a3d" }}
''', encoding="utf-8")
    target = REPOSITORY / "core/target"
    result = subprocess.run(
        ["cargo", "build", "--manifest-path", str(root / "Cargo.toml")],
        cwd=root, env={**os.environ, "CARGO_TARGET_DIR": str(target)},
        capture_output=True, text=True, timeout=300, check=False,
    )
    assert result.returncode == 0, result.stderr
    name = "c-two-relay-stream-cross.exe" if os.name == "nt" else "c-two-relay-stream-cross"
    # Execute a private copy: another pytest worker may rebuild the shared Cargo
    # binary, which Windows cannot replace while a resource is running from it.
    executable_directory = target / "relay-stream-cross" / uuid.uuid4().hex
    executable_directory.mkdir(parents=True)
    binary = executable_directory / name
    shutil.copy2(target / "debug" / name, binary)
    try:
        yield RustHarness(root, binary)
    finally:
        binary.unlink()
        executable_directory.rmdir()


@pytest.fixture(params=["python-to-rust", "rust-to-python"])
def relay_cross(request, relay_cross_rust, start_c3_relay, monkeypatch):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    # All processes inherit one private root. Windows proves the same behavior
    # through native named pipes; it has no Unix path or signal dependency.
    with tempfile.TemporaryDirectory(prefix="r") as directory:
        if os.name != "nt":
            monkeypatch.setenv("C2_IPC_ROOT", directory)
        relay = start_c3_relay()
        session = Session(request.param, relay, relay_cross_rust)
        try:
            yield session
        finally:
            try:
                session.close()
            finally:
                relay.stop()


def record_result(session, report, record_property):
    report["direction"] = session.direction
    report["resource"] = session.resource_stats()
    assert report["delivered"] == report["complete_items"] == session.count
    assert report["resource"] == {
        "steps": session.count + 1, "produced": session.count, "cleanups": 1,
    }
    record_property("relay_cross", json.dumps(report, sort_keys=True))
    return report
