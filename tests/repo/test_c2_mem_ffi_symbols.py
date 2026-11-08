"""Keep the public C declarations aligned with the Node ABI loader."""

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[2]
HEADER = ROOT / "core/foundation/c2-mem-ffi/include/c2_mem_ffi.h"
LOADER = ROOT / "core/foundation/c2-mem-ffi/bindings/typescript/native/node_c2_mem_ffi_loader.c"


def test_header_declarations_match_required_loader_symbols() -> None:
    header_symbols = re.findall(
        r"\b(?:C2MemFfiStatus|uint32_t|void|const char\s*\*)\s*(c2_mem_ffi_\w+)\s*\(",
        HEADER.read_text(),
    )
    required_symbols = re.findall(
        r'LOAD_REQUIRED\(\w+, "(c2_mem_ffi_\w+)"\)',
        LOADER.read_text(),
    )

    assert len(header_symbols) == len(set(header_symbols))
    assert len(required_symbols) == len(set(required_symbols))
    assert len(header_symbols) == len(required_symbols) == 28
    assert set(header_symbols) == set(required_symbols)
