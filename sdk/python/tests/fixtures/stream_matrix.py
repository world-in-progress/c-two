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
CHUNK_SIZE = 64 * 1024


@dataclass(frozen=True)
class TransportCase:
    name: str
    large: bool
    prefer_shm: bool
    shm_budget: int
    reassembly: str = "none"

    def overrides(self) -> dict[str, int | float | bool]:
        return {
            "pool_enabled": False,
            "pool_prewarm_segments": 0,
            "pool_min_retained_segments": 0,
            "spill_threshold": 1.0,
            "chunk_size": CHUNK_SIZE,
            "shm_backing_budget_bytes": self.shm_budget,
            "file_backing_budget_bytes": 8 * 1024 * 1024 if self.reassembly == "file" else 0,
            "live_reassembly_budget_bytes": 8 * 1024 * 1024,
        }


TRANSPORT_CASES = (
    TransportCase("inline", False, True, 8 * 1024 * 1024),
    TransportCase("shm", True, True, 8 * 1024 * 1024),
    TransportCase("no-shm-inline", False, True, 0),
    TransportCase("chunked-file-input", True, True, 0, "file"),
    TransportCase("chunked-shm-input", True, False, 8 * 1024 * 1024, "shm"),
)


def process_environment(overrides=None, ipc_root=None) -> dict[str, str]:
    environment = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": ""}
    if ipc_root is not None and os.name != "nt":
        environment["C2_IPC_ROOT"] = str(ipc_root)
    for key, value in (overrides or {}).items():
        environment[f"C2_IPC_{key.upper()}"] = str(value).lower()
    return environment


def assert_transport_stats(case, encoded_len, input_stats, server_stats, client_stats) -> None:
    """Section 9 matrix: actual backing in both directions, peaks and refunds."""
    outgoing = client_stats["runtime_outgoing"]["cells"]
    server = server_stats["server"]["cells"]
    at_input = input_stats["cells"]
    if case.large and case.shm_budget == 0:
        # Actual ITEM_DATA boundaries are already proved by
        # core/transport/c2-server/src/stream_tests.rs::
        # inline_encodes_each_granted_chunk_directly_from_the_domain_item.
        # This matrix checks the complete item and truly unavailable SHM.
        assert encoded_len > case.overrides()["chunk_size"]
        assert outgoing["shm"]["peak_bytes"] == 0
        assert server["shm"]["peak_bytes"] == 0
    for cells in (outgoing, server, at_input):
        for kind in ("shm", "file", "reassembly"):
            assert cells[kind]["used_bytes"] == 0, (case.name, kind, cells)
            assert cells[kind]["peak_bytes"] <= cells[kind]["limit_bytes"]
        assert cells["shm"]["limit_bytes"] == case.shm_budget
    # Stream ITEM_DATA uses the B-reserved Vec, not unary reassembly.
    assert outgoing["file"]["peak_bytes"] == 0
    assert outgoing["reassembly"]["peak_bytes"] == 0
    request_shm = case.large and case.prefer_shm and case.shm_budget > 0
    if request_shm:
        assert outgoing["shm"]["peak_bytes"] >= encoded_len
    else:
        assert outgoing["shm"]["peak_bytes"] == 0
    for cells in (at_input, server):
        if case.reassembly != "none":
            assert cells["reassembly"]["peak_bytes"] >= encoded_len > CHUNK_SIZE
        else:
            assert cells["reassembly"]["peak_bytes"] == 0
        if case.reassembly == "file":
            assert cells["file"]["peak_bytes"] >= encoded_len
        else:
            assert cells["file"]["peak_bytes"] == 0
    if case.reassembly == "shm":
        assert at_input["shm"]["peak_bytes"] >= encoded_len
    else:
        assert at_input["shm"]["peak_bytes"] == 0
    if request_shm or case.reassembly == "shm":
        assert server["shm"]["peak_bytes"] >= encoded_len
    else:
        assert server["shm"]["peak_bytes"] == 0
    budget = client_stats["payload_buffer"]
    assert budget["used_bytes"] == 0
    assert budget["peak_bytes"] >= encoded_len


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

    def client(self, address: str, route: str, shm: bool, large: bool, hold: bool,
               *, ipc_overrides=None, ipc_root=None) -> dict:
        result = subprocess.run(
            [str(self.binary), "client", address, route, "shm" if shm else "no-shm",
             "large" if large else "inline", "hold" if hold else "materialized"],
            cwd=self.root, env=process_environment(ipc_overrides, ipc_root),
            capture_output=True, text=True, timeout=60, check=False,
        )
        assert result.returncode == 0, result.stderr
        receipt = json.loads(result.stdout.strip())
        assert receipt["items"] == 2 and receipt["path"] == "direct_ipc"
        return receipt

    def host(self, address: str, route: str, shm: bool, large: bool,
             *, ipc_overrides=None, gated=False, ipc_root=None) -> RustHostProcess:
        return RustHostProcess(self, address, route, shm, large, ipc_overrides, gated, ipc_root)


class RustHostProcess:
    def __init__(self, harness: RustHarness, address: str, route: str, shm: bool, large: bool,
                 ipc_overrides, gated, ipc_root) -> None:
        self.process = subprocess.Popen(
            [str(harness.binary), "gated-host" if gated else "host", address, route, "shm" if shm else "no-shm",
             "large" if large else "inline"],
            cwd=harness.root, env=process_environment(ipc_overrides, ipc_root),
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
    runtime.set_transport_policy(shm_threshold=64 * 1024)
    runtime.set_server(server_id=address.removeprefix("ipc://"), ipc_overrides=overrides)

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
