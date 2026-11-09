"""Safe ZIP inventory for release-candidate promotion.

Before the promotion extracts a downloaded candidate artifact, it lists the
archive's members with their sizes and SHA-256 digests and rejects
non-canonical, colliding and symbolic-link members.
"""

from __future__ import annotations

import hashlib
from pathlib import Path, PurePosixPath
import re
import stat
from typing import Any, Mapping
import zipfile


INVENTORY_FIELDS = {"path", "bytes", "sha256"}
_SHA256 = re.compile(r"^[0-9a-f]{64}$")


class CandidateError(RuntimeError):
    """A candidate artifact failed safe-ZIP validation."""


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
    label: str,
    directory_names: set[str],
) -> list[tuple[str, str]]:
    if len(names) != len(set(names)):
        raise CandidateError(f"{label} contains duplicate archive paths")
    result: list[tuple[str, str]] = []
    for raw in names:
        relative = raw
        if raw in directory_names:
            if not raw.endswith("/") or raw.endswith("//"):
                raise CandidateError(
                    f"{label} contains non-canonical directory {raw!r}"
                )
            relative = raw[:-1]
        checked_relative_path(relative)
        result.append((raw, relative))
    normalized = [relative for _, relative in result]
    if len(normalized) != len(set(normalized)):
        raise CandidateError(
            f"{label} contains colliding normalized archive paths"
        )
    return result


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
