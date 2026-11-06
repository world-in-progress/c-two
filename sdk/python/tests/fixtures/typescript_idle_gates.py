"""Private process protocol for the generated Node idle-eviction gates."""
from __future__ import annotations

from contextlib import contextmanager
import json
from pathlib import Path
import subprocess
from typing import Any, Callable, Iterator

from tests.fixtures.process_control import readline_with_timeout


@contextmanager
def node_gate_client(
    project: tuple[Path, str], work: Path, address: str, ipc_root: str | None,
) -> Iterator[Callable[..., dict[str, Any]]]:
    root, node = project
    fixture = Path(__file__).with_suffix(".mjs")
    config = work / "node-gates.json"
    config.write_text(json.dumps({
        "address": address, "ipcRoot": ipc_root,
        "managerModule": str(root / "dist/manager/typescript/c_two_contract.js"),
        "builderModule": str(root / "dist/builder/typescript/c_two_contract.js"),
    }), encoding="utf-8")
    # Resolve the native package beside the generated modules without editing
    # the existing persistent-IPC fixture or any shared test helper.
    script = root / fixture.name
    script.write_bytes(fixture.read_bytes())
    with (work / "node-gates.stderr").open("w", encoding="utf-8") as errors:
        process = subprocess.Popen(
            [node, str(script), str(config)], cwd=root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors,
            text=True, encoding="utf-8", bufsize=1,
        )
        assert process.stdin is not None and process.stdout is not None

        def request(op: str = "call", route: str = "manager") -> dict[str, Any]:
            process.stdin.write(json.dumps({"op": op, "route": route}) + "\n")
            process.stdin.flush()
            line = readline_with_timeout(process.stdout, 20)
            assert line, f"Node exited: {work}/node-gates.stderr"
            with (work / "node-gates.jsonl").open("a", encoding="utf-8") as log:
                log.write(line)
            return json.loads(line)

        try:
            assert json.loads(readline_with_timeout(process.stdout, 20)) == {"ready": True}
            yield request
        finally:
            try:
                if process.poll() is None:
                    cleaned = request("exit")
                    assert cleaned["ok"] and cleaned["stats"]["used_operations"] == "0", cleaned
                    process.stdin.close()
                    assert process.wait(timeout=10) == 0
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                process.stdin.close()
                process.stdout.close()
