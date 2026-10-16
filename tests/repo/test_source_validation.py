from __future__ import annotations

import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import sys
from types import SimpleNamespace


def _runner():
    path = Path(__file__).resolve().parents[2] / "tools/ci/source_validation.py"
    spec = importlib.util.spec_from_file_location("source_validation", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _benchmark():
    path = Path(__file__).resolve().parents[2] / "tools/benchmarks/ipc_memory.py"
    spec = importlib.util.spec_from_file_location("_ipc_memory_benchmark_gate_test", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_failure_keeps_exit_code_and_does_not_prevent_next_gate(tmp_path):
    runner = _runner()
    arguments = {"cwd": tmp_path, "output": tmp_path, "environment": os.environ.copy()}
    failure = runner.run_step("fails", [sys.executable, "-c", "print('compiler evidence'); raise SystemExit(7)"], **arguments)
    success = runner.run_step("next", [sys.executable, "-c", "print('next gate ran')"], **arguments)
    assert failure["status"] == "failed" and failure["exit_code"] == 7
    assert "compiler evidence" in (tmp_path / "fails.log").read_text()
    assert success["status"] == "passed" and success["exit_code"] == 0


def test_timeout_records_failure_and_reaps_child(tmp_path):
    record = _runner().run_step(
        "timeout", [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(60)"],
        cwd=tmp_path, output=tmp_path, environment=os.environ.copy(), timeout=0.3,
    )
    assert record["status"] == "timed_out"
    assert record["process_exited"] is True
    assert record["exit_code"] != 0
    assert "started" in (tmp_path / "timeout.log").read_text()


def test_failed_prerequisite_marks_scope_failed_and_runs_independent_gate(tmp_path, monkeypatch):
    runner = _runner()
    # Exercise the Windows evidence orchestration with real portable child
    # commands; this test does not claim to execute a Windows native backend.
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        ("wheel", [sys.executable, "-c", "print('wheel built')"], ()),
        ("install", [sys.executable, "-c", "raise SystemExit(7)"], ()),
        ("dependent", [sys.executable, "-c", "raise AssertionError('must not run')"], ("wheel", "install")),
        ("independent", [sys.executable, "-c", "print('independent executed')"], ()),
    ])
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "local-platform", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert evidence["status"] == "failed"
    assert evidence["scope"] == "local-platform"
    assert evidence["applicable_gates"] == ["wheel", "install", "dependent", "independent"]
    assert [step["status"] for step in evidence["steps"]] == ["passed", "failed", "not_run", "passed"]
    assert evidence["steps"][1]["exit_code"] == 7
    assert not (output / "dependent.log").exists()
    assert "independent executed" in (output / "independent.log").read_text()


def test_full_scope_preserves_coverage_and_reuses_prepared_outputs():
    runner = _runner()
    inventories = {}
    for platform_name in ("linux", "macos", "windows"):
        gates = runner.gates(sys.executable, Path("evidence"), runner.FULL_SCOPE, target_platform=platform_name)
        names = [name for name, _, _ in gates]
        assert len(set(names)) == len(names)
        inventories[platform_name] = set(names)
        index = {name: position for position, name in enumerate(names)}
        assert runner.SOURCE_MODE_GATES <= set(names)
        assert index["cli-build"] < index["cli-test"] < index["cli-artifact"] < index["python-suite"]
        for name, command, _ in gates[index["cli-artifact"]:]:
            assert not any("cli/Cargo.toml" in part for part in command), name
        dependencies = {name: set(required) for name, _, required in gates}
        assert dependencies["cli-artifact"] == {"cli-test"}
        assert dependencies["fastdb-npm-build"] == {"fastdb-npm-install", "fastdb-wasm-build"}
        assert dependencies["c2-mem-node-tests"] == {"c2-mem-build"}
        assert dependencies["c2-mem-node-package"] == {"c2-mem-build"}
        assert {"python-build", "cli-artifact", "python310-install", "typescript-packages"} <= dependencies["python-suite"]
        assert dependencies[runner.IPC_MEMORY_GATE] == {"python-build", "source-harness-tests"}
        commands = {name: command for name, command, _ in gates}
        assert "--ignore-scripts" in commands["c2-mem-node-tests"]
        assert "--ignore-scripts" in commands["c2-mem-npm-install"]
        assert "--prepared" in commands["c2-mem-node-package"]
        assert "--workers" in commands["python-suite"] and "--fastdb-package" in commands["python-suite"]
        assert "--features" in commands["core-test"] and "c2-http/relay" in commands["core-test"]
        assert "--all-targets" in commands["core-check"]
        assert "--all-features" in commands["rust-sdk-test"]
        assert "--all-features" in commands["fastdb-rust-test"]
        assert "fastdb4py==0.2.2" in commands["fastdb-wheel-build"]
        assert "../fastdb" not in commands["fastdb-wheel-build"]
        assert "--lib" not in commands["core-test"]
        assert all(not deps for name, _, deps in gates if name in runner.INDEPENDENT_SETUP_GATES)
    assert inventories["linux"] == inventories["macos"]
    assert inventories["windows"] - inventories["linux"] == {"windows-standard-user-consumer"}
    assert inventories["linux"] <= inventories["windows"]
    local = runner.gates(sys.executable, Path("evidence"), runner.LOCAL_PLATFORM_SCOPE)
    assert [name for name, _, _ in local] == ["local-platform-tests"]
    assert "c2-core" in local[0][1] and "c2-http/relay" in local[0][1] and "--lib" not in local[0][1]
    python = {name for name, _, _ in runner.gates(sys.executable, Path("evidence"), runner.PYTHON_SCOPE)}
    assert {"python-suite", "python-native-test", "python-build", "cli-artifact", "python310-install"} <= python
    assert "core-test" not in python and "python-wheel-build" not in python


def test_ipc_memory_matrix_gate_runs_full_required_stats_bounded_matrix():
    runner = _runner()
    gate_list = runner.gates(sys.executable, Path("evidence"), runner.FULL_SCOPE)
    command, dependencies = next(
        (command, deps) for name, command, deps in gate_list if name == runner.IPC_MEMORY_GATE
    )
    # The driver runs inside the python-build venv, so its default --python
    # (sys.executable) gives every spawned worker the same venv interpreter.
    assert command[:5] == ["uv", "run", "--no-sync", "python",
                           str(runner.ROOT / "tools/benchmarks/ipc_memory.py")]
    assert "--python" not in command
    def flag(name):
        return command[command.index(name) + 1]
    assert flag("--mode") == "matrix"
    assert Path(flag("--output-dir")) == Path("evidence") / runner.IPC_MEMORY_EVIDENCE_DIR
    assert "--memory-stats" in command
    assert "--require-memory-stats" in command
    # The complete table must run: no subset selector, no post-shutdown exemption.
    assert "--only" not in command
    assert "--allow-unavailable-stats-after-shutdown" not in command
    assert dependencies == ("python-build", "source-harness-tests")
    child_timeout = float(flag("--child-timeout"))
    row_timeout = float(flag("--row-timeout"))
    benchmark = _benchmark()
    rows = benchmark.matrix_rows()
    assert len(rows) == 9
    assert 0 < child_timeout <= benchmark.MAX_CHILD_TIMEOUT_S
    assert 0 < row_timeout <= benchmark.MAX_ROW_TIMEOUT_S
    step_timeout = runner.GATE_TIMEOUT_S[runner.IPC_MEMORY_GATE]
    assert math.isfinite(step_timeout)
    assert math.isfinite(runner.STEP_TIMEOUT_S)
    # The step bound must cover every row consuming its whole row budget.
    assert step_timeout >= len(rows) * row_timeout


def test_gate_environment_selects_system_mode_except_for_app_gates(tmp_path):
    runner = _runner()
    lib_dir = tmp_path / "coresdk" / "lib"
    lib_dir.mkdir(parents=True)
    base = {
        "FASTDB_PAYLOAD_LINK_MODE": "system",
        "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": "/ambient/runner/lib",
        "PATH": "/usr/bin",
    }
    system = runner.gate_environment("core-test", base, lib_dir)
    assert system["FASTDB_PAYLOAD_LINK_MODE"] == "system"
    assert system["FASTDB_PAYLOAD_SYSTEM_LIB_DIR"] == str(lib_dir)
    assert system["PATH"].startswith(f"{lib_dir}{runner.os.pathsep}")
    for gate in runner.SOURCE_MODE_GATES:
        child = runner.gate_environment(gate, base, lib_dir)
        assert child["FASTDB_PAYLOAD_LINK_MODE"] == "source", gate
        assert "FASTDB_PAYLOAD_SYSTEM_LIB_DIR" not in child, gate
        assert child["PATH"] == "/usr/bin"
    assert runner.gate_environment("core-test", base, None) == base


def test_validate_system_lib_dir_requires_absolute_directory_with_library(tmp_path):
    runner = _runner()
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / runner.platform_fastdb_library()).write_bytes(b"host library")
    resolved, problem = runner.validate_system_lib_dir(prepared)
    assert problem is None and resolved is not None
    relative = runner.validate_system_lib_dir(Path("coresdk/lib"))
    assert relative[0] is None and "absolute" in relative[1]
    empty = runner.validate_system_lib_dir(tmp_path / "missing")
    assert empty[0] is None and "missing" in empty[1]
    without_library = tmp_path / "empty-lib"
    without_library.mkdir()
    result = runner.validate_system_lib_dir(without_library)
    assert result[0] is None and "missing" in result[1]


