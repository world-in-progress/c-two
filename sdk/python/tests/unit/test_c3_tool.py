from __future__ import annotations

import importlib.util
import os
from pathlib import Path

import pytest


def _load_c3_tool():
    root = next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / "tools" / "dev" / "generate_banner.py").is_file()
    )
    path = root / "tools" / "dev" / "c3_tool.py"
    spec = importlib.util.spec_from_file_location("c3_tool", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_build_uses_cli_manifest_without_linking(monkeypatch, tmp_path):
    tool = _load_c3_tool()
    calls: list[list[str]] = []
    repo = tmp_path / "repo"
    manifest = repo / "cli" / "Cargo.toml"
    manifest.parent.mkdir(parents=True)
    manifest.write_text("[package]\nname = \"c2-cli\"\n", encoding="utf-8")

    def fake_run(command: list[str], check: bool):
        calls.append(command)

    monkeypatch.setattr(tool.subprocess, "run", fake_run)

    binary = tool.build_c3(repo, release=False)

    assert binary == repo / "cli" / "target" / "debug" / tool.binary_name()
    assert calls == [["cargo", "build", "--manifest-path", str(manifest)]]


def test_link_creates_bin_entry_without_rebuilding(monkeypatch, tmp_path):
    tool = _load_c3_tool()
    binary = tmp_path / "cli" / "target" / "debug" / tool.binary_name()
    binary.parent.mkdir(parents=True)
    binary.write_text("binary", encoding="utf-8")
    bin_dir = tmp_path / "dev-bin"

    monkeypatch.setattr(tool.subprocess, "run", lambda *_args, **_kwargs: pytest.fail("unexpected build"))

    link = tool.link_c3(binary, bin_dir, force=False, copy=False)

    assert link == bin_dir / tool.binary_name()
    assert link.exists()
    if os.name != "nt":
        assert link.is_symlink()
        assert link.resolve() == binary


def test_link_refuses_to_replace_unrelated_existing_entry(tmp_path):
    tool = _load_c3_tool()
    binary = tmp_path / "target" / tool.binary_name()
    binary.parent.mkdir(parents=True)
    binary.write_text("binary", encoding="utf-8")
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    existing = bin_dir / tool.binary_name()
    existing.write_text("other", encoding="utf-8")

    with pytest.raises(SystemExit):
        tool.link_c3(binary, bin_dir, force=False, copy=False)


def test_default_link_dir_prefers_existing_path_directory(monkeypatch, tmp_path):
    tool = _load_c3_tool()
    repo = tmp_path / "repo"
    repo_bin = repo / ".bin"
    cargo_bin = tmp_path / ".cargo" / "bin"
    repo.mkdir()
    cargo_bin.mkdir(parents=True)
    monkeypatch.setenv("PATH", str(cargo_bin))

    assert tool.default_link_dir(repo) == cargo_bin
    repo_bin.mkdir()
    monkeypatch.setenv("PATH", str(repo_bin))
    assert tool.default_link_dir(repo) == repo_bin
