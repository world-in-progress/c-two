"""Build and audit an archive-only Cargo registry for the local candidate."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tomllib
from typing import Any, Iterable, Iterator, Mapping


C2_VERSION = "0.1.0"
FASTDB_VERSION = "0.2.0"
CARGO_LOCAL_REGISTRY_VERSION = "0.2.12"
FIRST_PARTY_VERSIONS = {
    "c2-local-security": C2_VERSION,
    "c2-config": C2_VERSION,
    "c2-contract": C2_VERSION,
    "c2-codegen": C2_VERSION,
    "c2-error": C2_VERSION,
    "c2-mem": C2_VERSION,
    "c2-mem-ffi": C2_VERSION,
    "c2-wire": C2_VERSION,
    "c2-ipc": C2_VERSION,
    "c2-local": C2_VERSION,
    "c2-http": C2_VERSION,
    "c2-server": C2_VERSION,
    "c2-core": C2_VERSION,
    "c2-python-native": C2_VERSION,
    "c-two": C2_VERSION,
    "fastdb": FASTDB_VERSION,
    "fastdb-sys": FASTDB_VERSION,
}
DEPENDENCY_TABLES = {
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
}
FORBIDDEN_SOURCE_KEYS = {"patch", "replace", "source"}
_EXACT_VERSION = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")


class RegistryError(RuntimeError):
    """A source manifest, crate archive, or local registry is unsafe."""


def _dependency_tables(
    value: object,
    *,
    path: str = "$",
) -> Iterator[tuple[str, Mapping[str, Any]]]:
    if not isinstance(value, dict):
        return
    for key, member in value.items():
        member_path = f"{path}.{key}"
        if key in DEPENDENCY_TABLES:
            if not isinstance(member, dict):
                raise RegistryError(
                    f"{member_path} must be a dependency table"
                )
            yield member_path, member
        elif isinstance(member, dict):
            yield from _dependency_tables(member, path=member_path)


def _dependency_name(alias: str, specification: object) -> str:
    if isinstance(specification, dict):
        package = specification.get("package", alias)
        if not isinstance(package, str) or not package:
            raise RegistryError(
                f"dependency {alias!r} has an invalid package alias"
            )
        return package
    return alias


def _dependency_version(specification: object) -> str | None:
    if isinstance(specification, str):
        return specification
    if isinstance(specification, dict):
        value = specification.get("version")
        return value if isinstance(value, str) else None
    return None


def scan_source_path_dependencies(repository: Path) -> list[str]:
    """Return every first-party path dependency missing its exact version."""

    findings: list[str] = []
    for manifest_path in sorted(repository.rglob("Cargo.toml")):
        if any(
            part in {"target", ".venv", "node_modules"}
            for part in manifest_path.parts
        ):
            continue
        try:
            document = tomllib.loads(
                manifest_path.read_text(encoding="utf-8")
            )
        except (OSError, tomllib.TOMLDecodeError) as error:
            raise RegistryError(
                f"could not parse {manifest_path}: {error}"
            ) from error
        for table_path, table in _dependency_tables(document):
            for alias, specification in table.items():
                if not isinstance(specification, dict):
                    continue
                dependency = _dependency_name(alias, specification)
                path = specification.get("path")
                if path is None:
                    continue
                if not isinstance(path, str) or not path:
                    findings.append(
                        f"{manifest_path.relative_to(repository)} "
                        f"{table_path}.{alias} has an invalid path"
                    )
                    continue
                expected = FIRST_PARTY_VERSIONS.get(dependency)
                if expected is None:
                    resolved = (manifest_path.parent / path).resolve()
                    try:
                        resolved.relative_to(repository.resolve())
                    except ValueError:
                        if resolved.name not in {"fastdb", "fastdb-sys"}:
                            continue
                        expected = FASTDB_VERSION
                    else:
                        findings.append(
                            f"{manifest_path.relative_to(repository)} "
                            f"{table_path}.{alias} points inside C-Two but "
                            f"{dependency!r} is not in the first-party set"
                        )
                        continue
                actual = specification.get("version")
                if actual != expected:
                    findings.append(
                        f"{manifest_path.relative_to(repository)} "
                        f"{table_path}.{alias} path dependency requires "
                        f"version {expected!r}, found {actual!r}"
                    )
    return findings


def require_versioned_source_paths(repository: Path) -> None:
    findings = scan_source_path_dependencies(repository)
    if findings:
        raise RegistryError(
            "first-party path dependencies are not release-ready:\n"
            + "\n".join(findings)
        )


def render_rust_consumer_manifest() -> str:
    return f"""\
