"""Tests for .github/scripts/resolve_fastdb_release.py"""

from __future__ import annotations

import hashlib
import io
import json
import sys
from pathlib import Path

import pytest

_ROOT = next(
    parent
    for parent in Path(__file__).resolve().parents
    if (parent / ".github" / "scripts" / "resolve_fastdb_release.py").is_file()
)
_SCRIPT_DIR = str(_ROOT / ".github" / "scripts")

REQUIRE = ">=3.10"


@pytest.fixture(scope="module")
def resolver():
    sys.path.insert(0, _SCRIPT_DIR)
    sys.modules.pop("resolve_fastdb_release", None)
    import resolve_fastdb_release

    yield resolve_fastdb_release
    sys.path.remove(_SCRIPT_DIR)
    sys.modules.pop("resolve_fastdb_release", None)


def wheel_file(name: str, sha: str = "0" * 64, requires_python: str | None = REQUIRE,
               yanked: bool = False) -> dict:
    return {
        "filename": name,
        "packagetype": "bdist_wheel",
        "url": f"https://files.example/{name}",
        "digests": {"sha256": sha},
        "requires_python": requires_python,
        "yanked": yanked,
    }


def sdist_file(name: str, sha: str = "1" * 64) -> dict:
    return {
        "filename": name,
        "packagetype": "sdist",
        "url": f"https://files.example/{name}",
        "digests": {"sha256": sha},
        "requires_python": REQUIRE,
        "yanked": False,
    }


@pytest.fixture
def linux_x86_64(monkeypatch, resolver):
    from packaging.tags import Tag

    monkeypatch.setattr(resolver, "sys_tags", lambda: iter([
        Tag("cp312", "cp312", "manylinux_2_24_x86_64"),
        Tag("cp312", "cp312", "manylinux_2_24_x86_64"),
        Tag("cp312", "cp312", "manylinux2014_x86_64"),
        Tag("cp312", "abi3", "manylinux_2_24_x86_64"),
        Tag("py3", "none", "any"),
    ]))
    monkeypatch.setattr(resolver.platform, "python_version", lambda: "3.12.3")
    monkeypatch.setattr(resolver, "fetch_release", lambda project, version: [])
    return resolver


def test_selects_the_best_ranked_compatible_wheel(resolver, linux_x86_64):
    files = [
        resolver.record(sdist_file("fastdb4py-0.2.1.tar.gz")),
        resolver.record(wheel_file("fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl", "a" * 64)),
        resolver.record(wheel_file("fastdb4py-0.2.1-cp312-cp312-macosx_11_0_arm64.whl", "b" * 64)),
        resolver.record(wheel_file("fastdb4py-0.2.1-cp311-cp311-manylinux_2_24_x86_64.whl", "c" * 64)),
    ]
    chosen = linux_x86_64.select_wheel(files, "3.12.3", linux_x86_64.sys_tags())
    assert chosen["filename"] == "fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl"


def test_free_threaded_and_release_abis_are_not_interchangeable(resolver, linux_x86_64):
    from packaging.tags import Tag

    files = [
        resolver.record(wheel_file("fastdb4py-0.2.1-cp314-cp314t-manylinux_2_24_x86_64.whl")),
        resolver.record(wheel_file("fastdb4py-0.2.1-cp314-cp314-manylinux_2_24_x86_64.whl", "d" * 64)),
    ]
    release_tags = [Tag("cp314", "cp314", "manylinux_2_24_x86_64"), Tag("py3", "none", "any")]
    chosen = linux_x86_64.select_wheel(files, "3.14.0", release_tags)
    assert chosen["filename"] == "fastdb4py-0.2.1-cp314-cp314-manylinux_2_24_x86_64.whl"
    freethreaded_tags = [Tag("cp314", "cp314t", "manylinux_2_24_x86_64"), Tag("py3", "none", "any")]
    chosen = linux_x86_64.select_wheel(files, "3.14.0", freethreaded_tags)
    assert chosen["filename"] == "fastdb4py-0.2.1-cp314-cp314t-manylinux_2_24_x86_64.whl"


def test_exact_abi_matching_uses_the_interpreters_own_tag(resolver, linux_x86_64):
    interpreter, abi = resolver.exact_interpreter_tags(linux_x86_64.sys_tags())
    assert (interpreter, abi) == ("cp312", "cp312")
    assert resolver.wheel_matches_exact_abi(
        "fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl", interpreter, abi)
    assert not resolver.wheel_matches_exact_abi(
        "fastdb4py-0.2.1-cp311-cp311-manylinux_2_24_x86_64.whl", interpreter, abi)
    assert not resolver.wheel_matches_exact_abi(
        "fastdb4py-0.2.1-cp312-abi3-manylinux_2_24_x86_64.whl", interpreter, abi)
    assert not resolver.wheel_matches_exact_abi("not-a-wheel.whl", interpreter, abi)

    from packaging.tags import Tag
    freethreaded = iter([Tag("cp314", "cp314t", "manylinux_2_24_x86_64")])
    assert resolver.exact_interpreter_tags(freethreaded) == ("cp314", "cp314t")
    with pytest.raises(resolver.ResolutionError):
        resolver.exact_interpreter_tags(iter([]))


