"""Tests for .github/scripts/release_candidate.py"""

from __future__ import annotations

import hashlib
import io
import json
import sys
import tarfile
from pathlib import Path

import pytest

_ROOT = next(
    parent
    for parent in Path(__file__).resolve().parents
    if (parent / ".github" / "scripts" / "release_candidate.py").is_file()
)
_SCRIPT_DIR = str(_ROOT / ".github" / "scripts")


@pytest.fixture(scope="module")
def rc():
    sys.path.insert(0, _SCRIPT_DIR)
    sys.modules.pop("release_candidate", None)
    import release_candidate

    yield release_candidate
    sys.path.remove(_SCRIPT_DIR)
    sys.modules.pop("release_candidate", None)


def write(tree: Path, name: str, payload: bytes) -> Path:
    path = tree / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(payload)
    return path


def sidecar(rc, path: Path) -> Path:
    sidecar_path = path.with_name(path.name + ".sha256")
    sidecar_path.write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n",
                            encoding="utf-8")
    return sidecar_path


# ── checksum / verify ────────────────────────────────────────────────────────


def test_checksum_writes_portable_sidecars_and_verify_accepts_them(rc, tmp_path):
    artifact = write(tmp_path, "c3-x86_64-pc-windows-msvc.exe", b"binary")
    assert rc.main(["checksum", str(artifact)]) == 0
    digest = hashlib.sha256(b"binary").hexdigest()
    assert artifact.with_name(artifact.name + ".sha256").read_text(
        encoding="utf-8") == f"{digest}  c3-x86_64-pc-windows-msvc.exe\n"
    assert rc.main(["verify", str(artifact)]) == 0


def test_verify_fails_on_missing_sidecar_and_tampered_bytes(rc, tmp_path, capsys):
    untouched = write(tmp_path, "kept.bin", b"kept")
    sidecar(rc, untouched)

    no_sidecar = write(tmp_path, "lonely.bin", b"lonely")
    tampered = write(tmp_path, "changed.bin", b"original")
    sidecar(rc, tampered)
    tampered.write_bytes(b"mutated")

    assert rc.main(["verify", str(untouched), str(no_sidecar), str(tampered)]) == 1
    errors = capsys.readouterr().err
    assert "missing sidecar for lonely.bin" in errors
    assert "digest mismatch for changed.bin" in errors


def test_verify_rejects_malformed_sidecar_checksums(rc, tmp_path, capsys):
    artifact = write(tmp_path, "artifact.whl", b"wheel")
    artifact.with_name("artifact.whl.sha256").write_text("not-a-digest  artifact.whl\n",
                                                         encoding="utf-8")
    assert rc.main(["verify", str(artifact)]) == 1
    assert "Malformed sidecar" in capsys.readouterr().err


# ── inspect-sdist ────────────────────────────────────────────────────────────


def build_sdist(directory: Path, members: dict[str, bytes]) -> Path:
    archive = directory / "c_two-0.6.0.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        for name, payload in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(payload)
            tar.addfile(info, io.BytesIO(payload))
    return archive


