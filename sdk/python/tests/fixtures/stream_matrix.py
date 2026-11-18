"""Python/Rust direct IPC stream matrix fixtures with one shared Cargo target."""

from __future__ import annotations

import json
import multiprocessing
import os
import shutil
import subprocess
from collections.abc import Iterator
from contextlib import contextmanager
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
CHUNK_SIZE = 128 * 1024


@dataclass(frozen=True)
class TransportCase:
    name: str
    large: bool
    memory_budget_bytes: int
    file_spill_budget_bytes: int = 0

    def limits(self) -> dict[str, int]:
        return {
            "memory_budget_bytes": self.memory_budget_bytes,
            "file_spill_budget_bytes": self.file_spill_budget_bytes,
        }


TRANSPORT_CASES = (
    TransportCase("inline", False, 8 * 1024 * 1024),
    TransportCase("shm", True, 8 * 1024 * 1024),
    TransportCase("bounded-inline", False, 192 * 1024),
    TransportCase("chunked-file-input", True, 320 * 1024, 8 * 1024 * 1024),
    TransportCase("bounded-shm", True, 640 * 1024),
)


def process_environment(limits=None, ipc_root=None) -> dict[str, str]:
    environment = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": ""}
    if ipc_root is not None and os.name != "nt":
        environment["C2_IPC_ROOT"] = str(ipc_root)
    for key, value in (limits or {}).items():
        environment[f"C2_{key.upper()}"] = str(value)
    return environment


def assert_transport_stats(case, encoded_len, input_stats, server_stats, client_stats) -> None:
    """Check role-domain reservation and settlement for both language adapters."""
    roles = (client_stats["client"], server_stats["server"], input_stats)
    for role in roles:
        assert role["frozen"]
        for key, limit in case.limits().items():
            cell = role[key.removesuffix("_budget_bytes")]
            assert cell["limit_bytes"] == limit
            assert 0 <= cell["used_bytes"] <= cell["peak_bytes"] <= limit
    assert client_stats["client"]["memory"]["peak_bytes"] >= encoded_len
    assert client_stats["client"]["file_spill"]["peak_bytes"] == 0
    assert server_stats["server"]["file_spill"]["used_bytes"] == 0
    assert input_stats["memory"]["peak_bytes"] >= encoded_len
    if case.file_spill_budget_bytes:
        assert encoded_len > CHUNK_SIZE
        assert server_stats["server"]["file_spill"]["peak_bytes"] >= encoded_len
    else:
        assert server_stats["server"]["file_spill"]["peak_bytes"] == 0


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
        self.observe_input = None
        self.input_stats = None

    def ping(self) -> None:
        return None

    def echo(self, payload: Payload) -> Iterator[Payload]:
        inspect(payload, self.large)
        self.calls += 1
        if self.observe_input is not None:
            self.input_stats = self.observe_input()
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
    @contextmanager
    def prepare(cls, root: Path, *, name: str) -> Iterator[RustHarness]:
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
name = {json.dumps(name)}
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ path = {json.dumps(str(REPOSITORY / 'sdk/rust'))} }}
fastdb = "=0.2.4"
serde_json = "1"
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
        executable = name + (".exe" if os.name == "nt" else "")
        built_binary = target / "debug" / executable
        assert built_binary.is_file()
        # Callers use distinct Cargo names so publishing one cannot unlink the
        # other's binary. Run a private copy so Windows can rebuild either one
        # while its resource process is still running.
        binary = root / executable
        shutil.copy2(built_binary, binary)
        try:
            yield cls(root, binary)
        finally:
            binary.unlink()

    def client(self, address: str, route: str, large: bool, hold: bool,
               *, limits=None, ipc_root=None) -> dict:
        result = subprocess.run(
            [str(self.binary), "client", address, route,
             "large" if large else "inline", "hold" if hold else "materialized"],
            cwd=self.root, env=process_environment(limits, ipc_root),
            capture_output=True, text=True, timeout=60, check=False,
        )
        assert result.returncode == 0, result.stderr
        receipt = json.loads(result.stdout.strip())
        assert receipt["items"] == 2 and receipt["path"] == "direct_ipc"
        return receipt

    def host(self, address: str, route: str, large: bool,
             *, limits=None, gated=False, ipc_root=None) -> RustHostProcess:
        return RustHostProcess(self, address, route, large, limits, gated, ipc_root)


class RustHostProcess:
    def __init__(self, harness: RustHarness, address: str, route: str, large: bool,
                 limits, gated, ipc_root) -> None:
        self.process = subprocess.Popen(
            [str(harness.binary), "gated-host" if gated else "host", address, route,
             "large" if large else "inline"],
            cwd=harness.root, env=process_environment(limits, ipc_root),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        try:
            assert self.process.stdout is not None
            ready = readline_with_timeout(self.process.stdout, 30).rstrip("\r\n")
            assert ready == f"READY address={address}", ready
        except BaseException:
            self.process.kill()
            self.process.communicate()
            raise

    def stats(self) -> dict:
        self.process.stdin.write("stats\n")
        self.process.stdin.flush()
        return json.loads(readline_with_timeout(self.process.stdout, 5))

    def wait_blocked(self) -> None:
        assert readline_with_timeout(self.process.stdout, 5).strip() == "CALL calls=1"
        assert readline_with_timeout(self.process.stdout, 5).strip() == "STEP_BLOCKED"

    def stop(self) -> dict:
        try:
            out, err = self.process.communicate(input="\n", timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            out, err = self.process.communicate()
            raise AssertionError(f"Rust host did not stop: {out}\n{err}") from None
        assert self.process.returncode == 0, err
        assert "HOST_RECEIPT calls=1" in out, out
        return json.loads(out.splitlines()[0])

    def abort(self) -> str:
        if self.process.poll() is None:
            self.process.kill()
        out, _ = self.process.communicate()
        return out


def _gated_python_host(address, route, root, calls, step_entered, release, ready, overrides):
    runtime = cc.Runtime()
    if os.name != "nt":
        runtime.set_local_endpoint(root=root)
    runtime.set_server(server_id=address.removeprefix("ipc://"), **overrides)

    class GatedPayloads(PythonHost):
        def echo(self, payload):
            inspect(payload, True)
            with calls.get_lock():
                calls.value += 1
            output = build_payload(True)
            self.outputs.append(output)
            yield output
            step_entered.set()
            assert release.wait(30)
            yield output

    resource = GatedPayloads(True)
    try:
        runtime.register(Portable, resource, name=route)
        ready.send(runtime.server_address())
        ready.close()
        release.wait()
    finally:
        runtime.shutdown(timeout=5)
        resource.close()


class PythonHostProcess:
    """A native-architecture spawn child killed while its second step is running."""

    def __init__(self, address, route, root, overrides):
        context = multiprocessing.get_context("spawn")
        ready, child = context.Pipe(duplex=False)
        self.calls = context.Value("I", 0)
        self.step_entered = context.Event()
        self.release = context.Event()
        self.process = context.Process(
            target=_gated_python_host,
            args=(address, route, str(root), self.calls, self.step_entered,
                  self.release, child, overrides),
        )
        self.process.start()
        child.close()
        try:
            assert ready.poll(15), "Python resource did not become ready"
            assert ready.recv() == address
        except BaseException:
            self.abort()
            raise
        finally:
            ready.close()

    def wait_blocked(self) -> None:
        assert self.step_entered.wait(5)
        assert self.calls.value == 1

    def abort(self) -> None:
        if self.process.is_alive():
            self.process.kill()
        self.process.join(5)
        assert not self.process.is_alive()
