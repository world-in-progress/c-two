#!/usr/bin/env python3
"""Exercise the packaged Java SDK with only the installed JDK and candidate JAR."""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from tools.dev.test_platform import ProcessTree

from release_candidate import (
    JAVA_ARTIFACT, JAVA_NATIVE_LIBRARIES, JAVA_SMOKE_SCHEMA, MAVEN_GROUP, java_filenames,
    parse_sidecar, sha256_file, sidecar_for, validate_java_version,
)

DESCRIPTOR = ROOT / "sdk/java/src/test/resources/io/ctwo/generated/acceptance.contract.json"


def _run_checked(command: list[str], cwd: Path, env: dict[str, str], timeout: int) -> None:
    log_path = cwd / "step.log"
    with log_path.open("wb") as stream:
        tree = ProcessTree(command, cwd, env, stream)
        try:
            code = tree.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            tree.stop()
            raise
        finally:
            tree.close()
    output = log_path.read_text(encoding="utf-8", errors="replace")
    if output:
        sys.stdout.write(output)
    if code != 0:
        raise subprocess.CalledProcessError(code, command, output=output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--fastdb-version", required=True)
    parser.add_argument("--platform", choices=JAVA_NATIVE_LIBRARIES, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    jar_name, _, _, pom_name = java_filenames(args.version)
    main_jar = args.package_dir / jar_name
    delivered_pom = args.package_dir / pom_name
    jar_sha = sha256_file(main_jar)
    if jar_sha != parse_sidecar(sidecar_for(main_jar)):
        raise RuntimeError("Candidate main JAR differs from its checksum")
    if sha256_file(delivered_pom) != parse_sidecar(sidecar_for(delivered_pom)):
        raise RuntimeError("Candidate POM differs from its checksum")
    validate_java_version(args.fastdb_version)
    fastdb_jar = f"fastdb-java-{args.fastdb_version}.jar"
    maven = shutil.which("mvn.cmd" if os.name == "nt" else "mvn") or shutil.which("mvn")
    if not maven:
        raise RuntimeError("Maven is required")
    environment = os.environ.copy()
    for key in ("C2_JAVA_LIBRARY_PATH", "FASTDB_JAVA_LIBRARY_PATH", "JAVA_TOOL_OPTIONS",
                "JDK_JAVA_OPTIONS", "_JAVA_OPTIONS", "CLASSPATH", "MAVEN_ARGS", "MAVEN_OPTS"):
        environment.pop(key, None)

    with tempfile.TemporaryDirectory(prefix="c2-java-smoke-", dir=os.getenv("RUNNER_TEMP")) as directory:
        scratch = Path(directory)
        shutil.copyfile(Path(__file__).with_name("JavaPackageSmoke.java"), scratch / "JavaPackageSmoke.java")
        shutil.copyfile(DESCRIPTOR, scratch / "contract.json")
        contract = json.loads(DESCRIPTOR.read_text(encoding="utf-8"))
        echo = next(method for method in contract["methods"] if method["name"] == "echo")
        (scratch / "input-spec.json").write_text(json.dumps(echo["bindings"]["input"]["spec"]), encoding="utf-8")
        # Install the candidate JAR and delivered POM into an isolated local repository
        # so the consumer POM proves transitive resolution of fastdb-java via the delivered POM.
        _run_checked([
            maven, "-B", "-Dmaven.repo.local=repository",
            "org.apache.maven.plugins:maven-install-plugin:3.1.3:install-file",
            f"-Dfile={main_jar.resolve()}", f"-DpomFile={delivered_pom.resolve()}",
        ], cwd=scratch, env=environment, timeout=180)
        (scratch / "pom.xml").write_text(
            '<project xmlns="http://maven.apache.org/POM/4.0.0">'
            "<modelVersion>4.0.0</modelVersion>"
            f"<groupId>{MAVEN_GROUP}</groupId><artifactId>java-package-smoke</artifactId>"
            "<version>1</version><dependencies><dependency>"
            f"<groupId>{MAVEN_GROUP}</groupId><artifactId>{JAVA_ARTIFACT}</artifactId>"
            f"<version>{args.version}</version></dependency></dependencies></project>",
            encoding="utf-8",
        )
        ipc_dir = scratch / "ipc"
        ipc_dir.mkdir(mode=0o700)
        if os.name != "nt":
            os.chmod(ipc_dir, 0o700)
        (scratch / "native-tmp").mkdir()
        libs_dir = scratch / "libs"
        _run_checked([
            maven, "-B", "-Dmaven.repo.local=repository",
            "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:copy-dependencies",
            "-DincludeScope=runtime", f"-DoutputDirectory={libs_dir}",
        ], cwd=scratch, env=environment, timeout=300)
        resolved_jar = libs_dir / jar_name
        resolved_fastdb = libs_dir / fastdb_jar
        if not resolved_jar.is_file() or sha256_file(resolved_jar) != jar_sha:
            raise RuntimeError("Resolved candidate JAR is missing or differs from its checksum")
        if not resolved_fastdb.is_file():
            raise RuntimeError(f"Delivered POM did not transitively resolve {fastdb_jar}")
        dependencies = os.pathsep.join(str(path) for path in sorted(libs_dir.glob("*.jar")))
        _run_checked(["javac", "--release", "17", "-cp", dependencies, "JavaPackageSmoke.java"],
                     cwd=scratch, env=environment, timeout=60)
        _run_checked([
            "java", "-Xcheck:jni", "-Djava.awt.headless=true",
            f"-Djava.io.tmpdir={scratch / 'native-tmp'}",
            "-cp", os.pathsep.join((".", dependencies)), "JavaPackageSmoke", "contract.json", "input-spec.json",
        ], cwd=scratch, env=environment, timeout=60)
        if sha256_file(resolved_jar) != jar_sha:
            raise RuntimeError("Candidate main JAR changed during the smoke test")
        fastdb_sha = sha256_file(resolved_fastdb)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({
        "schema": JAVA_SMOKE_SCHEMA,
        "status": "passed",
        "source_commit_sha": args.source_sha,
        "platform": args.platform,
        "version": args.version,
        "fastdb_version": args.fastdb_version,
        "jar_sha256": jar_sha,
        "fastdb_jar_sha256": fastdb_sha,
    }, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
