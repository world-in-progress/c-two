"""Portable helpers for the build-and-validate-only release candidate workflow.

Subcommands:

``checksum``
    Write ``<file>.sha256`` sidecars in the ``<digest>  <name>`` form without
    depending on ``shasum`` being present (Windows Git Bash, runners).
``verify``
    Fail unless every listed file has a sidecar whose digest matches the bytes.
``inspect-sdist``
    Validate the actual member tree of the built sdist: required members, the
    rewritten in-archive maturin manifest, and the closure of path edges that
    are actually reachable from it. Maturin rewrites registry dependencies it
    had to patch into in-archive path dependencies and prunes workspace
    members the root crate cannot reach, so the inspector follows reachable
    dependency and ``workspace = true`` inheritance edges from the maturin
    ``manifest-path`` crate instead of requiring every manifest in the archive
    to be self-contained. Path escapes and missing reachable Rust sources are
    still rejected.
``manifest``
    Aggregate the immutable, source-bound candidate manifest: every retained
    artifact with its SHA-256, the exact expected CLI/wheel/sdist matrix,
    receipt counts, and a passing-status gate on every receipt that carries
    one. Product packages, FastDB dependency-proof inputs, and installer
    assets are classified separately so FastDB archives can never be counted
    as C-Two publishables.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path, PurePosixPath

MANIFEST_SCHEMA = "c-two.release-candidate.v1"
SDIST_REPORT_SCHEMA = "c-two.release-candidate.sdist-inspection.v1"
DEPENDENCY_TABLES = ("dependencies", "build-dependencies", "dev-dependencies")
# Host-provided installers are retained once under their canonical release
# names; both the repository names and the canonical names classify as the
# installer asset kind, never as CLI executables or product packages.
INSTALLER_NAMES = ("install-c3.sh", "install-c3.ps1",
                   "c3-installer.sh", "c3-installer.ps1")
FASTDB_PROOF_NAMES = ("fastdb-LICENSE", "fastdb-THIRD_PARTY_NOTICES.txt")
PASSING_RECEIPT_STATUSES = ("passed", "executed")
MAVEN_GROUP = "io.github.world-in-progress"
JAVA_ARTIFACT = "c-two-sdk"
JAVA_NATIVE_LIBRARIES = {
    "linux-x86_64": "libc2_java.so",
    "macos-arm64": "libc2_java.dylib",
    "windows-x86_64": "c2_java.dll",
}
JAVA_INSPECT_SCHEMA = "c-two.release-candidate.java-inspection.v1"
JAVA_SMOKE_SCHEMA = "c-two.release-candidate.java-smoke.v1"
POM_NAMESPACE = {"m": "http://maven.apache.org/POM/4.0.0"}


class HelperError(RuntimeError):
    """A validation failed; the candidate must not be accepted."""


# Strict ASCII-only SemVer 2.0 subset for c3 (no leading zeros, no build metadata).
C3_VERSION_PATTERN = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$",
    re.ASCII,
)
_C3_PRERELEASE_RANK = {"alpha": 0, "beta": 1, "rc": 2}


def parse_c3_version(value: str) -> tuple[int, int, int, str | None, int | None]:
    """Parse and validate a c3 SemVer 2.0 version (stable or alpha/beta/rc)."""
    if not isinstance(value, str) or not value or not value.isascii():
        raise HelperError(f"c3 version {value!r} must be a non-empty ASCII SemVer string")
    match = C3_VERSION_PATTERN.fullmatch(value)
    if match is None:
        raise HelperError(
            f"c3 version {value!r} must match X.Y.Z or X.Y.Z-{{alpha,beta,rc}}.N "
            "(strict ASCII SemVer 2.0 without leading zeros)"
        )
    major, minor, patch = int(match.group(1)), int(match.group(2)), int(match.group(3))
    pre_kind = match.group(4)
    pre_num = int(match.group(5)) if match.group(5) is not None else None
    return major, minor, patch, pre_kind, pre_num


def validate_c3_version(value: str) -> bool:
    """Validate c3 version and return True when it is a pre-release."""
    _, _, _, pre_kind, _ = parse_c3_version(value)
    return pre_kind is not None


def c3_version_sort_key(value: str) -> tuple[int, int, int, int, int, int]:
    """Return a total SemVer 2.0 precedence key for validated c3 versions."""
    major, minor, patch, pre_kind, pre_num = parse_c3_version(value)
    if pre_kind is None:
        return (major, minor, patch, 1, 3, 0)
    return (major, minor, patch, 0, _C3_PRERELEASE_RANK[pre_kind], pre_num or 0)


def validate_c_two_version(value: str) -> bool:
    """Validate canonical PEP 440 c-two version (stable or a/b/rc) and return is_prerelease."""
    if not isinstance(value, str) or not value or not value.isascii():
        raise HelperError(f"c-two version {value!r} must be a non-empty ASCII PEP 440 string")
    from packaging.version import InvalidVersion, Version

    try:
        parsed = Version(value)
    except InvalidVersion as error:
        raise HelperError(f"c-two version {value!r} is not valid PEP 440: {error}") from error
    if str(parsed) != value:
        raise HelperError(
            f"c-two version {value!r} is not canonical PEP 440 (expected {str(parsed)!r})"
        )
    if (
        parsed.epoch != 0
        or parsed.dev is not None
        or parsed.local is not None
        or parsed.post is not None
        or len(parsed.release) != 3
        or (parsed.pre is not None and parsed.pre[0] not in ("a", "b", "rc"))
    ):
        raise HelperError(
            f"c-two version {value!r} must be X.Y.Z or X.Y.Z{{a,b,rc}}N "
            "(epoch 0, no dev/post/local segments)"
        )
    return bool(parsed.is_prerelease)


def validate_java_version(value: str) -> bool:
    """Validate the Java version and return whether it is a pre-release."""
    if not isinstance(value, str) or not C3_VERSION_PATTERN.fullmatch(value):
        raise HelperError(f"Java version {value!r} must match X.Y.Z or X.Y.Z-{{alpha,beta,rc}}.N")
    return "-" in value


def read_pom(path: Path) -> ET.Element:
    try:
        project = ET.parse(path).getroot()
    except (ET.ParseError, OSError) as error:
        raise HelperError(f"cannot read Maven POM {path.name}: {error}") from error
    if project.tag != f"{{{POM_NAMESPACE['m']}}}project":
        raise HelperError(f"{path.name} is missing the Maven project namespace")
    return project


def verify_fastdb_java(project: ET.Element, version: str) -> None:
    dependencies = project.findall("m:dependencies/m:dependency", POM_NAMESPACE)
    fastdb = [dependency for dependency in dependencies
              if dependency.findtext("m:groupId", namespaces=POM_NAMESPACE) == MAVEN_GROUP
              and dependency.findtext("m:artifactId", namespaces=POM_NAMESPACE) == "fastdb-java"]
    if (len(fastdb) != 1
            or fastdb[0].findtext("m:version", namespaces=POM_NAMESPACE) != version
            or fastdb[0].findtext("m:scope", "compile", POM_NAMESPACE) != "compile"):
        raise HelperError(f"POM must declare exactly one compile dependency on {MAVEN_GROUP}:fastdb-java:{version}")


def read_repo_versions(root: Path) -> tuple[str, str, str]:
    """Read and validate the Python, CLI and Java versions from repository manifests."""
    import tomllib

    pyproject = tomllib.loads((root / "sdk" / "python" / "pyproject.toml").read_text(encoding="utf-8"))
    cargo = tomllib.loads((root / "cli" / "Cargo.toml").read_text(encoding="utf-8"))
    c_two_version = pyproject.get("project", {}).get("version")
    c3_version = cargo.get("package", {}).get("version")
    validate_c_two_version(c_two_version)
    validate_c3_version(c3_version)
    java_version = read_pom(root / "sdk/java/pom.xml").findtext("m:version", namespaces=POM_NAMESPACE)
    validate_java_version(java_version)
    return c_two_version, c3_version, java_version


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sidecar_for(path: Path) -> Path:
    return path.with_name(path.name + ".sha256")


def artifact_entry(path: Path) -> dict:
    return {"name": path.name, "bytes": path.stat().st_size, "sha256": sha256_file(path)}


def parse_sidecar(sidecar: Path) -> str:
    text = sidecar.read_text(encoding="utf-8").strip()
    digest = text.split(None, 1)[0] if text else ""
    if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest.lower()):
        raise HelperError(f"Malformed sidecar: {sidecar}")
    return digest.lower()


def command_checksum(options: argparse.Namespace) -> int:
    for name in options.files:
        path = Path(name)
        sidecar = sidecar_for(path)
        sidecar.write_text(f"{sha256_file(path)}  {path.name}\n", encoding="utf-8")
        print(f"checksum {sidecar}")
    return 0


def command_verify(options: argparse.Namespace) -> int:
    failures: list[str] = []
    for name in options.files:
        path = Path(name)
        sidecar = sidecar_for(path)
        if not sidecar.is_file():
            failures.append(f"missing sidecar for {path.name}")
            continue
        try:
            expected = parse_sidecar(sidecar)
        except HelperError as error:
            failures.append(str(error))
            continue
        actual = sha256_file(path)
        if actual != expected:
            failures.append(f"digest mismatch for {path.name}: sidecar {expected}, actual {actual}")
    if failures:
        for failure in failures:
            print(f"error: {failure}", file=sys.stderr)
        return 1
    print(f"verified {len(options.files)} artifact(s) against sidecars")
    return 0


def _dependency_specs(document: dict) -> list[tuple[str, dict]]:
    """Yield ``(dependency name, spec)`` from every dependency table.

    Covers plain and ``[target.<cfg>.…]`` tables so target-gated path
    dependencies (for example Windows-only crates) are followed too.
    """
    specs: list[tuple[str, dict]] = []
    documents = [document]
    documents.extend(target for target in document.get("target", {}).values()
                     if isinstance(target, dict))
    for source in documents:
        for table in DEPENDENCY_TABLES:
            for name, value in source.get(table, {}).items():
                if isinstance(value, dict):
                    specs.append((name, value))
    return specs


def _find_workspace_root(manifest: str, members: dict, read) -> tuple[str, dict] | None:
    """Nearest enclosing ``Cargo.toml`` with a ``[workspace]`` table.

    Mirrors cargo's ancestor walk for dependency inheritance. A manifest can
    be its own workspace root when it declares ``[workspace]`` alongside
    ``[package]``.
    """
    import tomllib

    parts = PurePosixPath(manifest).parts
    for depth in range(len(parts) - 1, 0, -1):
        candidate = str(PurePosixPath(*parts[:depth], "Cargo.toml"))
        if candidate in members:
            document = tomllib.loads(read(candidate).decode("utf-8"))
            if "workspace" in document:
                return candidate, document
    return None


def _resolve_dependency_directory(base: PurePosixPath, relative: str) -> PurePosixPath | None:
    """Join ``relative`` onto ``base`` and normalize ``..`` segments.

    Returns ``None`` when the normalized path escapes the archive root.
    """
    stack: list[str] = []
    for part in (*base.parts, *PurePosixPath(relative).parts):
        if part in (".", "/"):
            continue
        if part == "..":
            if not stack or stack[-1] == "..":
                stack.append("..")
            else:
                stack.pop()
        else:
            stack.append(part)
    if stack and stack[0] == "..":
        return None
    return PurePosixPath(*stack)


def inspect_sdist(archive: Path) -> dict:
    import tomllib

    report: dict = {
        "schema": SDIST_REPORT_SCHEMA,
        "archive": archive.name,
        "missing": [],
        "errors": [],
        "path_dependencies": [],
    }
    with tarfile.open(archive, "r:gz") as tar:
        members = {member.name: member for member in tar.getmembers() if member.isfile()}

        def read(name: str) -> bytes:
            handle = tar.extractfile(members[name])
            if handle is None:
                raise HelperError(f"Cannot read archive member: {name}")
            return handle.read()

        tops = {PurePosixPath(name).parts[0] for name in members}
        stray = [name for name in members if len(PurePosixPath(name).parts) < 2]
        if len(tops) != 1 or stray:
            raise HelperError(f"sdist must have exactly one root directory: {sorted(tops)}")
        root = tops.pop()
        report["root"] = root

        for required in ("PKG-INFO", "pyproject.toml"):
            if f"{root}/{required}" not in members:
                report["missing"].append(required)
        if report["missing"]:
            report["status"] = "failed"
            return report

        pyproject = tomllib.loads(read(f"{root}/pyproject.toml").decode("utf-8"))
        project = pyproject.get("project", {})
        name, version = project.get("name"), project.get("version")
        if not name or not version:
            report["errors"].append("pyproject.toml is missing project.name or project.version")
            report["status"] = "failed"
            return report
        report["project"] = {"name": name, "version": version}

        pkg_info: dict[str, str] = {}
        for line in read(f"{root}/PKG-INFO").decode("utf-8").splitlines():
            if not line.strip():
                break
            if ": " in line:
                key, value = line.split(": ", 1)
                pkg_info[key] = value
        normalized = pkg_info.get("Name", "").replace("_", "-").lower()
        if normalized != name.replace("_", "-").lower() or pkg_info.get("Version") != version:
            report["errors"].append(
                f"PKG-INFO identity {pkg_info.get('Name')} {pkg_info.get('Version')} "
                f"differs from pyproject {name} {version}")

        # Maturin rewrites this path inside the sdist to the embedded crate
        # tree (for example "c-two/sdk/python/native/Cargo.toml"), so it must
        # be read from the archive's own pyproject, never from the repository.
        manifest_path = pyproject.get("tool", {}).get("maturin", {}).get("manifest-path", "Cargo.toml")
        report["manifest_path"] = manifest_path
        manifest_member = str(PurePosixPath(root, *PurePosixPath(manifest_path).parts))
        if manifest_member not in members:
            report["missing"].append(manifest_path)
            report["status"] = "failed"
            return report

        pending = [manifest_member]
        visited: set[str] = set()
        workspace_roots: dict[str, tuple[str, dict] | None] = {}
        while pending:
            cargo = pending.pop()
            if cargo in visited:
                continue
            visited.add(cargo)
            document = tomllib.loads(read(cargo).decode("utf-8"))
            base = PurePosixPath(cargo).parent
            if str(base) not in workspace_roots:
                workspace_roots[str(base)] = _find_workspace_root(cargo, members, read)
            workspace = workspace_roots[str(base)]
            workspace_manifest = workspace[0] if workspace else None
            workspace_document = workspace[1] if workspace else None

            edges: list[tuple[str, str, PurePosixPath]] = [
                (dependency, spec["path"], base)
                for dependency, spec in _dependency_specs(document)
                if "path" in spec
            ]
            if workspace_document is not None:
                workspace_directory = PurePosixPath(workspace_manifest).parent
                workspace_dependencies = workspace_document.get("workspace", {}).get("dependencies", {})
                for dependency, spec in _dependency_specs(document):
                    inherited = workspace_dependencies.get(dependency)
                    if spec.get("workspace") is True and isinstance(inherited, dict) and "path" in inherited:
                        edges.append((dependency, inherited["path"], workspace_directory))

            for dependency, relative, base_dir in sorted(edges):
                target = _resolve_dependency_directory(base_dir, relative)
                if target is None:
                    report["errors"].append(
                        f"{cargo} dependency {dependency} path {relative} escapes the archive root")
                    continue
                directory = str(target)
                cargo_name = f"{directory}/Cargo.toml"
                sources = [member for member in members
                           if member.startswith(f"{directory}/src/") and member.endswith(".rs")]
                report["path_dependencies"].append({
                    "from": cargo,
                    "name": dependency,
                    "directory": directory,
                    "cargo_manifest": cargo_name in members,
                    "rust_sources": len(sources),
                })
                if cargo_name not in members:
                    report["missing"].append(f"{relative} ({cargo_name})")
                elif not sources:
                    report["errors"].append(
                        f"embedded crate {directory} (from {cargo}) has no .rs files under src/")
                else:
                    pending.append(cargo_name)
        report["cargo_manifests"] = len(visited)
        report["workspace_roots"] = sorted(
            {manifest for manifest, _ in (value for value in workspace_roots.values() if value)})
    report["status"] = "passed" if not report["missing"] and not report["errors"] else "failed"
    return report


def command_inspect_sdist(options: argparse.Namespace) -> int:
    report = inspect_sdist(Path(options.archive))
    if options.source_sha:
        report["source_commit_sha"] = options.source_sha
    if options.output:
        Path(options.output).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n",
                                        encoding="utf-8")
    print(json.dumps({key: report[key] for key in ("status", "missing", "errors")}, indent=2))
    return 0 if report["status"] == "passed" else 1


def java_filenames(version: str) -> tuple[str, str, str, str]:
    validate_java_version(version)
    prefix = f"{JAVA_ARTIFACT}-{version}"
    return tuple(f"{prefix}{suffix}" for suffix in (".jar", "-sources.jar", "-javadoc.jar", ".pom"))


def verify_java_native_header(archive: zipfile.ZipFile, platform: str, member: str) -> None:
    with archive.open(member) as stream:
        header = stream.read(64)
        if platform == "linux-x86_64":
            matches = (len(header) == 64 and header[:6] == b"\x7fELF\x02\x01"
                       and int.from_bytes(header[18:20], "little") == 62)
        elif platform == "macos-arm64":
            matches = (len(header) >= 32 and header[:4] == b"\xcf\xfa\xed\xfe"
                       and int.from_bytes(header[4:8], "little") == 0x0100000c)
        else:
            matches = False
            if len(header) == 64 and header[:2] == b"MZ":
                offset = int.from_bytes(header[60:64], "little")
                if 64 <= offset <= archive.getinfo(member).file_size - 24:
                    stream.seek(offset)
                    matches = stream.read(6) == b"PE\0\0\x64\x86"
    if not matches:
        raise HelperError(f"{member} has an invalid {platform} binary header")


def inspect_java(root: Path, version: str, fastdb_version: str, source_sha: str) -> dict:
    main, sources, javadoc, pom = (root / name for name in java_filenames(version))
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise HelperError("Java inspection requires an exact source commit SHA")
    library_members = {platform: f"META-INF/native/{platform}/{library}"
                       for platform, library in JAVA_NATIVE_LIBRARIES.items()}
    try:
        for path, required in (
            (main, {"io/ctwo/Runtime.class", "META-INF/LICENSE", *library_members.values()}),
            (sources, {"io/ctwo/Runtime.java", "META-INF/LICENSE"}),
            (javadoc, {"index.html"}),
        ):
            with zipfile.ZipFile(path) as archive:
                names = archive.namelist()
                if len(names) != len(set(names)):
                    raise HelperError(f"{path.name} has duplicate members")
                missing = required - set(names)
                if missing:
                    raise HelperError(f"{path.name} is missing {sorted(missing)}")
                if path == sources and any(name.startswith("META-INF/native/") for name in names):
                    raise HelperError("sources JAR contains META-INF/native/ entries")
                if path == main:
                    for platform, member in library_members.items():
                        verify_java_native_header(archive, platform, member)
    except (OSError, zipfile.BadZipFile) as error:
        raise HelperError(f"cannot inspect Java packages: {error}") from error

    project = read_pom(pom)
    for field, expected in (("groupId", MAVEN_GROUP), ("artifactId", JAVA_ARTIFACT), ("version", version)):
        if project.findtext(f"m:{field}", namespaces=POM_NAMESPACE) != expected:
            raise HelperError(f"POM {field} must be {expected!r}")
    verify_fastdb_java(project, fastdb_version)
    return {
        "schema": JAVA_INSPECT_SCHEMA,
        "source_commit_sha": source_sha,
        "version": version,
        "fastdb_version": fastdb_version,
        "files": {path.name: sha256_file(path) for path in (main, sources, javadoc, pom)},
        "status": "passed",
    }


def command_inspect_java(options: argparse.Namespace) -> int:
    report = inspect_java(Path(options.root), options.version, options.expect_fastdb_version, options.source_sha)
    Path(options.output).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"inspected Java packages for {options.version}")
    return 0


def verify_java_receipts(java: list[dict], receipts: dict, meta: dict, source_sha: str) -> None:
    version = meta.get("java_version")
    filenames = java_filenames(version)
    hashes = {entry["name"]: entry["sha256"] for entry in java}
    if len(java) != len(hashes) or set(hashes) != set(filenames):
        raise HelperError(f"Java inventory must contain exactly {list(filenames)}")
    expected_receipts = {"java-inspect.json", *(f"java-smoke-{platform}.json" for platform in JAVA_NATIVE_LIBRARIES)}
    if {name for name in receipts if name.startswith(("java-inspect", "java-smoke-"))} != expected_receipts:
        raise HelperError(f"Java evidence must contain exactly {sorted(expected_receipts)}")

    def require_receipt(name: str, schema: str) -> dict:
        receipt = receipts.get(name)
        if (not isinstance(receipt, dict) or receipt.get("schema") != schema
                or receipt.get("status") != "passed"
                or receipt.get("source_commit_sha") != source_sha
                or receipt.get("version") != version
                or receipt.get("fastdb_version") != meta.get("fastdb_version")):
            raise HelperError(f"{name} does not bind the passing Java and FastDB versions to {source_sha}")
        return receipt

    if require_receipt("java-inspect.json", JAVA_INSPECT_SCHEMA).get("files") != hashes:
        raise HelperError("java-inspect.json SHA-256 inventory differs from the retained Java files")
    for platform in JAVA_NATIVE_LIBRARIES:
        name = f"java-smoke-{platform}.json"
        receipt = require_receipt(name, JAVA_SMOKE_SCHEMA)
        if receipt.get("platform") != platform or receipt.get("jar_sha256") != hashes[filenames[0]]:
            raise HelperError(f"{name} does not bind the retained main JAR SHA-256 and platform")


def _python_abi_pair(python: str) -> tuple[str, str]:
    """Map an advertised CPython row like ``3.12``/``3.14t`` to its exact tag pair."""
    value = str(python).strip().lower()
    if not re.fullmatch(r"3\.\d{2}t?", value):
        raise HelperError(f"unsupported advertised Python row: {python!r}")
    interpreter = "cp" + value.removesuffix("t").replace(".", "")
    abi = interpreter + ("t" if value.endswith("t") else "")
    return interpreter, abi


def wheel_platform_matches_target(platform: str, target: str) -> bool:
    if target == "x86_64-pc-windows-msvc":
        return platform == "win_amd64"
    if target == "aarch64-pc-windows-msvc":
        return platform == "win_arm64"
    architectures = {
        "x86_64-unknown-linux-gnu": ("linux", "x86_64"),
        "aarch64-unknown-linux-gnu": ("linux", "aarch64"),
        "x86_64-apple-darwin": ("macos", "x86_64"),
        "aarch64-apple-darwin": ("macos", "arm64"),
    }
    expected = architectures.get(target)
    if expected is None:
        return False
    system, architecture = expected
    prefix_ok = (platform.startswith(("linux_", "manylinux"))
                 if system == "linux" else platform.startswith("macosx_"))
    return prefix_ok and platform.endswith("_" + architecture)


def target_supports_python(target: str, python: str) -> bool:
    # Windows ARM64 CPython distributions start at 3.11.
    return (target, python) != ("aarch64-pc-windows-msvc", "3.10")


def _validate_fastdb_decisions(problems: list[str], decisions: list[tuple[str, dict]],
                               retained_sdists: list[dict], listing: dict | None) -> None:
    """Bind every receipt FastDB decision to verified published metadata."""
    for name, decision in decisions:
        mode = decision.get("mode")
        if mode == "wheel":
            if listing is None or listing.get(decision.get("filename")) != decision.get("sha256"):
                problems.append(
                    f"receipt {name} fastdb wheel {decision.get('filename')} is not the "
                    "verified published file from rc-fastdb-public.json")
        elif mode == "sdist":
            if len(retained_sdists) != 1 or retained_sdists[0]["sha256"] != decision.get("sha256"):
                problems.append(
                    f"receipt {name} fastdb sdist sha256 does not match the single retained "
                    "verified sdist")
            built = decision.get("built_wheel")
            if isinstance(built, dict) and built.get("derived_from_sdist_sha256") != decision.get("sha256"):
                problems.append(
                    f"receipt {name} built-wheel derivation does not match its own sdist sha256")
        else:
            problems.append(f"receipt {name} fastdb decision has no wheel/sdist mode")


def _validate_candidate_bindings(options: argparse.Namespace, problems: list[str],
                                 cli: list[dict], wheels: list[dict], fastdb_proof: list[dict],
                                 parsed_receipts: dict[str, dict | None],
                                 meta: dict[str, str] | None = None) -> tuple[int, dict]:
    """Cross-bind retained bytes, receipts, published metadata, and row coverage.

    Returns the ABI-row count for the manifest counts section. Deep checks are
    opt-in: ``--expect-abi-python`` activates the row-set/binding validation
    and the FastDB proof checks run whenever a FastDB proof inventory exists
    alongside the published listing receipt.
    """
    from packaging.utils import InvalidWheelFilename, parse_wheel_filename

    rows: dict[tuple[str, str], tuple[str, dict]] = {}
    cli_by_name = {entry["name"]: entry for entry in cli}
    wheels_by_identity = {(entry["name"], entry["sha256"]): entry for entry in wheels}
    proof_by_name = {entry["name"]: entry for entry in fastdb_proof}
    referenced_wheels: set[tuple[str, str]] = set()

    listing_payload = parsed_receipts.get("rc-fastdb-public.json")
    listing = None
    if isinstance(listing_payload, dict):
        listing = {file["filename"]: file["sha256"]
                   for file in listing_payload.get("files", [])
                   if isinstance(file, dict) and "filename" in file and "sha256" in file}

    decisions: list[tuple[str, dict]] = []
    built_wheels: dict[tuple[str, str], dict] = {}
    for name, payload in sorted(parsed_receipts.items()):
        if not isinstance(payload, dict):
            continue
        decision = payload.get("fastdb")
        if name.startswith("fastdb-"):
            if payload.get("schema") != "c-two.fastdb-release.resolution.v1":
                problems.append(f"provisioning receipt {name} has an invalid schema")
                continue
            decision = payload
        if isinstance(decision, dict) and "sha256" in decision:
            decisions.append((name, decision))
            built = decision.get("built_wheel")
            if isinstance(built, dict) and "name" in built and "sha256" in built:
                built_wheels[(built["name"], built["sha256"])] = decision

    retained_sdists = [entry for entry in fastdb_proof
                       if entry["name"].startswith("fastdb4py-") and entry["name"].endswith(".tar.gz")]
    if fastdb_proof and options.expect_fastdb_proof is not None:
        if len(retained_sdists) != 1:
            problems.append(f"expected exactly one retained FastDB sdist, found "
                            f"{len(retained_sdists)}: {[e['name'] for e in retained_sdists]}")
        elif listing is None or listing.get(retained_sdists[0]["name"]) != retained_sdists[0]["sha256"]:
            problems.append(f"retained FastDB sdist {retained_sdists[0]['name']} does not "
                            "match the verified published listing")
        for entry in fastdb_proof:
            if not entry["name"].endswith(".whl"):
                continue
            published = listing is not None and listing.get(entry["name"]) == entry["sha256"]
            referenced = (entry["name"], entry["sha256"]) in built_wheels
            if not published and not referenced:
                problems.append(f"retained FastDB wheel {entry['name']} is neither the "
                                "verified published file nor a receipt-built wheel")

    _validate_fastdb_decisions(problems, decisions, retained_sdists, listing)

    if options.expect_abi_python:
        allowed_pairs = {_python_abi_pair(python) for python in options.expect_abi_python}
        for name, payload in sorted(parsed_receipts.items()):
            if not name.startswith("abi-") or not isinstance(payload, dict):
                continue
            row = payload.get("row")
            if not isinstance(row, dict) or "target" not in row or "python" not in row:
                problems.append(f"receipt {name} has no row.target/row.python binding")
                continue
            key = (str(row["target"]), str(row["python"]))
            if key in rows:
                problems.append(f"duplicate ABI row {key[0]}/{key[1]}: {name} and {rows[key][0]}")
            rows[key] = (name, payload)
        targets = set()
        for entry in cli:
            stem = entry["name"][:-len(".exe")] if entry["name"].endswith(".exe") else entry["name"]
            targets.add(stem[len("c3-"):])
        expected = {(target, python) for target in targets for python in options.expect_abi_python
                    if target_supports_python(target, python)}
        for key in sorted(expected - set(rows)):
            problems.append(f"missing ABI row {key[0]}/{key[1]}")
        for key in sorted(set(rows) - expected):
            problems.append(f"unexpected ABI row {key[0]}/{key[1]} ({rows[key][0]})")

        for (target, python), (name, payload) in sorted(rows.items()):
            artifacts = payload.get("artifacts")
            if not isinstance(artifacts, list):
                problems.append(f"receipt {name} has no artifact inventory")
                continue

            def artifact(predicate) -> dict | None:
                return next((entry for entry in artifacts
                             if isinstance(entry, dict) and predicate(entry)), None)

            ctwo_art = artifact(lambda entry: entry.get("distribution") == "c-two")
            fastdb_art = artifact(lambda entry: entry.get("distribution") == "fastdb4py")
            cli_art = artifact(lambda entry: str(entry.get("name", "")).startswith("c3-"))
            for label, found in (("c-two wheel", ctwo_art), ("fastdb4py wheel", fastdb_art),
                                 ("c3 executable", cli_art)):
                if found is None:
                    problems.append(f"receipt {name} does not bind its {label}")
            decision = payload.get("fastdb")
            if not isinstance(decision, dict) or "sha256" not in decision:
                problems.append(f"receipt {name} has no fastdb decision")

            if ctwo_art is not None:
                identity = (ctwo_art.get("name"), ctwo_art.get("sha256"))
                if identity not in wheels_by_identity:
                    problems.append(f"receipt {name} c-two wheel {identity[0]} is not a "
                                    "retained wheel with those bytes")
                else:
                    referenced_wheels.add(identity)
                    if options.expect_wheel_version and ctwo_art.get("version") != options.expect_wheel_version:
                        problems.append(
                            f"receipt {name} c-two wheel version is {ctwo_art.get('version')!r}, "
                            f"expected {options.expect_wheel_version!r}"
                        )
                    try:
                        _, _, _, wheel_tags = parse_wheel_filename(str(identity[0]))
                    except InvalidWheelFilename:
                        problems.append(f"receipt {name} c-two wheel has an unparseable name")
                        wheel_tags = ()
                    if not any(wheel_platform_matches_target(tag.platform, target)
                               for tag in wheel_tags):
                        problems.append(f"receipt {name} wheel platform does not match row target {target}")
                    try:
                        pair = _python_abi_pair(python)
                    except HelperError as error:
                        problems.append(f"receipt {name}: {error}")
                        pair = None
                    if pair is not None:
                        if pair not in allowed_pairs:
                            problems.append(f"receipt {name} advertises unsupported python {python}")
                        elif not any((tag.interpreter, tag.abi) == pair for tag in wheel_tags):
                            problems.append(f"receipt {name} wheel {identity[0]} does not carry "
                                            f"the {pair[0]}-{pair[1]} tag of row python {python}")
                        recorded = payload.get("wheel_abi")
                        if (isinstance(recorded, dict)
                                and (recorded.get("interpreter"), recorded.get("abi")) != pair):
                            problems.append(f"receipt {name} wheel_abi {recorded} differs from "
                                            f"row python {python}")

            if cli_art is not None:
                cli_name = str(cli_art.get("name"))
                if not cli_name.startswith(f"c3-{target}"):
                    problems.append(f"receipt {name} CLI artifact {cli_name} does not match "
                                    f"row target {target}")
                inventory = cli_by_name.get(cli_name)
                if inventory is None or inventory["sha256"] != cli_art.get("sha256"):
                    problems.append(f"receipt {name} CLI bytes do not match the retained "
                                    f"{cli_name or 'CLI artifact'}")
                if meta and "c3_version" in meta:
                    expected_cli_version = f"c3 {meta['c3_version']}"
                    if payload.get("cli_version") != expected_cli_version:
                        problems.append(
                            f"receipt {name} cli_version {payload.get('cli_version')!r} "
                            f"does not match {expected_cli_version!r}"
                        )

            expected_ctwo_version = (
                meta.get("c_two_version") if meta else None
            ) or options.expect_wheel_version
            if expected_ctwo_version:
                checks = payload.get("checks")
                if isinstance(checks, list):
                    for check in checks:
                        if not isinstance(check, dict):
                            continue
                        installed_versions = check.get("versions")
                        actual_version = (
                            installed_versions.get("c-two")
                            if isinstance(installed_versions, dict)
                            else None
                        )
                        if actual_version != expected_ctwo_version:
                            problems.append(
                                f"receipt {name} check installed c-two version "
                                f"{actual_version!r} does not match {expected_ctwo_version!r}"
                            )

            if fastdb_art is not None and isinstance(decision, dict):
                mode = decision.get("mode")
                if mode == "wheel":
                    if (fastdb_art.get("name"), fastdb_art.get("sha256")) != \
                            (decision.get("filename"), decision.get("sha256")):
                        problems.append(f"receipt {name} consumed fastdb wheel differs from "
                                        "its decision")
                elif mode == "sdist":
                    built = decision.get("built_wheel")
                    if not isinstance(built, dict) or \
                            (fastdb_art.get("name"), fastdb_art.get("sha256")) != \
                            (built.get("name"), built.get("sha256")):
                        problems.append(f"receipt {name} consumed fastdb wheel differs from "
                                        "its built_wheel decision")

        for identity in sorted(set(wheels_by_identity) - referenced_wheels):
            problems.append(f"retained wheel {identity[0]} is not consumed by any ABI row")

    if options.expect_wheel_name or options.expect_wheel_version:
        expected_name = (options.expect_wheel_name.replace("_", "-").lower()
                         if options.expect_wheel_name else None)
        allowed_pairs = ({_python_abi_pair(python) for python in options.expect_abi_python}
                         if options.expect_abi_python else None)
        for entry in wheels:
            try:
                wheel_name, wheel_version, _, wheel_tags = parse_wheel_filename(entry["name"])
            except InvalidWheelFilename:
                problems.append(f"retained wheel {entry['name']} has an unparseable filename")
                continue
            if expected_name and wheel_name != expected_name:
                problems.append(f"wheel {entry['name']} distribution is {wheel_name}, expected "
                                f"{expected_name}")
            if options.expect_wheel_version and str(wheel_version) != options.expect_wheel_version:
                problems.append(f"wheel {entry['name']} version is {wheel_version}, expected "
                                f"{options.expect_wheel_version}")
            if allowed_pairs is not None and not any(
                    (tag.interpreter, tag.abi) in allowed_pairs for tag in wheel_tags):
                problems.append(f"wheel {entry['name']} carries none of the advertised ABI tags")

    return len(rows), {name: entry for name, entry in proof_by_name.items()}


def command_manifest(options: argparse.Namespace) -> int:
    root = Path(options.root)
    output = Path(options.output)
    files = sorted(path for path in root.rglob("*")
                   if path.is_file() and path.name != output.name and not path.name.endswith(".sha256"))

    cli: list[dict] = []
    wheels: list[dict] = []
    java: list[dict] = []
    sdist: list[dict] = []
    fastdb_proof: list[dict] = []
    installers: list[dict] = []
    receipts: list[dict] = []
    problems: list[str] = []
    for path in files:
        name = path.name
        if not sidecar_for(path).is_file():
            problems.append(f"missing sidecar for {path.relative_to(root)}")
        if name in INSTALLER_NAMES:
            # Host-provided installers are their own asset kind; they are
            # never counted as CLI executables or product packages.
            installers.append(artifact_entry(path))
        elif name.startswith("c3-") and not name.endswith((".json", ".whl", ".tar.gz")):
            cli.append(artifact_entry(path))
        elif name.startswith("fastdb4py-") and name.endswith((".whl", ".tar.gz")):
            fastdb_proof.append(artifact_entry(path))
        elif name in FASTDB_PROOF_NAMES:
            # Verbatim upstream license evidence for the statically linked
            # FastDB core; dependency proof, not a C-Two publishable.
            fastdb_proof.append(artifact_entry(path))
        elif name.startswith(f"{JAVA_ARTIFACT}-") and name.endswith((".jar", ".pom")):
            if options.java_prefix and not name.startswith(options.java_prefix):
                problems.append(f"unexpected Java name {name}, expected prefix {options.java_prefix}")
            java.append(artifact_entry(path))
        elif name.endswith(".whl"):
            if options.wheel_prefix and not name.startswith(options.wheel_prefix):
                problems.append(f"unexpected wheel name {name}, expected prefix {options.wheel_prefix}")
            else:
                wheels.append(artifact_entry(path))
        elif name.endswith(".tar.gz"):
            if name.startswith(options.sdist_prefix):
                sdist.append(artifact_entry(path))
            else:
                problems.append(f"unexpected archive name {name}, expected prefix {options.sdist_prefix}")
        elif name.endswith(".json"):
            receipts.append(artifact_entry(path))
        else:
            problems.append(f"unclassified candidate file {path.relative_to(root)}")

    expected_counts = {"cli": options.expect_cli, "wheels": options.expect_wheels,
                       "sdist": options.expect_sdist, "java": options.expect_java,
                       "fastdb_proof": options.expect_fastdb_proof,
                       "installers": options.expect_installers}
    actual_counts = {"cli": len(cli), "wheels": len(wheels), "sdist": len(sdist), "java": len(java),
                     "fastdb_proof": len(fastdb_proof), "installers": len(installers)}
    for kind, expected in expected_counts.items():
        if expected is not None and actual_counts[kind] != expected:
            problems.append(f"expected {expected} {kind} artifacts, found {actual_counts[kind]}")
    for required in options.require_cli_name or []:
        if not any(entry["name"] == required for entry in cli):
            problems.append(f"missing required CLI artifact {required}")

    receipts_by_prefix: dict[str, int] = {}
    unmatched: list[str] = []
    for entry in receipts:
        matched = False
        for prefix in options.expect_json_prefix or []:
            if entry["name"].startswith(prefix):
                receipts_by_prefix[prefix] = receipts_by_prefix.get(prefix, 0) + 1
                matched = True
        if not matched:
            unmatched.append(entry["name"])
    if unmatched:
        problems.append(f"receipts without an expected prefix: {sorted(unmatched)}")
    for expected in options.expect_json_count or []:
        prefix, count = expected.rsplit("=", 1)
        if receipts_by_prefix.get(prefix, 0) != int(count):
            problems.append(f"expected {count} receipts with prefix {prefix!r}, "
                            f"found {receipts_by_prefix.get(prefix, 0)}")

    # Every receipt that reports a status must report a passing one; a
    # "skipped" row is retained evidence of a missing gate, not a pass.
    parsed_receipts: dict[str, dict | None] = {}
    for entry in receipts:
        try:
            parsed_receipts[entry["name"]] = json.loads(
                (root / entry["name"]).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            problems.append(f"unreadable receipt {entry['name']}: {error}")
            parsed_receipts[entry["name"]] = None
        status = (parsed_receipts[entry["name"]].get("status")
                  if isinstance(parsed_receipts[entry["name"]], dict) else None)
        if status is not None and status not in PASSING_RECEIPT_STATUSES:
            problems.append(f"receipt {entry['name']} has non-passing status {status!r}")

    # Receipts that claim to bind the candidate source must name exactly the
    # sha this manifest is being sealed for.
    for prefix in options.require_source_sha_prefix or []:
        for name, payload in sorted(parsed_receipts.items()):
            if not name.startswith(prefix):
                continue
            if not isinstance(payload, dict) or payload.get("source_commit_sha") != options.source_sha:
                problems.append(f"receipt {name} does not bind source sha {options.source_sha}")

    meta = dict(entry.split("=", 1) for entry in options.meta)
    if "c_two_version" in meta:
        try:
            validate_c_two_version(meta["c_two_version"])
        except HelperError as error:
            problems.append(str(error))
        if options.expect_wheel_version and options.expect_wheel_version != meta["c_two_version"]:
            problems.append(
                f"meta c_two_version {meta['c_two_version']!r} differs from "
                f"--expect-wheel-version {options.expect_wheel_version!r}"
            )
        expected_sdist_name = f"c_two-{meta['c_two_version']}.tar.gz"
        for entry in sdist:
            if entry["name"] != expected_sdist_name:
                problems.append(
                    f"sdist {entry['name']} does not match expected {expected_sdist_name}"
                )
        inspect_payload = parsed_receipts.get("sdist-inspect.json")
        if isinstance(inspect_payload, dict):
            project = inspect_payload.get("project")
            if (
                not isinstance(project, dict)
                or not isinstance(project.get("name"), str)
                or project["name"].replace("_", "-").lower() != "c-two"
                or project.get("version") != meta["c_two_version"]
            ):
                problems.append(
                    f"sdist-inspect.json project {project!r} does not match "
                    f"c-two {meta['c_two_version']!r}"
                )
    if java or options.expect_java_version or "java_version" in meta:
        if options.expect_java_version and meta.get("java_version") != options.expect_java_version:
            problems.append("meta java_version differs from --expect-java-version")
        try:
            verify_java_receipts(java, parsed_receipts, meta, options.source_sha)
        except HelperError as error:
            problems.append(str(error))
    if "c3_version" in meta:
        try:
            validate_c3_version(meta["c3_version"])
        except HelperError as error:
            problems.append(str(error))

    abi_rows, _proof_by_name = _validate_candidate_bindings(
        options, problems, cli, wheels, fastdb_proof, parsed_receipts, meta=meta)

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1

    manifest = {
        "schema": MANIFEST_SCHEMA,
        "source_commit_sha": options.source_sha,
        "event": options.event,
        "ref": options.ref,
        "meta": meta,
        "counts": {**actual_counts, "abi_rows": abi_rows,
                   "receipts_by_prefix": receipts_by_prefix,
                   "receipts": len(receipts)},
        "cli": cli,
        "wheels": wheels,
        "java": java,
        "sdist": sdist,
        "fastdb_proof": fastdb_proof,
        "installers": installers,
        "receipts": receipts,
    }
    output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    sidecar_for(output).write_text(f"{sha256_file(output)}  {output.name}\n", encoding="utf-8")
    print(f"manifest {output} cli={len(cli)} wheels={len(wheels)} sdist={len(sdist)} "
          f"java={len(java)} fastdb_proof={len(fastdb_proof)} installers={len(installers)} receipts={len(receipts)}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    checksum = commands.add_parser("checksum", help="Write .sha256 sidecars")
    checksum.add_argument("files", nargs="+")
    checksum.set_defaults(handler=command_checksum)

    verify = commands.add_parser("verify", help="Verify files against .sha256 sidecars")
    verify.add_argument("files", nargs="+")
    verify.set_defaults(handler=command_verify)

    inspect = commands.add_parser("inspect-sdist", help="Validate the sdist member tree")
    inspect.add_argument("archive")
    inspect.add_argument("--output", default=None)
    inspect.add_argument("--source-sha", default=None,
                         help="Stamp the sealed source sha into the inspection report")
    inspect.set_defaults(handler=command_inspect_sdist)

    inspect_java_parser = commands.add_parser("inspect-java", help="Validate the Java Maven packages")
    inspect_java_parser.add_argument("--root", required=True)
    inspect_java_parser.add_argument("--version", required=True)
    inspect_java_parser.add_argument("--expect-fastdb-version", required=True)
    inspect_java_parser.add_argument("--source-sha", required=True)
    inspect_java_parser.add_argument("--output", required=True)
    inspect_java_parser.set_defaults(handler=command_inspect_java)

    manifest = commands.add_parser("manifest", help="Aggregate the promotion manifest")
    manifest.add_argument("--root", required=True)
    manifest.add_argument("--output", required=True)
    manifest.add_argument("--source-sha", required=True)
    manifest.add_argument("--event", default="")
    manifest.add_argument("--ref", default="")
    manifest.add_argument("--meta", action="append", default=[],
                          help="key=value provenance such as c_two_version=0.6.0")
    manifest.add_argument("--expect-cli", type=int, default=None)
    manifest.add_argument("--expect-wheels", type=int, default=None)
    manifest.add_argument("--expect-java", type=int, default=None)
    manifest.add_argument("--java-prefix", default=None)
    manifest.add_argument("--expect-java-version", default=None)
    manifest.add_argument("--expect-sdist", type=int, default=None)
    manifest.add_argument("--expect-fastdb-proof", type=int, default=None)
    manifest.add_argument("--expect-installers", type=int, default=None)
    manifest.add_argument("--require-cli-name", action="append", default=[])
    manifest.add_argument("--wheel-prefix", default=None)
    manifest.add_argument("--sdist-prefix", default="c_two-")
    manifest.add_argument("--expect-wheel-name", default=None,
                          help="Distribution name every product wheel must carry")
    manifest.add_argument("--expect-wheel-version", default=None,
                          help="Version every product wheel must carry")
    manifest.add_argument("--expect-abi-python", action="append", default=[],
                          help="Advertised Python row such as 3.12 or 3.14t; activates the "
                               "exact target-x-ABI row-set and hash cross-binding validation")
    manifest.add_argument("--require-source-sha-prefix", action="append", default=[],
                          help="Receipt name prefix whose files must bind --source-sha")
    manifest.add_argument("--expect-json-prefix", action="append", default=[],
                          help="Receipt name prefix that every JSON receipt must match")
    manifest.add_argument("--expect-json-count", action="append", default=[],
                          help="prefix=count exact receipt count, such as smoke-=6")
    manifest.set_defaults(handler=command_manifest)

    options = parser.parse_args(argv)
    try:
        return options.handler(options)
    except HelperError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