def test_full_scope_requires_prepared_system_sdk(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        (name, [sys.executable, "-c", "print('gate')"], ()) for name in runner.SOURCE_MODE_GATES
    ])
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert evidence["status"] == "failed"
    assert "--fastdb-system-lib-dir" in evidence["error"]
    assert evidence["fastdb_sdk"]["system_lib_dir"] is None


def test_full_scope_rejects_sdk_without_the_platform_library(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: [
        (name, [sys.executable, "-c", "print('gate')"], ()) for name in runner.SOURCE_MODE_GATES
    ])
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert "fastdb.lib" in evidence["error"]


def test_full_scope_children_receive_split_link_modes(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    probe = "import os; print(os.environ.get('FASTDB_PAYLOAD_LINK_MODE')); print(os.environ.get('FASTDB_PAYLOAD_SYSTEM_LIB_DIR'))"
    stubbed = [("core-test", [sys.executable, "-c", probe], ())]
    stubbed += [
        (name, [sys.executable, "-c", probe if name == "cli-build" else "print('source gate')"], ())
        for name in sorted(runner.SOURCE_MODE_GATES)
    ]
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: stubbed)
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / "fastdb.lib").write_bytes(b"import library")
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    assert result == 0
    core_log = (output / "core-test.log").read_text()
    assert core_log.splitlines()[:2] == ["system", str(prepared.resolve())]
    cli_log = (output / "cli-build.log").read_text()
    assert cli_log.splitlines()[:2] == ["source", "None"]


