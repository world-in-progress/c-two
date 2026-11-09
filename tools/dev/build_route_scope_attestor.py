#!/usr/bin/env python3
"""Build the test-only IPC registration attestor before starting pytest."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def main() -> None:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "core/target")).resolve()
    project = target / "route-scope-attestor"
    project.mkdir(parents=True, exist_ok=True)
    source = ROOT / "sdk/python/tests/fixtures/route_scope_attestor.rs"
    (project / "Cargo.toml").write_text(
        '[package]\nname="c2-route-scope-attestor"\nversion="0.0.0"\nedition="2024"\n'
        'rust-version="1.95"\n[workspace]\n'
        f'[[bin]]\nname="c2-route-scope-attestor"\npath={json.dumps(str(source))}\n'
        '[dependencies]\n'
        f'c2-config={{path={json.dumps(str(ROOT / "core/foundation/c2-config"))}}}\n'
        f'c2-ipc={{path={json.dumps(str(ROOT / "core/transport/c2-ipc"))}}}\n'
        'tokio={version="1",features=["rt","macros"]}\n',
        encoding="utf-8",
    )
    # Start from the workspace's pinned dependency graph, including on a cold
    # runner. Cargo adds this standalone test package to its copy of the lock.
    (project / "Cargo.lock").write_bytes((ROOT / "core/Cargo.lock").read_bytes())
    subprocess.run(
        ["cargo", "build", "--manifest-path", str(project / "Cargo.toml")],
        cwd=ROOT, env={**os.environ, "CARGO_TARGET_DIR": str(target)}, check=True,
    )
    name = "c2-route-scope-attestor.exe" if os.name == "nt" else "c2-route-scope-attestor"
    print(target / "debug" / name)


if __name__ == "__main__":
    main()
