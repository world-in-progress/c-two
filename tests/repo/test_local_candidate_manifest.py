from __future__ import annotations

from copy import deepcopy
import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
from typing import Any, Callable
import zipfile

import pytest


REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY))

import tools.local_rc.build_candidate as candidate_builder  # noqa: E402
from tools.local_rc.artifact_manifest import (  # noqa: E402
    MANIFEST_FILENAME,
    MANIFEST_SCHEMA,
    CandidateError,
    artifact_descriptor,
    archive_inventory,
    canonical_json_bytes,
    construct_manifest,
    load_and_validate_manifest,
    verify_manifest,
    write_manifest,
)
from tools.local_rc.build_candidate import (  # noqa: E402
    CARGO_LOCK_MANIFESTS,
    RUST_PACKAGE_ORDER,
    prepare_packaging_snapshot,
    require_absent_output,
)


C2_COMMIT = "1" * 40
FASTDB_COMMIT = "2" * 40


def test_rust_package_order_covers_the_complete_first_party_closure() -> None:
    assert tuple(name for name, _ in RUST_PACKAGE_ORDER) == (
        "c2-local-security",
        "c2-config",
        "c2-local",
        "c2-contract",
        "c2-error",
        "c2-mem",
        "c2-codegen",
        "c2-mem-ffi",
        "c2-wire",
        "c2-server",
        "c2-ipc",
        "c2-http",
        "c2-core",
        "c2-python-native",
        "c2-cli",
        "c-two",
    )


def test_candidate_output_must_be_absent(tmp_path: Path) -> None:
    output = tmp_path / "candidate"
    require_absent_output(output)
    output.mkdir()
    with pytest.raises(CandidateError, match="absent"):
        require_absent_output(output)