def test_full_scope_fails_when_source_mode_gate_names_disappear(tmp_path, monkeypatch):
    runner = _runner()
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    real_gates = runner.gates
    renamed = [
        (name if name != "cli-build" else "cli-compile", command, dependencies)
        for name, command, dependencies in real_gates(sys.executable, Path("evidence"), runner.FULL_SCOPE)
    ]
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: renamed)
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True)
    (prepared / "fastdb.lib").write_bytes(b"import library")
    output = tmp_path / "evidence"
    result = runner.main([
        "--scope", "full", "--output", str(output),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ])
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1
    assert "cli-build" in evidence["error"] and "no longer exist" in evidence["error"]


def _stub_windows_main(monkeypatch, runner, tmp_path, stubbed, output_name="evidence"):
    """Run main() on Windows with stubbed gates and a prepared system CoreSDK."""
    monkeypatch.setattr(runner, "os", SimpleNamespace(name="nt", environ=os.environ, walk=os.walk))
    monkeypatch.setattr(runner.platform, "machine", lambda: "AMD64")
    monkeypatch.setattr(runner, "ROOT", tmp_path)
    monkeypatch.setattr(runner, "capture", lambda command, cwd: {"exit_code": 0, "output": "a" * 40})
    monkeypatch.setattr(runner, "gates", lambda python, output, scope: stubbed)
    prepared = tmp_path / "coresdk" / "lib"
    prepared.mkdir(parents=True, exist_ok=True)
    (prepared / "fastdb.lib").write_bytes(b"import library")
    return [
        "--scope", "full", "--output", str(tmp_path / output_name),
        "--expected-c-two-sha", "a" * 40, "--expected-fastdb-sha", "a" * 40,
        "--fastdb-system-lib-dir", str(prepared),
    ]


