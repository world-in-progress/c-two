"""Strict, portable evidence for a C-Two local release candidate."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile
from typing import Any, Iterable, Mapping
import zipfile


MANIFEST_SCHEMA = "c-two.local-release-candidate-manifest.v1"
MANIFEST_FILENAME = "local-release-candidate-manifest.v1.json"
EVIDENCE_FILENAMES = {
    "package-consumer-receipt.v1.json",
    "portable-matrix-receipt.v1.json",
    "typescript-real-call-receipt.v1.json",
}
SOURCE_ORDER = ("c-two", "fastdb")
SOURCE_FIELDS = {
    "repository",
    "commit",
    "source_date_epoch",
    "worktree_clean",
    "worktree_state_sha256",
}
BUILD_FIELDS = {
    "commands",
    "platform",
    "target",
    "toolchains",
    "verified_platforms",
    "unverified_platforms",
    "unsupported_platforms",
}
ARTIFACT_FIELDS = {
    "owner_repository",
    "source_commit",
    "kind",
    "path",
    "package",
    "bytes",
    "sha256",
    "inventory",
    "build",
}
PACKAGE_FIELDS = {"name", "version"}
INVENTORY_FIELDS = {"path", "bytes", "sha256"}
_SHA256 = re.compile(r"^[0-9a-f]{64}$")
_COMMIT = re.compile(r"^[0-9a-f]{40}$")
_ABSOLUTE_MARKERS = (
    "/Users/",
    "/private/",
    "/tmp/",
    "../c-two",
    "../fastdb",
    "../toodle",
)


class CandidateError(RuntimeError):
    """The local candidate or retained evidence is invalid."""


def canonical_json_bytes(document: Any) -> bytes:
    return (
        json.dumps(
            document,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def load_json_no_duplicates(source: str, label: str) -> Any:
    def object_from_pairs(
        pairs: list[tuple[str, Any]],
    ) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise CandidateError(
                    f"{label} contains duplicate JSON key {key!r}"
                )
            result[key] = value
        return result

    try:
        return json.loads(source, object_pairs_hook=object_from_pairs)
    except json.JSONDecodeError as error:
        raise CandidateError(f"{label} is invalid JSON: {error}") from error


def checked_relative_path(value: str) -> str:
    path = PurePosixPath(value)
    if (
        not value
        or "\\" in value
        or path.is_absolute()
        or ".." in path.parts
        or "." in path.parts
        or path.as_posix() != value
        or "//" in value
    ):
        raise CandidateError(
            f"candidate path must be canonical and relative: {value!r}"
        )
    return value


def sha256_bytes(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _exact_fields(
    value: Mapping[str, Any],
    expected: set[str],
    label: str,
) -> None:
    actual = set(value)
    missing = sorted(expected - actual)
    unknown = sorted(actual - expected)
    if missing:
        raise CandidateError(f"{label} missing field(s): {missing}")
    if unknown:
        raise CandidateError(f"{label} has unknown field(s): {unknown}")


def _sha256(value: object, label: str) -> str:
    if not isinstance(value, str) or _SHA256.fullmatch(value) is None:
        raise CandidateError(f"{label} must be a lowercase SHA-256")
    return value


def _commit(value: object, label: str) -> str:
    if not isinstance(value, str) or _COMMIT.fullmatch(value) is None:
        raise CandidateError(f"{label} must be a lowercase 40-hex commit")
    return value


def _string(value: object, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise CandidateError(f"{label} must be a non-empty string")
    return value


def _strings(value: object, label: str) -> list[str]:
    if not isinstance(value, list):
        raise CandidateError(f"{label} must be an array")
    result = [_string(item, f"{label}[{index}]") for index, item in enumerate(value)]
    if len(result) != len(set(result)):
        raise CandidateError(f"{label} contains duplicate values")
    if result != sorted(result):
        raise CandidateError(f"{label} must be sorted")
    return result


def file_record(path: Path, relative_path: str) -> dict[str, Any]:
    checked_relative_path(relative_path)
    if path.is_symlink() or not path.is_file():
        raise CandidateError(
            f"candidate inventory member must be a regular file: {path}"
        )
    contents = path.read_bytes()
    return {
        "path": relative_path,
        "bytes": len(contents),
        "sha256": sha256_bytes(contents),
    }


def validate_inventory(
    inventory: object,
    label: str,
) -> list[dict[str, Any]]:
    if not isinstance(inventory, list) or not inventory:
        raise CandidateError(f"{label} inventory must be a non-empty array")
    paths: list[str] = []
    result: list[dict[str, Any]] = []
    for index, raw in enumerate(inventory):
        if not isinstance(raw, dict):
            raise CandidateError(
                f"{label} inventory[{index}] must be an object"
            )
        _exact_fields(raw, INVENTORY_FIELDS, f"{label} inventory[{index}]")
        path = raw["path"]
        size = raw["bytes"]
        if not isinstance(path, str):
            raise CandidateError(
                f"{label} inventory[{index}].path must be a string"
            )
        checked_relative_path(path)
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            raise CandidateError(
                f"{label} inventory[{index}].bytes must be a non-negative integer"
            )
        _sha256(raw["sha256"], f"{label} inventory[{index}].sha256")
        paths.append(path)
        result.append(raw)
    if len(paths) != len(set(paths)):
        raise CandidateError(f"{label} inventory contains duplicate paths")
    if paths != sorted(paths):
        raise CandidateError(f"{label} inventory paths must be sorted")
    return result


def _checked_archive_names(
    names: list[str],
    *,
    root: str | None,
    label: str,
    directory_names: set[str] | None = None,
) -> list[tuple[str, str]]:
    if len(names) != len(set(names)):
        raise CandidateError(f"{label} contains duplicate archive paths")
    prefix = f"{root}/" if root is not None else None
    directories = set() if directory_names is None else directory_names
    result: list[tuple[str, str]] = []
    for raw in names:
        normalized_raw = raw
        if raw in directories:
            if not raw.endswith("/") or raw.endswith("//"):
                raise CandidateError(
                    f"{label} contains non-canonical directory {raw!r}"
                )
            normalized_raw = raw[:-1]
        if root is not None:
            if normalized_raw == root:
                continue
            if prefix is None or not normalized_raw.startswith(prefix):
                raise CandidateError(
                    f"{label} contains member outside {root}/: {raw!r}"
                )
            relative = normalized_raw[len(prefix) :]
        else:
            relative = normalized_raw
        checked_relative_path(relative)
        result.append((raw, relative))
    normalized = [relative for _, relative in result]
    if len(normalized) != len(set(normalized)):
        raise CandidateError(
            f"{label} contains colliding normalized archive paths"
        )
    return result


def tar_inventory(
    archive_path: Path,
    *,
    root: str,
    label: str,
) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            members = archive.getmembers()
            special = [
                member.name
                for member in members
                if not (member.isfile() or member.isdir())
            ]
            if special:
                raise CandidateError(
                    f"{label} contains non-file archive members: {special}"
                )
            mapped = _checked_archive_names(
                [member.name for member in members],
                root=root,
                label=label,
            )
            by_name = {member.name: member for member in members}
            for raw, relative in mapped:
                member = by_name[raw]
                if member.isdir():
                    continue
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise CandidateError(
                        f"{label} archive member is unreadable: {raw}"
                    )
                contents = extracted.read()
                records.append(
                    {
                        "path": relative,
                        "bytes": len(contents),
                        "sha256": sha256_bytes(contents),
                    }
                )
    except (tarfile.TarError, OSError) as error:
        raise CandidateError(
            f"could not inspect {label} {archive_path}: {error}"
        ) from error
    records.sort(key=lambda item: item["path"])
    return validate_inventory(records, label)


def zip_inventory(
    archive_path: Path,
    *,
    label: str,
) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    try:
        with zipfile.ZipFile(archive_path) as archive:
            infos = archive.infolist()
            mapped = _checked_archive_names(
                [info.filename for info in infos],
                root=None,
                label=label,
                directory_names={
                    info.filename for info in infos if info.is_dir()
                },
            )
            by_name = {info.filename: info for info in infos}
            for raw, relative in mapped:
                info = by_name[raw]
                unix_mode = (info.external_attr >> 16) & 0xFFFF
                if unix_mode and stat.S_ISLNK(unix_mode):
                    raise CandidateError(
                        f"{label} contains symbolic-link member {raw!r}"
                    )
                if info.is_dir():
                    continue
                contents = archive.read(info)
                records.append(
                    {
                        "path": relative,
                        "bytes": len(contents),
                        "sha256": sha256_bytes(contents),
                    }
                )
    except (zipfile.BadZipFile, OSError) as error:
        raise CandidateError(
            f"could not inspect {label} {archive_path}: {error}"
        ) from error
    records.sort(key=lambda item: item["path"])
    return validate_inventory(records, label)


def directory_inventory(
    directory: Path,
    *,
    label: str,
) -> list[dict[str, Any]]:
    if directory.is_symlink() or not directory.is_dir():
        raise CandidateError(f"{label} must be a regular directory")
    records: list[dict[str, Any]] = []
    for path in sorted(directory.rglob("*"), key=os.fspath):
        if path.is_symlink():
            raise CandidateError(
                f"{label} contains symbolic link "
                f"{path.relative_to(directory).as_posix()}"
            )
        if path.is_file():
            records.append(
                file_record(path, path.relative_to(directory).as_posix())
            )
    return validate_inventory(records, label)


def archive_inventory(
    path: Path,
    *,
    label: str,
) -> list[dict[str, Any]]:
    if path.suffix == ".crate":
        return tar_inventory(
            path,
            root=path.name[: -len(".crate")],
            label=label,
        )
    if path.name.endswith(".tar.gz"):
        return tar_inventory(
            path,
            root=path.name[: -len(".tar.gz")],
            label=label,
        )
    if path.suffix == ".tgz":
        return tar_inventory(path, root="package", label=label)
    if path.suffix == ".whl":
        return zip_inventory(path, label=label)
    return [file_record(path, path.name)]


def _validate_build(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CandidateError(f"{label} must be an object")
    _exact_fields(value, BUILD_FIELDS, label)
    commands = value["commands"]
    if not isinstance(commands, list) or not commands:
        raise CandidateError(f"{label}.commands must be a non-empty array")
    for index, command in enumerate(commands):
        _string(command, f"{label}.commands[{index}]")
    _string(value["platform"], f"{label}.platform")
    _string(value["target"], f"{label}.target")
    toolchains = value["toolchains"]
    if not isinstance(toolchains, dict) or not toolchains:
        raise CandidateError(f"{label}.toolchains must be a non-empty object")
    for name, version in toolchains.items():
        _string(name, f"{label}.toolchains key")
        _string(version, f"{label}.toolchains.{name}")
    _strings(value["verified_platforms"], f"{label}.verified_platforms")
    _strings(value["unverified_platforms"], f"{label}.unverified_platforms")
    _strings(value["unsupported_platforms"], f"{label}.unsupported_platforms")
    return value


def artifact_descriptor(
    candidate_root: Path,
    *,
    path: str,
    kind: str,
    owner_repository: str,
    source_commit: str,
    package: Mapping[str, str],
    build: Mapping[str, Any],
) -> dict[str, Any]:
    relative = checked_relative_path(path)
    artifact = candidate_root / relative
    try:
        artifact.resolve(strict=True).relative_to(
            candidate_root.resolve(strict=True)
        )
    except (FileNotFoundError, ValueError) as error:
        raise CandidateError(
            f"artifact path escapes or is missing: {relative}"
        ) from error
    if artifact.is_symlink():
        raise CandidateError(f"artifact must not be a symlink: {relative}")
    if artifact.is_dir():
        inventory = directory_inventory(artifact, label=kind)
        size = sum(item["bytes"] for item in inventory)
        digest = sha256_bytes(canonical_json_bytes(inventory))
    elif artifact.is_file():
        inventory = archive_inventory(artifact, label=kind)
        size = artifact.stat().st_size
        digest = sha256_file(artifact)
    else:
        raise CandidateError(
            f"artifact must be a regular file or directory: {relative}"
        )
    descriptor = {
        "owner_repository": owner_repository,
        "source_commit": source_commit,
        "kind": kind,
        "path": relative,
        "package": dict(package),
        "bytes": size,
        "sha256": digest,
        "inventory": inventory,
        "build": json.loads(canonical_json_bytes(build)),
    }
    _validate_artifact(descriptor, "artifact")
    return descriptor


def _validate_source(
    value: object,
    label: str,
) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CandidateError(f"{label} must be an object")
    _exact_fields(value, SOURCE_FIELDS, label)
    repository = _string(value["repository"], f"{label}.repository")
    if repository not in SOURCE_ORDER:
        raise CandidateError(f"{label}.repository is not recognized")
    _commit(value["commit"], f"{label}.commit")
    epoch = value["source_date_epoch"]
    if not isinstance(epoch, int) or isinstance(epoch, bool) or epoch <= 0:
        raise CandidateError(
            f"{label}.source_date_epoch must be a positive integer"
        )
    if value["worktree_clean"] is not True:
        raise CandidateError(f"{label}.worktree_clean must be true")
    _sha256(
        value["worktree_state_sha256"],
        f"{label}.worktree_state_sha256",
    )
    return value


def _validate_artifact(
    value: object,
    label: str,
) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CandidateError(f"{label} must be an object")
    _exact_fields(value, ARTIFACT_FIELDS, label)
    owner = _string(
        value["owner_repository"],
        f"{label}.owner_repository",
    )
    if owner not in {"c-two", "fastdb", "build-tool", "third-party"}:
        raise CandidateError(f"{label}.owner_repository is not recognized")
    _commit(value["source_commit"], f"{label}.source_commit")
    _string(value["kind"], f"{label}.kind")
    path = value["path"]
    if not isinstance(path, str):
        raise CandidateError(f"{label}.path must be a string")
    checked_relative_path(path)
    package = value["package"]
    if not isinstance(package, dict):
        raise CandidateError(f"{label}.package must be an object")
    _exact_fields(package, PACKAGE_FIELDS, f"{label}.package")
    _string(package["name"], f"{label}.package.name")
    _string(package["version"], f"{label}.package.version")
    size = value["bytes"]
    if not isinstance(size, int) or isinstance(size, bool) or size < 0:
        raise CandidateError(f"{label}.bytes must be a non-negative integer")
    _sha256(value["sha256"], f"{label}.sha256")
    validate_inventory(value["inventory"], label)
    _validate_build(value["build"], f"{label}.build")
    return value


def reject_absolute_manifest_paths(
    value: object,
    *,
    label: str = "$",
) -> None:
    if isinstance(value, dict):
        for key, member in value.items():
            reject_absolute_manifest_paths(
                member,
                label=f"{label}.{key}",
            )
    elif isinstance(value, list):
        for index, member in enumerate(value):
            reject_absolute_manifest_paths(
                member,
                label=f"{label}[{index}]",
            )
    elif isinstance(value, str):
        if any(marker in value for marker in _ABSOLUTE_MARKERS):
            raise CandidateError(
                f"{label} retains an absolute or active-source path"
            )


def validate_manifest(
    document: object,
    *,
    expected_commits: Mapping[str, str] | None = None,
) -> dict[str, Any]:
    if not isinstance(document, dict):
        raise CandidateError("candidate manifest must be an object")
    _exact_fields(
        document,
        {"schema", "sources", "build", "artifacts"},
        "$",
    )
    if document["schema"] != MANIFEST_SCHEMA:
        raise CandidateError(
            f"$.schema must be {MANIFEST_SCHEMA!r}"
        )
    sources = document["sources"]
    if not isinstance(sources, list):
        raise CandidateError("$.sources must be an array")
    validated_sources = [
        _validate_source(source, f"$.sources[{index}]")
        for index, source in enumerate(sources)
    ]
    repositories = [source["repository"] for source in validated_sources]
    if tuple(repositories) != SOURCE_ORDER:
        raise CandidateError(
            f"$.sources must have exact order {SOURCE_ORDER!r}"
        )
    if expected_commits is not None:
        if set(expected_commits) != set(SOURCE_ORDER):
            raise CandidateError(
                "expected_commits must contain exact c-two and fastdb keys"
            )
        for source in validated_sources:
            expected = expected_commits[source["repository"]]
            if source["commit"] != expected:
                raise CandidateError(
                    f"{source['repository']} source commit does not match "
                    "the expected commit"
                )
    source_commits = {
        source["repository"]: source["commit"]
        for source in validated_sources
    }
    build = _validate_build(document["build"], "$.build")
    artifacts = document["artifacts"]
    if not isinstance(artifacts, list) or not artifacts:
        raise CandidateError("$.artifacts must be a non-empty array")
    validated_artifacts = [
        _validate_artifact(artifact, f"$.artifacts[{index}]")
        for index, artifact in enumerate(artifacts)
    ]
    paths = [artifact["path"] for artifact in validated_artifacts]
    if len(paths) != len(set(paths)):
        raise CandidateError("$.artifacts contains duplicate paths")
    if paths != sorted(paths):
        raise CandidateError("$.artifacts must be sorted by path")
    for index, artifact in enumerate(validated_artifacts):
        owner = artifact["owner_repository"]
        if owner in source_commits:
            if artifact["source_commit"] != source_commits[owner]:
                raise CandidateError(
                    f"$.artifacts[{index}].source_commit does not match "
                    f"the {owner} source commit"
                )
        elif artifact["source_commit"] != source_commits["c-two"]:
            raise CandidateError(
                f"$.artifacts[{index}].source_commit must identify "
                "the C-Two candidate build input"
            )
    reject_absolute_manifest_paths(document)
    return {
        "schema": MANIFEST_SCHEMA,
        "sources": validated_sources,
        "build": build,
        "artifacts": validated_artifacts,
    }


def _artifact_files(
    candidate_root: Path,
    artifact: Mapping[str, Any],
) -> set[str]:
    relative = artifact["path"]
    path = candidate_root / relative
    if path.is_file():
        return {relative}
    if path.is_dir():
        return {
            (PurePosixPath(relative) / record["path"]).as_posix()
            for record in artifact["inventory"]
        }
    return set()


def _verify_artifact(
    candidate_root: Path,
    artifact: Mapping[str, Any],
) -> None:
    path = candidate_root / artifact["path"]
    if not path.exists():
        raise CandidateError(
            f"candidate artifact is missing: {artifact['path']}"
        )
    if path.is_file():
        if path.stat().st_size != artifact["bytes"]:
            raise CandidateError(
                f"candidate artifact byte count changed: {artifact['path']}"
            )
        if sha256_file(path) != artifact["sha256"]:
            raise CandidateError(
                f"candidate artifact SHA-256 changed: {artifact['path']}"
            )
    actual = artifact_descriptor(
        candidate_root,
        path=artifact["path"],
        kind=artifact["kind"],
        owner_repository=artifact["owner_repository"],
        source_commit=artifact["source_commit"],
        package=artifact["package"],
        build=artifact["build"],
    )
    if actual["bytes"] != artifact["bytes"]:
        raise CandidateError(
            f"candidate artifact byte count changed: {artifact['path']}"
        )
    if actual["sha256"] != artifact["sha256"]:
        raise CandidateError(
            f"candidate artifact SHA-256 changed: {artifact['path']}"
        )
    if actual["inventory"] != artifact["inventory"]:
        raise CandidateError(
            f"candidate artifact inventory changed: {artifact['path']}"
        )


def _validate_candidate_file_set(
    candidate_root: Path,
    artifacts: Iterable[Mapping[str, Any]],
) -> None:
    expected = {MANIFEST_FILENAME}
    for artifact in artifacts:
        expected.update(_artifact_files(candidate_root, artifact))
    expected.update(
        filename
        for filename in EVIDENCE_FILENAMES
        if (candidate_root / filename).is_file()
    )
    actual = {
        path.relative_to(candidate_root).as_posix()
        for path in candidate_root.rglob("*")
        if path.is_file()
    }
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        raise CandidateError(
            "candidate file set does not match its manifest: "
            f"missing={missing}, unexpected={unexpected}"
        )


def construct_manifest(
    candidate_root: Path,
    *,
    sources: list[dict[str, Any]],
    artifacts: list[dict[str, Any]],
    build: dict[str, Any],
    enforce_candidate_set: bool = True,
) -> dict[str, Any]:
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "sources": sources,
        "build": build,
        "artifacts": sorted(artifacts, key=lambda artifact: artifact["path"]),
    }
    validated = validate_manifest(manifest)
    for artifact in validated["artifacts"]:
        _verify_artifact(candidate_root, artifact)
    if enforce_candidate_set:
        _validate_candidate_file_set(candidate_root, validated["artifacts"])
    return validated


def write_manifest(
    destination: Path,
    manifest: Mapping[str, Any],
    *,
    candidate_root: Path | None = None,
    enforce_candidate_set: bool = True,
) -> None:
    validated = validate_manifest(dict(manifest))
    root = candidate_root if candidate_root is not None else destination.parent
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(canonical_json_bytes(validated))
    verify_manifest(
        root,
        expected_commits={
            source["repository"]: source["commit"]
            for source in validated["sources"]
        },
        enforce_candidate_set=enforce_candidate_set,
    )


def load_and_validate_manifest(
    path: Path,
    *,
    expected_commits: Mapping[str, str] | None = None,
    enforce_candidate_set: bool = True,
) -> dict[str, Any]:
    try:
        source = path.read_text(encoding="utf-8")
    except OSError as error:
        raise CandidateError(
            f"could not read candidate manifest {path}: {error}"
        ) from error
    document = load_json_no_duplicates(source, str(path))
    validated = validate_manifest(
        document,
        expected_commits=expected_commits,
    )
    if canonical_json_bytes(validated) != source.encode("utf-8"):
        raise CandidateError("candidate manifest is not canonical JSON")
    for artifact in validated["artifacts"]:
        _verify_artifact(path.parent, artifact)
    if enforce_candidate_set:
        _validate_candidate_file_set(path.parent, validated["artifacts"])
    return validated


def verify_manifest(
    candidate_root: Path,
    *,
    expected_commits: Mapping[str, str] | None = None,
    enforce_candidate_set: bool = True,
) -> dict[str, Any]:
    return load_and_validate_manifest(
        candidate_root / MANIFEST_FILENAME,
        expected_commits=expected_commits,
        enforce_candidate_set=enforce_candidate_set,
    )
