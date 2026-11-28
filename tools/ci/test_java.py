#!/usr/bin/env python3
"""Build the Java SDK and verify the generated bindings with real peers."""

from __future__ import annotations

import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback
import xml.etree.ElementTree as ET
from zipfile import ZipFile


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.dev.test_platform import ProcessTree

SDK = ROOT / "sdk/java"
FIXTURES = SDK / "src/test/interop"
TARGETS = ("java", "python", "rust")
DIRECTIONS = (("java", "python"), ("python", "java"), ("java", "rust"), ("rust", "java"))
ROUTE = "java-interop"
EXECUTABLE_SUFFIX = ".exe" if os.name == "nt" else ""
JAVA_LIBRARY = {"win32": "c2_java.dll", "darwin": "libc2_java.dylib"}.get(sys.platform, "libc2_java.so")
RECEIPT = "java-validation-receipt.v1.json"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


@contextmanager
def launch(command, log, env, *, cwd=ROOT):
    with log.open("wb") as stream:
        stream.write((subprocess.list2cmdline(command) + "\n").encode("utf-8"))
        stream.flush()
        tree = ProcessTree(command, cwd, env, stream)
        try:
            yield tree
        finally:
            tree.close()


def wait_success(tree, log, deadline):
    code = tree.process.wait(timeout=max(0, deadline - time.monotonic()))
    require(code == 0, f"{log.name} exited with {code}")


def wait_for_line(tree, log, prefix, deadline):
    while time.monotonic() < deadline:
        for line in log.read_text(encoding="utf-8", errors="replace").splitlines(keepends=True)[1:]:
            if line.startswith(prefix) and line.endswith("\n"):
                return line.removeprefix(prefix).strip()
        if tree.process.poll() is not None:
            raise RuntimeError(f"{log.name} exited before {prefix!r} (exit {tree.process.returncode})")
        time.sleep(0.025)
    raise TimeoutError(f"{log.name} did not report {prefix!r}")


def run(command, log, env, timeout, *, cwd=ROOT):
    print(f"Running {log.stem}", flush=True)
    with launch(command, log, env, cwd=cwd) as tree:
        wait_success(tree, log, time.monotonic() + timeout)


def executable(name, java_home=None):
    if java_home:
        path = Path(java_home) / "bin" / (name + EXECUTABLE_SUFFIX)
        require(path.is_file(), f"JAVA_HOME does not contain {path.name}")
        return str(path)
    path = shutil.which(name)
    require(path is not None, f"{name} is missing from PATH")
    return path


def junit_evidence(reports):
    files = sorted(reports.glob("TEST-*.xml"))
    require(files, "Maven Surefire did not produce XML reports")
    totals = dict.fromkeys(("tests", "failures", "errors", "skipped"), 0)
    fastdb_jars = set()
    for file in files:
        suite = ET.parse(file).getroot()
        for field in totals:
            totals[field] += int(suite.attrib[field])
        # Use the classpath Maven resolved for the actual tests. This also
        # works with an offline or custom local repository.
        classpath = suite.find("./properties/property[@name='surefire.test.class.path']")
        if classpath is not None:
            for entry in classpath.attrib["value"].split(os.pathsep):
                jar = Path(entry)
                if jar.name == "fastdb-java-0.4.0.jar" and jar.is_file():
                    fastdb_jars.add(jar.resolve())
    require(totals["tests"] > 0 and not any(totals[key] for key in ("failures", "errors", "skipped")),
            f"Expected all JUnit tests to run and pass, got {totals}")
    require(len(fastdb_jars) == 1, f"Expected one resolved FastDB Java 0.4.0 JAR, got {fastdb_jars}")
    return totals, str(fastdb_jars.pop())


def verify_jars():
    target = SDK / "target"
    license_bytes = (ROOT / "LICENSE").read_bytes()
    jars = sorted(target.glob("c-two-sdk-*.jar"))
    mains = [jar for jar in jars if not jar.name.endswith(("-sources.jar", "-javadoc.jar"))]
    require(len(mains) == 1, f"Expected one Java SDK main JAR, got {mains}")
    main = mains[0]
    sources = main.with_name(main.stem + "-sources.jar")
    for path in (main, sources):
        with ZipFile(path) as jar:
            require(jar.read("META-INF/LICENSE") == license_bytes, f"{path.name} has an incorrect license")
            if path == sources:
                require(not any(name.startswith("META-INF/native/") for name in jar.namelist()),
                        f"{path.name} contains native binaries")
    return str(main)


