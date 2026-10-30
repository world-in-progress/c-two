"""Choose source validation from a complete PR, push or merge-group diff."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path, PurePosixPath


sys.path.insert(0, str(Path(__file__).resolve().parent))
from source_evidence import CANONICAL_REPOSITORY, GitHubApi, PromotionError, find_source_evidence


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
    if event == "push" and source and not force:
        repository = os.environ["GITHUB_REPOSITORY"]
        if repository != CANONICAL_REPOSITORY:
            # A fork's main mirrors commits that the canonical repository validates.
            source = False
            print(f"Push outside {CANONICAL_REPOSITORY}; source validation runs there or by explicit dispatch")
        else:
            try:
                api = GitHubApi(os.environ.get("GITHUB_API_URL", "https://api.github.com"),
                                repository, os.environ["GITHUB_TOKEN"])
                evidence = find_source_evidence(api, os.environ["GITHUB_SHA"], other_commit=True)
            except PromotionError as error:
                print(f"No reusable same-tree source evidence; requiring full validation: {error}")
            else:
                source = False
                print(f"Reusing same-tree Source Validation {evidence['run_id']}: {evidence['run_url']}")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"source={str(source).lower()}\n")
    print("Full cross-platform source validation" if source else "Workflow policy validation; no runtime compilation")


if __name__ == "__main__":
    main()
