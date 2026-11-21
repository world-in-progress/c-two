"""Rust <-> Python portable FastDB publish/subscribe end to end (ADR 0005).

One Python ``@cc.crm`` contract (a ``@cc.read`` unary plus two ``@cc.topic``
FastDB topics, and a 0-method topic-only contract) is exported as one
``c-two.contract.v2`` descriptor. ``c2-codegen`` compiles it for Rust and
Python, and the generated code drives the real cross-process paths:

- Rust host -> Python subscribers over direct IPC 17 (natural ``@cc.topic``
  members with ``cc.hold``, plus the generated ``ContractClient`` subscribe
  methods) and through one real ``c3 relay`` with single-upstream fan-out.
- Python host -> Rust client over direct IPC 17, including the topic-only
  contract with no unary method.
"""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType
from typing import Any, Iterator

import pytest

import c_two as cc
from c_two.error import CallCancelled, SubscriptionLagged
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload
from tests.fixtures.process_control import readline_with_timeout

REPOSITORY = Path(__file__).resolve().parents[4]
RUST_HARNESS_SOURCE = (
    REPOSITORY
    / "sdk/python/tests/fixtures/pubsub_interop_rust.rs"
)
TARGET_DIR = REPOSITORY / os.environ.get("CARGO_TARGET_DIR", "core/target")
HARNESS_NAME = "c-two-pubsub-interop-harness"

pytestmark = pytest.mark.timeout(300)

# Inline record.v1 FastDB spec: the test never depends on a sibling fastdb
# checkout, so it runs from any worktree and in CI as-is.
SPEC: dict[str, Any] = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one", "type": {"kind": "str", "nullable": True}},
    ],
    "components": [],
}


@cc.crm(namespace="test.pubsub-crosslang", version="0.1.0")
class Sensor:
    @cc.read
    def ping(self) -> None: ...

    @cc.topic
    @cc.transfer(output=SPEC)
    def state(self) -> Payload: ...

    @cc.topic
    @cc.transfer(output=SPEC)
    def ticks(self) -> Payload: ...


@cc.crm(namespace="test.pubsub-crosslang.solo", version="0.1.0")
class Beacon:
    @cc.topic
    @cc.transfer(output=SPEC)
    def field(self) -> Payload: ...


class SensorImpl:
    def ping(self) -> None:
        return None


class BeaconImpl:
    pass


def _unique(prefix: str) -> str:
    return f"{prefix}-{os.getpid()}-{uuid.uuid4().hex[:8]}"


def _payload(text: str) -> Payload:
    spec = CompiledSpec.compile(json.dumps(SPEC, separators=(",", ":")).encode())
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


def _publish(publisher: Any, text: str) -> int:
    payload = _payload(text)
    try:
        return publisher.publish(payload)
    finally:
        payload.close()


def _payload_text(payload: Payload) -> str:
    with payload.entry_view(0) as entry:
        with entry.at(0) as item:
            with item.acquire() as acquired:
                return acquired.str()


def _close(payload: Payload) -> None:
    try:
        payload.close()
    except Exception:
        pass


def _write_artifacts(root: Path, artifact_set: Any) -> list[dict[str, Any]]:
    inventory: list[dict[str, Any]] = []
    for artifact in artifact_set.artifacts:
        actual = hashlib.sha256(artifact.bytes).hexdigest()
        assert actual == artifact.sha256
        destination = root / artifact.relative_path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(artifact.bytes)
        inventory.append(
            {
                "path": artifact.relative_path,
                "sha256": artifact.sha256,
                "size": len(artifact.bytes),
                "owner": artifact.owner,
            }
        )
    assert inventory == sorted(inventory, key=lambda item: item["path"])
    return inventory