def good_members() -> dict[str, bytes]:
    """Mirror the member tree of the real maturin-built 0.6.0 sdist.

    Maturin rewrites ``manifest-path`` to the embedded crate tree, rewrites
    patched registry dependencies into in-archive path dependencies (the
    fastdb crates), prunes unreachable workspace members (c2-mem-ffi is
    listed by the embedded workspace manifest but absent), and leaves unused
    ``[workspace.dependencies]`` entries in place. The fastdb bindings
    workspace also carries ``[[test]]`` path entries that point outside the
    archive; they are not dependency edges and must not be followed.
    """
    return {
        "c_two-0.6.0/PKG-INFO": b"Metadata-Version: 2.1\nName: c-two\nVersion: 0.6.0\n\n",
        "c_two-0.6.0/pyproject.toml": (
            b"[project]\nname = 'c-two'\nversion = '0.6.0'\n\n"
            b"[tool.maturin]\nmanifest-path = 'c-two/sdk/python/native/Cargo.toml'\n"
        ),
        "c_two-0.6.0/c-two/sdk/python/native/Cargo.toml": (
            b"[package]\nname = 'c2-python-native'\n\n"
            b"[dependencies]\n"
            b"c2-codegen = { path = '../../../core/foundation/c2-codegen' }\n"
            b"c2-core = { path = '../../../core/runtime/c2-core' }\n"
        ),
        "c_two-0.6.0/c-two/sdk/python/native/src/lib.rs": b"pub fn native() {}\n",
        # The embedded workspace manifest lists the pruned c2-mem-ffi member
        # and an unused workspace dependency path; neither is reachable from
        # the maturin root crate, so neither may be required.
        "c_two-0.6.0/c-two/core/Cargo.toml": (
            b"[workspace]\nmembers = [\n"
            b"  'foundation/c2-codegen',\n"
            b"  'foundation/c2-contract',\n"
            b"  'foundation/c2-mem',\n"
            b"  'foundation/c2-mem-ffi',\n"
            b"  'runtime/c2-core',\n"
            b"]\n\n"
            b"[workspace.dependencies]\n"
            b"c2-mem-ffi = { path = 'foundation/c2-mem-ffi' }\n"
            b"c2-mem = { path = 'foundation/c2-mem' }\n"
        ),
        "c_two-0.6.0/c-two/core/foundation/c2-codegen/Cargo.toml": (
            b"[package]\nname = 'c2-codegen'\n\n"
            b"[dependencies]\n"
            # Maturin rewrote the patched crates.io fastdb dependency into an
            # in-archive path dependency.
            b"fastdb = { version = '0.2.1', path = '../../../../fastdb/bindings/rust/fastdb' }\n"
        ),
        "c_two-0.6.0/c-two/core/foundation/c2-codegen/src/lib.rs": b"pub fn codegen() {}\n",
        "c_two-0.6.0/c-two/core/foundation/c2-contract/Cargo.toml": b"[package]\nname = 'c2-contract'\n",
        "c_two-0.6.0/c-two/core/foundation/c2-contract/src/lib.rs": b"pub fn contract() {}\n",
        "c_two-0.6.0/c-two/core/foundation/c2-mem/Cargo.toml": b"[package]\nname = 'c2-mem'\n",
        "c_two-0.6.0/c-two/core/foundation/c2-mem/src/lib.rs": b"pub fn mem() {}\n",
        "c_two-0.6.0/c-two/core/runtime/c2-core/Cargo.toml": (
            b"[package]\nname = 'c2-core'\n\n"
            b"[dependencies]\n"
            b"c2-contract = { path = '../../foundation/c2-contract' }\n"
            b"c2-mem = { workspace = true }\n"
        ),
        "c_two-0.6.0/c-two/core/runtime/c2-core/src/lib.rs": b"pub fn core() {}\n",
        "c_two-0.6.0/fastdb/bindings/rust/Cargo.toml": (
            b"[package]\nname = 'fastdb-payload-integration-tests'\npublish = false\n\n"
            b"[workspace]\nmembers = ['fastdb-sys', 'fastdb']\n\n"
            b"[[test]]\nname = 'payload'\n"
            # Not a dependency edge; the referenced tests are pruned from the
            # archive and following this path would falsely escape the root.
            b"path = '../../tests/rust/payload/tests/payload.rs'\n"
        ),
        "c_two-0.6.0/fastdb/bindings/rust/fastdb/Cargo.toml": (
            b"[package]\nname = 'fastdb'\n\n"
            b"[dependencies]\n"
            b"fastdb-sys = { version = '=0.2.0', path = '../fastdb-sys' }\n"
        ),
        "c_two-0.6.0/fastdb/bindings/rust/fastdb/src/lib.rs": b"pub fn fastdb() {}\n",
        "c_two-0.6.0/fastdb/bindings/rust/fastdb-sys/Cargo.toml": (
            b"[package]\nname = 'fastdb-sys'\n\n[lib]\npath = 'src/lib.rs'\n"
        ),
        "c_two-0.6.0/fastdb/bindings/rust/fastdb-sys/build.rs": b"fn main() {}\n",
        "c_two-0.6.0/fastdb/bindings/rust/fastdb-sys/src/lib.rs": b"pub fn sys() {}\n",
    }


