"""Private process protocol for the generated Node idle-eviction gates."""
from __future__ import annotations

from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from typing import Any, Callable, Iterator

import c_two as cc
import c_two.endpoint
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

from tests.fixtures.process_control import readline_with_timeout


EVIDENCE_SPEC = {
    "schema": "fastdb.payload.v1", "profile": "record.v1",
    "entries": [{"id": "evidence", "cardinality": "one", "type": {"kind": "str", "nullable": False}}],
    "components": [],
}


@cc.crm(namespace="test.typescript-idle-gates", version="0.1.0")
class IdleGate:
    __cc_rpc_methods__ = ["ping", "items", "echo"]

    @cc.transfer(output=EVIDENCE_SPEC)
    def ping(self) -> Payload: ...

    @cc.stream
    @cc.transfer(output=EVIDENCE_SPEC)
    def items(self) -> Iterator[Payload]: ...

    @cc.transfer(input=EVIDENCE_SPEC, output=EVIDENCE_SPEC)
    def echo(self, payload: Payload) -> Payload: ...


class GateResource:
    """A resource-side barrier and instance evidence, with no transport hooks."""

    def __init__(self) -> None:
        self.identity: dict[str, str] = {}
        self.calls = 0
        self.block = False
        self.entered = threading.Event()
        self.release = threading.Event()
        self.outputs: list[Payload] = []
        self.stream_calls = 0
        self.echo_calls = 0
        self.steps = 0
        self.cleaned = 0
        self.stream_entered = threading.Event()
        self.on_call: Callable[[dict[str, Any]], None] | None = None

    def evidence(self) -> dict[str, Any]:
        return {**self.identity, "calls": self.calls, "stream_calls": self.stream_calls,
                "echo_calls": self.echo_calls, "steps": self.steps, "cleaned": self.cleaned}

    def record(self, method: str) -> None:
        if self.on_call:
            self.on_call({"method": method, **self.evidence()})

    def ping(self) -> Payload:
        self.calls += 1
        self.record("ping")
        evidence = {**self.identity, "calls": self.calls}
        if self.block:
            self.entered.set()
            assert self.release.wait(15), "resource barrier was not released"
        return self.payload(evidence)

    def items(self) -> Iterator[Payload]:
        self.stream_calls += 1
        self.record("items")
        try:
            self.steps += 1
            yield self.payload(self.evidence())
            self.stream_entered.set()
            assert self.release.wait(15), "stream step was not released"
            self.steps += 1
            yield self.payload(self.evidence())
        finally:
            self.cleaned += 1

    def echo(self, payload: Payload) -> Payload:
        with payload.entry_view(0) as entry, entry.at(0) as item, item.acquire() as acquired:
            value = json.loads(acquired.str())
        self.echo_calls += 1
        self.record("echo")
        return self.payload(value)

    def payload(self, evidence: dict[str, Any]) -> Payload:
        spec = CompiledSpec.compile(json.dumps(EVIDENCE_SPEC).encode())
        builder = Builder.create(spec)
        try:
            builder.entry_begin(0, 1).value_str(json.dumps(evidence))
            plan = builder.freeze()
        finally:
            builder.close()
            spec.close()
        try:
            payload = plan.execute(BuildPolicy.ALLOW_STAGING).payload
        finally:
            plan.close()
        self.outputs.append(payload)
        return payload

    def close(self) -> None:
        for payload in self.outputs:
            payload.close()