def _import_generated_python(generated_root: Path, module_name: str) -> ModuleType:
    module_path = generated_root / "python/c_two_contract.py"
    sys.modules.pop(module_name, None)
    spec = importlib.util.spec_from_file_location(module_name, module_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


@dataclass(frozen=True)
class PubsubHarness:
    root: Path
    binary: Path
    descriptor_mixed: str
    descriptor_solo: str
    generated_mixed: ModuleType
    generated_solo: ModuleType


@pytest.fixture(scope="module")
def pubsub_harness(tmp_path_factory: Any) -> Iterator[PubsubHarness]:
    """Compile the shared descriptor for both languages and build the harness.

    The Rust harness embeds the generated Rust modules of both contracts; the
    generated Python modules are imported for the client-side checks. The
    artifact receipt binds this run to exact bytes, mirroring the portable
    payload cross-language proof.
    """
    work = tmp_path_factory.mktemp("pubsub-xlang")

    descriptor_mixed = cc.contract.export_descriptor(Sensor)
    descriptor_solo = cc.contract.export_descriptor(Beacon)
    mixed_value = json.loads(descriptor_mixed)
    solo_value = json.loads(descriptor_solo)
    assert [method["name"] for method in mixed_value["methods"]] == ["ping"]
    assert [topic["name"] for topic in mixed_value["topics"]] == ["state", "ticks"]
    assert solo_value["methods"] == []
    assert [topic["name"] for topic in solo_value["topics"]] == ["field"]

    inventories: dict[str, list[dict[str, Any]]] = {}
    for name, descriptor in (
        ("mixed", descriptor_mixed),
        ("solo", descriptor_solo),
    ):
        encoded = descriptor.encode("utf-8")
        for target in ("rust", "python"):
            artifacts = cc.contract.compile_artifacts(encoded, target=target)
            repeat = cc.contract.compile_artifacts(encoded, target=target)
            assert artifacts == repeat, f"{name}/{target} codegen is not deterministic"
            tree = work / f"generated-{name}" if target == "rust" else work / f"generated-python-{name}"
            inventories[f"{name}-{target}"] = _write_artifacts(tree, artifacts)
    print(
        "ARTIFACT_RECEIPT "
        + json.dumps(
            {
                "mixed_descriptor_sha256": hashlib.sha256(
                    descriptor_mixed.encode("utf-8"),
                ).hexdigest(),
                "solo_descriptor_sha256": hashlib.sha256(
                    descriptor_solo.encode("utf-8"),
                ).hexdigest(),
                "artifacts": inventories,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )

    generated_mixed = _import_generated_python(
        work / "generated-python-mixed",
        "c_two_pubsub_xlang_generated_mixed",
    )
    generated_solo = _import_generated_python(
        work / "generated-python-solo",
        "c_two_pubsub_xlang_generated_solo",
    )

    source = work / "src"
    source.mkdir()
    shutil.copyfile(RUST_HARNESS_SOURCE, source / "main.rs")

    manifest = f"""\
[package]
name = "{HARNESS_NAME}"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ version = "0.1.0", path = {json.dumps(str(REPOSITORY / "sdk/rust"))} }}
fastdb = "=0.2.4"
serde_json = "1"
"""
    (work / "Cargo.toml").write_text(manifest, encoding="utf-8")

    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(TARGET_DIR)
    completed = subprocess.run(
        ["cargo", "build", "--quiet", "--manifest-path", str(work / "Cargo.toml")],
        cwd=work,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert completed.returncode == 0, (
        "generated Rust pubsub harness failed to compile\n"
        f"stdout:\n{completed.stdout}\n"
        f"stderr:\n{completed.stderr}"
    )
    binary = TARGET_DIR / "debug" / (
        f"{HARNESS_NAME}.exe" if os.name == "nt" else HARNESS_NAME
    )
    assert binary.is_file()
    yield PubsubHarness(
        root=work,
        binary=binary,
        descriptor_mixed=descriptor_mixed,
        descriptor_solo=descriptor_solo,
        generated_mixed=generated_mixed,
        generated_solo=generated_solo,
    )


class HarnessProcess:
    """One harness subprocess speaking line-oriented JSON events."""

    def __init__(self, binary: Path, args: list[str]) -> None:
        environment = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": ""}
        self._process = subprocess.Popen(
            [str(binary), *args],
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )

    def read(self) -> dict[str, Any]:
        assert self._process.stdout is not None
        line = readline_with_timeout(self._process.stdout, 30)
        assert line, f"harness produced no event line; stderr:\n{self._stderr()}"
        return json.loads(line)

    def send(self, command: str) -> None:
        assert self._process.stdin is not None
        self._process.stdin.write(command + "\n")
        self._process.stdin.flush()

    def request(self, command: str) -> dict[str, Any]:
        self.send(command)
        return self.read()

    def _stderr(self) -> str:
        if self._process.poll() is not None and self._process.stderr is not None:
            return self._process.stderr.read()
        return "<process still running>"

    def wait(self, marker: str) -> None:
        # The harness prints its exit marker as the final line before it
        # returns; read it through the buffered stream because communicate()
        # drains the raw fd and would miss lines read ahead into the wrapper.
        line = readline_with_timeout(self._process.stdout, 30)
        assert line.strip() == marker, f"expected {marker!r}, got {line!r}"
        try:
            stdout, stderr = self._process.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            self.abort()
            raise AssertionError(f"harness did not exit after {marker!r}")
        assert self._process.returncode == 0, (
            f"harness exited with {self._process.returncode}\n"
            f"stdout:\n{stdout}\nstderr:\n{stderr}"
        )
        assert not stdout.strip(), f"unexpected trailing harness output:\n{stdout}"

    def stop(self, marker: str) -> None:
        """Ask the harness to quit, then verify its clean exit marker."""
        if self._process.poll() is None:
            try:
                event = self.request("quit")
                assert event["event"] == "stopped", event
            except BaseException:
                self.abort()
                raise
        self.wait(marker)

    def abort(self) -> None:
        if self._process.poll() is None:
            self._process.kill()
        self._process.communicate(timeout=10)


# ------------------------------------------------------------------
# Contract compilation and hash consistency
# ------------------------------------------------------------------


@pytest.mark.timeout(600)
class TestContractCompilation:

    def test_descriptor_topics_and_generated_bindings(self, pubsub_harness: PubsubHarness) -> None:
        mixed = json.loads(pubsub_harness.descriptor_mixed)
        assert mixed["schema"] == "c-two.contract.v2"
        assert [method["name"] for method in mixed["methods"]] == ["ping"]
        assert mixed["methods"][0]["access"] == "read"
        assert [topic["name"] for topic in mixed["topics"]] == ["state", "ticks"]
        for topic in mixed["topics"]:
            assert topic["output"] == {"kind": "payload"}
            assert topic["binding"]["kind"] == "fastdb"
            assert topic["binding"]["spec"]["profile"] == "record.v1"
        solo = json.loads(pubsub_harness.descriptor_solo)
        assert solo["methods"] == []
        assert [topic["name"] for topic in solo["topics"]] == ["field"]

        generated = pubsub_harness.generated_mixed
        assert [(m.index, m.name, m.access) for m in generated.METHODS] == [
            (0, "ping", "read"),
        ]
        assert [(t.index, t.name) for t in generated.TOPICS] == [
            (0, "state"),
            (1, "ticks"),
        ]
        assert generated.DESCRIPTOR_SHA256 == hashlib.sha256(
            pubsub_harness.descriptor_mixed.encode("utf-8"),
        ).hexdigest()
        assert generated.ABI_HASH == mixed["fingerprints"]["abi_hash"]
        assert generated.SIGNATURE_HASH == mixed["fingerprints"]["signature_hash"]

        solo_generated = pubsub_harness.generated_solo
        assert solo_generated.METHODS == ()
        assert [(t.index, t.name) for t in solo_generated.TOPICS] == [(0, "field")]


# ------------------------------------------------------------------
# Direction 1: Rust host -> Python subscribers over direct IPC 17
# ------------------------------------------------------------------


class TestRustHostDirectIpc:

    def test_natural_topic_client_covers_the_full_direct_ipc_surface(
        self, pubsub_harness: PubsubHarness,
    ) -> None:
        route = _unique("pubsub-xlang-rust")
        address = f"ipc://pubsub_xlang_rust_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        host = HarnessProcess(pubsub_harness.binary, ["host", address, route, "-"])
        client_runtime = cc.Runtime()
        try:
            assert host.read() == {"event": "ready", "address": address}

            proxy = client_runtime.connect(Sensor, name=route, address=address)
            # Atomic initial state: `latest` reads the frame committed before
            # the route opened, at seq 1 with nothing dropped.
            state = proxy.state()
            assert (state.topic_name, state.start_seq) == ("state", 1)
            initial = next(state)
            assert _payload_text(initial.value) == "init"
            assert (initial.seq, initial.dropped) == (1, 0)

            # Live increments with start=next attach after the current head.
            following = proxy.client.subscribe("state", start="next")
            assert following.start_seq == 2
            published = host.request("publish state n1")
            assert published == {"event": "published", "topic": "state", "seq": 2}
            live = next(following)
            assert _payload_text(live.value) == "n1"
            assert (live.seq, live.dropped) == (2, 0)

            # Unary calls stay usable on the same open connection.
            assert proxy.ping() is None

            # cc.hold on the natural member: retained local copies survive the
            # subscription's terminal state and die only on explicit release.
            held_stream = cc.hold(proxy.state)()
            assert host.request("publish state held-frame")["seq"] == 3
            replay = next(held_stream)
            fresh = next(held_stream)
            assert isinstance(fresh.value, cc.HeldResult)
            assert (replay.seq, _payload_text(replay.value.value)) == (2, "n1")
            assert (fresh.seq, _payload_text(fresh.value.value)) == (3, "held-frame")
            assert held_stream.close(timeout=5.0)["completed"] is True
            assert _payload_text(fresh.value.value) == "held-frame"
            fresh.value.release()
            with pytest.raises(RuntimeError):
                _ = fresh.value.value
            _close(replay.value.value)
            _close(initial.value)
            _close(live.value)
            assert state.close(timeout=5.0)["completed"] is True
            assert following.close(timeout=5.0)["completed"] is True

            # Lag policies on the capacity-2 ticks topic: skip reports the
            # exact dropped count and lands on the newest retained frame;
            # error terminates the subscription with SubscriptionLagged (720).
            skip = proxy.ticks()
            strict = proxy.client.subscribe("ticks", on_lag="error")
            assert host.request("publish ticks v1")["seq"] == 1
            assert next(skip).seq == 1
            assert next(strict).seq == 1
            assert host.request("publish ticks v2")["seq"] == 2
            # Let both client channels buffer seq 2 and grant demand, then
            # one back-to-back host-side burst evicts the next expected frame.
            time.sleep(0.05)
            assert host.request("burst ticks v 3 7") == {
                "event": "burst", "topic": "ticks", "from": 3, "to": 7, "last_seq": 7,
            }
            last_seq = 1
            saw_skip = False
            for _ in range(8):
                delivery = next(skip)
                if delivery.dropped:
                    # Exact drop count against the last in-order frame, and a
                    # complete payload matching the frame it landed on.
                    assert delivery.dropped == delivery.seq - (last_seq + 1)
                    assert _payload_text(delivery.value) == f"v{delivery.seq}"
                    saw_skip = True
                    break
                assert delivery.dropped == 0
                last_seq = delivery.seq
            assert saw_skip, "skip subscriber must report dropped > 0"
            with pytest.raises(SubscriptionLagged) as lagged:
                for _ in strict:
                    pass
            assert lagged.value.code == 720
            assert lagged.value.details["topic"] == "ticks"
            assert skip.close(timeout=5.0)["completed"] is True
            assert strict.close(timeout=5.0)["completed"] is True

            # Unregister ends the live subscription with 718 naming the cause;
            # the same CRM's next member call follows the re-registered route
            # with a fresh sequence space restarting at 1.
            watching = proxy.state()
            assert next(watching).seq == 3
            assert host.request("unregister") == {"event": "unregistered"}
            with pytest.raises(CallCancelled) as cancelled:
                next(watching)
            assert cancelled.value.code == 718
            assert cancelled.value.details["cause"] == "route_unregistered"
            assert watching.close(timeout=5.0)["completed"] is True

            assert host.request("reregister r2-init") == {"event": "reregistered"}
            refollow = proxy.state()
            assert refollow.start_seq == 1
            reboot = next(refollow)
            assert (_payload_text(reboot.value), reboot.seq) == ("r2-init", 1)
            assert host.request("publish state post-reboot")["seq"] == 2
            after = next(refollow)
            assert (_payload_text(after.value), after.seq) == ("post-reboot", 2)
            assert refollow.close(timeout=5.0)["completed"] is True
            cc.close(proxy)
        finally:
            client_runtime.shutdown(timeout=5.0)
            host.stop("OK rust-host")

    def test_generated_contract_client_subscribes_and_holds(
        self, pubsub_harness: PubsubHarness,
    ) -> None:
        route = _unique("pubsub-xlang-gen")
        address = f"ipc://pubsub_xlang_gen_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        host = HarnessProcess(pubsub_harness.binary, ["host", address, route, "-"])
        client_runtime = cc.Runtime()
        try:
            assert host.read() == {"event": "ready", "address": address}
            proxy = client_runtime.connect(Sensor, name=route, address=address)
            client = pubsub_harness.generated_mixed.ContractClient(proxy.client)

            # The generated unary method runs beside the subscription.
            assert client.method_0_ping() is None

            subscription = client.subscribe_topic_0_state()
            assert (subscription.topic_name, subscription.start_seq) == ("state", 1)
            initial = next(subscription)
            assert (_payload_text(initial.value), initial.seq) == ("init", 1)
            assert host.request("publish state gen-frame")["seq"] == 2
            generated = next(subscription)
            assert (_payload_text(generated.value), generated.seq) == ("gen-frame", 2)

            held_subscription = client.hold_subscribe_topic_0_state()
            assert host.request("publish state gen-held")["seq"] == 3
            replay = next(held_subscription)
            fresh = next(held_subscription)
            assert isinstance(fresh.value, cc.HeldResult)
            assert (replay.seq, _payload_text(replay.value.value)) == (2, "gen-frame")
            assert (fresh.seq, _payload_text(fresh.value.value)) == (3, "gen-held")
            assert held_subscription.close(timeout=5.0)["completed"] is True
            assert _payload_text(fresh.value.value) == "gen-held"
            fresh.value.release()
            with pytest.raises(RuntimeError):
                _ = fresh.value.value
            _close(replay.value.value)
            _close(initial.value)
            _close(generated.value)
            assert subscription.close(timeout=5.0)["completed"] is True

            # The second topic's generated symbol terminates with 720 under
            # on_lag=error once the capacity-2 ring evicts its next frame.
            strict = client.subscribe_topic_1_ticks(on_lag="error")
            assert host.request("publish ticks v1")["seq"] == 1
            assert next(strict).seq == 1
            assert host.request("publish ticks v2")["seq"] == 2
            time.sleep(0.05)
            host.request("burst ticks v 3 7")
            with pytest.raises(SubscriptionLagged) as lagged:
                for _ in strict:
                    pass
            assert lagged.value.code == 720
            assert strict.close(timeout=5.0)["completed"] is True
            cc.close(proxy)
        finally:
            client_runtime.shutdown(timeout=5.0)
            host.stop("OK rust-host")

    def test_topic_only_rust_host_serves_python_subscriber(
        self, pubsub_harness: PubsubHarness,
    ) -> None:
        route = _unique("pubsub-xlang-solo")
        address = f"ipc://pubsub_xlang_solo_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        host = HarnessProcess(pubsub_harness.binary, ["solo-host", address, route])
        client_runtime = cc.Runtime()
        try:
            assert host.read() == {"event": "ready", "address": address}
            proxy = client_runtime.connect(Beacon, name=route, address=address)
            field = proxy.field()
            assert (field.topic_name, field.start_seq) == ("field", 1)
            boot = next(field)
            assert (_payload_text(boot.value), boot.seq) == ("boot", 1)
            assert host.request("publish field reading-2")["seq"] == 2
            second = next(field)
            assert (_payload_text(second.value), second.seq, second.dropped) == (
                "reading-2", 2, 0,
            )
            assert field.close(timeout=5.0)["completed"] is True
            assert host.request("active") == {"event": "active", "count": 0}
            cc.close(proxy)
        finally:
            client_runtime.shutdown(timeout=5.0)
            host.stop("OK rust-solo-host")


# ------------------------------------------------------------------
# Direction 1: Rust host -> two Python subscribers through one c3 relay
# ------------------------------------------------------------------


def _select_c3_binary(monkeypatch: pytest.MonkeyPatch) -> None:
    """Let the shared relay fixture see a c3 that is only on PATH."""
    if os.environ.get("CTWO_TEST_C3_BIN"):
        return
    found = shutil.which("c3")
    if found:
        monkeypatch.setenv("CTWO_TEST_C3_BIN", found)


class TestRustHostRelay:

    def test_two_python_subscribers_share_one_upstream_through_c3_relay(
        self,
        pubsub_harness: PubsubHarness,
        start_c3_relay: Any,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
    ) -> None:
        _select_c3_binary(monkeypatch)
        monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
        if os.name != "nt":
            # One private endpoint root shared by the relay and the Rust host.
            ipc_root = tmp_path / "ipc-root"
            ipc_root.mkdir(mode=0o700)
            monkeypatch.setenv("C2_IPC_ROOT", str(ipc_root))
        relay = start_c3_relay()

        route = _unique("pubsub-xlang-relay")
        address = f"ipc://pubsub_xlang_relay_{os.getpid()}_{uuid.uuid4().hex[:8]}"
        host = HarnessProcess(pubsub_harness.binary, ["host", address, route, relay.url])
        first = cc.Runtime()
        second = cc.Runtime()
        try:
            assert host.read() == {"event": "ready", "address": address}
            proxy_a = first.connect(Sensor, name=route, address=relay.url)
            proxy_b = second.connect(Sensor, name=route, address=relay.url)
            assert proxy_a.client.observed_path == "explicit_relay"
            assert proxy_b.client.observed_path == "explicit_relay"

            subscriber_a = proxy_a.state()
            subscriber_b = proxy_b.client.subscribe("state", start="next")
            assert subscriber_a.start_seq == 1
            initial = next(subscriber_a)
            assert (_payload_text(initial.value), initial.seq) == ("init", 1)

            assert host.request("publish state relay-frame")["seq"] == 2
            frame_a = next(subscriber_a)
            frame_b = next(subscriber_b)
            for frame in (frame_a, frame_b):
                assert _payload_text(frame.value) == "relay-frame"
                assert (frame.seq, frame.dropped) == (2, 0)
            # The two subscribers received byte-identical complete payloads.
            assert frame_a.value.binary_bytes() == frame_b.value.binary_bytes()
            _close(initial.value)
            _close(frame_a.value)
            _close(frame_b.value)

            # Single-relay fan-out: both HTTP subscribers share one upstream
            # IPC subscription on the Rust host.
            assert host.request("active") == {"event": "active", "count": 1}

            assert subscriber_a.close(timeout=5.0)["completed"] is True
            assert subscriber_b.close(timeout=5.0)["completed"] is True
            cc.close(proxy_a)
            cc.close(proxy_b)
        finally:
            first.shutdown(timeout=5.0)
            second.shutdown(timeout=5.0)
            host.stop("OK rust-host")


# ------------------------------------------------------------------
# Direction 2: Python host -> Rust client over direct IPC 17
# ------------------------------------------------------------------


class TestPythonHostDirectIpc:

    def test_rust_client_verifies_the_full_direct_ipc_surface(
        self, pubsub_harness: PubsubHarness,
    ) -> None:
        server = cc.Runtime()
        route = _unique("pubsub-xlang-python")
        initial = _payload("init")
        try:
            registration = server.register(
                Sensor,
                SensorImpl(),
                name=route,
                topic_capacity={"state": 4, "ticks": 2},
                initial_topics={"state": initial},
            )
        finally:
            initial.close()
        address = server.server_address()
        assert address is not None
        client = HarnessProcess(pubsub_harness.binary, ["client", address, route])
        try:
            # Atomic initial state + held delivery + latest/next starts.
            assert client.read() == {"event": "initial-ok"}
            assert _publish(registration.publisher("state"), "held-value") == 2
            assert client.read() == {"event": "next-attached"}
            assert _publish(registration.publisher("state"), "third") == 3
            assert client.read() == {"event": "held-ok"}

            # Lag policies: skip with the exact dropped count, error 720.
            assert client.read() == {"event": "lag-attached"}
            ticks = registration.publisher("ticks")
            assert _publish(ticks, "v1") == 1
            assert client.read() == {"event": "ticks-first"}
            assert _publish(ticks, "v2") == 2
            time.sleep(0.1)
            for beat in range(3, 8):
                assert _publish(ticks, f"v{beat}") == beat
            assert client.read() == {"event": "lag-ok"}

            # Unregister ends the Rust-side subscription with 718 and the same
            # client follows the re-registered route with a fresh seq space.
            assert client.read() == {"event": "live-attached"}
            server.unregister(route)
            reboot = _payload("r2-init")
            try:
                second = server.register(
                    Sensor,
                    SensorImpl(),
                    name=route,
                    initial_topics={"state": reboot},
                )
            finally:
                reboot.close()
            client.send("reregistered")
            assert client.read() == {"event": "refollow-ok"}
            assert _publish(second.publisher("state"), "post-reboot") == 2
            assert client.read() == {"event": "done"}
            client.wait("OK rust-client")
        finally:
            client.abort()
            server.shutdown(timeout=5.0)

    def test_topic_only_python_host_serves_rust_subscriber(
        self, pubsub_harness: PubsubHarness,
    ) -> None:
        server = cc.Runtime()
        route = _unique("pubsub-xlang-pysolo")
        boot = _payload("boot")
        try:
            registration = server.register(
                Beacon,
                BeaconImpl(),
                name=route,
                initial_topics={"field": boot},
            )
        finally:
            boot.close()
        address = server.server_address()
        assert address is not None
        client = HarnessProcess(pubsub_harness.binary, ["solo-client", address, route])
        try:
            assert client.read() == {"event": "solo-initial"}
            assert _publish(registration.publisher("field"), "reading-2") == 2
            assert client.read() == {"event": "done"}
            client.wait("OK rust-solo-client")
        finally:
            client.abort()
            server.shutdown(timeout=5.0)
