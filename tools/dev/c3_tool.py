#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path


def repo_root() -> Path:
    return next(parent for parent in Path(__file__).resolve().parents if (parent / "cli").is_dir())


def binary_name() -> str:
    return "c3.exe" if os.name == "nt" else "c3"


def cli_manifest(repo: Path) -> Path:
    return repo / "cli" / "Cargo.toml"


def cli_binary(repo: Path, *, release: bool) -> Path:
    profile = "release" if release else "debug"
    return repo / "cli" / "target" / profile / binary_name()


def build_c3(repo: Path, *, release: bool) -> Path:
    command = ["cargo", "build", "--manifest-path", str(cli_manifest(repo))]
    if release:
        command.append("--release")
    subprocess.run(command, check=True)
    return cli_binary(repo, release=release)


def _path_entries() -> list[Path]:
    return [Path(entry).expanduser().resolve() for entry in os.environ.get("PATH", "").split(os.pathsep) if entry]


def default_link_dir(repo: Path) -> Path:
    repo_bin = (repo / ".bin").resolve()
    entries = _path_entries()
    if repo_bin in entries:
        return repo_bin

    for entry in entries:
        if entry.name == "bin" and entry.parent.name == ".cargo":
            return entry

    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).expanduser()
    cargo_bin = (cargo_home / "bin").resolve()
    if cargo_bin in entries:
        return cargo_bin

    return repo_bin


def _is_ours(existing: Path, binary: Path) -> bool:
    if existing.is_symlink():
        return existing.resolve() == binary.resolve()
    return False


def link_c3(binary: Path, bin_dir: Path, *, force: bool, copy: bool) -> Path:
    if not binary.exists():
        raise SystemExit(f"c3 binary not found: {binary}\nRun with --build first.")

    bin_dir.mkdir(parents=True, exist_ok=True)
    link = bin_dir / binary_name()
    if link.exists() or link.is_symlink():
        if not force and not _is_ours(link, binary):
            raise SystemExit(f"refusing to replace existing {link}; pass --force to overwrite")
        link.unlink()

    if copy or os.name == "nt":
        shutil.copy2(binary, link)
        if os.name != "nt":
            link.chmod(0o755)
    else:
        link.symlink_to(binary)
    return link


def main() -> None:
    parser = argparse.ArgumentParser(description="Build and link the local c3 development binary.")
    parser.add_argument("--build", action="store_true", help="Build cli/target/debug/c3.")
    parser.add_argument("--release", action="store_true", help="Use a release build with --build.")
    parser.add_argument("--link", action="store_true", help="Link an existing c3 binary into a bin directory.")
    parser.add_argument("--bin-dir", type=Path, default=None, help="Directory that should receive c3.")
    parser.add_argument("--copy", action="store_true", help="Copy instead of symlinking.")
    parser.add_argument("--force", action="store_true", help="Replace an existing c3 entry.")
    args = parser.parse_args()

    root = repo_root()
    binary = build_c3(root, release=args.release) if args.build else cli_binary(root, release=args.release)
    print(f"c3 binary: {binary}")

    if args.link:
        bin_dir = args.bin_dir.expanduser() if args.bin_dir else default_link_dir(root)
        link = link_c3(binary, bin_dir, force=args.force, copy=args.copy)
        print(f"linked: {link}")
        if bin_dir.resolve() not in _path_entries():
            print(f'add to PATH: export PATH="{bin_dir.resolve()}:$PATH"')


if __name__ == "__main__":
    main()