REACHABLE_CRATES = {
    "c_two-0.6.0/c-two/sdk/python/native",
    "c_two-0.6.0/c-two/core/foundation/c2-codegen",
    "c_two-0.6.0/c-two/core/foundation/c2-contract",
    "c_two-0.6.0/c-two/core/foundation/c2-mem",
    "c_two-0.6.0/c-two/core/runtime/c2-core",
    "c_two-0.6.0/fastdb/bindings/rust/fastdb",
    "c_two-0.6.0/fastdb/bindings/rust/fastdb-sys",
}


def test_inspect_sdist_accepts_the_reachable_maturin_closure(rc, tmp_path):
    report = rc.inspect_sdist(build_sdist(tmp_path, good_members()))
    assert report["status"] == "passed", report
    assert report["project"] == {"name": "c-two", "version": "0.6.0"}
    assert report["manifest_path"] == "c-two/sdk/python/native/Cargo.toml"
    embedded = {entry["directory"] for entry in report["path_dependencies"]}
    assert embedded == REACHABLE_CRATES - {"c_two-0.6.0/c-two/sdk/python/native"}
    # Pruned members and unused workspace dependency paths never appear.
    assert not any("c2-mem-ffi" in directory for directory in embedded)
    assert report["workspace_roots"] == ["c_two-0.6.0/c-two/core/Cargo.toml",
                                         "c_two-0.6.0/fastdb/bindings/rust/Cargo.toml"]
    assert report["cargo_manifests"] == len(REACHABLE_CRATES)
    inherited = [entry for entry in report["path_dependencies"]
                 if entry["from"].endswith("c2-core/Cargo.toml") and entry["name"] == "c2-mem"]
    assert inherited and inherited[0]["rust_sources"] == 1


def test_inspect_sdist_stamps_the_sealed_source_sha(rc, tmp_path):
    archive = build_sdist(tmp_path, good_members())
    assert rc.main(["inspect-sdist", str(archive), "--output", str(tmp_path / "r.json"),
                    "--source-sha", "a" * 40]) == 0
    report = json.loads((tmp_path / "r.json").read_text(encoding="utf-8"))
    assert report["source_commit_sha"] == "a" * 40


def test_inspect_sdist_fails_when_a_reachable_dependency_is_missing(rc, tmp_path, capsys):
    members = good_members()
    del members["c_two-0.6.0/fastdb/bindings/rust/fastdb/Cargo.toml"]
    del members["c_two-0.6.0/fastdb/bindings/rust/fastdb/src/lib.rs"]
    archive = build_sdist(tmp_path, members)
    assert rc.main(["inspect-sdist", str(archive)]) == 1
    report = rc.inspect_sdist(archive)
    assert report["status"] == "failed"
    assert any("fastdb" in missing for missing in report["missing"])


def test_inspect_sdist_fails_when_a_reachable_crate_has_no_sources(rc, tmp_path):
    members = good_members()
    del members["c_two-0.6.0/c-two/core/foundation/c2-contract/src/lib.rs"]
    report = rc.inspect_sdist(build_sdist(tmp_path, members))
    assert report["status"] == "failed"
    assert any("no .rs files" in error for error in report["errors"])


def test_inspect_sdist_fails_when_an_inherited_workspace_path_is_missing(rc, tmp_path):
    members = good_members()
    for name in list(members):
        if "/c2-mem/" in name:
            del members[name]
    report = rc.inspect_sdist(build_sdist(tmp_path, members))
    assert report["status"] == "failed"
    assert any("c2-mem" in missing for missing in report["missing"])


def test_inspect_sdist_rejects_paths_escaping_the_archive(rc, tmp_path):
    members = good_members()
    members["c_two-0.6.0/c-two/core/foundation/c2-contract/Cargo.toml"] = (
        b"[package]\nname = 'c2-contract'\n\n"
        b"[dependencies]\n"
        b"evil = { path = '../../../../../../../../evil' }\n"
    )
    report = rc.inspect_sdist(build_sdist(tmp_path, members))
    assert report["status"] == "failed"
    assert any("escapes the archive root" in error for error in report["errors"])


