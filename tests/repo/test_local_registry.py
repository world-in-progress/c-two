from __future__ import annotations

import io
from pathlib import Path
import sys
import tarfile

import pytest


REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY))

from tools.local_rc.local_registry import (  # noqa: E402
    C2_VERSION,
    FASTDB_VERSION,
    RegistryError,
    add_crate_archive,
    assert_no_forbidden_source_references,
    inspect_normalized_manifest,
    render_cargo_config,
    render_rust_consumer_manifest,
    scan_source_path_dependencies,
    validate_rust_consumer_manifest,
)


def _crate_with_manifest(tmp_path: Path, manifest: str) -> Path:
    archive = tmp_path / "fixture-0.1.0.crate"
    contents = manifest.encode("utf-8")
    with tarfile.open(archive, "w:gz") as tar:
        info = tarfile.TarInfo("fixture-0.1.0/Cargo.toml")
        info.size = len(contents)
        tar.addfile(info, io.BytesIO(contents))
    return archive


def test_every_first_party_path_dependency_has_an_exact_version() -> None:
    findings = scan_source_path_dependencies(REPOSITORY)
    assert findings == []


def test_isolated_consumer_manifest_is_version_only() -> None:
    manifest = render_rust_consumer_manifest()

    assert f'c-two = "={C2_VERSION}"' in manifest
    assert f'fastdb = "={FASTDB_VERSION}"' in manifest
    assert "path =" not in manifest
    assert "git =" not in manifest
    assert "[patch." not in manifest
    assert "[replace]" not in manifest
    validate_rust_consumer_manifest(manifest)


@pytest.mark.parametrize(
    "injection",
    [
        '\n[patch.crates-io]\nc-two = { path = "/tmp/c-two" }\n',
        '\n[replace]\n"c-two:0.1.0" = { git = "https://example.test/c-two" }\n',
        '\n[source.checkout]\ndirectory = "../c-two"\n',
    ],
)
def test_isolated_consumer_rejects_source_escape_hatches(
    injection: str,
) -> None:
    with pytest.raises(RegistryError):
        validate_rust_consumer_manifest(
            render_rust_consumer_manifest() + injection
        )


def test_rendered_registry_config_is_offline_and_run_local(
    tmp_path: Path,
) -> None:
    registry = tmp_path / "registry"
    config = render_cargo_config(registry)

    assert 'replace-with = "local-candidate"' in config
    assert f'local-registry = "{registry.as_posix()}"' in config
    assert "offline = true" in config
    assert str(REPOSITORY) not in config


def test_packaged_normalized_manifest_has_versions_and_no_paths(
    tmp_path: Path,
) -> None:
    archive = _crate_with_manifest(
        tmp_path,
        """
[package]
name = "fixture"
version = "0.1.0"

[dependencies]
c2-core = "0.1.0"
fastdb = "0.2.0"
""",
    )

    inspect_normalized_manifest(archive)


def test_local_archive_import_writes_checksum_and_dependency_index(
    tmp_path: Path,
) -> None:
    archive = _crate_with_manifest(
        tmp_path,
        """
[package]
name = "fixture"
version = "0.1.0"

[dependencies]
c2-core = { version = "0.1.0", features = ["relay"] }
fastdb = { version = "0.2.0", optional = true }
""",
    )
    registry = tmp_path / "registry"
    registry.mkdir()

    entry = add_crate_archive(registry, archive)

    imported = registry / "fixture-0.1.0.crate"
    index = registry / "index/fi/xt/fixture"
    assert imported.read_bytes() == archive.read_bytes()
    assert entry["name"] == "fixture"
    assert entry["vers"] == "0.1.0"
    assert entry["cksum"] == __import__("hashlib").sha256(
        archive.read_bytes()
    ).hexdigest()
    assert [
        (dependency["name"], dependency["req"])
        for dependency in entry["deps"]
    ] == [("c2-core", "0.1.0"), ("fastdb", "0.2.0")]
    assert index.read_text(encoding="utf-8").endswith("\n")


@pytest.mark.parametrize(
    ("manifest", "message"),
    [
        (
            """
[package]
name = "fixture"
version = "0.1.0"
[dependencies]
c2-core = { path = "../../core/runtime/c2-core" }
""",
            "path",
        ),
        (
            """
[package]
name = "fixture"
version = "0.1.0"
[dependencies]
c2-core = "*"
""",
            "exact",
        ),
    ],
)
def test_packaged_normalized_manifest_rejects_paths_or_loose_versions(
    tmp_path: Path,
    manifest: str,
    message: str,
) -> None:
    archive = _crate_with_manifest(tmp_path, manifest)

    with pytest.raises(RegistryError, match=message):
        inspect_normalized_manifest(archive)


def test_archives_and_diagnostics_reject_active_repository_paths(
    tmp_path: Path,
) -> None:
    clean = tmp_path / "clean.txt"
    clean.write_text("portable package diagnostics", encoding="utf-8")
    assert_no_forbidden_source_references(
        [clean],
        forbidden_roots=(REPOSITORY, REPOSITORY.parent / "fastdb"),
    )

    hostile = tmp_path / "hostile.txt"
    hostile.write_text(
        f"linked from {REPOSITORY}/sdk/rust",
        encoding="utf-8",
    )
    with pytest.raises(RegistryError, match="active repository"):
        assert_no_forbidden_source_references(
            [hostile],
            forbidden_roots=(REPOSITORY, REPOSITORY.parent / "fastdb"),
        )