@contextmanager
def node_gate_client(
    project: tuple[Path, str], work: Path, address: str, ipc_root: str | None,
    *, mode: str = "relay-local", portable: bool = False,
    call_timeout_ms: int | None = None,
    expected_server_identity: dict[str, str] | None = None,
    record_cleanup: bool = False,
) -> Iterator[Callable[..., dict[str, Any]]]:
    root, node = project
    fixture = Path(__file__).with_suffix(".mjs")
    config = work / "node-gates.json"
    module = root / "dist/gates/typescript/c_two_contract.js"
    config.write_text(json.dumps({
        "address": address, "ipcRoot": ipc_root, "mode": mode,
        "portable": portable, "callTimeoutMs": call_timeout_ms,
        "expectedServerIdentity": expected_server_identity,
        "managerModule": str(module if portable else root / "dist/manager/typescript/c_two_contract.js"),
        "builderModule": str(module if portable else root / "dist/builder/typescript/c_two_contract.js"),
        "payloadModule": str(next((root / "dist/gates/typescript/payloads").glob("*/fastdb_payload_*.js"))) if portable else None,
    }), encoding="utf-8")
    # Resolve the native package beside the generated modules without editing
    # the existing persistent-IPC fixture or any shared test helper.
    script = root / fixture.name
    script.write_bytes(fixture.read_bytes())
    with (work / "node-gates.stderr").open("w", encoding="utf-8") as errors:
        process = subprocess.Popen(
            [node, str(script), str(config)], cwd=root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors,
            text=True, encoding="utf-8", bufsize=1,
        )
        assert process.stdin is not None and process.stdout is not None

        def request(op: str = "call", route: str = "manager", **arguments: Any) -> dict[str, Any]:
            command = {"op": op, "route": route, **arguments}
            process.stdin.write(json.dumps(command) + "\n")
            process.stdin.flush()
            line = readline_with_timeout(process.stdout, 20)
            assert line, f"Node exited: {work}/node-gates.stderr"
            with (work / "node-gates.jsonl").open("a", encoding="utf-8") as log:
                log.write(json.dumps(command) + "\n" + line)
            return json.loads(line)

        try:
            assert json.loads(readline_with_timeout(process.stdout, 20)) == {"ready": True}
            yield request
        finally:
            cleaned = None
            try:
                if process.poll() is None:
                    cleaned = request("exit")
                    assert cleaned["ok"] and cleaned["stats"]["used_operations"] == "0", cleaned
                    process.stdin.close()
                    assert process.wait(timeout=10) == 0
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                process.stdin.close()
                process.stdout.close()
                if record_cleanup:
                    (work / "node-gates-cleanup.json").write_text(json.dumps({
                        "exit_code": process.returncode,
                        "stats": cleaned["stats"] if cleaned is not None else None,
                    }, indent=2), encoding="utf-8")


class AcceptedConnections:
    """Server-side physical facts; IDs belong to this source, never to Node."""

    def __init__(self, events) -> None:
        self.events = events
        self.opened: dict[int, Any] = {}
        self.closed: dict[int, str] = {}
        self.ended = False

    def consume(self, event) -> None:
        if event.side != "server":
            return
        if event.kind == "opened":
            self.opened[event.connection_id] = event.value
        else:
            self.closed[event.connection_id] = event.value.reason

    def snapshot(self) -> dict[str, Any]:
        while not self.ended:
            try:
                event = self.events.recv(0.0)
            except StopIteration:
                self.ended = True
                break
            if event is None:
                break
            self.consume(event)
        return {
            "opened": list(self.opened), "closed": dict(self.closed),
            "active": [key for key, value in self.opened.items() if value.snapshot() is None],
            "source_ended": self.ended,
        }

    def wait_closed(self, candidates: set[int]) -> int:
        deadline = time.monotonic() + 8
        while True:
            self.snapshot()
            closed = candidates.intersection(self.closed)
            if closed:
                return next(iter(closed))
            remaining = deadline - time.monotonic()
            assert remaining > 0, (candidates, self.snapshot())
            event = self.events.recv(remaining)
            assert event is not None, (candidates, self.snapshot())
            self.consume(event)