def test_inspect_sdist_requires_identity_members_and_matching_metadata(rc, tmp_path):
    missing_pkg_info = good_members()
    del missing_pkg_info["c_two-0.6.0/PKG-INFO"]
    assert rc.inspect_sdist(build_sdist(tmp_path, missing_pkg_info))["missing"] == ["PKG-INFO"]

    missing_manifest = good_members()
    del missing_manifest["c_two-0.6.0/c-two/sdk/python/native/Cargo.toml"]
    report = rc.inspect_sdist(build_sdist(tmp_path, missing_manifest))
    assert report["status"] == "failed"
    assert report["missing"] == ["c-two/sdk/python/native/Cargo.toml"]

    mismatched = good_members()
    mismatched["c_two-0.6.0/PKG-INFO"] = (
        b"Metadata-Version: 2.1\nName: c-two\nVersion: 0.5.0\n\n")
    report = rc.inspect_sdist(build_sdist(tmp_path, mismatched))
    assert report["status"] == "failed"
    assert any("differs from pyproject" in error for error in report["errors"])

    stray_root = good_members()
    stray_root["loose-file.txt"] = b"outside the root directory"
    with pytest.raises(rc.HelperError):
        rc.inspect_sdist(build_sdist(tmp_path, stray_root))


# ── manifest ─────────────────────────────────────────────────────────────────

SOURCE_SHA = "a" * 40
SDIST_PAYLOAD = b"fastdb-sdist"
SDIST_SHA = hashlib.sha256(SDIST_PAYLOAD).hexdigest()
# Two targets x three advertised pythons keep the fixture small while the
# workflow uses the same validation with 5 x 6.
TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")
PYTHONS = ("3.10", "3.11", "3.12")
PLATFORM_BY_TARGET = {
    "x86_64-unknown-linux-gnu": "manylinux_2_28_x86_64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
}
# x86_64 rows consume published FastDB wheels; aarch64 rows (like Linux ARM64
# and macOS Intel in the real matrix) build the verified sdist per row.
FASTDB_MODE_BY_TARGET = {
    "x86_64-unknown-linux-gnu": "wheel",
    "aarch64-apple-darwin": "sdist",
}