def compile_java(javac, binding, fixture, classes, classpath, log, env, timeout):
    classes.mkdir()
    sources = sorted((binding / "java/src/main/java").rglob("*.java"))
    require(sources, "Java codegen produced no sources")
    run([javac, "--release", "17", "-encoding", "UTF-8", "-Xlint:all", "-Werror",
         "-cp", classpath, "-d", str(classes), *map(str, sources), str(fixture)],
        log, env, timeout)
    return [*java_flags(env), "-cp", os.pathsep.join((str(classes), classpath)), fixture.stem]


def java_flags(env):
    return [executable("java", env.get("JAVA_HOME")), "-Xcheck:jni", "-Djava.awt.headless=true",
            "-Dc2.java.library.path=" + str(Path(env["CARGO_TARGET_DIR"]) / "release" / JAVA_LIBRARY)]


def contract_identity(descriptor, trees):
    descriptor = descriptor.read_bytes()
    identity = json.loads(descriptor)["fingerprints"] | {
        "descriptor_sha256": hashlib.sha256(descriptor).hexdigest(),
    }
    sources = {
        "java": "java/src/main/java/io/ctwo/generated/test/interop/InteropContract.java",
        "python": "python/c_two_contract.py",
        "rust": "rust/c_two_contract.rs",
    }
    for target, tree in trees.items():
        metadata = tree / "metadata"
        canonical = (metadata / "contract.json").read_bytes().rstrip(b"\n")
        release = json.loads((metadata / "contract-release-ref.json").read_bytes())
        require(canonical == descriptor and release["descriptor_sha256"] == identity["descriptor_sha256"],
                f"{target} generated a different contract identity")
        binding = (tree / sources[target]).read_text(encoding="utf-8")
        for name, digest in identity.items():
            require(re.search(rf'\b{name.upper()}(?:\s*:\s*&str)?\s*=\s*"{digest}"', binding),
                    f"{target} generated a different {name}")
    return identity


def build_rust_peer(work, output, env, timeout):
    require(env.get("FASTDB_PAYLOAD_LINK_MODE", "system") == "system",
            "The Rust peer requires FASTDB_PAYLOAD_LINK_MODE=system")
    lib_dir = env.get("FASTDB_PAYLOAD_SYSTEM_LIB_DIR")
    require(lib_dir, "Set FASTDB_PAYLOAD_SYSTEM_LIB_DIR to the FastDB 0.4.0 CoreSDK lib directory")
    lib_dir = Path(lib_dir).resolve(strict=True)
    env = {**env, "FASTDB_PAYLOAD_LINK_MODE": "system", "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": str(lib_dir)}
    if sys.platform == "win32":
        path_keys = [key for key in env if key.upper() == "PATH"]
        existing = env[path_keys[0]] if path_keys else ""
        for key in path_keys:
            env.pop(key)
        env["PATH"] = os.pathsep.join(filter(None, (str(lib_dir), existing)))
    else:
        loader = "DYLD_LIBRARY_PATH" if sys.platform == "darwin" else "LD_LIBRARY_PATH"
        env[loader] = os.pathsep.join(filter(None, (str(lib_dir), env.get(loader))))
    sources = work / "src"
    sources.mkdir()
    for name in ("peer.rs", "echo.json", "number.json"):
        shutil.copyfile(FIXTURES / name, sources / ("main.rs" if name == "peer.rs" else name))
    manifest = work / "Cargo.toml"
    manifest.write_text(
        '[package]\nname = "c2-java-interop-peer"\nversion = "0.0.0"\nedition = "2024"\n'
        '[dependencies]\n'
        f'c-two = {{ path = {json.dumps((ROOT / "sdk/rust").as_posix(), ensure_ascii=False)} }}\n'
        'fastdb = "=0.4.0"\nsha2 = "=0.11.0"\n',
        encoding="utf-8",
    )
    shutil.copyfile(ROOT / "sdk/rust/Cargo.lock", work / "Cargo.lock")
    run(["cargo", "build", "--manifest-path", str(manifest)],
        output / "rust-peer-build.log", env, timeout, cwd=work)
    binary = Path(env["CARGO_TARGET_DIR"]) / "debug" / ("c2-java-interop-peer" + EXECUTABLE_SUFFIX)
    return [str(binary)], env


