"""Controllable real resource process; abrupt exit leaves a durable call journal."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import threading

import c_two as cc
import c_two.endpoint
from tests.fixtures.persistent_lifecycle_support import Events, Lifecycle, Resource
from tests.fixtures.process_control import python_process_options, readline_with_timeout


class JournalResource(Resource):
    def __init__(self, identity: dict, label: str, journal: Path):
        super().__init__(identity, label)
        self.journal = journal
        self.journal_lock = threading.Lock()

    def reply(self, method: str, value: str) -> dict:
        reply = super().reply(method, value)
        with self.journal_lock, self.journal.open("a", encoding="utf-8") as output:
            output.write(json.dumps({"method": method, "calls": reply["calls"]}) + "\n")
        return reply


def serve(route: str, server_id: str, anchor: str, journal: str) -> None:
    runtime = cc.Runtime()
    runtime.set_server(server_id=server_id)
    if anchor:
        runtime.set_relay_anchor(anchor)
    events = Events(runtime)
    runtime.start()
    identity = dict(runtime._runtime_session.ensure_server())
    resource = JournalResource(identity, route, Path(journal))
    runtime.register(
        Lifecycle, resource, name=route,
        concurrency=cc.ConcurrencyConfig(mode="parallel", max_workers=4),
    )
    credential = runtime.server_credential()
    print(json.dumps({"identity": identity, "pid": os.getpid(),
                      "credential": credential.to_json() if credential else None}), flush=True)
    try:
        for command in sys.stdin:
            operation = command.strip()
            if operation == "snapshot":
                events.drain()
                print(json.dumps({"calls": resource.snapshot(), "connections": events.records}), flush=True)
            elif operation == "stop":
                resource.release.set()
                outcome = runtime.shutdown(timeout=10)
                events.drain()
                print(json.dumps({"shutdown": outcome, "calls": resource.snapshot(),
                                  "connections": events.records}), flush=True)
                return
            else:
                raise AssertionError(f"unknown operation: {operation}")
    finally:
        resource.release.set()
        assert runtime.shutdown(timeout=10)["completed"]


class Peer:
    def __init__(self, route: str, server_id: str, anchor: str, journal: Path, root: str | None):
        self.journal = journal
        self.root = root
        self.errors = journal.with_suffix(".stderr").open("w+", encoding="utf-8")
        sdk = Path(__file__).resolve().parents[2]
        env = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": "",
               "PYTHONPATH": os.pathsep.join([str(sdk), os.environ.get("PYTHONPATH", "")])}
        self.process = subprocess.Popen(
            [sys.executable, "-u", "-m", "tests.fixtures.persistent_lifecycle_peer",
             route, server_id, anchor, str(journal)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors,
            text=True, env=env, **python_process_options(),
        )
        self.stopped = False
        try:
            self.ready = self.read()
        except BaseException:
            self.process.kill()
            self.process.wait(timeout=5)
            self.errors.seek(0)
            diagnostic = self.errors.read()
            self.process.stdout.close()
            self.process.stdin.close()
            self.errors.close()
            raise AssertionError(f"resource startup failed: {diagnostic}") from None
        self.identity = self.ready["identity"]
        assert self.ready["pid"] == self.process.pid

    def read(self) -> dict:
        line = readline_with_timeout(self.process.stdout, 15)
        self.errors.flush()
        self.errors.seek(0)
        assert line, self.errors.read()
        return json.loads(line)

    def command(self, operation: str) -> dict:
        self.process.stdin.write(operation + "\n")
        self.process.stdin.flush()
        return self.read()

    def journal_records(self) -> list[dict]:
        if not self.journal.exists():
            return []
        return [json.loads(line) for line in self.journal.read_text().splitlines()]

    def reap(self) -> dict:
        context = cc.endpoint.local_endpoint_context(root=self.root)
        address = self.identity["ipc_address"]
        if self.ready["credential"] is not None:
            credential = cc.endpoint.EndpointCredential.from_json(self.ready["credential"])
            result = cc.endpoint.reap_endpoint(address, credential, context=context)
            if os.name == "nt":
                assert result["status"] == "not-applicable", result
            else:
                assert result["status"] in {"reaped", "already-absent"}, result
        observed = cc.endpoint.inspect_endpoint(address, context=context)
        assert observed["status"] in {"absent", "not-applicable"}, observed
        return {"status": observed["status"]}

    def kill(self) -> dict:
        before = self.command("snapshot")
        self.process.kill()
        self.process.wait(timeout=5)
        self.stopped = True
        self.cleanup = self.reap()
        return {**self.ready, **before, "exit_code": self.process.returncode,
                "cleanup": self.cleanup, "journal": self.journal_records()}

    def close(self) -> dict:
        try:
            if self.stopped:
                return {"cleanup": self.cleanup}
            outcome = self.command("stop")
            assert outcome["shutdown"]["completed"], outcome
            assert self.process.wait(timeout=5) == 0
            self.stopped = True
            self.cleanup = self.reap()
            return {**self.ready, **outcome, "cleanup": self.cleanup}
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
                self.reap()
            self.process.stdout.close()
            self.process.stdin.close()
            self.errors.close()


if __name__ == "__main__":
    serve(*sys.argv[1:])
