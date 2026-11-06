"""Write the TypeScript project consumed by the Node cross-language tests."""

import json
from pathlib import Path
import sys

import pytest


def require_c2_mem_node_build(repository: Path) -> None:
    package = repository / "core/foundation/c2-mem-ffi/bindings/typescript"
    library = {"darwin": "libc2_mem_ffi.dylib", "linux": "libc2_mem_ffi.so", "win32": "c2_mem_ffi.dll"}[sys.platform]
    missing = [path for path in (
        package / "dist/index.js", package / "dist/index.d.ts",
        package / "dist/native/c2_mem_ffi_node.node", package / "dist/native" / library,
    ) if not path.is_file()]
    if missing:
        pytest.fail(
            "c2-mem-ffi Node binding is not built: " + ", ".join(map(str, missing))
            + ". From the checkout root, run `npm ci --ignore-scripts --prefix core/foundation/c2-mem-ffi/bindings/typescript` "
            "then `node core/foundation/c2-mem-ffi/bindings/typescript/scripts/build-runtime.mjs`.",
            pytrace=False,
        )


def require_fastdb_node_build(package: Path) -> None:
    missing = [path for path in (
        package / "dist/index.js", package / "dist/payload/index.d.ts",
        package / "src/wasm/fastdb4ts.wasm",
    ) if not path.is_file()]
    if missing:
        pytest.fail(
            "fastdb4ts is not built: " + ", ".join(map(str, missing))
            + ". With the pinned sibling FastDB checkout and Emscripten prepared, run "
            "`npm ci --prefix ../fastdb/ts/fastdb4ts`, "
            "`npm run build:wasm --prefix ../fastdb/ts/fastdb4ts` and "
            "`npm run build --prefix ../fastdb/ts/fastdb4ts` from the checkout root.",
            pytrace=False,
        )


def write_node_tsconfig(root: Path) -> Path:
    root = root.resolve()
    config = root / "tsconfig.json"
    config.write_text(json.dumps({
        "compilerOptions": {
            "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext",
            "lib": ["ES2022", "DOM"], "strict": True, "skipLibCheck": False,
            "rootDir": str(root / "generated"), "outDir": str(root / "dist"),
            "typeRoots": [str(root / "node_modules/@types")],
        },
        "include": [str(root / "generated/**/*.ts")],
    }, indent=2), encoding="utf-8")
    return config
