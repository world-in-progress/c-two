"""Install exact wheels outside the checkout and exercise native IPC and relay."""
from __future__ import annotations

import argparse
import ctypes
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


# The embedded consumer is copied outside the source checkout and executed
# only with the new environment's interpreter and installed SDKs.
CONSUMER = r'''
import importlib.metadata
import hashlib
import json
from pathlib import Path
import os
import sys
from urllib.parse import urlparse
from urllib.request import url2pathname
import c_two as cc
import c_two._native
import fastdb4py
import numpy
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
    def __init__(self):
        self.inputs = []

    def echo(self, payload: Payload) -> Payload:
        self.inputs.append(payload)
        return payload

    def verify_borrowed_inputs(self):
        assert len(self.inputs) == 4
        for payload in self.inputs:
            try:
                payload.entry_view(0)
                raise AssertionError("borrowed input remained valid after callback")
            except PayloadError as error:
                assert error.symbol == "VIEW_INVALIDATED", error
        return len(self.inputs)

def origins():
    root = Path(sys.prefix).resolve()
    paths = {"c_two": cc.__file__, "c_two._native": c_two._native.__file__,
             "fastdb4py": fastdb4py.__file__, "fastdb_library": _ffi.library()._name,
             "numpy": numpy.__file__}
    for name, value in paths.items():
        path = Path(value).resolve()
        if not path.is_relative_to(root):
            raise RuntimeError(f"{name} escaped isolated environment: {path}")
    installed_wheels = {}
    for expected in json.loads(Path(sys.argv[-1]).read_text(encoding="utf-8")):
        distribution = importlib.metadata.distribution(expected["distribution"])
        direct = json.loads(distribution.read_text("direct_url.json"))
        url = urlparse(direct["url"])
        assert url.scheme == "file" and not url.netloc, direct
        source = Path(url2pathname(url.path)).resolve()
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        assert source == Path(expected["path"]).resolve()
        assert digest == expected["sha256"] and distribution.version == expected["version"]
        installed_wheels[expected["distribution"]] = {"direct_url":direct, "sha256":digest}
    return {"paths":paths, "prefix":str(root), "pid":os.getpid(),
            "installed_wheels":installed_wheels,
            "versions":{name:importlib.metadata.version(name) for name in ("c-two","fastdb4py","numpy")}}

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
    resource = Resource()
    cc.register(Echo, resource, name="wheel-echo",
                input_lifetime={"echo":cc.InputLifetime.BORROWED})
    emit({**facts, "address":cc.server_address(), "status":"ready"})
    try:
        assert sys.stdin.readline().strip() == "stop"
        invalidated = resource.verify_borrowed_inputs()
    finally:
        cc.shutdown()
    emit({"status":"stopped", "borrowed_inputs_invalidated":invalidated})
else:
    cc.set_client(ipc_overrides={"pool_segment_size":8 * 1024 * 1024})
    source = build()
    proxy = cc.connect(Echo, name="wheel-echo", address=sys.argv[2])
    try:
        observed_mode = proxy.client._mode
        assert observed_mode == ("ipc" if sys.argv[3] == "direct" else "http")
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
              "observed_mode":observed_mode, "payload_bytes":len(DATA),
              "held_view_invalidated":True})
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
    """Build the subprocess environment for the isolated consumer.

    External FastDB hints are removed so the self-contained app claim is
    actually tested: FASTDB_* variables (including FASTDB_SDK), macOS DYLD_*
    loaders, LD_LIBRARY_PATH, and PATH entries that live inside an exported
    FastDB SDK are dropped. Every other OS/Python PATH entry is preserved
    because Windows system DLLs and the toolchain still need them.
    """
    excluded = ("PYTHON", "C2_", "FASTDB_", "DYLD_", "LD_LIBRARY_PATH", "UV_", "PIP_", "CONDA_")
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(excluded) and key != "VIRTUAL_ENV"}
    sdk_roots: list[Path] = []
    for key in os.environ:
        if key.startswith("FASTDB_") and key.endswith(("_SDK", "_HOME", "_ROOT", "_LIB_DIR")):
            value = os.environ.get(key) or ""
            if not value:
                continue
            candidate = Path(value).expanduser()
            try:
                candidate = candidate.resolve()
            except OSError:
                pass
            sdk_roots.append(candidate)
    if sdk_roots and environment.get("PATH"):
        kept = []
        for entry in environment["PATH"].split(os.pathsep):
            if not entry:
                continue
            candidate = Path(entry).expanduser()
            try:
                candidate = candidate.resolve()
            except OSError:
                pass
            if any(candidate == root or root in candidate.parents for root in sdk_roots):
                continue
            kept.append(entry)
        environment["PATH"] = os.pathsep.join(kept)
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


def cleanup_processes(host, relay, reader=None) -> dict[str, object]:
    """Reap every owned process even when another stop or pipe close fails."""
    cleanup: dict[str, object] = {"errors": []}
    for name, process in (("relay", relay), ("host", host)):
        if process is not None:
            try:
                cleanup[f"{name}_stop"] = stop(process)
            except Exception as error:
                cleanup["errors"].append(f"{name}: {error}")
    if reader is not None:
        reader.join(timeout=5)
        if reader.is_alive():
            cleanup["errors"].append("host stdout reader did not stop")
    if host is not None:
        for stream in (host.stdin, host.stdout):
            if stream is not None:
                try:
                    stream.close()
                except Exception as error:
                    cleanup["errors"].append(f"host pipe close: {error}")
    cleanup["processes_exited"] = all(p is not None and p.poll() is not None for p in (host, relay))
    return cleanup


def host_result(messages: queue.Queue[str], log, timeout: float = 30) -> dict[str, object]:
    deadline = time.monotonic() + timeout
    while True:
        line = messages.get(timeout=max(0, deadline - time.monotonic()))
        if not line:
            raise RuntimeError("Host stdout closed before its result")
        if line.startswith("C2_WHEEL_RESULT="):
            return read_result(line)
        log.write(line)
        log.flush()


def windows_administrator() -> bool:
    """Check enabled token membership; an API failure is not a nonadmin pass."""
    api = ctypes.WinDLL("advapi32", use_last_error=True, winmode=0x00000800)
    create_sid = api.CreateWellKnownSid
    create_sid.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p,
                          ctypes.POINTER(ctypes.c_ulong)]
    create_sid.restype = ctypes.c_int
    sid = ctypes.create_string_buffer(68)  # SECURITY_MAX_SID_SIZE
    size = ctypes.c_ulong(len(sid))
    if not create_sid(26, None, sid, ctypes.byref(size)):  # WinBuiltinAdministratorsSid
        raise ctypes.WinError(ctypes.get_last_error())
    check = api.CheckTokenMembership
    check.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)]
    check.restype = ctypes.c_int
    member = ctypes.c_int()
    if not check(None, sid, ctypes.byref(member)):
        raise ctypes.WinError(ctypes.get_last_error())
    return bool(member.value)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fastdb-wheel", type=Path, required=True)
    parser.add_argument("--c-two-wheel", type=Path, required=True)
    parser.add_argument("--c3", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--require-standard-user", action="store_true")
    parser.add_argument("--source-sha", default=None,
                        help="Source commit sha this gate's bytes were built from; "
                             "recorded so the candidate manifest can cross-bind receipts")
    options = parser.parse_args()
    receipt = {"schema":"c-two.installed-wheel-smoke.v1", "status":"failed",
               "platform":platform.platform(), "interpreter":sys.version, "checks":[], "cleanup":{}}
    if options.source_sha:
        receipt["source_commit_sha"] = options.source_sha
    options.receipt = options.receipt.resolve()
    options.receipt.parent.mkdir(parents=True, exist_ok=True)
    host = relay = reader = None
    work = None
    try:
        administrator = windows_administrator() if os.name == "nt" else None
        receipt["identity"] = {"administrator": administrator}
        if options.require_standard_user and (os.name != "nt" or administrator):
            raise RuntimeError("This gate requires a Windows token without administrator membership")
        fastdb, fastdb_info = wheel(options.fastdb_wheel, "fastdb4py")
        ctwo, ctwo_info = wheel(options.c_two_wheel, "c-two")
        c3 = options.c3.resolve(strict=True)
        receipt["artifacts"] = [fastdb_info, ctwo_info, artifact(c3)]
        with tempfile.TemporaryDirectory(prefix="c-two wheel 安装 ") as directory:
            work = Path(directory)
            environment = clean_environment()
            venv = work / "venv"
            run(["uv", "--no-config", "venv", "--python", sys.executable, str(venv)], work, environment)
            python = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
            run(["uv", "--no-config", "pip", "install", "--python", str(python), str(fastdb), str(ctwo)],
                work, environment, timeout=300)
            receipt["installed_packages"] = json.loads(run(
                ["uv", "--no-config", "pip", "list", "--python", str(python), "--format", "json"], work, environment))
            inputs = work / "inputs.json"
            inputs.write_text(json.dumps([{**info, "path":str(path)} for path, info in
                                          ((fastdb, fastdb_info), (ctwo, ctwo_info))]), encoding="utf-8")
            script = work / "consumer.py"
            script.write_text(CONSUMER, encoding="utf-8")
            executable = work / c3.name
            shutil.copy2(c3, executable)
            if os.name != "nt":
                # A downloaded artifact may arrive without the executable bit;
                # restoring it never changes the bytes the sidecar digest binds.
                executable.chmod(executable.stat().st_mode | 0o111)
            receipt["cli_version"] = run([str(executable), "--version"], work, environment).strip()
            if not receipt["cli_version"].startswith("c3 "):
                raise RuntimeError(f"Invalid c3 version response: {receipt['cli_version']!r}")
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
                    host = subprocess.Popen([str(python), "-u", str(script), "host", url, str(inputs)],
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
                    ready = host_result(messages, host_log)
                    if ready.get("status") != "ready":
                        raise RuntimeError(f"Host not ready: {ready}")
                    receipt["host"] = ready
                    for mode, address in [("direct", ready["address"]), ("relay", url)]:
                        result = read_result(run([str(python), str(script), "client", address, mode, str(inputs)],
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
                    stopped = host_result(messages, host_log, timeout=5)
                    if host.returncode != 0 or stopped != {"status":"stopped", "borrowed_inputs_invalidated":4}:
                        raise RuntimeError(f"Host shutdown failed: {stopped}, exit {host.returncode}")
                    receipt["cleanup"]["host_orderly_shutdown"] = True
                    receipt["borrowed_inputs_invalidated"] = stopped["borrowed_inputs_invalidated"]
                finally:
                    receipt["cleanup"].update(cleanup_processes(host, relay, reader))
                    host_log.flush()
                    relay_log.flush()
                    # Preserve only bounded diagnostics, never the environment
                    # or the temporary venv/native binaries in the receipt.
                    receipt["logs"] = {name:(work / name).read_text(encoding="utf-8", errors="replace")[-16000:]
                                       for name in ("host.log", "relay.log")}
            if receipt["cleanup"]["errors"] or not receipt["cleanup"]["processes_exited"]:
                raise RuntimeError(f"Process cleanup failed: {receipt['cleanup']}")
        receipt["cleanup"]["temporary_directory_removed"] = not work.exists()
        if not receipt["cleanup"]["temporary_directory_removed"]:
            raise RuntimeError("Temporary consumer directory remains")
        receipt["status"] = "passed"
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
    finally:
        if work is not None:
            receipt["cleanup"]["temporary_directory_removed"] = not work.exists()
        options.receipt.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(f"Installed wheel smoke: {receipt['status']} ({options.receipt})")
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
