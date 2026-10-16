"""Method name/index dispatch table for the Python resource bridge."""
from __future__ import annotations


class MethodTable:
    """Maps method names to 2-byte indices (and back).

    Built during handshake and used per-request to replace method name
    strings with compact indices.
    """

    def __init__(self) -> None:
        self._name_to_idx: dict[str, int] = {}
        self._idx_to_name: dict[int, str] = {}

    @classmethod
    def from_methods(cls, method_names: list[str]) -> MethodTable:
        """Create a table assigning sequential indices to method names."""
        table = cls()
        for i, name in enumerate(method_names):
            table._name_to_idx[name] = i
            table._idx_to_name[i] = name
        return table

    def add(self, name: str, idx: int) -> None:
        self._name_to_idx[name] = idx
        self._idx_to_name[idx] = name

    def index_of(self, name: str) -> int:
        return self._name_to_idx[name]

    def name_of(self, idx: int) -> str:
        return self._idx_to_name[idx]

    def has_name(self, name: str) -> bool:
        return name in self._name_to_idx

    def has_index(self, idx: int) -> bool:
        return idx in self._idx_to_name

    def __len__(self) -> int:
        return len(self._name_to_idx)

    def names(self) -> list[str]:
        return list(self._name_to_idx.keys())