[package]
name = "c-two-local-candidate-consumer"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = "={C2_VERSION}"
fastdb = "={FASTDB_VERSION}"
"""


def validate_rust_consumer_manifest(source: str) -> dict[str, Any]:
    try:
        document = tomllib.loads(source)
    except tomllib.TOMLDecodeError as error:
        raise RegistryError(f"consumer manifest is invalid TOML: {error}") from error
    for key in FORBIDDEN_SOURCE_KEYS:
        if key in document:
            raise RegistryError(
                f"consumer manifest contains forbidden [{key}] source escape"
            )
    dependencies = document.get("dependencies")
    if not isinstance(dependencies, dict):
        raise RegistryError("consumer manifest must have [dependencies]")
    if set(dependencies) != {"c-two", "fastdb"}:
        raise RegistryError(
            "consumer manifest must depend only on c-two and fastdb"
        )
    expected = {
        "c-two": f"={C2_VERSION}",
        "fastdb": f"={FASTDB_VERSION}",
    }
    for name, required in expected.items():
        if dependencies[name] != required:
            raise RegistryError(
                f"consumer dependency {name} must be exactly {required!r}"
            )
    lowered = source.lower()
    if "path =" in lowered or "git =" in lowered:
        raise RegistryError(
            "consumer manifest contains a path or Git dependency"
        )
    return document


def render_cargo_config(registry_dir: Path) -> str:
    if not registry_dir.is_absolute():
        raise RegistryError("local registry path must be absolute at run time")
    return f"""\
[source.crates-io]
replace-with = "local-candidate"

[source.local-candidate]
local-registry = "{registry_dir.as_posix()}"

