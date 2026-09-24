"""Install exact wheels outside the checkout and exercise native IPC and relay."""
from __future__ import annotations

import argparse
from email.parser import Parser
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import zipfile


# This program is copied outside the source checkout and executed only with the
# new environment's interpreter. It imports the public installed SDK surfaces.
CONSUMER = r'''
import importlib.metadata
import json
from pathlib import Path
import os
import sys
import c_two as cc
import c_two._native
import fastdb4py
from fastdb4py.payload import Builder, BuildPolicy, CompiledSpec, Payload, PayloadError
from fastdb4py.payload import _ffi

SPEC = {"schema":"fastdb.payload.v1", "profile":"record.v1",
        "entries":[{"id":"data", "cardinality":"one",
                    "type":{"kind":"bytes", "nullable":False}}], "components":[]}
DATA = bytes(range(256)) * 4096

@cc.crm(namespace="ci.installed-wheels", version="1.0.0")
class Echo:
    @cc.transfer(input=SPEC, output=SPEC)
    def echo(self, payload: Payload) -> Payload: ...

class Resource:
    def echo(self, payload: Payload) -> Payload:
        return payload

def origins():
    root = Path(sys.prefix).resolve()
    paths = {"c_two": cc.__file__, "c_two._native": c_two._native.__file__,
             "fastdb4py": fastdb4py.__file__, "fastdb_library": _ffi.library()._name}
    for name, value in paths.items():
        path = Path(value).resolve()
        if not path.is_relative_to(root):
            raise RuntimeError(f"{name} escaped isolated environment: {path}")
    return {"paths":paths, "prefix":str(root), "pid":os.getpid(),
            "versions":{name:importlib.metadata.version(name) for name in ("c-two","fastdb4py")}}

def build():
    spec = CompiledSpec.compile(json.dumps(SPEC).encode())
    builder = Builder.create(spec)
    try:
        builder.entry_begin(0, 1).value_bytes(DATA)
        plan = builder.freeze()
        try:
            return plan.execute(BuildPolicy.ALLOW_STAGING).payload
        finally:
            plan.close()
    finally:
        builder.close()
        spec.close()

def inspect(payload):
    with payload.entry_view(0) as sequence:
        root = sequence.at(0)
    with root.acquire() as access:
        assert access.bytes() == DATA
    return root

def emit(value):
    print("C2_WHEEL_RESULT=" + json.dumps(value), flush=True)

facts = origins()
cc.set_transport_policy(shm_threshold=4096)
if sys.argv[1] == "host":
    cc.set_server(ipc_overrides={"pool_segment_size":8 * 1024 * 1024})
    cc.set_relay_anchor(sys.argv[2])
    cc.register(Echo, Resource(), name="wheel-echo",
                input_lifetime={"echo":cc.InputLifetime.BORROWED})
    emit({**facts, "address":cc.server_address(), "status":"ready"})
    try:
        assert sys.stdin.readline().strip() == "stop"
    finally:
        cc.shutdown()
    emit({"status":"stopped"})
else:
    cc.set_client(ipc_overrides={"pool_segment_size":8 * 1024 * 1024})
    source = build()
    proxy = cc.connect(Echo, name="wheel-echo", address=sys.argv[2])
    try:
        ordinary = proxy.echo(source)
        try:
            inspect(ordinary).close()
        finally:
            ordinary.close()
        with cc.hold(proxy.echo)(source) as held:
            retained = held.value
            view = inspect(retained)
            assert cc.hold_stats()["active_holds"] == 1
        try:
            with view.acquire():
                raise AssertionError("released checked view remained valid")
        except PayloadError as error:
            assert error.symbol == "VIEW_INVALIDATED", error
        finally:
            view.close()
            retained.close()
        assert cc.hold_stats()["active_holds"] == 0
        emit({**facts, "status":"passed", "transport":sys.argv[3],
              "payload_bytes":len(DATA), "held_view_invalidated":True})
    finally:
        cc.close(proxy)
        source.close()
        cc.shutdown()
'''


def artifact(path: Path) -> dict[str, object]:
    return {"name": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "bytes": path.stat().st_size}


def wheel(path: Path, distribution: str) -> tuple[Path, dict[str, object]]:
    candidates = sorted(path.glob("*.whl")) if path.is_dir() else [path]
    if len(candidates) != 1 or not candidates[0].is_file():
        raise ValueError(f"Expected exactly one {distribution} wheel at {path}")
    selected = candidates[0].resolve()
    with zipfile.ZipFile(selected) as archive:
        names = [name for name in archive.namelist() if name.endswith(".dist-info/METADATA")]
        if len(names) != 1:
            raise ValueError(f"Ambiguous wheel metadata: {selected}")
        metadata = Parser().parsestr(archive.read(names[0]).decode("utf-8"))
    if metadata["Name"].replace("_", "-").lower() != distribution:
        raise ValueError(f"Unexpected distribution in {selected}: {metadata['Name']}")
    return selected, {**artifact(selected), "distribution": distribution, "version": metadata["Version"]}


def clean_environment() -> dict[str, str]:
    excluded = ("PYTHON", "C2_", "FASTDB_", "DYLD_", "LD_LIBRARY_PATH", "UV_PROJECT_ENVIRONMENT")
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(excluded) and key != "VIRTUAL_ENV"}
    environment.update({"C2_ENV_FILE":"", "C2_RELAY_ANCHOR_ADDRESS":"", "PYTHONUTF8":"1"})
    return environment


