"""Run native Windows gates and retain failures without changing success receipts."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
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
FASTDB_LINK_MODE_ENV = "FASTDB_PAYLOAD_LINK_MODE"
FASTDB_SYSTEM_LIB_DIR_ENV = "FASTDB_PAYLOAD_SYSTEM_LIB_DIR"
# Deployable applications link FastDB statically through their fastdb-sys Git
# patches, so every gate that builds or tests one of them must replace the base
# system-mode environment with explicit source link mode.
SOURCE_MODE_GATES = frozenset({
    "cli-build",
    "cli-test",
    "python-native-test",
    "python-wheel-build",
    "python-build",
})
IPC_MEMORY_GATE = "ipc-memory-matrix"
IPC_MEMORY_EVIDENCE_DIR = "ipc-memory-matrix"
IPC_MEMORY_BENCHMARK = ROOT / "tools/benchmarks/ipc_memory.py"
# Explicit finite join budgets for the benchmark's own validated --child-timeout
# and --row-timeout interface; the driver rejects anything unbounded.
IPC_MEMORY_CHILD_TIMEOUT_S = 120
IPC_MEMORY_ROW_TIMEOUT_S = 300
STEP_TIMEOUT_S = 1200
# Nine rows can each consume the whole row budget, so the matrix step is
# bounded above the summed row budget (9 x 300 s) plus startup margin instead
# of by the single-command default.
IPC_MEMORY_STEP_TIMEOUT_S = 3600
GATE_TIMEOUT_S = {IPC_MEMORY_GATE: IPC_MEMORY_STEP_TIMEOUT_S}


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
    environment: dict[str, str], timeout: float = STEP_TIMEOUT_S,
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


def accepted_matrix_row_names() -> list[str] | None:
    """Row names of the accepted benchmark's pure matrix_rows table.

    Derived from tools/benchmarks/ipc_memory.py itself so acceptance and
    execution share one row table instead of a drifting copy. Returns None
    when the table cannot be loaded or is not uniquely named; acceptance
    then fails closed.
    """
    try:
        spec = importlib.util.spec_from_file_location(
            "c-two-ipc-memory-benchmark-acceptance", IPC_MEMORY_BENCHMARK)
        if spec is None or spec.loader is None:
            return None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        names = [str(row["name"]) for row in module.matrix_rows()]
    except Exception:
        return None
    if not names or len(names) != len(set(names)):
        return None
    return names


def matrix_evidence_problem(evidence_dir: Path) -> str | None:
    """Explain why retained matrix evidence is not the required full stats run.

    A clean benchmark exit alone is not acceptance: an --only subset can also
    exit 0, and header flags alone do not prove any row ran. The evidence must
    carry the complete row table derived from the accepted benchmark, passing
    strict-integer totals, one agreed native hash mapped to every row, and
    existing per-row result files inside the evidence directory. Internal
    memory-snapshot and workload semantics stay owned by the benchmark and its
    harness tests; this is receipt validation, not a benchmark re-run.
    """
    expected = accepted_matrix_row_names()
    if expected is None:
        return "accepted benchmark row table is unavailable; cannot validate matrix completeness"
    try:
        matrix = json.loads((evidence_dir / "matrix.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return f"matrix evidence unreadable: {error}"
    if not isinstance(matrix, dict):
        return "matrix evidence is not a JSON object"
    if matrix.get("schema") != "c-two.ipc-memory-matrix.v1":
        return f"unexpected matrix evidence schema: {matrix.get('schema')!r}"
    if matrix.get("complete") is not True:
        return "matrix evidence is incomplete: an --only subset or interrupted rows must not pass"
    if matrix.get("memory_stats") is not True or matrix.get("require_memory_stats") is not True:
        return "matrix evidence was not produced with --memory-stats --require-memory-stats"
    if matrix.get("allow_unavailable_stats_after_shutdown") is not False:
        return "matrix evidence used the post-shutdown stats exemption"
    rows = matrix.get("rows")
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        return "matrix evidence rows are not a list of objects"
    names = [row.get("name") for row in rows]
    if names != expected:
        return f"matrix rows {names!r} are not the complete unique accepted table {expected!r}"
    if matrix.get("rows_not_selected") != [] or matrix.get("failures") != []:
        return (
            "matrix evidence skipped rows or recorded driver failures: "
            f"rows_not_selected={matrix.get('rows_not_selected')!r} failures={matrix.get('failures')!r}"
        )
    totals = matrix.get("totals")
    if not isinstance(totals, dict):
        return "matrix evidence carries no totals object"
    for field, wanted in (
        ("rows_total", len(expected)), ("rows_selected", len(expected)),
        ("rows_not_selected", 0), ("passed", len(expected)), ("failed", 0),
    ):
        value = totals.get(field)
        # Receipt validation reads strict integers: True is not 1 here.
        if not isinstance(value, int) or isinstance(value, bool) or value != wanted:
            return f"matrix totals[{field}] is {value!r}, not the strict integer {wanted}"
    native = matrix.get("native")
    if not isinstance(native, dict) or native.get("agreement") is not True:
        return "matrix evidence lacks agreeing native provenance across rows"
    digest = native.get("single_installed_hash")
    if (
        not isinstance(digest, str) or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
    ):
        return f"matrix native hash is not a SHA-256 digest: {digest!r}"
    if native.get("rows_missing_provenance") != [] or native.get("rows_inconsistent") != []:
        return "matrix evidence has rows with missing or inconsistent native provenance"
    per_row = native.get("per_row")
    if (
        not isinstance(per_row, dict) or sorted(per_row) != sorted(expected)
        or any(per_row.get(name) != digest for name in expected)
    ):
        return "matrix evidence does not map every row to the single agreed native hash"
    evidence_root = evidence_dir.resolve()
    for row in rows:
        name = row["name"]
        if row.get("ok") is not True:
            return f"matrix row {name!r} did not pass"
        exit_code = row.get("exit_code")
        if not isinstance(exit_code, int) or isinstance(exit_code, bool) or exit_code != 0:
            return f"matrix row {name!r} exit_code is {exit_code!r}, not integer 0"
        if row.get("terminated_by_matrix") is not False:
            return f"matrix row {name!r} was terminated by the matrix"
        result_file = row.get("result_file")
        expected_file = f"rows/{name}.json"
        # The benchmark records str(Path.relative_to(...)), so Windows
        # receipts use backslashes. Validate the exact portable relative
        # name before resolving it, including when auditing them on Unix.
        if not isinstance(result_file, str) or result_file.replace("\\", "/") != expected_file:
            return f"matrix row {name!r} result_file is {result_file!r}, not {expected_file}"
        result_path = evidence_dir / expected_file
        if not result_path.resolve().is_relative_to(evidence_root):
            return f"matrix row {name!r} result file is not inside the evidence directory: {result_file!r}"
        if not result_path.is_file():
            return f"matrix row {name!r} result file is missing: {result_file!r}"
    return None


def npm_command(*arguments: str) -> list[str]:
    if os.name != "nt":
        return ["npm", *arguments]
    # CreateProcess does not execute npm.cmd directly. Use the JavaScript entry
    # point bundled beside setup-node's node.exe without introducing a shell.
    node = Path(shutil.which("node") or "node.exe")
    script = node.parent / "node_modules/npm/bin/npm-cli.js"
    return [str(node), str(script), *arguments]


def platform_fastdb_library() -> str:
    if os.name == "nt":
        return "fastdb.lib"
    if sys.platform == "darwin":
        return "libfastdb.dylib"
    return "libfastdb.so"


def validate_system_lib_dir(raw: Path) -> tuple[Path | None, str | None]:
    """Resolve and validate the prepared CoreSDK lib directory, or explain why."""
    if not raw.is_absolute():
        return None, f"{FASTDB_SYSTEM_LIB_DIR_ENV} must be an absolute path: {raw}"
    resolved = raw.resolve()
    if not resolved.is_dir():
        return None, f"prepared FastDB CoreSDK lib directory is missing: {resolved}"
    library = resolved / platform_fastdb_library()
    if not library.is_file():
        return None, f"prepared FastDB CoreSDK is missing {library.name}: {library}"
    return resolved, None


def system_mode_environment(base: dict[str, str], lib_dir: Path) -> dict[str, str]:
    """Link core/RustSDK consumers against the published system CoreSDK."""
    environment = dict(base)
    environment[FASTDB_LINK_MODE_ENV] = "system"
    environment[FASTDB_SYSTEM_LIB_DIR_ENV] = str(lib_dir)
    # System-linked consumer binaries resolve fastdb.dll through PATH.
    separator = ";" if os.name == "nt" else ":"
    path_keys = [key for key in environment if key.upper() == "PATH"]
    existing = environment[path_keys[0]] if path_keys else ""
    for key in path_keys:
        environment.pop(key)
    environment["PATH"] = f"{lib_dir}{separator}{existing}" if existing else str(lib_dir)
    return environment


def source_mode_environment(base: dict[str, str]) -> dict[str, str]:
    """Drop system-mode overrides so app workspaces use their Git-patch source builds."""
    environment = dict(base)
    environment[FASTDB_LINK_MODE_ENV] = "source"
    environment.pop(FASTDB_SYSTEM_LIB_DIR_ENV, None)
    return environment


def gate_environment(
    name: str, base: dict[str, str], system_lib_dir: Path | None
) -> dict[str, str]:
    if system_lib_dir is None:
        return dict(base)
    if name in SOURCE_MODE_GATES:
        return source_mode_environment(base)
    return system_mode_environment(base, system_lib_dir)


def gates(python: str, output: Path, scope: str = FULL_SCOPE) -> list[Gate]:
    def cargo(name: str, manifest: str, *arguments: str) -> Gate:
        return name, ["cargo", "test", "--locked", "--manifest-path", manifest, *arguments], ()

    if scope == LOCAL_PLATFORM_SCOPE:
        # Include native integration tests (notably owner-control) and Core
        # call-state tests at each implementation stage, without SDK matrices.
        return [cargo("local-platform-tests", "core/Cargo.toml", "--no-fail-fast",
                      "--features", "c2-http/relay",
                      "-p", "c2-config", "-p", "c2-error", "-p", "c2-local-security", "-p", "c2-local",
                      "-p", "c2-mem", "-p", "c2-mem-ffi", "-p", "c2-wire",
                      "-p", "c2-ipc", "-p", "c2-server", "-p", "c2-http", "-p", "c2-core")]
    if scope != FULL_SCOPE:
        raise ValueError(f"Unknown native gate scope: {scope}")
    fastdb_typescript = "../fastdb/ts/fastdb4ts"
    c2_mem_typescript = "core/foundation/c2-mem-ffi/bindings/typescript"
    fastdb_build = npm_command("run", "build", "--prefix", fastdb_typescript)
    if os.name == "nt":
        shell = os.environ.get("C2_FASTDB_NPM_SCRIPT_SHELL") or str(
            Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git/bin/bash.exe"
        )
        fastdb_build.extend(["--script-shell", shell])
    return [
        ("python310-install", ["uv", "python", "install", "3.10"], ()),
        ("fastdb-npm-install", npm_command("ci", "--prefix", fastdb_typescript), ()),
        # The persistent generated-client regression is part of the ordinary
        # suite, before the payload matrix's independent WASM/package build.
        ("fastdb-npm-build", fastdb_build, ("fastdb-npm-install",)),
        ("c2-mem-npm-install", npm_command("ci", "--prefix", c2_mem_typescript), ()),
        ("c2-mem-node-tests", npm_command("test", "--prefix", c2_mem_typescript), ("c2-mem-npm-install",)),
        ("c2-mem-node-package", npm_command("run", "pack:check", "--prefix", c2_mem_typescript), ("c2-mem-npm-install",)),
        cargo("fastdb-rust-test", "../fastdb/bindings/rust/Cargo.toml", "--workspace", "--all-features"),
        ("core-check", ["cargo", "check", "--locked", "--manifest-path", "core/Cargo.toml", "--workspace", "--all-targets"], ()),
        cargo("core-test", "core/Cargo.toml", "--workspace"),
        ("cli-build", ["cargo", "build", "--locked", "--manifest-path", "cli/Cargo.toml", "--bins"], ()),
        cargo("cli-test", "cli/Cargo.toml"),
        # Retain the CLI only after every gate that can relink it has run.
        # Windows relinks are not byte-stable, so a copy taken before cli-test
        # would not match the binary every later gate and receipt actually uses.
        ("cli-artifact", [python, "-c",
                          "from pathlib import Path; import shutil, sys; "
                          "target = Path(sys.argv[2]); target.parent.mkdir(parents=True, exist_ok=True); "
                          "shutil.copy2(sys.argv[1], target)",
                          str(ROOT / "cli/target/debug/c3.exe"), str(output / "cli/c3.exe")],
         ("cli-test",)),
        cargo("rust-sdk-test", "sdk/rust/Cargo.toml", "--all-features"),
        cargo("python-native-test", "sdk/python/native/Cargo.toml"),
        ("fastdb-wheel-build", ["uv", "build", "--wheel", "--python", python, "--out-dir", str(output / "wheels/fastdb"), "--no-create-gitignore", "../fastdb"], ()),
        ("python-wheel-build", ["uv", "build", "--wheel", "--python", python, "--out-dir", str(output / "wheels/c-two"), "--no-create-gitignore", "sdk/python"], ()),
        ("windows-wheel-consumer", [python, str(ROOT / "tools/ci/windows_wheel_smoke.py"),
                                    "--fastdb-wheel", str(output / "wheels/fastdb"),
                                    "--c-two-wheel", str(output / "wheels/c-two"),
                                    "--c3", str(output / "cli/c3.exe"),
                                    "--receipt", str(output / "installed-wheel-full-receipt.v1.json")],
         ("fastdb-wheel-build", "python-wheel-build", "cli-artifact")),
        ("windows-standard-user-consumer", ["pwsh", "-NoLogo", "-NoProfile", "-NonInteractive", "-File",
                                             str(ROOT / "tools/ci/windows_standard_user.ps1"),
                                             "-PythonExecutable", python,
                                             "-UvExecutable", shutil.which("uv") or "uv",
                                             "-Helper", str(ROOT / "tools/ci/windows_wheel_smoke.py"),
                                             "-FastdbWheel", str(output / "wheels/fastdb"),
                                             "-CTwoWheel", str(output / "wheels/c-two"),
                                             "-C3", str(output / "cli/c3.exe"),
                                             "-Receipt", str(output / "installed-wheel-standard-user-full-receipt.v1.json")],
         ("windows-wheel-consumer",)),
        ("python-build", ["uv", "sync", "--locked", "--python", python], ()),
        ("windows-harness-tests", ["uv", "run", "--no-sync", "pytest",
                                   "tests/repo/test_windows_native_runner.py", "tests/repo/test_windows_wheel_smoke.py",
                                   "tests/repo/test_ipc_memory_benchmark.py",
                                   "-q", "--timeout=30", f"--junitxml={output / 'windows-harness-tests.xml'}"],
         ("python-build",)),
        # The matrix spawns real worker processes, so launch it only after the
        # harness tests proved the worker-dispatch protections and python-build
        # established the venv the driver passes to every child through its
        # default --python (sys.executable). Evidence lands under the run
        # output directory so the artifact manifest hashes bind it.
        (IPC_MEMORY_GATE, ["uv", "run", "--no-sync", "python", str(IPC_MEMORY_BENCHMARK),
                           "--mode", "matrix", "--output-dir", str(output / IPC_MEMORY_EVIDENCE_DIR),
                           "--memory-stats", "--require-memory-stats",
                           "--child-timeout", str(IPC_MEMORY_CHILD_TIMEOUT_S),
                           "--row-timeout", str(IPC_MEMORY_ROW_TIMEOUT_S)],
         ("python-build", "windows-harness-tests")),
        ("python-tests", ["uv", "run", "--no-sync", "pytest", "sdk/python/tests", "-q", "--timeout=30", *[f"--ignore={path}" for path in (*PORTABLE_TESTS, TYPESCRIPT_TEST)], f"--junitxml={output / 'python-tests.xml'}"], ("python-build", "fastdb-npm-build", "c2-mem-node-tests")),
        ("portable-tests", ["uv", "run", "--no-sync", "pytest", *PORTABLE_TESTS, "-q", "--timeout=300", f"--junitxml={output / 'portable-tests.xml'}"], ("python-build", "cli-artifact")),
        ("typescript-tests", ["uv", "run", "--no-sync", "pytest", TYPESCRIPT_TEST, "-q", "--timeout=600", f"--junitxml={output / 'typescript-tests.xml'}"], ("python-build", "fastdb-npm-install", "c2-mem-npm-install", "cli-artifact")),
    ]


def write_evidence(output: Path, evidence: dict[str, Any]) -> None:
    temporary = output / "run-evidence.json.tmp"
    temporary.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    temporary.replace(output / "run-evidence.json")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected-c-two-sha", required=True)
    parser.add_argument("--expected-fastdb-sha", required=True)
    parser.add_argument("--fastdb-system-lib-dir", type=Path, default=None,
                        help="absolute lib directory of the verified official FastDB "
                             "CoreSDK; supplies system link mode to core/RustSDK "
                             "consumer gates while app gates keep source mode")
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
        # Both matrices must exercise the retained cli/c3.exe bytes. Point them
        # at the artifact copy instead of a target-dir binary that any later
        # cargo invocation can relink, and require cli-artifact first.
        "C2_PORTABLE_MATRIX_C3_BIN": str(output / "cli/c3.exe"),
    })
    sources = {}
    for name, path, expected in (
        ("c-two", ROOT, options.expected_c_two_sha),
        ("fastdb", ROOT.parent / "fastdb", options.expected_fastdb_sha),
    ):
        result = capture(["git", "rev-parse", "HEAD"], path)
        sources[name] = {"expected_sha": expected, "actual_sha": result.get("output"), "verified": result.get("exit_code") == 0 and result.get("output") == expected}
    selected_gates = gates(sys.executable, output, options.scope)
    system_lib_dir: Path | None = None
    fastdb_sdk_error: str | None = None
    if options.fastdb_system_lib_dir is not None:
        system_lib_dir, fastdb_sdk_error = validate_system_lib_dir(options.fastdb_system_lib_dir)
    elif options.scope == FULL_SCOPE:
        # Core and the Rust SDK consume the registry fastdb-sys package, which
        # cannot build from source; the full scope is unrunnable without the
        # prepared system CoreSDK.
        fastdb_sdk_error = (
            "full scope requires --fastdb-system-lib-dir from "
            ".github/scripts/prepare_fastdb_sdk.py"
        )
    if options.scope == FULL_SCOPE:
        unknown_source_gates = sorted(SOURCE_MODE_GATES - {name for name, _, _ in selected_gates})
        if unknown_source_gates:
            fastdb_sdk_error = (
                f"source-mode gate names no longer exist: {', '.join(unknown_source_gates)}"
            )
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
        "fastdb_sdk": {
            "system_lib_dir": str(system_lib_dir) if system_lib_dir else None,
            "expected_library": platform_fastdb_library(),
            "source_mode_gates": sorted(SOURCE_MODE_GATES),
        },
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
    elif fastdb_sdk_error is not None:
        evidence["error"] = fastdb_sdk_error
    else:
        for name, command, dependencies in selected_gates:
            passed = {step["id"] for step in evidence["steps"] if step["status"] == "passed"}
            missing = [dependency for dependency in dependencies if dependency not in passed]
            if missing:
                record = {"id": name, "command": command, "status": "not_run", "reason": f"prerequisites did not pass: {', '.join(missing)}"}
            else:
                record = run_step(name, command, cwd=ROOT, output=output,
                                  environment=gate_environment(name, environment, system_lib_dir),
                                  timeout=GATE_TIMEOUT_S.get(name, STEP_TIMEOUT_S))
                if name == IPC_MEMORY_GATE and record["status"] == "passed":
                    problem = matrix_evidence_problem(output / IPC_MEMORY_EVIDENCE_DIR)
                    if problem is not None:
                        record["status"] = "failed"
                        record["evidence_problem"] = problem
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
