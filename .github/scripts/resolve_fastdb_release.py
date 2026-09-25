"""Resolve official fastdb4py release artifacts from PyPI for release candidates.

Wheel compatibility uses packaging tag and specifier semantics
(``packaging.tags.sys_tags``, ``packaging.utils.parse_wheel_filename``,
``packaging.specifiers.SpecifierSet``) instead of a homemade ABI parser, and
honors PEP 592 yanked files and Requires-Python metadata. Two modes:

``resolve``
    Pick the best compatible wheel for the running interpreter, or fall back
    to the sdist when ``--allow-sdist`` is given and no wheel is compatible.
    Optionally downloads the exact file and verifies its SHA-256.
``list``
    Dump every public file of the exact release with digests and URLs so the
    candidate receipt can record the published byte identities.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import sys
from pathlib import Path

from packaging.specifiers import SpecifierSet
from packaging.tags import sys_tags
from packaging.utils import InvalidWheelFilename, parse_wheel_filename
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

INDEX_TEMPLATE = "https://pypi.org/pypi/{project}/{version}/json"
DECISION_SCHEMA = "c-two.fastdb-release.resolution.v1"
LISTING_SCHEMA = "c-two.fastdb-release.listing.v1"


class ResolutionError(RuntimeError):
    """A release could not be resolved exactly as requested."""


def fetch_release(project: str, version: str) -> list[dict]:
    url = INDEX_TEMPLATE.format(project=project, version=version)
    try:
        with urlopen(url, timeout=60) as response:
            document = json.load(response)
    except HTTPError as error:
        if error.code == 404:
            raise ResolutionError(f"{project} {version} does not exist on PyPI") from error
        raise ResolutionError(f"PyPI index request failed for {url}: {error}") from error
    except URLError as error:
        raise ResolutionError(f"PyPI index request failed for {url}: {error}") from error
    return document.get("urls", [])


def record(file: dict) -> dict:
    return {
        "filename": file["filename"],
        "packagetype": file["packagetype"],
        "url": file["url"],
        "sha256": file["digests"]["sha256"],
        "requires_python": file.get("requires_python"),
        "yanked": bool(file.get("yanked")),
    }


def interpreter_allows(file: dict, python_version: str) -> bool:
    specifier = file.get("requires_python")
    return not specifier or python_version in SpecifierSet(specifier)


def exact_interpreter_tags(tags) -> tuple[str, str]:
    """Return the running interpreter's most specific ``(interpreter, abi)`` pair.

    ``packaging.tags.sys_tags`` yields best-first tags, so the first entry is
    the interpreter's own ABI (``cp312`` / ``cp314t``), never a broader abi3
    or py3 fallback. Release rows use this to prove the consumed wheel was
    built for exactly this interpreter instead of a homemade ABI guess.
    """
    first = next(iter(tags), None)
    if first is None:
        raise ResolutionError("packaging reported no system tags for this interpreter")
    return first.interpreter, first.abi


def wheel_matches_exact_abi(filename: str, interpreter: str, abi: str) -> bool:
    try:
        _, _, _, wheel_tags = parse_wheel_filename(filename)
    except InvalidWheelFilename:
        return False
    return any(tag.interpreter == interpreter and tag.abi == abi for tag in wheel_tags)


def pick_exact_wheel(*directories, unique_file: bool = False) -> Path:
    """Return the one wheel in ``directories`` built for this exact interpreter.

    Workflow rows capture this from ``pick-wheel`` stdout into a current-shell
    variable (``set -u`` safe within the same step) and additionally write it
    to GITHUB_ENV for later steps. ``unique_file`` demands the directory hold
    exactly one wheel (a freshly built/downloaded one); otherwise exactly one
    of the wheels present must match this interpreter's ABI tag.
    """
    files: list[Path] = []
    for directory in directories:
        files.extend(sorted(Path(directory).glob("*.whl")))
    supported_tags = set(sys_tags())
    interpreter, abi = exact_interpreter_tags(sys_tags())
    if unique_file and len(files) != 1:
        raise ResolutionError(
            f"expected exactly one wheel in {directories}, found {[f.name for f in files]}")
    matches = []
    for path in files:
        if not wheel_matches_exact_abi(path.name, interpreter, abi):
            continue
        _, _, _, tags = parse_wheel_filename(path.name)
        if supported_tags.intersection(tags):
            matches.append(path)
    if len(matches) != 1:
        raise ResolutionError(
            f"expected exactly one {interpreter}-{abi} wheel in {directories}; "
            f"candidates: {[f.name for f in files]}")
    return matches[0]


def select_wheel(files: list[dict], python_version: str, tags) -> dict | None:
    """Return the compatible wheel ranked earliest by ``tags`` (best first)."""
    rank = {tag: index for index, tag in enumerate(tags)}
    best: tuple[tuple[int, str], dict] | None = None
    for file in files:
        if file["packagetype"] != "bdist_wheel" or file["yanked"]:
            continue
        if not interpreter_allows(file, python_version):
            continue
        try:
            _, _, _, wheel_tags = parse_wheel_filename(file["filename"])
        except InvalidWheelFilename:
            continue
        positions = [rank[tag] for tag in wheel_tags if tag in rank]
        if not positions:
            continue
        key = (min(positions), file["filename"])
        if best is None or key < best[0]:
            best = (key, file)
    return None if best is None else best[1]


def select_sdist(files: list[dict], python_version: str) -> dict | None:
    candidates = [
        file for file in files
        if file["packagetype"] == "sdist" and not file["yanked"]
        and interpreter_allows(file, python_version)
    ]
    if len(candidates) > 1:
        raise ResolutionError("Multiple sdists published for one version")
    return candidates[0] if candidates else None


def download(file: dict, directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    destination = directory / file["filename"]
    digest = hashlib.sha256()
    with urlopen(file["url"], timeout=300) as response, destination.open("wb") as output:
        while True:
            chunk = response.read(1 << 20)
            if not chunk:
                break
            digest.update(chunk)
            output.write(chunk)
    if digest.hexdigest() != file["sha256"]:
        destination.unlink(missing_ok=True)
        raise ResolutionError(f"Downloaded {file['filename']} SHA-256 mismatch")
    return destination


def command_pick_wheel(options: argparse.Namespace) -> int:
    print(pick_exact_wheel(*options.directories, unique_file=options.unique_file))
    return 0


def command_resolve(options: argparse.Namespace) -> int:
    files = [record(file) for file in fetch_release(options.project, options.version)]
    python_version = platform.python_version()
    chosen = None
    if options.require_sdist:
        chosen = select_sdist(files, python_version)
        if chosen is None:
            print(f"No usable sdist published for {options.project} {options.version}",
                  file=sys.stderr)
            return 2
    else:
        chosen = select_wheel(files, python_version, sys_tags())
        if chosen is None and options.allow_sdist:
            chosen = select_sdist(files, python_version)
    if chosen is None:
        compatible = [
            file["filename"] for file in files
            if file["packagetype"] == "bdist_wheel" and not file["yanked"]
        ]
        detail = f"compatible wheels: {compatible or 'none'}"
        if not options.allow_sdist:
            print(f"No compatible wheel for {options.project} {options.version} "
                  f"on {platform.system()} {platform.machine()} ({detail}); "
                  "sdist fallback is not enabled", file=sys.stderr)
            return 2
        print(f"No wheel and no usable sdist for {options.project} {options.version} ({detail})",
              file=sys.stderr)
        return 2
    decision = {
        "schema": DECISION_SCHEMA,
        "project": options.project,
        "version": options.version,
        "mode": "wheel" if chosen["packagetype"] == "bdist_wheel" else "sdist",
        "python_version": python_version,
        "platform": {"system": platform.system(), "machine": platform.machine()},
        **chosen,
    }
    if options.download_dir is not None:
        path = download(chosen, Path(options.download_dir))
        decision["downloaded_path"] = str(path)
    payload = json.dumps(decision, indent=2, sort_keys=True) + "\n"
    if options.output:
        destination = Path(options.output)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(payload, encoding="utf-8")
    else:
        sys.stdout.write(payload)
    return 0


def command_list(options: argparse.Namespace) -> int:
    files = sorted(
        (record(file) for file in fetch_release(options.project, options.version)),
        key=lambda file: file["filename"],
    )
    if not files:
        raise ResolutionError(f"{options.project} {options.version} has no public files")
    listing = {
        "schema": LISTING_SCHEMA,
        "project": options.project,
        "version": options.version,
        "files": files,
    }
    payload = json.dumps(listing, indent=2, sort_keys=True) + "\n"
    if options.output:
        destination = Path(options.output)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(payload, encoding="utf-8")
    else:
        sys.stdout.write(payload)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    resolve = commands.add_parser("resolve", help="Pick the artifact for this interpreter")
    resolve.add_argument("--project", default="fastdb4py")
    resolve.add_argument("--version", required=True)
    resolve.add_argument("--allow-sdist", action="store_true",
                         help="Fall back to the published sdist when no wheel is compatible")
    resolve.add_argument("--require-sdist", action="store_true",
                         help="Select the published sdist even when a wheel is compatible")
    resolve.add_argument("--download-dir", type=Path, default=None,
                         help="Download the exact artifact here and verify its SHA-256")
    resolve.add_argument("--output", type=Path, default=None)
    resolve.set_defaults(handler=command_resolve)

    pick = commands.add_parser(
        "pick-wheel", help="Print the local wheel matching this exact interpreter ABI")
    pick.add_argument("directories", nargs="+")
    pick.add_argument("--unique-file", action="store_true",
                      help="The directories must hold exactly one wheel, and it must match")
    pick.set_defaults(handler=command_pick_wheel)

    listing = commands.add_parser("list", help="Dump every public file of the release")
    listing.add_argument("--project", default="fastdb4py")
    listing.add_argument("--version", required=True)
    listing.add_argument("--output", type=Path, default=None)
    listing.set_defaults(handler=command_list)

    options = parser.parse_args(argv)
    try:
        return options.handler(options)
    except ResolutionError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
