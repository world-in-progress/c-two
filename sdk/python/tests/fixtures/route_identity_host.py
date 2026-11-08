"""A controllable real Python resource host for route-identity mesh tests."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import sys

import c_two as cc
import c_two.endpoint
from tests.fixtures.persistent_lifecycle_support import Lifecycle
from tests.fixtures.persistent_lifecycle_peer import JournalResource
from tests.fixtures.process_control import python_process_options, readline_with_timeout


def serve(route: str, server_id: str, anchor: str, root: str, journal: str) -> None:
    runtime = cc.Runtime()
    runtime.set_server(server_id=server_id)
    if root:
        runtime.set_local_endpoint(root=root)
    runtime.set_relay_anchor(anchor)
    runtime.start()
    identity = dict(runtime._runtime_session.ensure_server())
    resource = JournalResource(identity, server_id, Path(journal))
    credential = runtime.server_credential()
    print(json.dumps({"identity": identity, "pid": os.getpid(),
                      "credential": credential.to_json() if credential else None}), flush=True)
    try:
        for line in sys.stdin:
            command = line.strip()
            if command == "register":
                try:
                    runtime.register(Lifecycle, resource, name=route,
                                     concurrency=cc.ConcurrencyConfig(mode="parallel", max_workers=4))
                except cc.error.CCBaseError as error:
                    cause = error.__cause__
                    failure = getattr(cause, "registration_failure", None) or {}
                    print(json.dumps({"error": type(error).__name__, "code": int(error.code),
                                      "message": error.message, "details": dict(error.details),
                                      "native": {"status_code": getattr(cause, "status_code", None),
                                                 "failure_source": failure.get("failure_source"),
                                                 "error_message": failure.get("error_message"),
                                                 "rollback": failure.get("rollback")}}), flush=True)
                else:
                    print(json.dumps({"registered": True}), flush=True)
            elif command == "unregister":
                runtime.unregister(route)
                print(json.dumps({"unregistered": True}), flush=True)
            elif command == "snapshot":
                print(json.dumps({"calls": resource.snapshot()}), flush=True)
            elif command == "stop":
                print(json.dumps({"shutdown": runtime.shutdown(timeout=10)}), flush=True)
                return
            else:
                raise AssertionError(f"unknown operation: {command}")
    finally:
        runtime.shutdown(timeout=10)


class Host:
    def __init__(self, route: str, server_id: str, anchor: str, root: str | None, journal: Path):
        self.root = root
        self.journal = journal
        self.error_path = journal.with_suffix(".stderr")
        self.errors = self.error_path.open("w+", encoding="utf-8")
        sdk = Path(__file__).resolve().parents[2]
        env = {**os.environ, "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": "",
               "PYTHONPATH": os.pathsep.join([str(sdk), os.environ.get("PYTHONPATH", "")])}
        self.process = subprocess.Popen(
            [sys.executable, "-u", "-m", "tests.fixtures.route_identity_host",
             route, server_id, anchor, root or "", str(journal)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors,
            text=True, env=env, **python_process_options(),
        )
        self.closed = False
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
            raise AssertionError(f"resource host startup failed: {diagnostic}") from None
        self.identity = self.ready["identity"]

    def read(self) -> dict:
        line = readline_with_timeout(self.process.stdout, 15)
        assert line, self.stderr()
        return json.loads(line)

    def command(self, operation: str) -> dict:
        self.process.stdin.write(operation + "\n")
        self.process.stdin.flush()
        return self.read()

    def stderr(self) -> str:
        return self.error_path.read_text(encoding="utf-8")

    def records(self) -> list[dict]:
        if not self.journal.exists():
            return []
        return [json.loads(line) for line in self.journal.read_text().splitlines()]

    def kill(self) -> None:
        os.kill(self.ready["pid"], signal.SIGTERM if os.name == "nt" else signal.SIGKILL)
        assert self.process.wait(timeout=5) != 0
        self.closed = True
        self.reap()

    def reap(self) -> None:
        context = cc.endpoint.local_endpoint_context(root=self.root)
        credential = self.ready["credential"]
        if credential is not None:
            result = cc.endpoint.reap_endpoint(
                self.identity["ipc_address"], cc.endpoint.EndpointCredential.from_json(credential),
                context=context,
            )
            expected = ("not-applicable",) if os.name == "nt" else ("reaped", "already-absent")
            assert result["status"] in expected, result
        inspected = cc.endpoint.inspect_endpoint(self.identity["ipc_address"], context=context)
        assert inspected["status"] == ("not-applicable" if os.name == "nt" else "absent"), inspected

    def close(self) -> None:
        try:
            if not self.closed:
                assert self.command("stop")["shutdown"]["completed"]
                assert self.process.wait(timeout=5) == 0
                self.closed = True
                self.reap()
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
            self.process.stdout.close()
            self.process.stdin.close()
            self.errors.close()


if __name__ == "__main__":
    serve(*sys.argv[1:])