def verify_channel(host, client, transport, peers, c3, work, output, env, timeout):
    name = f"{host}_host-{client}_client-{transport}"
    row = {"id": name, "host": host, "client": client, "transport": transport, "status": "failed"}
    start = time.monotonic()
    deadline = start + timeout
    server_id = f"c2j-{work.name.removeprefix('c2j-')}-{host[0]}{client[0]}-{transport[0]}"
    stop = work / f"{name}.stop"
    host_log = output / f"{name}-host.log"
    client_log = output / f"{name}-client.log"
    java_args = [str(FIXTURES), transport]
    host_args = (java_args if host == "java" else []) + [server_id, str(stop)]
    try:
        with ExitStack() as processes:
            host_process = processes.enter_context(launch([*peers[host], "host", *host_args], host_log, env))
            endpoint = wait_for_line(host_process, host_log, "READY ", deadline)
            require(endpoint.startswith("ipc://"), f"{host} did not bind an IPC endpoint: {endpoint}")
            if transport == "relay":
                relay_args = [c3, "relay", "--bind", "127.0.0.1:0", "--relay-id", server_id,
                              "--upstream", f"{ROUTE}={server_id}@{endpoint}"]
                log = output / f"{name}-relay.log"
                relay = processes.enter_context(launch(relay_args, log, env))
                endpoint = wait_for_line(relay, log, "C-Two relay listening at ", deadline)
            if client == "java":
                client_args = [*java_args, endpoint]
            elif client == "rust":
                client_args = [transport, endpoint]
            else:
                client_args = [endpoint, str(work / "generated-python/python")]
            with launch([*peers[client], "client", *client_args], client_log, env) as process:
                wait_success(process, client_log, deadline)
                row["client_exit_code"] = process.process.returncode
            stop.touch()
            wait_success(host_process, host_log, deadline)
            row["host_exit_code"] = host_process.process.returncode
            require("PASS client operations" in client_log.read_text(encoding="utf-8", errors="replace"),
                    f"{client_log.name} did not confirm the client checks")
            require("PASS host shutdown" in host_log.read_text(encoding="utf-8", errors="replace"),
                    f"{host_log.name} did not confirm shutdown")
        row["status"] = "passed"
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        row["error"] = str(error)
    row["duration_seconds"] = round(time.monotonic() - start, 3)
    print(f"{name}: {row['status']}" + (f" ({row['error']})" if "error" in row else ""), flush=True)
    return row