def test_pick_wheel_executes_the_selection_handoff(resolver, linux_x86_64, tmp_path, capsys):
    """The workflow rows capture ``pick-wheel`` stdout as a shell variable.

    This executes the real command path end to end: selection output, exit
    codes, and failure diagnostics, not just string shape checks.
    """
    wheels = tmp_path / "wheels"
    wheels.mkdir()
    for tag in ("cp310", "cp311", "cp312"):
        (wheels / f"c_two-0.6.0-{tag}-{tag}-manylinux_2_24_x86_64.whl").write_bytes(b"w")

    assert resolver.main(["pick-wheel", str(wheels)]) == 0
    selected = capsys.readouterr().out.strip()
    assert selected.endswith("c_two-0.6.0-cp312-cp312-manylinux_2_24_x86_64.whl")

    built = tmp_path / "fastdb-built"
    built.mkdir()
    (built / "fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl").write_bytes(b"f")
    assert resolver.main(["pick-wheel", "--unique-file", str(built)]) == 0
    assert "fastdb4py" in capsys.readouterr().out


def test_pick_wheel_fails_without_an_exact_match(resolver, linux_x86_64, tmp_path, capsys):
    wheels = tmp_path / "wheels"
    wheels.mkdir()
    (wheels / "c_two-0.6.0-cp311-cp311-manylinux_2_24_x86_64.whl").write_bytes(b"w")
    (wheels / "c_two-0.6.0-cp310-cp310-manylinux_2_24_x86_64.whl").write_bytes(b"w")
    with pytest.raises(resolver.ResolutionError, match="exactly one cp312-cp312"):
        resolver.pick_exact_wheel(wheels)

    assert resolver.main(["pick-wheel", str(wheels)]) == 1
    assert "cp312-cp312" in capsys.readouterr().err


def test_pick_wheel_unique_file_rejects_ambiguous_directories(resolver, linux_x86_64, tmp_path):
    one = tmp_path / "one"
    one.mkdir()
    (one / "fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl").write_bytes(b"a")
    other = tmp_path / "other"
    other.mkdir()
    (other / "fastdb4py-0.2.1-cp311-cp311-manylinux_2_24_x86_64.whl").write_bytes(b"b")
    with pytest.raises(resolver.ResolutionError, match="exactly one wheel"):
        resolver.pick_exact_wheel(one, other, unique_file=True)

    empty = tmp_path / "missing-dir"
    with pytest.raises(resolver.ResolutionError, match="exactly one wheel"):
        resolver.pick_exact_wheel(empty, unique_file=True)


def test_yanked_wheels_are_excluded(resolver, linux_x86_64):
    files = [
        resolver.record(wheel_file("fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl",
                                   yanked=True)),
        resolver.record(sdist_file("fastdb4py-0.2.1.tar.gz", "e" * 64)),
    ]
    assert linux_x86_64.select_wheel(files, "3.12.3", linux_x86_64.sys_tags()) is None
    sdist = linux_x86_64.select_sdist(files, "3.12.3")
    assert sdist["filename"] == "fastdb4py-0.2.1.tar.gz"


def test_requires_python_is_respected(resolver, linux_x86_64):
    files = [
        resolver.record(wheel_file("fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl",
                                   requires_python=">=3.99")),
    ]
    assert linux_x86_64.select_wheel(files, "3.12.3", linux_x86_64.sys_tags()) is None


def test_resolve_without_wheel_or_fallback_fails_with_distinct_exit(linux_x86_64, capsys):
    code = linux_x86_64.main(["resolve", "--version", "0.2.1"])
    assert code == 2
    assert "sdist fallback is not enabled" in capsys.readouterr().err


def test_resolve_allows_sdist_fallback_and_records_public_hash(linux_x86_64, tmp_path, monkeypatch):
    monkeypatch.setattr(linux_x86_64, "fetch_release", lambda project, version: [
        wheel_file("fastdb4py-0.2.1-cp312-cp312-win_amd64.whl", "f" * 64),
        sdist_file("fastdb4py-0.2.1.tar.gz", "9" * 64),
    ])
    output = tmp_path / "decision.json"
    code = linux_x86_64.main(["resolve", "--version", "0.2.1", "--allow-sdist",
                              "--output", str(output)])
    assert code == 0
    decision = json.loads(output.read_text(encoding="utf-8"))
    assert decision["mode"] == "sdist"
    assert decision["sha256"] == "9" * 64
    assert decision["url"].endswith("fastdb4py-0.2.1.tar.gz")
    assert decision["yanked"] is False


