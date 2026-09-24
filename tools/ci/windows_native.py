"""Run native Windows gates and retain failures without changing success receipts."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import time
from typing import Any, Sequence


ROOT = Path(__file__).resolve().parents[2]
PORTABLE_TESTS = (
    "sdk/python/tests/integration/test_portable_payload_cross_language.py",
    "sdk/python/tests/integration/test_portable_payload_matrix.py",
)
TYPESCRIPT_TEST = "sdk/python/tests/integration/test_typescript_real_calls.py"
LOCAL_PLATFORM_SCOPE = "local-platform"
FULL_SCOPE = "full"
SCOPES = (LOCAL_PLATFORM_SCOPE, FULL_SCOPE)
Gate = tuple[str, list[str], tuple[str, ...]]


def capture(command: Sequence[str], cwd: Path) -> dict[str, Any]:
    try:
        result = subprocess.run(
            command, cwd=cwd, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=30, check=False,
        )
        return {"exit_code": result.returncode, "output": (result.stdout + result.stderr).strip()}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"exit_code": None, "error": str(error)}


def stop_process_tree(process: subprocess.Popen[bytes]) -> None:
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=30, check=False,
        )
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process.poll() is None:
        process.kill()
    process.wait(timeout=30)


def run_step(
    name: str, command: Sequence[str], *, cwd: Path, output: Path,
    environment: dict[str, str], timeout: float = 1200,
) -> dict[str, Any]:
    """Execute one gate; a failed command remains a failed record and exit code."""
    log_path = output / f"{name}.log"
    started = time.monotonic()
    record: dict[str, Any] = {
        "id": name, "command": list(command), "status": "failed",
        "exit_code": None, "log": log_path.name,
    }
    print(f"::group::{name}", flush=True)
    print(subprocess.list2cmdline(command), flush=True)
    with log_path.open("wb") as log:
        process = None
        try:
            process = subprocess.Popen(
                command, cwd=cwd, env=environment, stdout=log,
                stderr=subprocess.STDOUT, start_new_session=os.name == "posix",
            )
            record["exit_code"] = process.wait(timeout=timeout)
            record["status"] = "passed" if process.returncode == 0 else "failed"
        except subprocess.TimeoutExpired:
            record["status"] = "timed_out"
            assert process is not None
            stop_process_tree(process)
            record["exit_code"] = process.returncode
        except OSError as error:
            record["error"] = str(error)
            log.write(str(error).encode("utf-8", errors="replace"))
        finally:
            record["duration_seconds"] = round(time.monotonic() - started, 3)
            record["process_exited"] = process is None or process.poll() is not None
    # Full output remains in the artifact; keep the Actions console bounded.
    print(log_path.read_text(encoding="utf-8", errors="replace")[-16000:], flush=True)
    print(f"{name}: {record['status']} (exit {record['exit_code']})", flush=True)
    print("::endgroup::", flush=True)
    return record


def npm_command(*arguments: str) -> list[str]:
    if os.name != "nt":
        return ["npm", *arguments]
    # CreateProcess does not execute npm.cmd directly. Use the JavaScript entry
    # point bundled beside setup-node's node.exe without introducing a shell.
    node = Path(shutil.which("node") or "node.exe")
    script = node.parent / "node_modules/npm/bin/npm-cli.js"
    return [str(node), str(script), *arguments]


def gates(python: str, output: Path, scope: str = FULL_SCOPE) -> list[Gate]:
    def cargo(name: str, manifest: str, *arguments: str) -> Gate:
        return name, ["cargo", "test", "--locked", "--manifest-path", manifest, *arguments], ()

    if scope == LOCAL_PLATFORM_SCOPE:
        return [cargo("local-platform-tests", "core/Cargo.toml", "--lib", "--no-fail-fast",
                      "-p", "c2-config", "-p", "c2-local-security", "-p", "c2-local",
                      "-p", "c2-mem", "-p", "c2-mem-ffi", "-p", "c2-wire",
                      "-p", "c2-ipc", "-p", "c2-server")]
    if scope != FULL_SCOPE:
        raise ValueError(f"Unknown native gate scope: {scope}")
    fastdb_typescript = "../fastdb/ts/fastdb4ts"
    c2_mem_typescript = "core/foundation/c2-mem-ffi/bindings/typescript"
    return [
        ("python310-install", ["uv", "python", "install", "3.10"], ()),
        ("fastdb-npm-install", npm_command("ci", "--prefix", fastdb_typescript), ()),
        ("c2-mem-npm-install", npm_command("ci", "--prefix", c2_mem_typescript), ()),
        ("c2-mem-node-tests", npm_command("test", "--prefix", c2_mem_typescript), ("c2-mem-npm-install",)),
        ("c2-mem-node-package", npm_command("run", "pack:check", "--prefix", c2_mem_typescript), ("c2-mem-npm-install",)),
        cargo("fastdb-rust-test", "../fastdb/bindings/rust/Cargo.toml", "--workspace", "--all-features"),
        ("core-check", ["cargo", "check", "--locked", "--manifest-path", "core/Cargo.toml", "--workspace", "--all-targets"], ()),
        cargo("core-test", "core/Cargo.toml", "--workspace"),
        ("cli-build", ["cargo", "build", "--locked", "--manifest-path", "cli/Cargo.toml", "--bins"], ()),
        cargo("cli-test", "cli/Cargo.toml"),
        cargo("rust-sdk-test", "sdk/rust/Cargo.toml", "--all-features"),
        cargo("python-native-test", "sdk/python/native/Cargo.toml"),
        ("fastdb-wheel-build", ["uv", "build", "--wheel", "--python", python, "--out-dir", str(output / "wheels/fastdb"), "--no-create-gitignore", "../fastdb"], ()),
        ("python-wheel-build", ["uv", "build", "--wheel", "--python", python, "--out-dir", str(output / "wheels/c-two"), "--no-create-gitignore", "sdk/python"], ()),
        ("windows-wheel-consumer", [python, str(ROOT / "tools/ci/windows_wheel_smoke.py"),
                                    "--fastdb-wheel", str(output / "wheels/fastdb"),
                                    "--c-two-wheel", str(output / "wheels/c-two"),
                                    "--c3", str(ROOT / "cli/target/debug/c3.exe"),
                                    "--receipt", str(output / "installed-wheel-full-receipt.v1.json")],
         ("fastdb-wheel-build", "python-wheel-build", "cli-build")),
        ("python-build", ["uv", "sync", "--locked", "--python", python], ()),
        ("python-tests", ["uv", "run", "--no-sync", "pytest", "sdk/python/tests", "-q", "--timeout=30", *[f"--ignore={path}" for path in (*PORTABLE_TESTS, TYPESCRIPT_TEST)], f"--junitxml={output / 'python-tests.xml'}"], ("python-build",)),
        ("portable-tests", ["uv", "run", "--no-sync", "pytest", *PORTABLE_TESTS, "-q", "--timeout=300", f"--junitxml={output / 'portable-tests.xml'}"], ("python-build",)),
        ("typescript-tests", ["uv", "run", "--no-sync", "pytest", TYPESCRIPT_TEST, "-q", "--timeout=600", f"--junitxml={output / 'typescript-tests.xml'}"], ("python-build", "fastdb-npm-install", "c2-mem-npm-install")),
    ]


def write_evidence(output: Path, evidence: dict[str, Any]) -> None:
    temporary = output / "run-evidence.json.tmp"
    temporary.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    temporary.replace(output / "run-evidence.json")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-c-two-sha", required=True)
    parser.add_argument("--expected-fastdb-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scope", choices=SCOPES, default=FULL_SCOPE)
    options = parser.parse_args(argv)
    output = options.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    # Preserve each crate's conventional output location used by the test
    # fixtures; shared CARGO_TARGET_DIR would hide c3 from relay tests.
    environment.pop("CARGO_TARGET_DIR", None)
    environment.update({
        "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": "", "PYTHONUTF8": "1",
        "C2_PORTABLE_MATRIX_RECEIPT": str(output / f"portable-matrix-{options.scope}-receipt.v1.json"),
        "C2_PORTABLE_MATRIX_EVIDENCE_STAGE": "development",
        "C2_TYPESCRIPT_RECEIPT": str(output / f"typescript-real-call-{options.scope}-receipt.v1.json"),
        "C2_TYPESCRIPT_EVIDENCE_STAGE": "development",
        "C2_TYPESCRIPT_FASTDB_SOURCE_SHA": options.expected_fastdb_sha,
    })
    sources = {}
    for name, path, expected in (
        ("c-two", ROOT, options.expected_c_two_sha),
        ("fastdb", ROOT.parent / "fastdb", options.expected_fastdb_sha),
    ):
        result = capture(["git", "rev-parse", "HEAD"], path)
        sources[name] = {"expected_sha": expected, "actual_sha": result.get("output"), "verified": result.get("exit_code") == 0 and result.get("output") == expected}
    selected_gates = gates(sys.executable, output, options.scope)
    toolchains = {
        "python": [sys.executable, "--version"], "rustc": ["rustc", "-vV"],
        "cargo": ["cargo", "--version"],
        "msvc": ["cl"] if os.name == "nt" else ["false"],
        "identity": ["whoami", "/all"] if os.name == "nt" else ["id"],
    }
    if options.scope == FULL_SCOPE:
        toolchains.update({
            "uv": ["uv", "--version"], "cmake": ["cmake", "--version"],
            "swig": ["swig", "-version"], "node": ["node", "--version"],
            "npm": npm_command("--version"),
            "emscripten": [sys.executable, str(Path(os.environ.get("EMSDK", "../emsdk")) / "upstream/emscripten/emcc.py"), "--version"],
        })
    evidence: dict[str, Any] = {
        "schema": "c-two.native-run-evidence.v1",
        "scope": options.scope,
        "applicable_gates": [name for name, _, _ in selected_gates],
        "started_at": datetime.now(timezone.utc).isoformat(),
        "status": "running", "sources": sources,
        "workflow_sha": os.environ.get("C2_WORKFLOW_SHA"),
        "run_id": os.environ.get("GITHUB_RUN_ID"),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "runner": {"requested_label": os.environ.get("C2_RUNNER_LABEL"), "os": platform.platform(), "machine": platform.machine(), "image": os.environ.get("ImageOS"), "image_version": os.environ.get("ImageVersion")},
        "toolchains": {name: capture(command, ROOT) for name, command in toolchains.items()},
        "steps": [], "artifacts": [],
    }
    write_evidence(output, evidence)
    if os.name != "nt" or platform.machine().lower() not in {"amd64", "x86_64"}:
        evidence["error"] = "This gate requires native x64 Windows."
    elif not all(source["verified"] for source in sources.values()):
        evidence["error"] = "Checked-out source SHA does not match the requested immutable source."
    else:
        for name, command, dependencies in selected_gates:
            passed = {step["id"] for step in evidence["steps"] if step["status"] == "passed"}
            missing = [dependency for dependency in dependencies if dependency not in passed]
            if missing:
                record = {"id": name, "command": command, "status": "not_run", "reason": f"prerequisites did not pass: {', '.join(missing)}"}
            else:
                record = run_step(name, command, cwd=ROOT, output=output, environment=environment)
            evidence["steps"].append(record)
            write_evidence(output, evidence)
    evidence["status"] = "passed" if evidence["steps"] and all(step["status"] == "passed" for step in evidence["steps"]) and "error" not in evidence else "failed"
    for artifact in sorted(output.rglob("*")):
        if artifact.is_file() and artifact.name != "run-evidence.json":
            evidence["artifacts"].append({"path": artifact.relative_to(output).as_posix(), "sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(), "bytes": artifact.stat().st_size})
    evidence["finished_at"] = datetime.now(timezone.utc).isoformat()
    write_evidence(output, evidence)
    print(f"Run evidence: {output / 'run-evidence.json'} ({evidence['status']})", flush=True)
    return 0 if evidence["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
