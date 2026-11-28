#!/usr/bin/env python3
"""Exercise the packaged Java SDK with only the installed JDK and candidate JAR."""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from release_candidate import (
    JAVA_NATIVE_LIBRARIES, JAVA_SMOKE_SCHEMA, MAVEN_GROUP, java_filenames, parse_sidecar,
    sha256_file, sidecar_for, validate_java_version,
)

ROOT = Path(__file__).resolve().parents[2]
DESCRIPTOR = ROOT / "sdk/java/src/test/resources/io/ctwo/generated/acceptance.contract.json"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--fastdb-version", required=True)
    parser.add_argument("--platform", choices=JAVA_NATIVE_LIBRARIES, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    main_jar = args.package_dir / java_filenames(args.version)[0]
    jar_sha = sha256_file(main_jar)
    if jar_sha != parse_sidecar(sidecar_for(main_jar)):
        raise RuntimeError("Candidate main JAR differs from its checksum")
    validate_java_version(args.fastdb_version)
    fastdb_jar = f"fastdb-java-{args.fastdb_version}.jar"
    maven = shutil.which("mvn")
    if not maven:
        raise RuntimeError("Maven is required")
    environment = os.environ.copy()
    for key in ("C2_JAVA_LIBRARY_PATH", "FASTDB_JAVA_LIBRARY_PATH", "JAVA_TOOL_OPTIONS",
                "JDK_JAVA_OPTIONS", "_JAVA_OPTIONS", "CLASSPATH", "MAVEN_ARGS", "MAVEN_OPTS"):
        environment.pop(key, None)

    with tempfile.TemporaryDirectory(prefix="c2-java-smoke-", dir=os.getenv("RUNNER_TEMP")) as directory:
        scratch = Path(directory)
        jar = scratch / main_jar.name
        shutil.copyfile(main_jar, jar)
        shutil.copyfile(Path(__file__).with_name("JavaPackageSmoke.java"), scratch / "JavaPackageSmoke.java")
        shutil.copyfile(DESCRIPTOR, scratch / "contract.json")
        contract = json.loads(DESCRIPTOR.read_text(encoding="utf-8"))
        echo = next(method for method in contract["methods"] if method["name"] == "echo")
        (scratch / "input-spec.json").write_text(json.dumps(echo["bindings"]["input"]["spec"]), encoding="utf-8")
        (scratch / "pom.xml").write_text(
            '<project xmlns="http://maven.apache.org/POM/4.0.0">'
            "<modelVersion>4.0.0</modelVersion>"
            f"<groupId>{MAVEN_GROUP}</groupId><artifactId>java-package-smoke</artifactId>"
            "<version>1</version></project>", encoding="utf-8",
        )
        (scratch / "ipc").mkdir()
        (scratch / "native-tmp").mkdir()
        subprocess.run([maven, "-B", "-Dmaven.repo.local=repository",
                        "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:copy",
                        f"-Dartifact={MAVEN_GROUP}:fastdb-java:{args.fastdb_version}", "-DoutputDirectory=."],
                       cwd=scratch, env=environment, check=True, timeout=300)
        dependencies = os.pathsep.join((jar.name, fastdb_jar))
        subprocess.run(["javac", "--release", "17", "-cp", dependencies, "JavaPackageSmoke.java"],
                       cwd=scratch, env=environment, check=True, timeout=60)
        subprocess.run(["java", "-Xcheck:jni", "-Djava.awt.headless=true",
                        f"-Djava.io.tmpdir={scratch / 'native-tmp'}",
                        "-cp", os.pathsep.join((".", dependencies)), "JavaPackageSmoke", "contract.json", "input-spec.json"],
                       cwd=scratch, env=environment, check=True, timeout=60)
        if sha256_file(jar) != jar_sha:
            raise RuntimeError("Candidate main JAR changed during the smoke test")
        fastdb_sha = sha256_file(scratch / fastdb_jar)

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