def test_ipc_memory_matrix_gate_requires_harness_gates_to_pass(tmp_path, monkeypatch):
    runner = _runner()
    launched = []
    real_run_step = runner.run_step

    def spy(name, command, **kwargs):
        launched.append(name)
        return real_run_step(name, command, **kwargs)

    monkeypatch.setattr(runner, "run_step", spy)
    stubbed = [
        ("python-build", [sys.executable, "-c", "print('venv built')"], ()),
        ("source-harness-tests", [sys.executable, "-c", "raise SystemExit(3)"], ("python-build",)),
        (runner.IPC_MEMORY_GATE,
         [sys.executable, "-c", "raise AssertionError('benchmark must not launch')"],
         ("python-build", "source-harness-tests")),
        ("independent-after-matrix", [sys.executable, "-c", "print('still executed')"], ()),
    ]
    stubbed += [
        (name, [sys.executable, "-c", "print('source gate')"], ())
        for name in sorted(runner.SOURCE_MODE_GATES - {"python-build"})
    ]
    result = runner.main(_stub_windows_main(monkeypatch, runner, tmp_path, stubbed))
    output = tmp_path / "evidence"
    evidence = json.loads((output / "run-evidence.json").read_text())
    assert result == 1 and evidence["status"] == "failed"
    steps = {step["id"]: step for step in evidence["steps"]}
    assert steps["source-harness-tests"]["status"] == "failed"
    assert steps["source-harness-tests"]["exit_code"] == 3
    assert steps[runner.IPC_MEMORY_GATE]["status"] == "not_run"
    assert "source-harness-tests" in steps[runner.IPC_MEMORY_GATE]["reason"]
    assert steps["independent-after-matrix"]["status"] == "passed"
    assert runner.IPC_MEMORY_GATE not in launched
    assert not (output / f"{runner.IPC_MEMORY_GATE}.log").exists()
    assert "still executed" in (output / "independent-after-matrix.log").read_text()