def validate(args, output, work, env, receipt):
    timeout = args.timeout
    c3 = os.path.abspath(args.c3)
    python = os.path.abspath(args.python)
    for path in (c3, python):
        require(Path(path).is_file(), f"Prebuilt executable is missing: {path}")
    mvn = args.mvn or shutil.which("mvn.cmd" if os.name == "nt" else "mvn") or shutil.which("mvn")
    require(mvn, "Maven is missing from PATH. Supply --mvn or install Maven.")
    mvn = os.path.abspath(mvn)
    javac = executable("javac", env.get("JAVA_HOME"))
    native = ["--locked", "--manifest-path", str(SDK / "native/Cargo.toml")]
    run(["cargo", "test", *native], output / "java-native-test.log", env, timeout)
    run(["cargo", "build", "--release", *native], output / "java-native-build.log", env, timeout)
    log = output / "java-native-dependency-tree.log"
    run(["cargo", "tree", *native, "-e", "normal"], log, env, timeout)
    tree = log.read_text(encoding="utf-8", errors="replace")
    require(not re.search(r"\b(fastdb|fastdb-sys|iroh|iroh-relay|noq|quinn) v\d", tree),
            f"Unexpected Java JNI dependency in {log.name}")

    maven = [mvn, "-f", str(SDK / "pom.xml"), "-B"]
    if args.maven_offline or env.get("C2_JAVA_MAVEN_OFFLINE") == "1":
        maven.append("-o")
    reports = work / "junit"
    run([*maven, "test", java_flags(env)[-1], f"-Dc2.java.testReportsDir={reports}"],
        output / "java-junit.log", env, timeout)
    receipt["junit"], fastdb_jar = junit_evidence(reports)
    run([*maven, "-DskipTests", "package", "source:jar-no-fork"],
        output / "java-package.log", env, timeout)
    classpath = os.pathsep.join((verify_jars(), fastdb_jar))

    descriptor = SDK / "src/test/resources/io/ctwo/generated/acceptance.contract.json"
    binding = work / "generated-acceptance"
    reference = work / "acceptance-release-ref.json"
    run([c3, "contract", "codegen", "java", str(descriptor), "--out-dir", str(binding)],
        output / "acceptance-codegen.log", env, timeout)
    run([c3, "contract", "release-ref", str(descriptor), "--out", str(reference)],
        output / "acceptance-release-ref.log", env, timeout)
    acceptance = compile_java(javac, binding, ROOT / "cli/tests/fixtures/GeneratedJavaIpc.java",
                              work / "acceptance-classes", classpath,
                              output / "acceptance-javac.log", env, timeout)
    run([*acceptance, str(reference)], output / "generated-java-ipc.log", env, timeout)
    receipt["generated_java_ipc"] = "passed"

    py_peer = [python, "-E", "-B", str(FIXTURES / "peer.py")]
    descriptor = work / "interop.contract.json"
    run([*py_peer, "describe", str(descriptor)], output / "interop-descriptor.log", env, timeout)
    bindings = {}
    for target in TARGETS:
        bindings[target] = work / f"generated-{target}"
        run([c3, "contract", "codegen", target, str(descriptor), "--out-dir", str(bindings[target])],
            output / f"interop-codegen-{target}.log", env, timeout)
    run([*py_peer, "codegen-check", str(descriptor), str(bindings["java"])],
        output / "interop-python-java-codegen.log", env, timeout)
    receipt["contract"] = contract_identity(descriptor, bindings)
    receipt["contract"]["python_java_artifacts_identical"] = True

    java = compile_java(javac, bindings["java"], FIXTURES / "InteropPeer.java",
                        work / "interop-classes", classpath, output / "interop-javac.log", env, timeout)
    rust, env = build_rust_peer(work, output, env, timeout)
    peers = {"java": java, "python": py_peer, "rust": rust}
    for host, client in DIRECTIONS:
        for transport in ("direct", "relay"):
            receipt["channels"].append(
                verify_channel(host, client, transport, peers, c3, work, output, env, timeout)
            )
    passed = sum(row["status"] == "passed" for row in receipt["channels"])
    print(f"{passed}/8 cross-language channels passed", flush=True)
    require(passed == 8, "Cross-language matrix failed. See channel logs.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", required=True, help="Python with editable C-Two and FastDB 0.4.0")
    parser.add_argument("--c3", required=True, help="Prebuilt c3 executable")
    parser.add_argument("--output", required=True, type=Path, help="New or empty evidence directory")
    parser.add_argument("--mvn", help="Maven executable")
    parser.add_argument("--maven-offline", action="store_true")
    parser.add_argument("--timeout", type=int, default=900, help="Seconds per build step or matrix channel")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        parser.error("--output must be new or empty")
    start = time.monotonic()
    receipt = {"schema": "c-two.java-validation-receipt.v1", "status": "failed", "channels": []}
    env = {**os.environ, "CARGO_TARGET_DIR": str(Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "core/target")).resolve()),
           "C2_ENV_FILE": "", "C2_RELAY_ANCHOR_ADDRESS": "", "C2_RELAY_USE_PROXY": "false"}

    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"Received signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    try:
        # Keep Unix socket paths within sockaddr_un's limit, even when the
        # evidence directory lives deep inside a CI workspace.
        with tempfile.TemporaryDirectory(prefix="c2j-", dir="/tmp" if os.name != "nt" else None) as path:
            work = Path(path)
            if os.name != "nt":
                (work / "ipc").mkdir(mode=0o700)
                env["C2_IPC_ROOT"] = str(work / "ipc")
            validate(args, output, work, env, receipt)
        receipt["status"] = "passed"
        return 0
    except (Exception, KeyboardInterrupt) as error:
        receipt["error"] = str(error)
        (output / "driver-error.log").write_text(traceback.format_exc(), encoding="utf-8")
        print(f"Java validation failed. {error} See {output}.", file=sys.stderr)
        return 1
    finally:
        receipt["duration_seconds"] = round(time.monotonic() - start, 3)
        (output / RECEIPT).write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