class GateProcess:
    """An owned resource process, with exact credential cleanup after real EOF."""

    def __init__(self, work: Path, ipc_root: str | None, server_id: str, relay: str | None, label: str,
                 max_payload_size: int | None = None) -> None:
        self.work = work
        self.ipc_root = ipc_root
        self.label = label
        self.cleanup: dict[str, Any] = {}
        config = work / f"resource-{label}.json"
        config.write_text(json.dumps({"server_id": server_id, "relay": relay, "ledger": str(work / "resources.jsonl"),
                                      "max_payload_size": max_payload_size}), encoding="utf-8")
        environment = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": ""}
        environment["PYTHONPATH"] = os.pathsep.join([str(Path(__file__).resolve().parents[2]), environment.get("PYTHONPATH", "")])
        if ipc_root is None:
            environment.pop("C2_IPC_ROOT", None)
        else:
            environment["C2_IPC_ROOT"] = ipc_root
        self.errors = (work / f"resource-{label}.stderr").open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [sys.executable, "-u", str(Path(__file__).resolve()), "--resource", str(config)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors,
            text=True, encoding="utf-8", env=environment, bufsize=1,
        )
        assert self.process.stdout is not None
        try:
            self.ready = self.read()
            assert self.ready["ready"], self.ready
        except BaseException:
            if self.process.poll() is None:
                self.process.kill()
            self.process.wait(timeout=5)
            assert self.process.stdin is not None
            self.process.stdin.close()
            self.process.stdout.close()
            self.errors.close()
            raise

    def read(self) -> dict[str, Any]:
        assert self.process.stdout is not None
        line = readline_with_timeout(self.process.stdout, 20)
        assert line, f"resource exited: {self.work}/resource-{self.label}.stderr"
        with (self.work / f"resource-{self.label}.jsonl").open("a", encoding="utf-8") as log:
            log.write(line)
        return json.loads(line)

    def request(self, op: str = "stats", **arguments: Any) -> dict[str, Any]:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps({"op": op, **arguments}) + "\n")
        self.process.stdin.flush()
        return self.read()

    def __enter__(self):
        return self

    def kill(self) -> None:
        self.process.kill()
        self.process.wait(timeout=5)
        self.reap()

    def reap(self) -> None:
        context = cc.endpoint.local_endpoint_context(**({"root": self.ipc_root} if self.ipc_root is not None else {}))
        credential = self.ready["credential"]
        if credential is not None:
            result = cc.endpoint.reap_endpoint(
                self.ready["address"], cc.endpoint.EndpointCredential.from_json(credential), context=context,
            )
            expected = ("not-applicable",) if os.name == "nt" else ("reaped", "already-absent")
            assert result["status"] in expected, result
            self.cleanup["reap"] = result["status"]
        inspection = cc.endpoint.inspect_endpoint(self.ready["address"], context=context)
        assert inspection["status"] == ("not-applicable" if os.name == "nt" else "absent"), inspection
        self.cleanup["endpoint"] = inspection["status"]

    def __exit__(self, *_):
        try:
            if self.process.poll() is None:
                self.cleanup.update(self.request("stop"))
                assert self.process.wait(timeout=10) == 0
                self.reap()
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
                self.reap()
            self.errors.close()
            assert self.process.stdin is not None and self.process.stdout is not None
            self.process.stdin.close()
            self.process.stdout.close()
            self.cleanup["exit_code"] = self.process.returncode
            (self.work / f"resource-{self.label}-cleanup.json").write_text(json.dumps(self.cleanup, indent=2), encoding="utf-8")


def _resource_main(config: dict[str, Any]) -> None:
    from c_two._native import RuntimeSession

    session = RuntimeSession(server_id=config["server_id"], use_process_relay_anchor=False)
    if config["max_payload_size"] is not None:
        session.set_server_options(config["server_id"], {"max_payload_size": config["max_payload_size"]})
    if config["relay"]:
        session.set_relay_anchor_address(config["relay"])
    server = session.ensure_server_bridge()
    connections = AcceptedConnections(session.connection_events())
    resources = {name: GateResource() for name in ("manager", "builder")}
    ledger_lock = threading.Lock()

    def record(value):
        with ledger_lock, Path(config["ledger"]).open("a", encoding="utf-8") as ledger:
            ledger.write(json.dumps(value) + "\n")

    for name, resource in resources.items():
        resource.on_call = record
        server.register_crm(IdleGate, resource, name=name)
    server.start()
    identity = dict(session.ensure_server())
    for name, resource in resources.items():
        resource.identity = {**identity, "route": name}
    credential = session.server_credential
    print(json.dumps({"ready": True, "identity": identity, "address": session.server_address,
                      "credential": credential.to_json() if credential else None}), flush=True)
    try:
        for line in sys.stdin:
            command = json.loads(line)
            op = command["op"]
            resource = resources[command.get("route", "manager")]
            if op == "arm":
                resource.block = True
            elif op == "entered":
                event = resource.stream_entered if command.get("stream") else resource.entered
                assert event.wait(5), "resource was not entered"
            elif op == "release":
                resource.release.set()
            elif op == "stop":
                for item in resources.values():
                    item.release.set()
                cleanup = server.shutdown(timeout=5)
                assert cleanup["completed"], cleanup
                for item in resources.values():
                    item.close()
                assert session.call_execution_snapshot()["used_operations"] == 0
                memory = session.memory_stats()
                assert memory["holds"]["active_holds"] == 0
                print(json.dumps({"ok": True, "shutdown": cleanup, "memory": memory, "connections": connections.snapshot()}), flush=True)
                break
            else:
                assert op == "stats", command
            print(json.dumps({"ok": True, "identity": identity,
                              "resources": {name: item.evidence() for name, item in resources.items()},
                              "connections": connections.snapshot()}), flush=True)
    finally:
        for resource in resources.values():
            resource.release.set()
        assert server.shutdown(timeout=5)["completed"]


if __name__ == "__main__":
    assert sys.argv[1] == "--resource"
    _resource_main(json.loads(Path(sys.argv[2]).read_text(encoding="utf-8")))