def test_ipc_memory_matrix_gate_status_binds_full_required_stats_evidence(tmp_path, monkeypatch):
    runner = _runner()
    benchmark = _benchmark()
    digest = "f" * 64

    def complete_body():
        specs = benchmark.matrix_rows()
        rows = [
            {"name": spec["name"], "workload": spec["workload"], "config": spec["config"],
             "ok": True, "terminated_by_matrix": False, "exit_code": 0,
             "worker_count": spec["workers"], "native_sha256": digest,
             "result_file": f"rows/{spec['name']}.json"}
            for spec in specs
        ]
        return {
            "schema": "c-two.ipc-memory-matrix.v1", "complete": True,
            "memory_stats": True, "require_memory_stats": True,
            "allow_unavailable_stats_after_shutdown": False,
            "rows": rows, "rows_not_selected": [], "failures": [],
            "totals": {"rows_total": len(rows), "rows_selected": len(rows),
                       "rows_not_selected": 0, "passed": len(rows), "failed": 0},
            "native": {"per_row": {spec["name"]: digest for spec in specs},
                       "rows_missing_provenance": [], "rows_inconsistent": [],
                       "single_installed_hash": digest, "agreement": True},
        }

    def mutated(change):
        body = complete_body()
        change(body)
        return body

    def mark_missing_native(body):
        name = body["rows"][0]["name"]
        body["native"]["per_row"][name] = None
        body["native"].update(rows_missing_provenance=[name], agreement=False, single_installed_hash=None)

    def mark_inconsistent_native(body):
        name = body["rows"][0]["name"]
        body["rows"][0]["native_sha256"] = "a" * 64
        body["native"]["per_row"][name] = f"inconsistent:{digest},{'a' * 64}"
        body["native"].update(rows_inconsistent=[name], agreement=False, single_installed_hash=None)

    header_only = {key: value for key, value in complete_body().items() if key in {
        "schema", "complete", "memory_stats", "require_memory_stats",
        "allow_unavailable_stats_after_shutdown",
    }}
    full_writer = (
        "import json, pathlib, sys; "
        "target = pathlib.Path(sys.argv[1]); target.mkdir(parents=True, exist_ok=True); "
        "matrix = json.loads(sys.argv[2]); "
        "(target / 'matrix.json').write_text(json.dumps(matrix)); "
        "rows_dir = target / 'rows'; rows_dir.mkdir(exist_ok=True); "
        "[(rows_dir / (row['name'] + '.json')).write_text(json.dumps(row)) for row in matrix.get('rows', [])]"
    )
    decoy_writer = (
        "import json, pathlib, sys; "
        "target = pathlib.Path(sys.argv[1]); target.mkdir(parents=True, exist_ok=True); "
        "matrix = json.loads(sys.argv[2]); "
        "(target / 'matrix.json').write_text(json.dumps(matrix)); "
        "rows_dir = target / 'rows'; rows_dir.mkdir(exist_ok=True); "
        "[(rows_dir / (row['name'] + '.json')).write_text(json.dumps(row)) for row in matrix.get('rows', [])]; "
        "(rows_dir / 'decoy.json').write_text('{}')"
    )
    matrix_only_writer = (
        "import json, pathlib, sys; "
        "target = pathlib.Path(sys.argv[1]); target.mkdir(parents=True, exist_ok=True); "
        "(target / 'matrix.json').write_text(json.dumps(json.loads(sys.argv[2])))"
    )
    empty_writer = "import pathlib, sys; pathlib.Path(sys.argv[1]).mkdir(parents=True, exist_ok=True)"

    variants = {
        "complete": (complete_body(), "passed", full_writer),
        "windows_result_paths": (mutated(lambda body: [
            row.update(result_file=row["result_file"].replace("/", "\\"))
            for row in body["rows"]
        ]), "passed", full_writer),
        # Header flags alone prove nothing: the pre-fix acceptance hole.
        "header_only": (header_only, "failed", full_writer),
        "only_subset": (mutated(lambda body: body.update(complete=False)), "failed", full_writer),
        "stats_optional": (mutated(lambda body: body.update(memory_stats=False)), "failed", full_writer),
        "stats_unrequired": (mutated(lambda body: body.update(require_memory_stats=False)), "failed", full_writer),
        "shutdown_exempt": (mutated(lambda body: body.update(allow_unavailable_stats_after_shutdown=True)), "failed", full_writer),
        "empty_rows": (mutated(lambda body: body.update(rows=[])), "failed", full_writer),
        "missing_row": (mutated(lambda body: body["rows"].pop()), "failed", full_writer),
        "duplicate_row": (mutated(lambda body: body["rows"].append(dict(body["rows"][0]))), "failed", full_writer),
        "failed_row": (mutated(lambda body: body["rows"][0].update(ok=False)), "failed", full_writer),
        "terminated_row": (mutated(lambda body: body["rows"][0].update(terminated_by_matrix=True)), "failed", full_writer),
        "bool_exit_code": (mutated(lambda body: body["rows"][0].update(exit_code=False)), "failed", full_writer),
        "float_exit_code": (mutated(lambda body: body["rows"][0].update(exit_code=0.0)), "failed", full_writer),
        "bool_totals": (mutated(lambda body: body["totals"].update(passed=True)), "failed", full_writer),
        "native_absent": (mutated(mark_missing_native), "failed", full_writer),
        "native_inconsistent": (mutated(mark_inconsistent_native), "failed", full_writer),
        "escaping_result_file": (mutated(lambda body: body["rows"][0].update(result_file="../matrix.json")), "failed", full_writer),
        "escaping_windows_result_file": (mutated(lambda body: body["rows"][0].update(result_file="..\\matrix.json")), "failed", full_writer),
        "row_files_absent": (complete_body(), "failed", matrix_only_writer),
        "wrong_result_file": (mutated(lambda body: body["rows"][0].update(result_file="rows/decoy.json")), "failed", decoy_writer),
        "missing_result_file": (mutated(lambda body: body["rows"][0].update(result_file="rows/absent.json")), "failed", full_writer),
        "matrix_json_absent": (complete_body(), "failed", empty_writer),
    }

    timeouts = {}
    real_run_step = runner.run_step

    def spy(name, command, **kwargs):
        timeouts[name] = kwargs["timeout"]
        return real_run_step(name, command, **kwargs)

    monkeypatch.setattr(runner, "run_step", spy)
    expected_names = [spec["name"] for spec in benchmark.matrix_rows()]
    for label, (body, expected_status, writer) in variants.items():
        # Fresh output per variant so stale evidence files cannot leak between runs.
        output = tmp_path / f"evidence-{label}"
        stubbed = [
            ("python-build", [sys.executable, "-c", "print('venv built')"], ()),
            (runner.IPC_MEMORY_GATE,
             [sys.executable, "-c", writer, str(output / runner.IPC_MEMORY_EVIDENCE_DIR), json.dumps(body)],
             ("python-build",)),
        ]
        stubbed += [
            (name, [sys.executable, "-c", "print('source gate')"], ())
            for name in sorted(runner.SOURCE_MODE_GATES - {"python-build"})
        ]
        result = runner.main(_stub_windows_main(monkeypatch, runner, tmp_path, stubbed, output_name=f"evidence-{label}"))
        evidence = json.loads((output / "run-evidence.json").read_text())
        step = next(s for s in evidence["steps"] if s["id"] == runner.IPC_MEMORY_GATE)
        assert step["status"] == expected_status, label
        assert (result == 0) == (expected_status == "passed"), label
        # The benchmark exit alone never decides acceptance: every negative here
        # exits 0 and must still be recorded as a failed gate.
        assert step["exit_code"] == 0, label
        assert timeouts[runner.IPC_MEMORY_GATE] == runner.IPC_MEMORY_STEP_TIMEOUT_S, label
        if expected_status == "passed":
            assert "evidence_problem" not in step
            matrix_artifact = [a for a in evidence["artifacts"]
                               if a["path"] == f"{runner.IPC_MEMORY_EVIDENCE_DIR}/matrix.json"]
            assert len(matrix_artifact) == 1
            matrix_bytes = (output / runner.IPC_MEMORY_EVIDENCE_DIR / "matrix.json").read_bytes()
            assert matrix_artifact[0]["sha256"] == hashlib.sha256(matrix_bytes).hexdigest()
            row_artifacts = {a["path"] for a in evidence["artifacts"]
                             if a["path"].startswith(f"{runner.IPC_MEMORY_EVIDENCE_DIR}/rows/")}
            assert row_artifacts == {
                f"{runner.IPC_MEMORY_EVIDENCE_DIR}/rows/{name}.json" for name in expected_names
            }
        else:
            assert step["evidence_problem"], label