def test_resolve_require_sdist_wins_even_with_a_compatible_wheel(linux_x86_64, tmp_path,
                                                                 monkeypatch):
    monkeypatch.setattr(linux_x86_64, "fetch_release", lambda project, version: [
        wheel_file("fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl", "a" * 64),
        sdist_file("fastdb4py-0.2.1.tar.gz", "9" * 64),
    ])
    output = tmp_path / "decision.json"
    code = linux_x86_64.main(["resolve", "--version", "0.2.1", "--require-sdist",
                              "--output", str(output)])
    assert code == 0
    decision = json.loads(output.read_text(encoding="utf-8"))
    assert decision["mode"] == "sdist"
    assert decision["filename"] == "fastdb4py-0.2.1.tar.gz"


def test_resolve_require_sdist_fails_when_no_sdist_is_published(linux_x86_64, tmp_path,
                                                                monkeypatch, capsys):
    monkeypatch.setattr(linux_x86_64, "fetch_release", lambda project, version: [
        wheel_file("fastdb4py-0.2.1-cp312-cp312-manylinux_2_24_x86_64.whl", "a" * 64),
    ])
    code = linux_x86_64.main(["resolve", "--version", "0.2.1", "--require-sdist",
                              "--output", str(tmp_path / "decision.json")])
    assert code == 2
    assert "No usable sdist published" in capsys.readouterr().err


def test_missing_version_is_an_error(linux_x86_64, monkeypatch):
    def fail(project, version):
        raise linux_x86_64.ResolutionError(f"{project} {version} does not exist on PyPI")

    monkeypatch.setattr(linux_x86_64, "fetch_release", fail)
    assert linux_x86_64.main(["resolve", "--version", "9.9.9"]) == 1


def test_list_records_every_public_file_with_digests(linux_x86_64, tmp_path, monkeypatch):
    monkeypatch.setattr(linux_x86_64, "fetch_release", lambda project, version: [
        wheel_file("fastdb4py-0.2.1-cp310-cp310-win_amd64.whl", "a" * 64),
        sdist_file("fastdb4py-0.2.1.tar.gz", "b" * 64),
    ])
    output = tmp_path / "listing.json"
    assert linux_x86_64.main(["list", "--version", "0.2.1", "--output", str(output)]) == 0
    listing = json.loads(output.read_text(encoding="utf-8"))
    assert listing["schema"] == linux_x86_64.LISTING_SCHEMA
    assert [file["filename"] for file in listing["files"]] == [
        "fastdb4py-0.2.1-cp310-cp310-win_amd64.whl",
        "fastdb4py-0.2.1.tar.gz",
    ]
    assert listing["files"][1]["sha256"] == "b" * 64


def test_download_verifies_digest_and_rejects_corruption(linux_x86_64, tmp_path, monkeypatch):
    payload = b"sdist-bytes"

    class FakeResponse(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *args):
            return False

    good = sdist_file("fastdb4py-0.2.1.tar.gz", hashlib.sha256(payload).hexdigest())
    monkeypatch.setattr(linux_x86_64, "urlopen", lambda url, timeout: FakeResponse(payload))
    path = linux_x86_64.download(linux_x86_64.record(good), tmp_path)
    assert path.read_bytes() == payload

    bad = sdist_file("fastdb4py-0.2.1-bad.tar.gz", "0" * 64)
    with pytest.raises(linux_x86_64.ResolutionError):
        linux_x86_64.download(linux_x86_64.record(bad), tmp_path)


def test_picker_rejects_matching_abi_on_another_platform(linux_x86_64, tmp_path):
    wheel = tmp_path / "c_two-0.6.0-cp312-cp312-win_amd64.whl"
    wheel.write_bytes(b"wrong platform")
    with pytest.raises(linux_x86_64.ResolutionError):
        linux_x86_64.pick_exact_wheel(tmp_path, unique_file=True)


def test_resolution_creates_separate_receipt_directory(linux_x86_64, monkeypatch, tmp_path):
    file = sdist_file("fastdb4py-0.2.1.tar.gz")
    monkeypatch.setattr(linux_x86_64, "fetch_release", lambda *_: [file])
    receipt = tmp_path / "new-receipts" / "decision.json"
    assert linux_x86_64.main(["resolve", "--version", "0.2.1", "--require-sdist", "--output", str(receipt)]) == 0
    assert json.loads(receipt.read_text())["mode"] == "sdist"