def digest_of(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sidecar_for_tree(path: Path) -> Path:
    path.with_name(path.name + ".sha256").write_text(
        f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n", encoding="utf-8")
    return path


def candidate_tree(tmp_path: Path) -> Path:
    """A coherent miniature candidate mirroring the real receipt shapes."""
    root = tmp_path / "candidate"
    payload_by_name: dict[str, bytes] = {}

    def retain(name: str, payload: bytes) -> None:
        payload_by_name[name] = payload
        sidecar_for_tree(write(root, name, payload))

    for target in TARGETS:
        retain(f"c3-{target}", f"cli:{target}".encode())

    wheel_sha: dict[str, str] = {}
    for target in TARGETS:
        for python in PYTHONS:
            tag = "cp" + python.replace(".", "")
            name = f"c_two-0.6.0-{tag}-{tag}-{PLATFORM_BY_TARGET[target]}.whl"
            retain(name, f"wheel:{target}:{python}".encode())
            wheel_sha[name] = digest_of(payload_by_name[name])
    retain("c_two-0.6.0.tar.gz", b"sdist")

    listing_files: list[dict] = []
    published_fastdb_sha: dict[str, str] = {}
    for python in PYTHONS:
        tag = "cp" + python.replace(".", "")
        name = f"fastdb4py-0.2.1-{tag}-{tag}-manylinux_2_28_x86_64.whl"
        payload = f"published:{python}".encode()
        published_fastdb_sha[name] = digest_of(payload)
        listing_files.append({"filename": name, "packagetype": "bdist_wheel",
                              "sha256": published_fastdb_sha[name]})
        # Like the real provisioning job, only the cp312 published wheel is
        # retained on disk; the other published wheels live in the verified
        # listing metadata that per-row decisions are checked against.
        if python == "3.12":
            retain(name, payload)
    listing_files.append({"filename": "fastdb4py-0.2.1.tar.gz", "packagetype": "sdist",
                          "sha256": SDIST_SHA})
    retain("fastdb4py-0.2.1.tar.gz", SDIST_PAYLOAD)
    retain("fastdb-LICENSE", b"verbatim upstream license")
    retain("fastdb-THIRD_PARTY_NOTICES.txt", b"verbatim notices")

    # The cp312 aarch64 built wheel is the one sdist-built FastDB wheel the
    # provisioning job retains; its identity comes from a receipt decision.
    built_name = "fastdb4py-0.2.1-cp312-cp312-macosx_11_0_arm64.whl"
    retain(built_name, b"built-fastdb")
    built_sha = digest_of(payload_by_name[built_name])

    def fastdb_decision(target: str, python: str) -> dict:
        if FASTDB_MODE_BY_TARGET[target] == "wheel":
            tag = "cp" + python.replace(".", "")
            name = f"fastdb4py-0.2.1-{tag}-{tag}-manylinux_2_28_x86_64.whl"
            return {"mode": "wheel", "filename": name, "sha256": published_fastdb_sha[name]}
        decision = {"mode": "sdist", "filename": "fastdb4py-0.2.1.tar.gz", "sha256": SDIST_SHA}
        if python == "3.12":
            decision["built_wheel"] = {"name": built_name, "sha256": built_sha,
                                       "bytes": len(payload_by_name[built_name]),
                                       "derived_from_sdist_sha256": SDIST_SHA}
        else:
            tag = "cp" + python.replace(".", "")
            row_built = f"fastdb4py-0.2.1-{tag}-{tag}-macosx_11_0_arm64.whl"
            decision["built_wheel"] = {"name": row_built, "sha256": "c" * 64,
                                       "bytes": 7, "derived_from_sdist_sha256": SDIST_SHA}
        return decision

    for target in TARGETS:
        for python in PYTHONS:
            tag = "cp" + python.replace(".", "")
            decision = fastdb_decision(target, python)
            if decision["mode"] == "wheel":
                fastdb_name, fastdb_sha = decision["filename"], decision["sha256"]
            else:
                fastdb_name = decision["built_wheel"]["name"]
                fastdb_sha = decision["built_wheel"]["sha256"]
            receipt = {
                "schema": "c-two.installed-wheel-smoke.v1",
                "status": "passed",
                "source_commit_sha": SOURCE_SHA,
                "platform": "test",
                "artifacts": [
                    {"name": fastdb_name, "sha256": fastdb_sha, "bytes": 5,
                     "distribution": "fastdb4py", "version": "0.2.1"},
                    {"name": f"c_two-0.6.0-{tag}-{tag}-{PLATFORM_BY_TARGET[target]}.whl",
                     "sha256": wheel_sha[
                         f"c_two-0.6.0-{tag}-{tag}-{PLATFORM_BY_TARGET[target]}.whl"],
                     "bytes": 6, "distribution": "c-two", "version": "0.6.0"},
                    {"name": f"c3-{target}", "sha256": digest_of(f"cli:{target}".encode()),
                     "bytes": 7},
                ],
                "row": {"target": target, "python": python, "fastdb_mode": decision["mode"]},
                "wheel_abi": {"interpreter": tag, "abi": tag},
                "fastdb": decision,
                "checks": [{"status": "passed"}, {"status": "passed"}],
            }
            retain(f"abi-{target}-{python}.json",
                   json.dumps(receipt, indent=2, sort_keys=True).encode())

    # Provisioning receipts: the wheel-mode target's published decision and
    # the sdist-mode target's built-wheel decision (which legitimizes the
    # retained built FastDB wheel in the proof inventory).
    retain("fastdb-x86_64-unknown-linux-gnu.json", json.dumps(fastdb_decision(
        "x86_64-unknown-linux-gnu", "3.12"), indent=2).encode())
    retain("fastdb-aarch64-apple-darwin.json", json.dumps(fastdb_decision(
        "aarch64-apple-darwin", "3.12"), indent=2).encode())

    retain("rc-context.json", json.dumps(
        {"schema": "c-two.release-candidate.context.v1",
         "source_commit_sha": SOURCE_SHA}).encode())
    retain("rc-fastdb-public.json", json.dumps(
        {"schema": "c-two.fastdb-release.listing.v1",
         "files": sorted(listing_files, key=lambda file: file["filename"])}).encode())
    retain("smoke-x86_64-unknown-linux-gnu.json", json.dumps(
        {"schema": "c-two.installed-wheel-smoke.v1", "status": "passed",
         "source_commit_sha": SOURCE_SHA}).encode())
    retain("sdist-inspect.json", json.dumps(
        {"schema": "c-two.release-candidate.sdist-inspection.v1", "status": "passed",
         "source_commit_sha": SOURCE_SHA}).encode())
    retain("sdist-proof.json", json.dumps(
        {"schema": "c-two.release-candidate.sdist-proof.v1", "status": "executed",
         "source_commit_sha": SOURCE_SHA,
         "sdist": {"name": "c_two-0.6.0.tar.gz", "sha256": digest_of(b"sdist")}}).encode())
    return root


def manifest_args(root: Path, output: Path) -> list[str]:
    args = [
        "manifest", "--root", str(root), "--output", str(output),
        "--source-sha", SOURCE_SHA,
        "--event", "push", "--ref", "refs/heads/dev-feature",
        "--meta", "c_two_version=0.6.0", "--meta", "c3_version=0.2.0",
        "--expect-cli", str(len(TARGETS)),
        "--expect-wheels", str(len(TARGETS) * len(PYTHONS)),
        "--expect-sdist", "1",
        "--expect-fastdb-proof", "5",
        "--wheel-prefix", "c_two-0.6.0-",
        "--expect-wheel-name", "c_two",
        "--expect-wheel-version", "0.6.0",
    ]
    for target in TARGETS:
        args += ["--require-cli-name", f"c3-{target}"]
    for python in PYTHONS:
        args += ["--expect-abi-python", python]
    for prefix in ("rc-context", "abi-", "smoke-", "sdist-"):
        args += ["--require-source-sha-prefix", prefix]
    for prefix in ("rc-context", "rc-fastdb-public", "fastdb-", "smoke-", "abi-", "sdist-"):
        args += ["--expect-json-prefix", prefix]
    args += [
        "--expect-json-count", "rc-context=1",
        "--expect-json-count", "rc-fastdb-public=1",
        "--expect-json-count", "fastdb-=2",
        "--expect-json-count", "smoke-=1",
        "--expect-json-count", f"abi-={len(TARGETS) * len(PYTHONS)}",
        "--expect-json-count", "sdist-=2",
    ]
    return args


def load_receipt(root: Path, name: str) -> dict:
    return json.loads((root / name).read_text(encoding="utf-8"))


def rewrite_receipt(root: Path, name: str, mutate) -> None:
    payload = load_receipt(root, name)
    mutate(payload)
    (root / name).write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n",
                             encoding="utf-8")
    sidecar_for_tree(root / name)


def test_manifest_binds_the_full_candidate_matrix(rc, tmp_path):
    root = candidate_tree(tmp_path)
    output = root / "rc-manifest.json"
    assert rc.main(manifest_args(root, output)) == 0
    manifest = json.loads(output.read_text(encoding="utf-8"))
    assert manifest["schema"] == "c-two.release-candidate.v1"
    assert manifest["source_commit_sha"] == SOURCE_SHA
    assert manifest["counts"]["cli"] == len(TARGETS)
    assert manifest["counts"]["wheels"] == len(TARGETS) * len(PYTHONS)
    assert manifest["counts"]["sdist"] == 1
    assert manifest["counts"]["fastdb_proof"] == 5
    assert manifest["counts"]["abi_rows"] == len(TARGETS) * len(PYTHONS)
    # Product wheels are only the c_two ones; FastDB inputs stay separate.
    assert all(wheel["name"].startswith("c_two-0.6.0-") for wheel in manifest["wheels"])
    assert output.with_name("rc-manifest.json.sha256").is_file()


def test_manifest_classifies_installers_as_their_own_asset_kind(rc, tmp_path):
    root = candidate_tree(tmp_path)
    sidecar_for_tree(write(root, "c3-installer.sh", b"installer"))
    sidecar_for_tree(write(root, "c3-installer.ps1", b"installer"))
    args = manifest_args(root, root / "rc-manifest.json") + ["--expect-installers", "2"]
    assert rc.main(args) == 0
    manifest = json.loads((root / "rc-manifest.json").read_text(encoding="utf-8"))
    assert sorted(entry["name"] for entry in manifest["installers"]) == \
        ["c3-installer.ps1", "c3-installer.sh"]
    assert manifest["counts"]["cli"] == len(TARGETS)


def test_manifest_fails_on_incomplete_inventory(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    (root / "c3-aarch64-apple-darwin").unlink()
    (root / "c3-aarch64-apple-darwin.sha256").unlink()
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    errors = capsys.readouterr().err
    assert f"expected {len(TARGETS)} cli artifacts, found {len(TARGETS) - 1}" in errors
    assert "missing required CLI artifact c3-aarch64-apple-darwin" in errors


def test_manifest_fails_on_missing_sidecar_unexpected_receipt_and_bad_wheel(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    (root / "rc-context.json.sha256").unlink()
    write(root, "surprise.json", b"{}")
    sidecar_for_tree(root / "surprise.json")
    write(root, "other-1.0-py3-none-any.whl", b"wheel")
    sidecar_for_tree(root / "other-1.0-py3-none-any.whl")
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    errors = capsys.readouterr().err
    assert "missing sidecar for rc-context.json" in errors
    assert "receipts without an expected prefix: ['surprise.json']" in errors
    assert "unexpected wheel name other-1.0-py3-none-any.whl" in errors


def test_manifest_fails_on_wrong_receipt_counts(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    args = manifest_args(root, root / "rc-manifest.json")
    args[args.index("--expect-json-count") + 1] = "smoke-=2"
    assert rc.main(args) == 1
    assert "expected 2 receipts with prefix 'smoke-', found 1" in capsys.readouterr().err


def test_manifest_fails_on_non_passing_receipt_status(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    rewrite_receipt(root, "abi-aarch64-apple-darwin-3.10.json",
                    lambda receipt: receipt.update(status="skipped"))
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "non-passing status 'skipped'" in capsys.readouterr().err


def test_manifest_fails_on_receipts_not_binding_the_source_sha(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    rewrite_receipt(root, "abi-x86_64-unknown-linux-gnu-3.11.json",
                    lambda receipt: receipt.update(source_commit_sha="0" * 40))
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "does not bind source sha" in capsys.readouterr().err


def test_manifest_fails_on_duplicate_and_missing_abi_rows(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    duplicate = load_receipt(root, "abi-x86_64-unknown-linux-gnu-3.10.json")
    write(root, "abi-x86_64-unknown-linux-gnu-3.10-again.json",
          json.dumps(duplicate, indent=2).encode())
    sidecar_for_tree(root / "abi-x86_64-unknown-linux-gnu-3.10-again.json")
    args = manifest_args(root, root / "rc-manifest.json")
    expected = f"abi-={len(TARGETS) * len(PYTHONS)}"
    args[args.index(expected)] = f"abi-={len(TARGETS) * len(PYTHONS) + 1}"
    assert rc.main(args) == 1
    assert "duplicate ABI row x86_64-unknown-linux-gnu/3.10" in capsys.readouterr().err

    root = candidate_tree(tmp_path)
    for path in root.glob("abi-aarch64-apple-darwin-3.12.json*"):
        path.unlink()
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "missing ABI row aarch64-apple-darwin/3.12" in capsys.readouterr().err


def test_manifest_fails_when_receipt_bytes_do_not_match_retained_artifacts(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)

    def wrong_wheel(receipt):
        receipt["artifacts"][1]["sha256"] = "0" * 64
    rewrite_receipt(root, "abi-x86_64-unknown-linux-gnu-3.10.json", wrong_wheel)
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "is not a retained wheel with those bytes" in capsys.readouterr().err

    root = candidate_tree(tmp_path)

    def wrong_cli(receipt):
        receipt["artifacts"][2]["sha256"] = "0" * 64
    rewrite_receipt(root, "abi-aarch64-apple-darwin-3.11.json", wrong_cli)
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "CLI bytes do not match" in capsys.readouterr().err


def test_manifest_fails_when_a_retained_wheel_is_not_consumed(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    extra = "c_two-0.6.0-cp313-cp313-manylinux_2_28_x86_64.whl"
    sidecar_for_tree(write(root, extra, b"wheel"))
    args = manifest_args(root, root / "rc-manifest.json")
    count_index = args.index("--expect-wheels")
    args[count_index + 1] = str(len(TARGETS) * len(PYTHONS) + 1)
    assert rc.main(args) == 1
    assert f"retained wheel {extra} is not consumed by any ABI row" in capsys.readouterr().err


def test_manifest_fails_when_fastdb_proof_is_not_verified_metadata(rc, tmp_path, capsys):
    # A retained FastDB wheel that is neither the published file (per the
    # verified listing) nor referenced as a receipt-built wheel is rejected.
    root = candidate_tree(tmp_path)
    stray = "fastdb4py-0.2.1-cp313-cp313-manylinux_2_28_x86_64.whl"
    sidecar_for_tree(write(root, stray, b"stray"))
    args = manifest_args(root, root / "rc-manifest.json")
    args[args.index("--expect-fastdb-proof") + 1] = "6"
    assert rc.main(args) == 1
    errors = capsys.readouterr().err
    assert f"retained FastDB wheel {stray}" in errors
    assert "neither the verified published file nor a receipt-built wheel" in errors

    # A retained sdist whose bytes disagree with the published listing is
    # rejected even though exactly one sdist is present.
    root = candidate_tree(tmp_path)
    (root / "fastdb4py-0.2.1.tar.gz").write_bytes(b"tampered")
    sidecar_for_tree(root / "fastdb4py-0.2.1.tar.gz")
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "does not match the verified published listing" in capsys.readouterr().err


def test_manifest_fails_when_a_decision_consumes_unpublished_bytes(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)

    def unpublished(receipt):
        receipt["fastdb"].update(filename="fastdb4py-0.2.1-cp312-cp312-win_amd64.whl")
        receipt["artifacts"][0].update(name="fastdb4py-0.2.1-cp312-cp312-win_amd64.whl",
                                       sha256="e" * 64)
    rewrite_receipt(root, "abi-x86_64-unknown-linux-gnu-3.12.json", unpublished)
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "is not the verified published file" in capsys.readouterr().err


def test_manifest_fails_on_wrong_wheel_identity_or_abi(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    args = manifest_args(root, root / "rc-manifest.json")
    args[args.index("--expect-wheel-version") + 1] = "0.5.0"
    assert rc.main(args) == 1
    assert "version is 0.6.0, expected 0.5.0" in capsys.readouterr().err

    root = candidate_tree(tmp_path)

    def wrong_abi(receipt):
        # Bind the retained cp311 wheel with its true bytes: the identity is
        # consistent, but the row advertises python 3.10.
        receipt["artifacts"][1].update(
            name="c_two-0.6.0-cp311-cp311-manylinux_2_28_x86_64.whl",
            sha256=digest_of("wheel:x86_64-unknown-linux-gnu:3.11".encode()))
    rewrite_receipt(root, "abi-x86_64-unknown-linux-gnu-3.10.json", wrong_abi)
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    errors = capsys.readouterr().err
    assert "does not carry the cp310-cp310 tag of row python 3.10" in errors


def test_manifest_rejects_unclassified_files(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    sidecar_for_tree(write(root, "release-notes.txt", b"notes"))
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "unclassified candidate file release-notes.txt" in capsys.readouterr().err


def test_free_threaded_row_uses_cpython_interpreter_with_t_abi(rc):
    from packaging.utils import parse_wheel_filename
    _, _, _, tags = parse_wheel_filename("c_two-0.6.0-cp314-cp314t-win_amd64.whl")
    pair = rc._python_abi_pair("3.14t")
    assert pair == ("cp314", "cp314t")
    assert pair in {(tag.interpreter, tag.abi) for tag in tags}


def test_manifest_rejects_a_wheel_for_another_target(rc, tmp_path, capsys):
    root = candidate_tree(tmp_path)
    name = "abi-x86_64-unknown-linux-gnu-3.12.json"
    other = load_receipt(root, "abi-aarch64-apple-darwin-3.12.json")
    foreign = next(a for a in other["artifacts"] if a.get("distribution") == "c-two")
    def swap(payload):
        payload["artifacts"] = [foreign if a.get("distribution") == "c-two" else a
                                for a in payload["artifacts"]]
    rewrite_receipt(root, name, swap)
    assert rc.main(manifest_args(root, root / "rc-manifest.json")) == 1
    assert "wheel platform does not match" in capsys.readouterr().err