def test_independent_setup_overlaps_but_consumers_wait(tmp_path, monkeypatch):
    import threading
    runner = _runner()
    first, second = threading.Event(), threading.Event()
    completed = []
    stubbed = [("python310-install", [], ()), ("fastdb-npm-install", [], ()),
               ("consumer", [], ("python310-install", "fastdb-npm-install"))]
    stubbed += [(name, [], ()) for name in sorted(runner.SOURCE_MODE_GATES)]
    args = _stub_windows_main(monkeypatch, runner, tmp_path, stubbed)
    def execute(name, command, **kwargs):
        if name == "python310-install":
            first.set()
            assert second.wait(2), "independent npm setup was serialized"
        elif name == "fastdb-npm-install":
            second.set()
            assert first.wait(2), "independent Python setup was serialized"
        elif name == "consumer":
            assert {"python310-install", "fastdb-npm-install"} <= set(completed)
        completed.append(name)
        return {"id": name, "status": "passed", "exit_code": 0}
    monkeypatch.setattr(runner, "run_step", execute)
    assert runner.main(args) == 0


def test_artifact_inventory_retains_proofs_without_disposable_build_trees(tmp_path):
    paths = ["cli/c3", "python-suite/run.json", "python-suite/portable.v1.json",
             "python-suite/ordinary/junit.xml", "python-suite/typescript/output.log",
             "python-suite/portable/tmp/build/unneeded", "python-suite/typescript/tmp/node_modules/typescript/index.js"]
    for name in paths:
        p = tmp_path / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("evidence")
    retained = {p.relative_to(tmp_path).as_posix() for p in _runner().retained_artifacts(tmp_path)}
    assert retained == set(paths[:5])


