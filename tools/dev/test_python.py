#!/usr/bin/env python3
"""窄的本机 Python 验证入口：普通文件 loadfile，完整矩阵串行。

不安装/重建 native 或 c3，不接受 pytest 过滤参数。动态 harness 的编译
仍由既有矩阵 fixture 串行完成；运行期间不能另开 Cargo/npm/uv 写任务。
loadfile/worker 命名空间依据：https://pytest-xdist.readthedocs.io/en/stable/
distribution.html 和 how-to.html。Windows 继续使用既有 native gate。
"""

import argparse
from collections import Counter
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import uuid
from typing import Any, Iterator


ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
SDK = "sdk/python/tests"
PORTABLE = (
    f"{SDK}/integration/test_portable_payload_cross_language.py",
    f"{SDK}/integration/test_portable_payload_matrix.py",
)
TYPESCRIPT = f"{SDK}/integration/test_typescript_real_calls.py"
# The SDK pyproject is pytest's root, so nodeids start with tests/.
WINDOWS_ENDPOINT_TEST = "tests/unit/test_endpoint_context.py::test_windows_root_override_is_explicitly_unsupported_and_preserves_default"
FASTDB_SHA = "4f99f86a662b0e950a0dd29800c25a1c9fca4def"
PLUGIN = "tools.dev.test_python"


class RunnerError(Exception):
    pass


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest() if hasattr(hashlib, "file_digest") else hashlib.sha256(stream.read()).hexdigest()


def validate_partition(full: list[str], groups: dict[str, list[str]]) -> None:
    """比较实际收集 nodeids，拒绝遗漏、重叠、重复或空组。"""
    if not full or any(count != 1 for count in Counter(full).values()):
        raise RunnerError("全套收集为空或含重复 nodeid")
    if any(not ids for ids in groups.values()):
        raise RunnerError("分组收集为空")
    merged = Counter(node for ids in groups.values() for node in ids)
    if merged != Counter(full):
        missing = sorted((Counter(full) - merged).elements())
        extra = sorted((merged - Counter(full)).elements())
        raise RunnerError(f"分组不等于全套：missing={missing}, extra/duplicate={extra}")


def group_arguments(name: str) -> list[str]:
    if name == "full":
        return [SDK]
    if name == "ordinary":
        return [SDK, *[f"--ignore={path}" for path in (*PORTABLE, TYPESCRIPT)]]
    if name == "portable":
        return list(PORTABLE)
    if name == "typescript":
        return [TYPESCRIPT]
    raise RunnerError(f"未知组：{name}")


def build_pytest_command(python: str, name: str, directory: Path, workers: int, *, collect: bool = False) -> list[str]:
    command = [python, "-m", "pytest", "-p", PLUGIN, *group_arguments(name), "-q", "-rs", "-p", "no:cacheprovider"]
    parallel = name == "ordinary" and workers > 1 and not collect
    command += ["-n", str(workers if parallel else 0), "--dist=loadfile" if parallel else "--dist=no", "--max-worker-restart=0"]
    if collect:
        command += ["--collect-only"]
    else:
        seconds = {"ordinary": 30, "portable": 300, "typescript": 600}[name]
        command += [f"--timeout={seconds}", f"--basetemp={directory / 'tmp'}", f"--junitxml={directory / 'junit.xml'}"]
    return command


@contextmanager
def resource_lease(resource: Path) -> Iterator[None]:
    """只协调这个入口；不宣称能阻止不遵守租约的外部构建。"""
    import fcntl

    # TMPDIR is deliberately different between test runs. A resource shared
    # by those runs still needs the same lease directory.
    leases = Path.home() / ".cache" / "c-two" / "python-test-leases"
    leases.mkdir(parents=True, exist_ok=True)
    key = hashlib.sha256(str(resource.resolve()).encode()).hexdigest()
    # 不 unlink：等待者可能已打开同一 inode。
    with (leases / key).open("a+b") as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RunnerError(f"另一验证入口正使用资源：{resource}") from error
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def group_alive(pid: int) -> bool:
    try:
        os.killpg(pid, 0)
        return True
    except ProcessLookupError:
        return False