[net]
offline = true
"""


def _load_crate_manifest(archive: Path) -> tuple[str, dict[str, Any]]:
    if archive.suffix != ".crate" or not archive.is_file():
        raise RegistryError(f"expected a .crate archive: {archive}")
    try:
        with tarfile.open(archive, "r:gz") as package:
            names = [
                member.name
                for member in package.getmembers()
                if member.isfile()
                and member.name.count("/") == 1
                and member.name.endswith("/Cargo.toml")
            ]
            if len(names) != 1:
                raise RegistryError(
                    f"{archive} must contain exactly one normalized Cargo.toml"
                )
            extracted = package.extractfile(names[0])
            if extracted is None:
                raise RegistryError(
                    f"{archive} normalized Cargo.toml is unreadable"
                )
            source = extracted.read().decode("utf-8")
    except (OSError, tarfile.TarError, UnicodeDecodeError) as error:
        raise RegistryError(f"could not inspect {archive}: {error}") from error
    try:
        document = tomllib.loads(source)
    except tomllib.TOMLDecodeError as error:
        raise RegistryError(
            f"{archive} normalized Cargo.toml is invalid: {error}"
        ) from error
    return source, document


def inspect_normalized_manifest(archive: Path) -> dict[str, Any]:
    source, document = _load_crate_manifest(archive)
    for table_path, table in _dependency_tables(document):
        for alias, specification in table.items():
            dependency = _dependency_name(alias, specification)
            if isinstance(specification, dict):
                if "path" in specification:
                    raise RegistryError(
                        f"{archive} {table_path}.{alias} retains a path"
                    )
                if "git" in specification:
                    raise RegistryError(
                        f"{archive} {table_path}.{alias} retains a Git source"
                    )
            expected = FIRST_PARTY_VERSIONS.get(dependency)
            if expected is None:
                continue
            version = _dependency_version(specification)
            if version != expected or _EXACT_VERSION.fullmatch(version or "") is None:
                raise RegistryError(
                    f"{archive} {table_path}.{alias} must have exact "
                    f"first-party version {expected!r}"
                )
    lowered = source.lower()
    if "[patch." in lowered or "\n[replace]" in lowered:
        raise RegistryError(f"{archive} normalized manifest has a source escape")
    return document


def inspect_all_normalized_manifests(
    archives: Iterable[Path],
) -> list[dict[str, Any]]:
    return [inspect_normalized_manifest(path) for path in archives]


def _index_path(registry_dir: Path, name: str) -> Path:
    lowered = name.lower()
    if len(lowered) == 1:
        relative = Path("1") / lowered
    elif len(lowered) == 2:
        relative = Path("2") / lowered
    elif len(lowered) == 3:
        relative = Path("3") / lowered[0] / lowered
    else:
        relative = Path(lowered[:2]) / lowered[2:4] / lowered
    return registry_dir / "index" / relative


def _index_dependency(
    alias: str,
    specification: object,
    *,
    kind: str | None,
    target: str | None,
) -> dict[str, Any]:
    if isinstance(specification, str):
        requirement = specification
        features: list[str] = []
        optional = False
        default_features = True
        package = None
        registry = None
    elif isinstance(specification, dict):
        requirement = specification.get("version")
        if not isinstance(requirement, str) or not requirement:
            raise RegistryError(
                f"packaged dependency {alias!r} has no version requirement"
            )
        raw_features = specification.get("features", [])
        if not isinstance(raw_features, list) or not all(
            isinstance(feature, str) for feature in raw_features
        ):
            raise RegistryError(
                f"packaged dependency {alias!r} has invalid features"
            )
        features = list(raw_features)
        optional = specification.get("optional", False)
        default_features = specification.get("default-features", True)
        if not isinstance(optional, bool) or not isinstance(
            default_features,
            bool,
        ):
            raise RegistryError(
                f"packaged dependency {alias!r} has invalid booleans"
            )
        raw_package = specification.get("package")
        if raw_package is not None and not isinstance(raw_package, str):
            raise RegistryError(
                f"packaged dependency {alias!r} has invalid package alias"
            )
        package = (
            raw_package
            if isinstance(raw_package, str) and raw_package != alias
            else None
        )
        raw_registry = specification.get("registry")
        registry = raw_registry if isinstance(raw_registry, str) else None
    else:
        raise RegistryError(
            f"packaged dependency {alias!r} has an invalid specification"
        )
    return {
        "name": alias,
        "req": requirement,
        "features": sorted(features),
        "optional": optional,
        "default_features": default_features,
        "target": target,
        "kind": kind,
        "registry": registry,
        "package": package,
    }


def _index_dependencies(document: Mapping[str, Any]) -> list[dict[str, Any]]:
    dependencies: list[dict[str, Any]] = []
    kinds = {
        "dependencies": None,
        "dev-dependencies": "dev",
        "build-dependencies": "build",
    }
    for table_name, kind in kinds.items():
        table = document.get(table_name, {})
        if not isinstance(table, dict):
            raise RegistryError(f"[{table_name}] must be a table")
        dependencies.extend(
            _index_dependency(
                alias,
                specification,
                kind=kind,
                target=None,
            )
            for alias, specification in table.items()
        )
    targets = document.get("target", {})
    if not isinstance(targets, dict):
        raise RegistryError("[target] must be a table")
    for target, target_document in targets.items():
        if not isinstance(target_document, dict):
            raise RegistryError(f"[target.{target}] must be a table")
        for table_name, kind in kinds.items():
            table = target_document.get(table_name, {})
            if not isinstance(table, dict):
                raise RegistryError(
                    f"[target.{target}.{table_name}] must be a table"
                )
            dependencies.extend(
                _index_dependency(
                    alias,
                    specification,
                    kind=kind,
                    target=target,
                )
                for alias, specification in table.items()
            )
    dependencies.sort(
        key=lambda dependency: (
            dependency["name"],
            dependency["kind"] or "",
            dependency["target"] or "",
            dependency["package"] or "",
        )
    )
    return dependencies


def add_crate_archive(
    registry_dir: Path,
    archive: Path,
) -> dict[str, Any]:
    """Import a locally built archive into Cargo's local-registry format.

    ``cargo-local-registry 0.2.12`` cannot import a local archive through its
    ``add`` command; it only fetches a named crate from an upstream registry.
    This function writes the same checksum-backed archive and index records
    that its ``sync`` command produces.
    """

    _, document = _load_crate_manifest(archive)
    package = document.get("package")
    if not isinstance(package, dict):
        raise RegistryError(f"{archive} has no [package] table")
    name = package.get("name")
    version = package.get("version")
    if not isinstance(name, str) or not name:
        raise RegistryError(f"{archive} package name is invalid")
    if not isinstance(version, str) or not version:
        raise RegistryError(f"{archive} package version is invalid")
    expected_filename = f"{name}-{version}.crate"
    if archive.name != expected_filename:
        raise RegistryError(
            f"{archive} must be named {expected_filename!r}"
        )
    features = document.get("features", {})
    if not isinstance(features, dict):
        raise RegistryError(f"{archive} [features] must be a table")
    for feature, values in features.items():
        if not isinstance(feature, str) or not isinstance(values, list) or not all(
            isinstance(value, str) for value in values
        ):
            raise RegistryError(f"{archive} has an invalid feature table")
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    entry: dict[str, Any] = {
        "name": name,
        "vers": version,
        "deps": _index_dependencies(document),
        "cksum": checksum,
        "features": {
            feature: list(values)
            for feature, values in sorted(features.items())
        },
        "yanked": False,
    }
    links = package.get("links")
    if links is not None:
        if not isinstance(links, str):
            raise RegistryError(f"{archive} package.links must be a string")
        entry["links"] = links
    rust_version = package.get("rust-version")
    if rust_version is not None:
        if not isinstance(rust_version, str):
            raise RegistryError(
                f"{archive} package.rust-version must be a string"
            )
        entry["rust_version"] = rust_version

    registry_dir.mkdir(parents=True, exist_ok=True)
    destination = registry_dir / expected_filename
    if destination.exists():
        if hashlib.sha256(destination.read_bytes()).hexdigest() != checksum:
            raise RegistryError(
                f"registry already contains different bytes for {expected_filename}"
            )
    else:
        shutil.copyfile(archive, destination)

    index_path = _index_path(registry_dir, name)
    index_path.parent.mkdir(parents=True, exist_ok=True)
    existing: list[dict[str, Any]] = []
    if index_path.exists():
        for line in index_path.read_text(encoding="utf-8").splitlines():
            try:
                existing.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise RegistryError(
                    f"registry index {index_path} is invalid: {error}"
                ) from error
    for current in existing:
        if current.get("vers") == version:
            if current != entry:
                raise RegistryError(
                    f"registry index already has different metadata for "
                    f"{name} {version}"
                )
            return entry
    existing.append(entry)
    existing.sort(key=lambda item: str(item.get("vers", "")))
    index_path.write_text(
        "".join(
            json.dumps(
                item,
                ensure_ascii=False,
                allow_nan=False,
                separators=(",", ":"),
                sort_keys=True,
            )
            + "\n"
            for item in existing
        ),
        encoding="utf-8",
    )
    return entry


def _forbidden_needles(
    forbidden_roots: Iterable[Path],
) -> tuple[bytes, ...]:
    needles: set[bytes] = {
        b"../c-two",
        b"../fastdb",
        b"../toodle",
    }
    for root in forbidden_roots:
        resolved = root.resolve().as_posix()
        needles.add(resolved.encode())
        needles.add((resolved + "/").encode())
    return tuple(sorted(needles))


def assert_no_forbidden_source_references(
    paths: Iterable[Path],
    *,
    forbidden_roots: Iterable[Path],
) -> None:
    needles = _forbidden_needles(forbidden_roots)
    for path in paths:
        if not path.is_file():
            raise RegistryError(f"source-reference scan input is not a file: {path}")
        contents = path.read_bytes()
        for needle in needles:
            if needle in contents:
                raise RegistryError(
                    f"{path} contains an active repository reference "
                    f"{needle.decode(errors='replace')!r}"
                )


def cargo_local_registry_version(
    *,
    cargo: str = "cargo",
) -> str:
    completed = subprocess.run(
        [cargo, "local-registry", "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
    )
    if completed.returncode != 0:
        raise RegistryError(
            "cargo-local-registry is unavailable; install exact version "
            f"{CARGO_LOCAL_REGISTRY_VERSION}\n{completed.stdout}"
        )
    output = completed.stdout.strip()
    match = re.search(r"cargo-local-registry\s+(\S+)", output)
    if match is None or match.group(1) != CARGO_LOCAL_REGISTRY_VERSION:
        raise RegistryError(
            "cargo-local-registry version must be exactly "
            f"{CARGO_LOCAL_REGISTRY_VERSION}, got {output!r}"
        )
    return output


def run(
    command: list[str],
    *,
    cwd: Path,
    env: Mapping[str, str] | None = None,
) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=None if env is None else dict(env),
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="strict",
    )
    if completed.returncode != 0:
        raise RegistryError(
            f"command failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{completed.stdout}"
        )
    return completed.stdout


def build_local_registry(
    *,
    closure_locks: Iterable[Path],
    registry_dir: Path,
    crate_archives: Iterable[Path],
    cargo: str = "cargo",
) -> None:
    """Populate an offline registry from crates.io plus first-party archives."""

    cargo_local_registry_version(cargo=cargo)
    if registry_dir.exists():
        raise RegistryError(
            f"local registry destination must be absent: {registry_dir}"
        )
    registry_dir.parent.mkdir(parents=True, exist_ok=True)
    locks = list(closure_locks)
    if not locks:
        raise RegistryError("no Cargo.lock closure inputs were provided")
    for index, closure_lock in enumerate(locks):
        command = [
            cargo,
            "local-registry",
            "sync",
            str(closure_lock),
            str(registry_dir),
        ]
        if index:
            command.append("--no-delete")
        run(command, cwd=closure_lock.parent)
    archives = sorted(crate_archives, key=os.fspath)
    if not archives:
        raise RegistryError("no first-party crate archives were provided")
    for archive in archives:
        inspect_normalized_manifest(archive)
        add_crate_archive(registry_dir, archive)


def normalized_manifest_bytes(
    archive: Path,
) -> bytes:
    """Return normalized Cargo.toml bytes for retained audit evidence."""

    source, _ = _load_crate_manifest(archive)
    return source.encode("utf-8")


def rewrite_path_dependencies_as_versions(source: str) -> str:
    """Remove only versioned inline ``path`` keys in a package snapshot.

    The source checkout remains ``version + path``. Candidate packaging uses
    this transformation in an untracked immutable snapshot so Cargo.toml.orig
    is also free from sibling-source escape paths.
    """

    try:
        document = tomllib.loads(source)
    except tomllib.TOMLDecodeError as error:
        raise RegistryError(f"source manifest is invalid TOML: {error}") from error
    replacements: list[tuple[str, str]] = []
    for _table_path, table in _dependency_tables(document):
        for alias, specification in table.items():
            if not isinstance(specification, dict) or "path" not in specification:
                continue
            dependency = _dependency_name(alias, specification)
            expected = FIRST_PARTY_VERSIONS.get(dependency)
            if expected is None or specification.get("version") != expected:
                raise RegistryError(
                    f"path dependency {alias!r} is not pinned before packaging"
                )
            path_value = specification["path"]
            if not isinstance(path_value, str):
                raise RegistryError(
                    f"path dependency {alias!r} has a non-string path"
                )
            escaped = re.escape(path_value)
            patterns = (
                (
                    rf"path\s*=\s*([\"']){escaped}\1\s*,\s*",
                    "",
                ),
                (
                    rf",\s*path\s*=\s*([\"']){escaped}\1",
                    "",
                ),
            )
            updated = source
            for pattern, replacement in patterns:
                updated, count = re.subn(pattern, replacement, updated, count=1)
                if count:
                    replacements.append((path_value, alias))
                    source = updated
                    break
            else:
                raise RegistryError(
                    f"could not remove path for dependency {alias!r}"
                )
    if "path =" in source:
        # Package/lib/bin target paths are legal; dependency verification below
        # distinguishes them from source escape hatches.
        rewritten = tomllib.loads(source)
        for table_path, table in _dependency_tables(rewritten):
            for alias, specification in table.items():
                if isinstance(specification, dict) and "path" in specification:
                    raise RegistryError(
                        f"{table_path}.{alias} retained a dependency path"
                    )
    return source
