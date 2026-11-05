"""Python/Rust direct IPC stream matrix fixtures with one shared Cargo build."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

import c_two as cc
from c_two.crm.descriptor import build_portable_contract_descriptor
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload
from tests.fixtures.process_control import readline_with_timeout


REPOSITORY = Path(__file__).resolve().parents[4]
RUST_SOURCE = Path(__file__).with_name("stream_matrix_rust.rs")
SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one", "type": {"kind": "str", "nullable": True}},
    ],
    "components": [],
}
LARGE_LEN = 256 * 1024


@cc.crm(namespace="test.python-rust-stream-matrix", version="0.1.0")
class Portable:
    __cc_rpc_methods__ = ["ping", "echo"]

    @cc.read
    def ping(self) -> None: ...

    @cc.stream
    @cc.transfer(input=SPEC, output=SPEC)
    def echo(self, payload: Payload) -> Iterator[Payload]: ...


def value(large: bool) -> str:
    return "x" * LARGE_LEN if large else "stream-matrix"


def build_payload(large: bool) -> Payload:
    spec = CompiledSpec.compile(json.dumps(SPEC, separators=(",", ":")).encode())
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_str(value(large))
        plan = builder.freeze()
    finally:
        builder.close()
        spec.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()


def inspect(payload: Payload, large: bool) -> None:
    with payload.entry_view(0) as entry:
        with entry.at(0) as item:
            with item.acquire() as acquired:
                assert acquired.str() == value(large)


class PythonHost:
    def __init__(self, large: bool) -> None:
        self.large = large
        self.calls = 0
        self.outputs: list[Payload] = []

    def ping(self) -> None:
        return None

    def echo(self, payload: Payload) -> Iterator[Payload]:
        inspect(payload, self.large)
        self.calls += 1
        for _ in range(2):
            output = build_payload(self.large)
            self.outputs.append(output)
            yield output

    def close(self) -> None:
        for output in self.outputs:
            output.close()


@dataclass
class RustHarness:
    root: Path
    binary: Path

    @classmethod
    def prepare(cls, root: Path) -> RustHarness:
        (root / "src").mkdir(parents=True)
        (root / "fixtures").mkdir()
        shutil.copyfile(RUST_SOURCE, root / "src/main.rs")
        descriptor = build_portable_contract_descriptor(Portable)
        (root / "fixtures/stream.contract.json").write_text(
            json.dumps(descriptor, sort_keys=True, separators=(",", ":")), encoding="utf-8",
        )
        (root / "fixtures/stream-spec.json").write_text(
            json.dumps(SPEC, sort_keys=True, separators=(",", ":")), encoding="utf-8",
        )
        manifest = f"""[package]
name = "c-two-stream-matrix-harness"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ path = {json.dumps(str(REPOSITORY / 'sdk/rust'))} }}
fastdb = "=0.2.4"
"""
        (root / "Cargo.toml").write_text(manifest, encoding="utf-8")
        target = REPOSITORY / "core/target"
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(target)
        environment["C2_ENV_FILE"] = ""
        result = subprocess.run(
            ["cargo", "build", "--manifest-path", str(root / "Cargo.toml")],
            cwd=root, env=environment, capture_output=True, text=True, timeout=300,
            check=False,
        )
        assert result.returncode == 0, result.stderr
        binary = target / "debug" / (
            "c-two-stream-matrix-harness.exe" if os.name == "nt" else "c-two-stream-matrix-harness"
        )
        assert binary.is_file()
        return cls(root, binary)

    def client(self, address: str, route: str, shm: bool, large: bool, hold: bool) -> None:
        result = subprocess.run(
            [str(self.binary), "client", address, route, "shm" if shm else "no-shm",
             "large" if large else "inline", "hold" if hold else "materialized"],
            cwd=self.root, env={**os.environ, "C2_ENV_FILE": ""},
            capture_output=True, text=True, timeout=60, check=False,
        )
        assert result.returncode == 0, result.stderr
        assert "CLIENT_RECEIPT items=2 path=direct_ipc" in result.stdout

    def host(self, address: str, route: str, shm: bool, large: bool) -> RustHostProcess:
        return RustHostProcess(self, address, route, shm, large)


class RustHostProcess:
    def __init__(self, harness: RustHarness, address: str, route: str, shm: bool, large: bool) -> None:
        self.process = subprocess.Popen(
            [str(harness.binary), "host", address, route, "shm" if shm else "no-shm",
             "large" if large else "inline"],
            cwd=harness.root, env={**os.environ, "C2_ENV_FILE": ""},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        try:
            assert self.process.stdout is not None
            ready = readline_with_timeout(self.process.stdout, 30)
            assert ready.startswith(f"READY address={address} route_uid="), ready
        except BaseException:
            self.process.kill()
            self.process.communicate()
            raise

    def stop(self) -> None:
        try:
            out, err = self.process.communicate(input="\n", timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            out, err = self.process.communicate()
            raise AssertionError(f"Rust host did not stop: {out}\n{err}") from None
        assert self.process.returncode == 0, err
        assert "HOST_RECEIPT calls=1" in out, out

    def abort(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.communicate()