def stop_group(process: subprocess.Popen) -> None:
    def send(sig: int) -> None:
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
    send(signal.SIGINT)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    if group_alive(process.pid):
        send(signal.SIGKILL)
    process.wait(timeout=5)


def run_step(command: list[str], directory: Path, environment: dict[str, str], timeout: float) -> dict:
    directory.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    record = {"command": command, "status": "failed", "exit_code": None, "log": str(directory / "output.log"), "cleanup_confirmed": False}
    with (directory / "output.log").open("wb") as log:
        process = None
        try:
            process = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            record["pid"] = process.pid
            record["exit_code"] = process.wait(timeout=timeout)
            record["status"] = "passed" if process.returncode == 0 else "failed"
            # pytest/fixture 应正常 join。父进程退出不等于子进程已退出。
            deadline = time.monotonic() + 2
            while group_alive(process.pid) and time.monotonic() < deadline:
                time.sleep(0.05)
            if group_alive(process.pid):
                stop_group(process)
                record["status"] = "failed"
                record["error"] = "父进程退出后进程组仍存活，已强制清理；不能记成功"
            else:
                record["cleanup_confirmed"] = True
            if process.returncode < 0:
                record["status"] = "cancelled"
                record["cleanup_confirmed"] = False
                record["error"] = "子进程被信号终止；fixture清理未获确认"
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            record["status"] = "cancelled" if isinstance(error, KeyboardInterrupt) else "timed_out"
            if process is not None:
                try:
                    stop_group(process)
                except (OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as cleanup_error:
                    record["cleanup_error"] = str(cleanup_error)
                record["exit_code"] = process.returncode
            record["error"] = "取消/超时后清理未经行为验收；保留证据并停止后续组"
        except OSError as error:
            record["error"] = str(error)
        finally:
            record["duration_seconds"] = time.monotonic() - started
            record["process_exited"] = process is None or process.poll() is not None
    write_json(directory / "step.json", record)
    return record


def stage_environment(base: dict[str, str], directory: Path) -> dict[str, str]:
    return {**base, "CTWO_PYTHON_STAGE_DIR": str(directory)}


def collected(directory: Path) -> list[str]:
    value = json.loads((directory / "collected-master.json").read_text())
    if value["collection_problems"]:
        raise RunnerError(f"收集包含失败/skip：{value['collection_problems']}")
    return value["nodeids"]


def validate_execution(directory: Path, expected: list[str], workers: int) -> None:
    files = sorted(directory.glob("collected-gw*.json")) if workers > 1 else [directory / "collected-master.json"]
    if len(files) != workers:
        raise RunnerError(f"worker收集证据缺失：期望{workers}，实际{len(files)}")
    for path in files:
        value = json.loads(path.read_text())
        if value["nodeids"] != expected or value["collection_problems"]:
            raise RunnerError(f"运行时收集与计划不同：{path}")
    evidence = json.loads((directory / "reports.json").read_text())
    reports = evidence["reports"]
    observed = {row["nodeid"] for row in reports}
    if observed != set(expected):
        raise RunnerError(f"实际执行集合不同：missing={sorted(set(expected)-observed)}, extra={sorted(observed-set(expected))}")
    calls = Counter(row["nodeid"] for row in reports if row["when"] == "call")
    if any(count != 1 for count in calls.values()):
        raise RunnerError("同一nodeid被执行多次")
    platform_skips = {
        row["nodeid"] for row in reports
        if sys.platform != "win32" and row["nodeid"] == WINDOWS_ENDPOINT_TEST
        and row["when"] == "setup" and row["outcome"] == "skipped"
        and row.get("skip_reason") == "Skipped: Windows named-pipe platform contract"
    }
    problems = [row for row in reports if row["outcome"] != "passed" and not (
        row["nodeid"] in platform_skips and row["when"] == "setup" and row["outcome"] == "skipped"
    )]
    if problems or set(calls) != set(expected) - platform_skips or evidence["exit_status"] != 0:
        raise RunnerError(f"实际测试有失败/skip/未执行call：{problems}")
    if not (directory / "junit.xml").is_file():
        raise RunnerError("JUnit缺失")


PROBE = """
import hashlib, importlib.metadata, json, sys
import pytest, xdist, pytest_timeout, numpy, pandas, pyarrow, httpx, PIL
import c_two, c_two._native as native
from fastdb4py.payload import Payload
from pathlib import Path
print(json.dumps({'prefix': sys.prefix, 'executable': sys.executable,
 'python_sdk': c_two.__file__,
 'native': native.__file__, 'native_sha256': hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest(),
 'versions': {name: importlib.metadata.version(name) for name in ['pytest', 'pytest-xdist', 'pytest-timeout', 'fastdb4py']}}))
"""


def capture(command: list[str], environment: dict[str, str]) -> str:
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RunnerError(f"必要输入探针失败(exit {result.returncode})：{command}\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def preflight(options: argparse.Namespace, environment: dict[str, str]) -> dict:
    info = json.loads(capture([options.python, "-c", PROBE], environment).splitlines()[-1])
    if info["versions"]["fastdb4py"] != "0.2.1":
        raise RunnerError("需要固定fastdb4py==0.2.1")
    if not options.c3.is_file():
        raise RunnerError(f"预建c3不存在：{options.c3}")
    if Path(info["python_sdk"]).resolve().parent != (ROOT / "sdk/python/src/c_two").resolve():
        raise RunnerError("选定解释器的c_two Python源码不属于本checkout；需要外部预先安装本树editable包")
    info["c3_version"] = capture([str(options.c3), "--version"], environment)
    info["c3_sha256"] = sha256(options.c3)
    info["source_sha"] = capture(["git", "rev-parse", "HEAD"], environment)
    info["tracked_diff_sha256"] = hashlib.sha256(capture(["git", "diff", "--binary", "HEAD"], environment).encode()).hexdigest()
    info["runner_inputs"] = {str(path.relative_to(ROOT)): sha256(path) for path in (
        ROOT / "tools/dev/test_python.py", ROOT / "sdk/python/tests/conftest.py",
        ROOT / "pyproject.toml", ROOT / "uv.lock",
    )}
    # 与既有syntax gate一致，只查询，不安装。
    python310 = shutil.which("python3.10")
    if python310 is None:
        python310 = capture(["uv", "python", "find", "3.10"], environment)
    if not python310 or not capture([python310, "--version"], environment).startswith("Python 3.10."):
        raise RunnerError("需要预先安装可运行的Python3.10，不能跳过syntax gate")
    info["python310"] = python310
    fixture = Path(environment.get("C2_PORTABLE_MATRIX_FIXTURE_ROOT", ROOT.parent / "fastdb/tests/golden/payload/v1/spec/valid"))
    record = fixture / "record-all-types.source.json"
    graph = fixture / "graph-all-values.source.json" if "C2_PORTABLE_MATRIX_FIXTURE_ROOT" in environment else ROOT.parent / "fastdb/tests/golden/payload/v1/binary/spec/graph-all-values.source.json"
    for path in (record, graph):
        if not path.is_file():
            raise RunnerError(f"collection前需要golden fixture：{path}")
    info["fixtures"] = {str(path): sha256(path) for path in (record, graph)}
    for command in ("cargo", "node", "npm"):
        if shutil.which(command) is None:
            raise RunnerError(f"必要工具不存在：{command}")
    if not capture(["node", "--version"], environment).startswith("v22."):
        raise RunnerError("需要Node22")
    info["tools"] = {command: capture([command, "--version"], environment) for command in ("cargo", "node", "npm")}
    packages = [options.fastdb_package, options.c2_mem_package, options.typescript_package]
    if any(packages) and not all(packages):
        raise RunnerError("三个TS tarball必须一起指定")
    if all(packages):
        for path in packages:
            if not path.is_file():
                raise RunnerError(f"预建tarball不存在：{path}")
        info["tarballs"] = {str(path): sha256(path) for path in packages}
    else:
        fastdb = ROOT.parent / "fastdb"
        if capture(["git", "-C", str(fastdb), "rev-parse", "HEAD"], environment) != FASTDB_SHA:
            raise RunnerError("FastDB sibling源码SHA不匹配")
        if capture(["git", "-C", str(fastdb), "status", "--porcelain"], environment):
            raise RunnerError("FastDB sibling必须干净")
        for path in (
            fastdb / "ts/fastdb4ts/src/wasm/fastdb4ts.wasm",
            fastdb / "ts/fastdb4ts/node_modules/typescript/bin/tsc",
            ROOT / "core/foundation/c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc",
        ):
            if not path.is_file():
                raise RunnerError(f"必须外部先准备/build：{path}")
    if environment.get("FASTDB_PAYLOAD_LINK_MODE") != "system":
        raise RunnerError("Rust harness需要预配置CoreSDK system link环境")
    lib = Path(environment.get("FASTDB_PAYLOAD_SYSTEM_LIB_DIR", ""))
    library = "libfastdb.dylib" if sys.platform == "darwin" else "libfastdb.so"
    if not lib.is_absolute() or not (lib / library).is_file():
        raise RunnerError("需要已验证的绝对CoreSDK lib目录")
    info["coresdk_library"] = {"path": str(lib / library), "sha256": sha256(lib / library)}
    return info


def validate_receipts(output: Path, c3_hash: str) -> None:
    from tools.local_rc.portable_matrix_receipt import load_and_validate_receipt as portable
    from tools.local_rc.typescript_receipt import load_and_validate_receipt as typescript

    p = portable(output / "portable.v1.json", expected_stage="development")
    t = typescript(output / "typescript.v1.json", expected_stage="development")
    # 结构验证包含精确行集/顺序/负例；还绑定本次c3字节。
    if any(row["c3_sha256"] != c3_hash for row in p["rows"] if row["transport"] == "relay") or t["packages"]["c3_sha256"] != c3_hash:
        raise RunnerError("收据c3哈希与本次输入不一致")


def execute(options: argparse.Namespace, output: Path, base: dict[str, str], info: dict, evidence: dict) -> int:
    plans = {}
    for name in ("full", "ordinary", "portable", "typescript"):
        directory = output / "collection" / name
        print(f"collect-{name}: 开始", flush=True)
        record = run_step(build_pytest_command(options.python, name, directory, options.workers, collect=True), directory, stage_environment(base, directory), options.stage_timeout)
        evidence["steps"].append({"id": f"collect-{name}", **record})
        write_json(output / "run.json", evidence)
        if record["status"] == "cancelled":
            raise KeyboardInterrupt
        if record["status"] != "passed" or not record["cleanup_confirmed"]:
            raise RunnerError(f"{name}收集未成功，禁止开始测试")
        plans[name] = collected(directory)
    validate_partition(plans["full"], {name: plans[name] for name in ("ordinary", "portable", "typescript")})
    write_json(output / "plan.json", plans)
    failed = False
    for name in ("ordinary", "portable", "typescript"):
        directory = output / name
        print(f"{name}: 开始", flush=True)
        record = run_step(build_pytest_command(options.python, name, directory, options.workers), directory, stage_environment(base, directory), options.stage_timeout)
        try:
            validate_execution(directory, plans[name], options.workers if name == "ordinary" and options.workers > 1 else 1)
        except (RunnerError, OSError, ValueError, KeyError) as error:
            record["validation_error"] = str(error)
            if record["status"] == "passed":
                record["status"] = "failed"
        failed |= record["status"] != "passed"
        write_json(directory / "step.json", record)
        evidence["steps"].append({"id": name, **record})
        write_json(output / "run.json", evidence)
        print(f"{name}: {record['status']} (exit={record['exit_code']})", flush=True)
        if record["status"] == "cancelled":
            raise KeyboardInterrupt
        if not record["cleanup_confirmed"]:
            raise RunnerError(f"{name}取消/超时或有残留风险，停止后续组")
    validate_receipts(output, info["c3_sha256"])
    if sha256(options.c3) != info["c3_sha256"] or sha256(Path(info["native"])) != info["native_sha256"]:
        raise RunnerError("运行期间native/c3被另一任务替换")
    for relative, digest in info.get("runner_inputs", {}).items():
        if sha256(ROOT / relative) != digest:
            raise RunnerError(f"运行期间入口/fixture/依赖输入被替换：{relative}")
    for path, digest in {**info.get("fixtures", {}), **info.get("tarballs", {})}.items():
        if sha256(Path(path)) != digest:
            raise RunnerError(f"运行期间外部输入被替换：{path}")
    library = info.get("coresdk_library")
    if library and sha256(Path(library["path"])) != library["sha256"]:
        raise RunnerError("运行期间CoreSDK被替换")
    return 1 if failed else 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--python", default=sys.executable, help="已建native与全部测试依赖的解释器；不运行uv sync")
    result.add_argument("--c3", type=Path, required=True, help="预建c3的文件路径")
    result.add_argument("--output", type=Path, required=True, help="必须不存在的新证据目录")
    result.add_argument("--workers", type=int, choices=(1, 2, 4), default=2, help="普通组：1为-n0串行；2/4为loadfile，矩阵始终-n0")
    result.add_argument("--stage-timeout", type=float, default=3600, help="每收集/执行组的有限wall timeout，秒，最大14400")
    result.add_argument("--fastdb-package", type=Path)
    result.add_argument("--c2-mem-package", type=Path)
    result.add_argument("--typescript-package", type=Path)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if os.name != "posix":
        raise SystemExit("当前入口仅支持本机Unix；Windows保留tools/ci/windows_native.py")
    if not math.isfinite(args.stage_timeout) or not 0 < args.stage_timeout <= 14400:
        raise SystemExit("stage-timeout必须为(0,14400]内有限值")
    # 保留venv的python symlink路径；resolve会把它变成全局解释器。
    args.python = os.path.abspath(os.path.expanduser(args.python))
    args.c3 = args.c3.expanduser().resolve()
    args.output = args.output.expanduser().resolve()
    for key in ("fastdb_package", "c2_mem_package", "typescript_package"):
        if getattr(args, key):
            setattr(args, key, getattr(args, key).expanduser().resolve())
    if any(os.environ.get(key) for key in ("PYTEST_ADDOPTS", "PYTEST_PLUGINS", "PYTEST_DISABLE_PLUGIN_AUTOLOAD")):
        raise SystemExit("拒绝继承PYTEST_ADDOPTS/PYTEST_PLUGINS：请清除后运行完整入口")
    try:
        args.output.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        raise SystemExit(f"拒绝复用证据目录：{args.output}")
    namespace = uuid.uuid4().hex
    base = {key: value for key, value in os.environ.items() if not key.startswith(("C2_", "CTWO_", "PYTEST_XDIST_"))}
    # 唯一允许继承的fixture定位输入；其他candidate hash/receipt override不得混入。
    if os.environ.get("C2_PORTABLE_MATRIX_FIXTURE_ROOT"):
        base["C2_PORTABLE_MATRIX_FIXTURE_ROOT"] = str(Path(os.environ["C2_PORTABLE_MATRIX_FIXTURE_ROOT"]).expanduser().resolve())
    base.update({"C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": "", "PYTHONDONTWRITEBYTECODE": "1", "CTWO_TEST_RUN_NAMESPACE": namespace,
        "CTWO_RELAY_START_LOCK": str(args.output / "relay-start.lock"), "CTWO_TEST_C3_BIN": str(args.c3),
        "C2_PORTABLE_MATRIX_C3_BIN": str(args.c3), "C2_PORTABLE_MATRIX_RECEIPT": str(args.output / "portable.v1.json"),
        "C2_TYPESCRIPT_RECEIPT": str(args.output / "typescript.v1.json"), "C2_PORTABLE_MATRIX_EVIDENCE_STAGE": "development",
        "C2_TYPESCRIPT_EVIDENCE_STAGE": "development", "C2_TYPESCRIPT_FASTDB_SOURCE_SHA": FASTDB_SHA,
        "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost", "PYTHONUTF8": "1",
        "CARGO_BUILD_JOBS": "2", "CMAKE_BUILD_PARALLEL_LEVEL": "2", "CARGO_TARGET_DIR": str(ROOT / "core/target"),
        "OMP_NUM_THREADS": "1", "OPENBLAS_NUM_THREADS": "1", "MKL_NUM_THREADS": "1", "NUMEXPR_NUM_THREADS": "1",
        "PYTHONPATH": str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", "")})
    for key, value in zip(("C2_TYPESCRIPT_FASTDB_PACKAGE", "C2_TYPESCRIPT_C2_MEM_PACKAGE", "C2_TYPESCRIPT_COMPILER_PACKAGE"), (args.fastdb_package, args.c2_mem_package, args.typescript_package)):
        if value:
            base[key] = str(value)
    evidence = {"schema": "c-two.python-test-run.v1", "status": "running", "workers": args.workers, "namespace": namespace, "cpu_count": os.cpu_count(), "started_at": datetime.now(timezone.utc).isoformat(), "steps": []}
    write_json(args.output / "run.json", evidence)
    def cancel(*_: Any) -> None:
        raise KeyboardInterrupt
    previous = signal.signal(signal.SIGTERM, cancel)
    started = time.monotonic()
    code = 1
    try:
        info = preflight(args, base)
        evidence["inputs"] = info
        with ExitStack() as stack:
            resources = (ROOT, Path(info["prefix"]), ROOT / "core/target", ROOT / "core/foundation/c2-mem-ffi/bindings/typescript", ROOT.parent / "fastdb")
            for resource in sorted(set(resources)):
                stack.enter_context(resource_lease(resource))
            code = execute(args, args.output, base, info, evidence)
        evidence["status"] = "passed" if code == 0 else "failed"
    except (RunnerError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        evidence["status"] = "failed"
        evidence["error"] = str(error)
    except KeyboardInterrupt:
        evidence["status"] = "cancelled"
        evidence["error"] = "用户取消；未证明清理完成"
        code = 130
    finally:
        signal.signal(signal.SIGTERM, previous)
        evidence["finished_at"] = datetime.now(timezone.utc).isoformat()
        evidence["duration_seconds"] = time.monotonic() - started
        evidence["exit_code"] = code
        write_json(args.output / "run.json", evidence)
    print(f"{evidence['status']}: {args.output / 'run.json'}")
    return code


# 以下hook只在pytest显式-p本模块时使用；不改变全局addopts。
_REPORTS: list[dict] = []
_COLLECTION_PROBLEMS: list[str] = []


def pytest_configure(config: Any) -> None:
    if config.getini("addopts"):
        import pytest
        raise pytest.UsageError("此完整入口拒绝pytest全局addopts；不允许隐式过滤/排序/并行矩阵")


def pytest_collectreport(report: Any) -> None:
    if report.failed or report.skipped:
        _COLLECTION_PROBLEMS.append(str(report.longrepr))


def pytest_collection_finish(session: Any) -> None:
    directory = os.environ.get("CTWO_PYTHON_STAGE_DIR")
    if directory:
        worker = os.environ.get("PYTEST_XDIST_WORKER", "master")
        write_json(Path(directory) / f"collected-{worker}.json", {"nodeids": [item.nodeid for item in session.items], "collection_problems": _COLLECTION_PROBLEMS})


def pytest_runtest_logreport(report: Any) -> None:
    row = {"nodeid": report.nodeid, "when": report.when, "outcome": report.outcome}
    if report.skipped and isinstance(report.longrepr, tuple):
        row["skip_reason"] = str(report.longrepr[2])
    _REPORTS.append(row)


def pytest_sessionfinish(session: Any, exitstatus: Any) -> None:
    directory = os.environ.get("CTWO_PYTHON_STAGE_DIR")
    if directory and not hasattr(session.config, "workerinput"):
        write_json(Path(directory) / "reports.json", {"exit_status": int(exitstatus), "reports": _REPORTS})


if __name__ == "__main__":
    raise SystemExit(main())
