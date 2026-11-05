"""Write the TypeScript project consumed by the Node cross-language tests."""

import json
from pathlib import Path


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
