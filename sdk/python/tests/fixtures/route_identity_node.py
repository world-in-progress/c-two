"""Compile and control the generated TypeScript transport in a real mesh test."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess

from tests.fixtures.process_control import readline_with_timeout


class NodeDriver:
    def __init__(self, run: Path, config: dict):
        repo = Path(__file__).resolve().parents[4]
        assets = repo / "core/foundation/c2-codegen/assets"
        source = assets / "typescript_transport.ts"
        compiler = repo / "core/foundation/c2-mem-ffi/bindings/typescript/node_modules/typescript/bin/tsc"
        output = run / "node"
        output.mkdir()
        (output / "package.json").write_text('{"type":"module"}\n')
        build = subprocess.run(
            ["node", str(compiler), "--strict", "--target", "ES2022", "--module", "ES2022",
             "--moduleResolution", "Bundler", "--lib", "ES2022,DOM", "--rootDir", str(assets),
             "--outDir", str(output), "--typeRoots", str(output / "node_modules/@types"),
             str(source)],
            capture_output=True, text=True, timeout=60,
        )
        assert build.returncode == 0, build.stdout + build.stderr
        self.config_path = run / "node-config.json"
        self.config_path.write_text(json.dumps(config))
        self.error_path = run / "node.stderr"
        self.errors = self.error_path.open("w+", encoding="utf-8")
        self.process = subprocess.Popen(
            ["node", str(Path(__file__).with_suffix(".mjs")),
             str(output / "typescript_transport.js"), str(self.config_path)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errors, text=True,
        )

    def command(self, step: str, **requests: str) -> dict:
        self.process.stdin.write(json.dumps({"step": step, **requests}) + "\n")
        self.process.stdin.flush()
        line = readline_with_timeout(self.process.stdout, 20)
        assert line, self.error_path.read_text()
        return json.loads(line)

    def close(self) -> None:
        try:
            if self.process.poll() is None:
                assert self.command("close") == {"closed": True}
                assert self.process.wait(timeout=5) == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
            self.process.stdout.close()
            self.process.stdin.close()
            self.errors.close()
