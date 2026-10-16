"""Choose source validation from a complete PR, push or merge-group diff."""

from __future__ import annotations

import json
import os
from pathlib import Path, PurePosixPath


def needs_source(paths: list[str], *, truncated: bool = False) -> bool:
    if truncated:
        return True
    for path in paths:
        if not path or PurePosixPath(path).is_absolute() or ".." in PurePosixPath(path).parts:
            return True
        if path.startswith((".github/", "tests/repo/", "tools/ci/", "tools/local_rc/", "docs/")):
            continue
        if path in {"README.md", "README.zh-CN.md", "AGENTS.md", "CHANGELOG.md", "LICENSE"}:
            continue
        # Runtime/build inputs and unclassified new paths receive full coverage.
        return True
    return False


def main() -> None:
    event = os.environ["EVENT_NAME"]
    force = os.environ.get("FORCE_FULL") == "true"
    raw = os.environ.get("CHANGED_FILES_JSON", "")
    count = int(os.environ.get("CHANGED_FILES", "0"))
    if force or event not in {"pull_request", "push", "merge_group", "workflow_dispatch"}:
        source = True
    elif event == "workflow_dispatch":
        source = False
    else:
        paths = json.loads(raw)
        if not isinstance(paths, list) or not all(isinstance(path, str) for path in paths):
            raise ValueError("changed paths must be a JSON array of strings")
        source = needs_source(paths, truncated=count >= 3000 or count > len(paths))
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"source={str(source).lower()}\n")
    print("Full cross-platform source validation" if source else "Workflow policy validation; no runtime compilation")


if __name__ == "__main__":
    main()
