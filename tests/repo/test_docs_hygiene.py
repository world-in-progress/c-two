"""Keep agent instructions readable and current reference links usable."""

from __future__ import annotations

from html.parser import HTMLParser
import os
from pathlib import Path, PurePath, PurePosixPath, PureWindowsPath
import subprocess
from urllib.parse import unquote, urlsplit

from markdown_it import MarkdownIt
import pytest


ROOT = Path(__file__).resolve().parents[2]
MARKDOWN = MarkdownIt()


def _tracked_files(*pathspecs: str) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", *pathspecs],
        cwd=ROOT,
        check=True,
        capture_output=True,
    )
    paths = (ROOT / os.fsdecode(path) for path in result.stdout.split(b"\0") if path)
    return [path for path in paths if path.exists() or path.is_symlink()]


DOCUMENTS = _tracked_files("*.md")
TRANSLATIONS = [
    path for path in DOCUMENTS
    if path.is_relative_to(ROOT / "docs/reference") and path.name.endswith(".zh-CN.md")
]


def _destinations(markdown: str):
    environment: dict = {}
    tokens = MARKDOWN.parse(markdown, environment)
    for reference in environment.get("references", {}).values():
        yield reference["href"]
    for token in tokens:
        for part in (token, *(token.children or [])):
            if part.type == "link_open":
                yield part.attrGet("href")
            elif part.type == "image":
                yield part.attrGet("src")
            elif part.type in {"html_block", "html_inline"}:
                parser = _HtmlLinks()
                parser.feed(part.content)
                yield from parser.destinations


class _HtmlLinks(HTMLParser):
    def __init__(self):
        super().__init__()
        self.destinations: list[str] = []

    def handle_starttag(self, tag, attrs):
        self.destinations.extend(
            value for name, value in attrs if name in {"href", "src"} and value
        )


def test_root_agent_instructions_fit_loader_budget():
    size = (ROOT / "AGENTS.md").stat().st_size
    assert size <= 32_768, f"AGENTS.md is {size:,} bytes; move mechanism detail into docs/reference/"


@pytest.mark.parametrize("document", DOCUMENTS, ids=lambda path: path.relative_to(ROOT).as_posix())
def test_relative_documentation_links_resolve(document):
    missing = []
    for destination in _destinations(document.read_text(encoding="utf-8")):
        url = urlsplit(destination)
        if url.scheme or url.netloc or not url.path or url.path.startswith("/"):
            continue
        path = document.parent / unquote(url.path)
        if not path.exists():
            missing.append(destination)
    assert not missing, f"{document.relative_to(ROOT)} has missing link targets: {missing}"


@pytest.mark.parametrize("translation", TRANSLATIONS, ids=lambda path: path.relative_to(ROOT).as_posix())
def test_reference_language_pairs_exist_and_link_to_each_other(translation):
    english = translation.with_name(translation.name.removesuffix(".zh-CN.md") + ".md")
    assert english.is_file(), f"{translation.relative_to(ROOT)} has no English counterpart"
    for document, counterpart in ((english, translation), (translation, english)):
        targets = set()
        for destination in _destinations(document.read_text(encoding="utf-8")):
            url = urlsplit(destination)
            if not url.scheme and not url.netloc and url.path:
                targets.add((document.parent / unquote(url.path)).resolve())
        assert counterpart.resolve() in targets, (
            f"{document.relative_to(ROOT)} must link to {counterpart.name}"
        )


def _workspace_layout_markers(checkout: PurePath, home: PurePath) -> tuple[str, ...]:
    try:
        parts = checkout.parent.relative_to(home).parts
    except ValueError:
        return ()
    if len(parts) < 2 or any(part.startswith(".") for part in parts[:2]):
        return ()
    return tuple(separator.join(parts[:2]) + separator for separator in ("/", "\\"))


@pytest.mark.parametrize(
    ("checkout", "home", "expected"),
    [
        (PurePosixPath("/account/Visible/Sources/project"), PurePosixPath("/account"),
         ("Visible/Sources/", "Visible\\Sources\\")),
        (PurePosixPath("/account/Visible/Sources/nested/project"), PurePosixPath("/account"),
         ("Visible/Sources/", "Visible\\Sources\\")),
        (PurePosixPath("/account/Visible/project"), PurePosixPath("/account"), ()),
        (PurePosixPath("/account/project"), PurePosixPath("/account"), ()),
        (PurePosixPath("/external/Visible/Sources/project"), PurePosixPath("/account"), ()),
        (PurePosixPath("/account/.hidden/Sources/project"), PurePosixPath("/account"), ()),
        (PurePosixPath("/account/Visible/.hidden/project"), PurePosixPath("/account"), ()),
        (PureWindowsPath("C:/account/Visible/Sources/project"), PureWindowsPath("C:/account"),
         ("Visible/Sources/", "Visible\\Sources\\")),
        (PureWindowsPath("D:/Visible/Sources/project"), PureWindowsPath("C:/account"), ()),
    ],
)
def test_workspace_layout_markers(checkout, home, expected):
    assert _workspace_layout_markers(checkout, home) == expected


# Hosted runner homes are shared CI conventions; this check guards developer machines.
@pytest.mark.skipif(os.environ.get("GITHUB_ACTIONS") == "true", reason="developer-machine path check")
def test_tracked_text_files_do_not_contain_current_home_path():
    common_dir = subprocess.run(
        ["git", "rev-parse", "--git-common-dir"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    checkout = (ROOT / common_dir).resolve().parent
    home = Path.home()
    markers = {str(home).replace("\\", "/"), str(home).replace("/", "\\")}
    markers.update(_workspace_layout_markers(checkout, home))
    markers.update(marker.replace("\\", "\\\\") for marker in tuple(markers))
    encoded_markers = tuple(marker.encode("utf-8") for marker in markers)
    offenders = []
    for path in _tracked_files():
        relative = path.relative_to(ROOT)
        content = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        if b"\0" in content:
            continue
        try:
            content.decode("utf-8")
        except UnicodeDecodeError:
            continue
        if any(marker in content for marker in encoded_markers):
            offenders.append(relative.as_posix())
    assert not offenders, f"tracked text files expose local home or workspace paths: {offenders}"