def test_wasm_build_keeps_tracked_loader_and_installs_matching_dist_pair(tmp_path, monkeypatch):
    import subprocess
    path = Path(__file__).resolve().parents[2] / "tools/ci/source_prerequisites.py"
    spec = importlib.util.spec_from_file_location("source_prerequisites", path)
    assert spec is not None and spec.loader is not None
    prerequisites = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(prerequisites)
    root = tmp_path / 'c-two'
    root.mkdir()
    fastdb = tmp_path / 'fastdb'
    original = fastdb / 'ts/fastdb4ts/src/wasm/fastdb4ts.js'
    original.parent.mkdir(parents=True)
    original.write_text('tracked loader')
    subprocess.run(['git', 'init', '-q', str(fastdb)], check=True)
    subprocess.run(['git', '-C', str(fastdb), 'add', '.'], check=True)
    subprocess.run(['git', '-C', str(fastdb), '-c', 'user.name=CI Test', '-c', 'user.email=ci@example.test', 'commit', '-qm', 'fixture'], check=True)
    monkeypatch.setattr(prerequisites, 'ROOT', root)
    monkeypatch.setenv('EMSDK', str(tmp_path / 'emsdk'))
    actual_run = prerequisites.run
    builds = []
    def simulated_compile(command, *, cwd, **kwargs):
        if command[0] == 'git':
            return actual_run(command, cwd=cwd, **kwargs)
        if command[0] == 'cmake':
            generated = cwd / 'ts/fastdb4ts/src/wasm'
            (generated / 'fastdb4ts.js').write_text('platform loader')
            (generated / 'fastdb4ts.wasm').write_bytes(b'matching wasm')
            builds.append(cwd)
        elif '--prefix' in command:
            dist = fastdb / 'ts/fastdb4ts/dist/wasm'
            dist.mkdir(parents=True)
            (dist / 'fastdb4ts.js').write_text(original.read_text())
        return None
    monkeypatch.setattr(prerequisites, 'run', simulated_compile)
    prerequisites.wasm()
    prerequisites.wasm()  # A complete pinned cache needs no second compilation.
    assert len(builds) == 1 and builds[0] != fastdb
    assert original.read_text() == 'tracked loader'
    prerequisites.fastdb_package()
    dist = fastdb / 'ts/fastdb4ts/dist/wasm'
    assert (dist / 'fastdb4ts.js').read_text() == 'platform loader'
    assert (dist / 'fastdb4ts.wasm').read_bytes() == b'matching wasm'
    assert original.read_text() == 'tracked loader'


def test_loopback_http_fixture_does_not_require_reverse_dns(monkeypatch):
    from http.client import HTTPConnection
    from http.server import BaseHTTPRequestHandler
    import socket
    import threading
    path = Path(__file__).resolve().parents[2] / 'sdk/python/tests/fixtures/http_server.py'
    spec = importlib.util.spec_from_file_location('loopback_http_fixture', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    def unavailable(*args):
        raise AssertionError('numeric loopback fixture attempted reverse DNS')
    monkeypatch.setattr(socket, 'getfqdn', unavailable)
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'ok')
        def log_message(self, *args):
            pass
    server = module.LoopbackHTTPServer(('127.0.0.1', 0), Handler)
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    client = HTTPConnection('127.0.0.1', server.server_port, timeout=2)
    try:
        client.request('GET', '/')
        response = client.getresponse()
        assert response.status == 200 and response.read() == b'ok'
        assert server.server_name == '127.0.0.1'
    finally:
        client.close()
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
    assert not worker.is_alive()