def run(command: list[str], cwd: Path, environment: dict[str, str], timeout: int = 120) -> str:
    result = subprocess.run(command, cwd=cwd, env=environment, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {command}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def read_result(output: str) -> dict[str, object]:
    lines = [line.removeprefix("C2_WHEEL_RESULT=") for line in output.splitlines()
             if line.startswith("C2_WHEEL_RESULT=")]
    if len(lines) != 1:
        raise RuntimeError(f"Expected one consumer result: {output}")
    return json.loads(lines[0])


def stop(process: subprocess.Popen[str]) -> str:
    if process.poll() is not None:
        return "already_exited"
    process.terminate()
    try:
        process.wait(timeout=10)
        return "terminated"
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)
        return "killed"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fastdb-wheel", type=Path, required=True)
    parser.add_argument("--c-two-wheel", type=Path, required=True)
    parser.add_argument("--c3", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    options = parser.parse_args()
    receipt = {"schema":"c-two.installed-wheel-smoke.v1", "status":"failed",
               "platform":platform.platform(), "interpreter":sys.version, "checks":[], "cleanup":{}}
    options.receipt = options.receipt.resolve()
    options.receipt.parent.mkdir(parents=True, exist_ok=True)
    host = relay = None
    try:
        fastdb, fastdb_info = wheel(options.fastdb_wheel, "fastdb4py")
        ctwo, ctwo_info = wheel(options.c_two_wheel, "c-two")
        c3 = options.c3.resolve(strict=True)
        receipt["artifacts"] = [fastdb_info, ctwo_info, artifact(c3)]
        with tempfile.TemporaryDirectory(prefix="c-two wheel 安装 ") as directory:
            work = Path(directory)
            environment = clean_environment()
            venv = work / "venv"
            run(["uv", "venv", "--python", sys.executable, str(venv)], work, environment)
            python = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
            run(["uv", "pip", "install", "--python", str(python), str(fastdb), str(ctwo)],
                work, environment, timeout=300)
            script = work / "consumer.py"
            script.write_text(CONSUMER, encoding="utf-8")
            executable = work / c3.name
            shutil.copy2(c3, executable)
            receipt["cli_version"] = run([str(executable), "--version"], work, environment).strip()
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            url = f"http://127.0.0.1:{port}"
            with (work / "relay.log").open("w", encoding="utf-8") as relay_log, \
                    (work / "host.log").open("w", encoding="utf-8") as host_log:
                try:
                    relay = subprocess.Popen([str(executable), "relay", "--bind", f"127.0.0.1:{port}",
                                              "--advertise-url", url], cwd=work, env=environment,
                                             stdout=relay_log, stderr=subprocess.STDOUT, text=True)
                    deadline = time.monotonic() + 30
                    while True:
                        if relay.poll() is not None:
                            raise RuntimeError("Isolated c3 exited before readiness")
                        try:
                            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                                break
                        except OSError:
                            if time.monotonic() >= deadline:
                                raise TimeoutError("Isolated c3 did not listen")
                            time.sleep(0.05)
                    host = subprocess.Popen([str(python), "-u", str(script), "host", url],
                                            cwd=work, env=environment, stdin=subprocess.PIPE,
                                            stdout=subprocess.PIPE, stderr=host_log,
                                            text=True, encoding="utf-8")
                    messages: queue.Queue[str] = queue.Queue()
                    def read_lines() -> None:
                        for line in host.stdout:
                            messages.put(line)
                        messages.put("")
                    reader = threading.Thread(target=read_lines, daemon=True)
                    reader.start()
                    ready = read_result(messages.get(timeout=30))
                    if ready.get("status") != "ready":
                        raise RuntimeError(f"Host not ready: {ready}")
                    receipt["host"] = ready
                    for mode, address in [("direct", ready["address"]), ("relay", url)]:
                        result = read_result(run([str(python), str(script), "client", address, mode],
                                                 work, environment))
                        if result.get("status") != "passed" or result["pid"] == ready["pid"]:
                            raise RuntimeError(f"Invalid independent consumer: {result}")
                        for info in (fastdb_info, ctwo_info):
                            if result["versions"][info["distribution"]] != info["version"]:
                                raise RuntimeError(f"Installed version differs from wheel: {result}")
                        receipt["checks"].append(result)
                    host.stdin.write("stop\n")
                    host.stdin.flush()
                    host.wait(timeout=30)
                    stopped = read_result(messages.get(timeout=5))
                    if host.returncode != 0 or stopped != {"status":"stopped"}:
                        raise RuntimeError(f"Host shutdown failed: {stopped}, exit {host.returncode}")
                    receipt["cleanup"]["host_orderly_shutdown"] = True
                finally:
                    if host is not None:
                        receipt["cleanup"]["host_stop"] = stop(host)
                        if host.stdin is not None:
                            host.stdin.close()
                        if host.stdout is not None:
                            host.stdout.close()
                    if relay is not None:
                        receipt["cleanup"]["relay_stop"] = stop(relay)
                    host_log.flush()
                    relay_log.flush()
                    # Preserve only bounded diagnostics, never the environment
                    # or the temporary venv/native binaries in the receipt.
                    receipt["logs"] = {name:(work / name).read_text(encoding="utf-8", errors="replace")[-16000:]
                                       for name in ("host.log", "relay.log")}
            receipt["cleanup"]["processes_exited"] = all(p is None or p.poll() is not None for p in (host, relay))
        receipt["cleanup"]["temporary_directory_removed"] = not work.exists()
        receipt["status"] = "passed"
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
    finally:
        options.receipt.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(f"Installed wheel smoke: {receipt['status']} ({options.receipt})")
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