def test_registry_closure_is_built_before_package_paths_are_rewritten(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    events: list[str] = []

    def record_snapshot(
        repository: Path,
        commit: str,
        destination: Path,
    ) -> None:
        del repository, commit
        events.append(f"snapshot:{destination.name}")

    monkeypatch.setattr(
        candidate_builder,
        "copy_git_snapshot",
        record_snapshot,
    )
    monkeypatch.setattr(
        candidate_builder,
        "build_local_registry",
        lambda **_: events.append("registry-sync"),
    )
    monkeypatch.setattr(
        candidate_builder,
        "build_bootstrap_registry",
        lambda **_: events.append("bootstrap-registry"),
    )
    monkeypatch.setattr(
        candidate_builder,
        "prepare_package_snapshot",
        lambda _: events.append("rewrite-paths"),
    )

    snapshot = prepare_packaging_snapshot(
        source_root=tmp_path / "source",
        registry=tmp_path / "registry",
        bootstrap_registry=tmp_path / "bootstrap-registry",
        bootstrap_cargo_home=tmp_path / "bootstrap-cargo-home",
        c2_commit=C2_COMMIT,
        fastdb_commit=FASTDB_COMMIT,
        fastdb_archives=(tmp_path / "fastdb.crate",),
        environment={},
        bootstrap_target=tmp_path / "bootstrap-target",
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert snapshot == tmp_path / "source/c-two"
    assert events == [
        "snapshot:c-two",
        "snapshot:fastdb",
        "registry-sync",
        "bootstrap-registry",
        "rewrite-paths",
    ]


def test_rust_closure_packaging_runs_from_the_source_snapshot(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    snapshot = tmp_path / "source/c-two"
    snapshot.mkdir(parents=True)
    observed_cwds: list[Path] = []
    monkeypatch.setattr(
        candidate_builder,
        "run",
        lambda _command, **kwargs: observed_cwds.append(kwargs["cwd"]),
    )
    monkeypatch.setattr(
        candidate_builder,
        "_crate_archive_path",
        lambda target, name, version: target / f"{name}-{version}.crate",
    )
    monkeypatch.setattr(
        candidate_builder,
        "inspect_normalized_manifest",
        lambda _: {},
    )

    archives = candidate_builder.package_rust_closure(
        snapshot=snapshot,
        environment={},
        package_target=tmp_path / "target",
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert len(archives) == len(RUST_PACKAGE_ORDER)
    assert observed_cwds == [snapshot] * len(RUST_PACKAGE_ORDER)


def test_bootstrap_archives_can_be_imported_before_the_next_package(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    snapshot = tmp_path / "source/c-two"
    snapshot.mkdir(parents=True)
    events: list[str] = []

    monkeypatch.setattr(
        candidate_builder,
        "run",
        lambda command, **_: events.append(
            f"package:{Path(command[3]).parent.name}"
        ),
    )
    monkeypatch.setattr(
        candidate_builder,
        "_crate_archive_path",
        lambda target, name, version: target / f"{name}-{version}.crate",
    )
    monkeypatch.setattr(
        candidate_builder,
        "inspect_normalized_manifest",
        lambda _: {},
    )

    archives = candidate_builder.package_rust_closure(
        snapshot=snapshot,
        environment={},
        package_target=tmp_path / "target",
        log=candidate_builder.RunLog(tmp_path / "run.log"),
        on_archive=lambda archive: events.append(f"import:{archive.name}"),
    )

    assert len(archives) == len(RUST_PACKAGE_ORDER)
    assert events[0].startswith("package:")
    assert events[1] == f"import:{archives[0].name}"
    assert events[2].startswith("package:")
    assert events[3] == f"import:{archives[1].name}"


def test_snapshot_locks_are_regenerated_against_the_final_registry(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    snapshot = tmp_path / "source/c-two"
    observed: list[tuple[Path, bool]] = []
    for relative_manifest in CARGO_LOCK_MANIFESTS:
        manifest = snapshot / relative_manifest
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text("[package]\n", encoding="utf-8")
        (manifest.parent / "Cargo.lock").write_text(
            "bootstrap checksum",
            encoding="utf-8",
        )

    def record_lock(command, **_) -> None:
        manifest = Path(command[-1])
        observed.append(
            (manifest, (manifest.parent / "Cargo.lock").exists())
        )

    monkeypatch.setattr(candidate_builder, "run", record_lock)

    candidate_builder.regenerate_package_snapshot_locks(
        snapshot=snapshot,
        environment={},
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert [path.relative_to(snapshot).as_posix() for path, _ in observed] == list(
        CARGO_LOCK_MANIFESTS
    )
    assert all(not existed for _, existed in observed)


def test_contract_codegen_creates_each_destination_parent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "candidate"
    observed: list[Path] = []

    def record_codegen(command, **_) -> None:
        destination = Path(command[-1])
        assert destination.parent.is_dir()
        observed.append(destination)

    monkeypatch.setattr(candidate_builder, "run", record_codegen)

    generated = candidate_builder.generate_contract_trees(
        c3=tmp_path / "c3",
        output=output,
        environment={},
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert generated == observed
    assert len(generated) == 9


def test_npm_pack_requires_the_exact_pinned_tarball_name(
    tmp_path: Path,
) -> None:
    destination = tmp_path / "typescript"
    destination.mkdir()
    expected = destination / "typescript-5.9.3.tgz"
    expected.write_bytes(b"package")

    assert (
        candidate_builder._required_npm_pack_file(
            destination,
            expected.name,
            "typescript",
        )
        == expected
    )
    with pytest.raises(CandidateError, match="exact file"):
        candidate_builder._required_npm_pack_file(
            destination,
            "typescript-latest.tgz",
            "typescript",
        )


def test_fastdb_manifest_build_is_derived_from_artifact_evidence() -> None:
    base = {
        "platform": "darwin-arm64",
        "toolchains": {"cargo": "cargo 1.91.0"},
        "verified_platforms": ["darwin-arm64"],
        "unverified_platforms": ["linux-aarch64"],
    }
    manifest = {
        "artifacts": [
            {
                "build": {
                    **base,
                    "commands": ["cargo package"],
                    "target": "3.14",
                    "toolchains": {
                        **base["toolchains"],
                        "python": "CPython 3.14.5",
                    },
                }
            },
            {
                "build": {
                    **base,
                    "commands": ["uv build"],
                    "target": "cp310",
                    "toolchains": {
                        **base["toolchains"],
                        "python": "CPython 3.10.17",
                    },
                }
            },
        ]
    }

    build = candidate_builder._aggregate_fastdb_manifest_build(manifest)

    assert build["commands"] == ["cargo package", "uv build"]
    assert build["platform"] == "darwin-arm64"
    assert build["target"] == "multi-target"
    assert build["toolchains"] == {
        "cargo": "cargo 1.91.0",
        "python@3.14": "CPython 3.14.5",
        "python@cp310": "CPython 3.10.17",
    }

    conflicting = deepcopy(manifest)
    conflicting["artifacts"][0]["build"]["target"] = "cp310"
    conflicting["artifacts"][1]["build"]["toolchains"]["cargo"] = "cargo 2"
    with pytest.raises(CandidateError, match="inconsistent"):
        candidate_builder._aggregate_fastdb_manifest_build(conflicting)


def test_python_build_does_not_create_unmanifested_gitignore_files(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "candidate"
    snapshot = tmp_path / "source/c-two"
    commands: list[list[str]] = []

    def build_artifact(command, **_) -> None:
        command = list(command)
        commands.append(command)
        destination = Path(command[command.index("--out-dir") + 1])
        destination.mkdir(parents=True, exist_ok=True)
        if "--sdist" in command:
            artifact = destination / "c_two-0.5.1.tar.gz"
        elif destination.name == "cp310":
            artifact = destination / "c_two-0.5.1-cp310.whl"
        else:
            artifact = destination / "c_two-0.5.1-cp314.whl"
        artifact.write_bytes(b"package")

    monkeypatch.setattr(candidate_builder, "run", build_artifact)
    monkeypatch.setattr(
        candidate_builder,
        "assert_no_forbidden_source_references",
        lambda *_args, **_kwargs: None,
    )

    candidate_builder.build_python_packages(
        snapshot=snapshot,
        output=output,
        python_current=tmp_path / "python3.14",
        python_310=tmp_path / "python3.10",
        environment={},
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert len(commands) == 3
    assert all("--no-create-gitignore" in command for command in commands)
    assert not tuple(output.rglob(".gitignore"))


def test_python_runtime_dependency_wheels_are_exact_and_abi_specific(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    output = tmp_path / "candidate"
    commands: list[list[str]] = []

    def download_wheel(command, **_) -> None:
        command = list(command)
        commands.append(command)
        destination = Path(command[command.index("--dest") + 1])
        destination.mkdir(parents=True, exist_ok=True)
        version = command[-1].split("==", 1)[1]
        tag = "cp310" if destination.name == "cp310" else "cp314"
        (destination / f"numpy-{version}-{tag}-{tag}-macosx.whl").write_bytes(
            b"wheel"
        )

    monkeypatch.setattr(candidate_builder, "run", download_wheel)
    monkeypatch.setattr(
        candidate_builder,
        "assert_no_forbidden_source_references",
        lambda *_args, **_kwargs: None,
    )

    wheels = candidate_builder.download_python_runtime_dependencies(
        output=output,
        python_current=tmp_path / "python3.14",
        python_310=tmp_path / "python3.10",
        environment={},
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    )

    assert [wheel.parent.name for wheel in wheels] == ["cp310", "current"]
    assert [command[-1] for command in commands] == [
        f"numpy=={candidate_builder.NUMPY_VERSIONS['cp310']}",
        f"numpy=={candidate_builder.NUMPY_VERSIONS['current']}",
    ]
    assert all("--only-binary=:all:" in command for command in commands)
    assert all("--no-deps" in command for command in commands)


def test_wheel_inventory_accepts_canonical_explicit_directories(
    tmp_path: Path,
) -> None:
    wheel = tmp_path / "numpy-2.2.6-cp310.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("numpy-2.2.6.dist-info/", b"")
        archive.writestr(
            "numpy-2.2.6.dist-info/METADATA",
            b"Name: numpy\nVersion: 2.2.6\n",
        )

    assert archive_inventory(wheel, label="numpy") == [
        {
            "path": "numpy-2.2.6.dist-info/METADATA",
            "bytes": 27,
            "sha256": hashlib.sha256(
                b"Name: numpy\nVersion: 2.2.6\n"
            ).hexdigest(),
        }
    ]

    hostile = tmp_path / "hostile.whl"
    with zipfile.ZipFile(hostile, "w") as archive:
        archive.writestr("numpy//", b"")
    with pytest.raises(CandidateError, match="non-canonical"):
        archive_inventory(hostile, label="hostile")


def test_pip_toolchain_evidence_drops_machine_specific_install_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        candidate_builder,
        "tool_version",
        lambda *_args, **_kwargs: (
            "pip 26.1.1 from /opt/homebrew/lib/python/site-packages/pip "
            "(python 3.14)"
        ),
    )

    assert candidate_builder.pip_version(
        tmp_path / "python",
        log=candidate_builder.RunLog(tmp_path / "run.log"),
    ) == "pip 26.1.1"


def _sources() -> list[dict[str, object]]:
    return [
        {
            "repository": "c-two",
            "commit": C2_COMMIT,
            "source_date_epoch": 1_784_912_400,
            "worktree_clean": True,
            "worktree_state_sha256": "a" * 64,
        },
        {
            "repository": "fastdb",
            "commit": FASTDB_COMMIT,
            "source_date_epoch": 1_784_912_300,
            "worktree_clean": True,
            "worktree_state_sha256": "b" * 64,
        },
    ]


def _build() -> dict[str, object]:
    return {
        "commands": ["cargo package --manifest-path $SOURCE_ROOT/Cargo.toml"],
        "platform": "darwin-arm64",
        "target": "aarch64-apple-darwin",
        "toolchains": {"cargo": "cargo 1.91.0", "rustc": "rustc 1.91.0"},
        "verified_platforms": ["darwin-arm64"],
        "unverified_platforms": ["linux-aarch64", "linux-x86_64"],
        "unsupported_platforms": ["win32-amd64"],
    }


def _candidate(tmp_path: Path) -> tuple[Path, dict[str, Any]]:
    root = tmp_path / "candidate"
    artifact = root / "rust/c-two-0.1.0.crate"
    artifact.parent.mkdir(parents=True)
    contents = b"[package]\nname = \"c-two\"\nversion = \"0.1.0\"\n"
    with tarfile.open(artifact, "w:gz") as archive:
        info = tarfile.TarInfo("c-two-0.1.0/Cargo.toml")
        info.size = len(contents)
        archive.addfile(info, io.BytesIO(contents))
    descriptor = artifact_descriptor(
        root,
        path="rust/c-two-0.1.0.crate",
        kind="rust-crate",
        owner_repository="c-two",
        source_commit=C2_COMMIT,
        package={"name": "c-two", "version": "0.1.0"},
        build=_build(),
    )
    manifest = construct_manifest(
        root,
        sources=_sources(),
        artifacts=[descriptor],
        build=_build(),
        enforce_candidate_set=False,
    )
    return root, manifest


def test_manifest_is_canonical_and_byte_stable(tmp_path: Path) -> None:
    root, manifest = _candidate(tmp_path)
    destination = root / MANIFEST_FILENAME

    write_manifest(
        destination,
        manifest,
        candidate_root=root,
        enforce_candidate_set=False,
    )
    first = destination.read_bytes()
    write_manifest(
        destination,
        manifest,
        candidate_root=root,
        enforce_candidate_set=False,
    )

    assert destination.read_bytes() == first
    assert first == canonical_json_bytes(manifest)
    assert first.endswith(b"\n")
    assert load_and_validate_manifest(
        destination,
        expected_commits={"c-two": C2_COMMIT, "fastdb": FASTDB_COMMIT},
        enforce_candidate_set=False,
    ) == manifest


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (
            lambda manifest: manifest.__setitem__("schema", "unknown.v9"),
            "schema",
        ),
        (
            lambda manifest: manifest["sources"][0].__setitem__(
                "commit",
                "3" * 40,
            ),
            "commit",
        ),
        (
            lambda manifest: manifest["artifacts"][0].__setitem__(
                "path",
                "/private/tmp/c-two.crate",
            ),
            "relative",
        ),
        (
            lambda manifest: manifest["artifacts"][0]["inventory"].append(
                deepcopy(manifest["artifacts"][0]["inventory"][0]),
            ),
            "duplicate",
        ),
        (
            lambda manifest: manifest["artifacts"][0].__setitem__(
                "sha256",
                "f" * 64,
            ),
            "SHA-256",
        ),
        (
            lambda manifest: manifest["artifacts"][0].__setitem__(
                "bytes",
                999,
            ),
            "byte",
        ),
    ],
)
def test_hostile_manifest_variants_are_rejected(
    tmp_path: Path,
    mutate: Callable[[dict[str, Any]], object],
    message: str,
) -> None:
    root, manifest = _candidate(tmp_path)
    mutate(manifest)
    destination = root / MANIFEST_FILENAME
    destination.write_bytes(canonical_json_bytes(manifest))

    with pytest.raises(CandidateError, match=message):
        load_and_validate_manifest(
            destination,
            expected_commits={"c-two": C2_COMMIT, "fastdb": FASTDB_COMMIT},
            enforce_candidate_set=False,
        )


def test_manifest_cannot_list_an_artifact_before_it_exists(
    tmp_path: Path,
) -> None:
    root, manifest = _candidate(tmp_path)
    (root / "rust/c-two-0.1.0.crate").unlink()
    destination = root / MANIFEST_FILENAME
    destination.write_bytes(canonical_json_bytes(manifest))

    with pytest.raises(CandidateError, match="missing"):
        verify_manifest(
            root,
            expected_commits={"c-two": C2_COMMIT, "fastdb": FASTDB_COMMIT},
            enforce_candidate_set=False,
        )


def test_manifest_detects_artifact_tampering(tmp_path: Path) -> None:
    root, manifest = _candidate(tmp_path)
    write_manifest(
        root / MANIFEST_FILENAME,
        manifest,
        candidate_root=root,
        enforce_candidate_set=False,
    )
    (root / "rust/c-two-0.1.0.crate").write_bytes(b"tampered")

    with pytest.raises(CandidateError, match="SHA-256|byte"):
        verify_manifest(
            root,
            expected_commits={"c-two": C2_COMMIT, "fastdb": FASTDB_COMMIT},
            enforce_candidate_set=False,
        )


def test_duplicate_json_keys_are_rejected(tmp_path: Path) -> None:
    root, _ = _candidate(tmp_path)
    destination = root / MANIFEST_FILENAME
    destination.write_text(
        json.dumps(
            {
                "schema": MANIFEST_SCHEMA,
                "sources": [],
                "artifacts": [],
            }
        ).replace(
            f'"schema": "{MANIFEST_SCHEMA}"',
            f'"schema": "{MANIFEST_SCHEMA}", "schema": "{MANIFEST_SCHEMA}"',
            1,
        ),
        encoding="utf-8",
    )

    with pytest.raises(CandidateError, match="duplicate JSON key"):
        load_and_validate_manifest(
            destination,
            expected_commits={"c-two": C2_COMMIT, "fastdb": FASTDB_COMMIT},
            enforce_candidate_set=False,
        )
